//! Driver for the VL53L7CX multizone time-of-flight sensor by
//! STMicroelectronics, for its motion indicator in autonomous ranging mode.
//!
//! It is a port of the parts of ST's ultra lite driver (ULD) 2.0.1 that this
//! use needs: initialisation, which uploads the sensor firmware, the ranging
//! settings, the motion indicator and detection threshold plugins, and
//! reading the motion indicator results. The functions follow the C sources
//! (vendored in knixx-go's `src/internal/vl53l7cx/VL53L7CX_ULD_driver_2.0.1`)
//! step by step, and a test compares their I2C traffic with that of the C
//! driver.
//!
//! Only the motion indicator output is enabled, with one target per zone.
//!
//! User manual UM3038 and datasheet DS13865.
#![no_std]

#[cfg(test)]
extern crate std;

use core::fmt;

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::I2c;

mod firmware;
mod motion;
#[cfg(test)]
mod tests;

use firmware::{DEFAULT_CONFIGURATION, DEFAULT_XTALK, FIRMWARE, GET_NVM_CMD};
pub use motion::{MAX_MOTION_DISTANCE, MAX_MOTION_SPAN, MIN_MOTION_DISTANCE, valid_motion_distance};

/// The default 7 bit I2C address (0x52 in 8 bit notation).
pub const ADDRESS: u8 = 0x29;

/// Resolution 4x4, in zones.
pub const RESOLUTION_4X4: u8 = 16;
/// Resolution 8x8, in zones.
pub const RESOLUTION_8X8: u8 = 64;

/// Continuous ranging mode.
pub const RANGING_CONTINUOUS: u8 = 1;
/// Autonomous ranging mode.
pub const RANGING_AUTONOMOUS: u8 = 3;

/// The largest motion threshold, which the sensor stores multiplied by
/// 65535 in a signed 32 bit value.
pub const MAX_MOTION_THRESHOLD: u16 = 32767;

/// A VL53L7CX error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The I2C bus failed.
    Bus(E),
    /// No sensor answers; returned by callers of [`Device::is_alive`].
    NotAlive,
    /// The sensor did not answer in time.
    Timeout,
    /// The sensor's MCU reported an error, with its status when known.
    Mcu(Option<u8>),
    /// A parameter out of range, or a call out of order.
    Invalid,
    /// The firmware disagrees on the size of the output data.
    DataSize,
    /// A frame whose header and footer do not match.
    Corrupted,
    /// A firmware data transfer larger than the temporary buffer.
    BufferSize,
}

impl<E: fmt::Debug> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bus(e) => write!(f, "vl53l7cx: {e:?}"),
            Error::NotAlive => f.write_str("vl53l7cx: no sensor found"),
            Error::Timeout => f.write_str("vl53l7cx: timeout"),
            Error::Mcu(None) => f.write_str("vl53l7cx: MCU error"),
            Error::Mcu(Some(s)) => write!(f, "vl53l7cx: MCU error {s:#x}"),
            Error::Invalid => f.write_str("vl53l7cx: invalid parameter"),
            Error::DataSize => f.write_str("vl53l7cx: unexpected output data size"),
            Error::Corrupted => f.write_str("vl53l7cx: corrupted frame"),
            Error::BufferSize => f.write_str("vl53l7cx: temporary buffer too small"),
        }
    }
}

impl<E: fmt::Debug> core::error::Error for Error<E> {}

const NVM_DATA_SIZE: usize = 492;
const OFFSET_SIZE: usize = 488;
const XTALK_SIZE: usize = 776;
const NB_TARGETS_ZONE: u8 = 1;

/// The size of the ULD's temporary buffer.
const BUF_SIZE: usize = 1024;

const UI_CMD_STATUS: u16 = 0x2C00;
const UI_CMD_START: u16 = 0x2C04;
const UI_CMD_END: u16 = 0x2FFF;

const DCI_ZONE_CONFIG: u16 = 0x5450;
const DCI_FREQ_HZ: u16 = 0x5458;
const DCI_INT_TIME: u16 = 0x545C;
const DCI_RANGING_MODE: u16 = 0xAD30;
const DCI_DSS_CONFIG: u16 = 0xAD38;
const DCI_MOTION_CONFIG: u16 = 0xBFAC;
const DCI_SINGLE_RANGE: u16 = 0xD964;
const DCI_OUTPUT_CONFIG: u16 = 0xD968;
const DCI_OUTPUT_ENABLES: u16 = 0xD970;
const DCI_OUTPUT_LIST: u16 = 0xD980;
const DCI_PIPE_CONTROL: u16 = 0xDB80;
const DCI_GLARE_FILTER: u16 = 0xE108;

const MOTION_IDX: u16 = 0xD858;

/// The block headers of the possible outputs, for one target per zone, in
/// the order of the ULD.
const OUTPUTS: [u32; 12] = [
    0x0000000D, // start
    0x54B400C0, // metadata
    0x54C00040, // common data
    0x54D00104, // ambient rate
    0x55D00404, // SPAD count
    0xDB840401, // number of targets detected
    0xDBC40404, // signal rate
    0xDEC40402, // range sigma
    0xDF440402, // distance
    0xE0440401, // reflectance
    0xE0840401, // target status
    0xD85808C0, // motion indicator
];

/// Enables the mandatory outputs (start, metadata and common data) and the
/// motion indicator.
const OUTPUT_ENABLES: [u32; 4] = [0x00000007 | 1 << 11, 0, 0, 0xC0000000];

/// The motion indicator results of a frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Motion {
    pub global1: u32,
    pub global2: u32,
    pub status: u8,
    pub nb_detected: u8,
    pub nb_aggregates: u8,
    /// The motion intensity per aggregate. In resolution 4x4 aggregate i is
    /// zone i.
    pub values: [u32; 32],
}

/// The fields of the motion indicator configuration that vary; the rest is
/// constant.
struct MotionConfig {
    /// 0 until motion_indicator_init.
    resolution: u8,
    ref_bin_offset: i32,
    feature_length: u8,
}

/// A VL53L7CX on an I2C bus. It holds about 2.3 KB of buffers, so keep it
/// off the stack (in a static cell).
pub struct Device<I, D> {
    bus: I,
    delay: D,
    pub address: u8,

    offset: [u8; OFFSET_SIZE],
    xtalk: [u8; XTALK_SIZE],
    stream_count: u8,
    data_read_size: u32,
    motion: MotionConfig,

    /// A register address followed by the ULD's temporary buffer, so that the
    /// buffer is written without copying it.
    tx: [u8; 2 + BUF_SIZE],
    /// The transfers of single bytes.
    small: [u8; 3],
}

/// The offset of the temporary buffer in tx.
const B: usize = 2;

fn put32(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
}

fn get32(b: &[u8]) -> u32 {
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

/// Reverses the byte order of each 32 bit word of b, converting between the
/// host format of the ULD and the sensor's.
fn swap(b: &mut [u8]) {
    for w in b.as_chunks_mut::<4>().0 {
        w.reverse();
    }
}

/// Word i of the grid g: unsigned for size 4, signed for 2.
fn grid_word(g: &[u8], size: usize, i: usize) -> i64 {
    if size == 4 {
        i64::from(get32(&g[4 * i..]))
    } else {
        i64::from(i16::from_le_bytes([g[2 * i], g[2 * i + 1]]))
    }
}

/// Sets word i of the grid g to v.
fn set_grid_word(g: &mut [u8], size: usize, i: usize, v: i64) {
    if size == 4 {
        put32(&mut g[4 * i..], v as u32);
    } else {
        g[2 * i..2 * i + 2].copy_from_slice(&(v as i16).to_le_bytes());
    }
}

/// Replaces the first 16 of the 64 words of the 8x8 grid g, whose words have
/// size bytes, by the averages of the 2x2 blocks of zones, and zeroes the
/// other 48. It follows the ULD's 4x4 extrapolation.
fn average_grid(g: &mut [u8], size: usize) {
    for j in 0..4 {
        for i in 0..4 {
            let k = 2 * i + 16 * j;
            let sum = grid_word(g, size, k)
                + grid_word(g, size, k + 1)
                + grid_word(g, size, k + 8)
                + grid_word(g, size, k + 9);
            if size == 4 {
                // unsigned 32 bit arithmetic, as in C
                set_grid_word(g, size, i + 4 * j, i64::from(sum as u32 / 4));
            } else {
                set_grid_word(g, size, i + 4 * j, sum / 4);
            }
        }
    }
    g[16 * size..64 * size].fill(0);
}

impl<I: I2c, D: DelayNs> Device<I, D> {
    /// Returns a device on bus at the default address, waiting with delay.
    pub fn new(bus: I, delay: D) -> Self {
        Device {
            bus,
            delay,
            address: ADDRESS,
            offset: [0; OFFSET_SIZE],
            xtalk: [0; XTALK_SIZE],
            stream_count: 0,
            data_read_size: 0,
            motion: MotionConfig {
                resolution: 0,
                ref_bin_offset: 0,
                feature_length: 0,
            },
            tx: [0; 2 + BUF_SIZE],
            small: [0; 3],
        }
    }

    /// The temporary buffer.
    fn buf(&mut self) -> &mut [u8] {
        &mut self.tx[B..]
    }

    async fn wait(&mut self, ms: u32) {
        self.delay.delay_ms(ms).await;
    }

    async fn wr_byte(&mut self, reg: u16, v: u8) -> Result<(), Error<I::Error>> {
        let [hi, lo] = reg.to_be_bytes();
        self.small = [hi, lo, v];
        self.bus
            .write(self.address, &self.small)
            .await
            .map_err(Error::Bus)
    }

    async fn rd_byte(&mut self, reg: u16) -> Result<u8, Error<I::Error>> {
        let [hi, lo] = reg.to_be_bytes();
        self.small[0] = hi;
        self.small[1] = lo;
        let (w, r) = self.small.split_at_mut(2);
        self.bus
            .write_read(self.address, w, r)
            .await
            .map_err(Error::Bus)?;
        Ok(self.small[2])
    }

    /// Reads n bytes at reg into the start of the temporary buffer.
    async fn rd_buf(&mut self, reg: u16, n: usize) -> Result<(), Error<I::Error>> {
        let a = reg.to_be_bytes();
        self.bus
            .write_read(self.address, &a, &mut self.tx[B..B + n])
            .await
            .map_err(Error::Bus)
    }

    /// Writes the first n bytes of the temporary buffer to reg.
    async fn wr_buf(&mut self, reg: u16, n: usize) -> Result<(), Error<I::Error>> {
        self.tx[..B].copy_from_slice(&reg.to_be_bytes());
        self.bus
            .write(self.address, &self.tx[..B + n])
            .await
            .map_err(Error::Bus)
    }

    /// Writes data, which may be larger than the temporary buffer, to reg and
    /// the registers following it, in chunks that go through the temporary
    /// buffer.
    async fn wr_string(&mut self, mut reg: u16, data: &[u8]) -> Result<(), Error<I::Error>> {
        for chunk in data.chunks(BUF_SIZE) {
            self.buf()[..chunk.len()].copy_from_slice(chunk);
            self.wr_buf(reg, chunk.len()).await?;
            reg = reg.wrapping_add(chunk.len() as u16);
        }
        Ok(())
    }

    async fn poll_for_answer(
        &mut self,
        size: usize,
        pos: usize,
        reg: u16,
        mask: u8,
        want: u8,
    ) -> Result<(), Error<I::Error>> {
        let mut timeout = 0;
        loop {
            self.rd_buf(reg, size).await?;
            self.wait(10).await;
            if timeout >= 200 {
                return Err(Error::Timeout);
            }
            if size >= 4 && self.tx[B + 2] >= 0x7f {
                return Err(Error::Mcu(None));
            }
            timeout += 1;
            if self.tx[B + pos] & mask == want {
                return Ok(());
            }
        }
    }

    async fn poll_for_mcu_boot(&mut self) -> Result<(), Error<I::Error>> {
        for _ in 0..500 {
            let s0 = self.rd_byte(0x06).await?;
            if s0 & 0x80 != 0 {
                let s1 = self.rd_byte(0x07).await?;
                if s1 != 0 {
                    return Err(Error::Mcu(Some(s1)));
                }
                return Ok(());
            }
            self.wait(1).await;
            if s0 & 0x01 != 0 {
                break;
            }
        }
        Ok(())
    }

    /// Performs register writes of single bytes in order.
    async fn writes(&mut self, regs: &[(u16, u8)]) -> Result<(), Error<I::Error>> {
        for &(reg, v) in regs {
            self.wr_byte(reg, v).await?;
        }
        Ok(())
    }

    /// Reports whether a VL53L7CX answers at the device's address.
    pub async fn is_alive(&mut self) -> Result<bool, Error<I::Error>> {
        self.wr_byte(0x7fff, 0x00).await?;
        let id = self.rd_byte(0x00).await?;
        let rev = self.rd_byte(0x01).await?;
        self.wr_byte(0x7fff, 0x02).await?;
        Ok(id == 0xF0 && rev == 0x02)
    }

    /// Boots the sensor: it uploads the firmware, which takes about 2 s at
    /// 400 kHz, loads the offset calibration from the sensor's NVM and sends
    /// the default configuration. The sensor is then in resolution 4x4, at
    /// 1 Hz.
    pub async fn init(&mut self) -> Result<(), Error<I::Error>> {
        // SW reboot sequence
        self.writes(&[(0x7fff, 0x00), (0x0009, 0x04), (0x000F, 0x40), (0x000A, 0x03)])
            .await?;
        self.rd_byte(0x7FFF).await?;
        self.writes(&[
            (0x000C, 0x01),
            (0x0101, 0x00),
            (0x0102, 0x00),
            (0x010A, 0x01),
            (0x4002, 0x01),
            (0x4002, 0x00),
            (0x010A, 0x03),
            (0x0103, 0x01),
            (0x000C, 0x00),
            (0x000F, 0x43),
        ])
        .await?;
        self.wait(1).await;
        self.writes(&[(0x000F, 0x40), (0x000A, 0x01)]).await?;
        self.wait(100).await;

        // Wait for the sensor to boot
        self.wr_byte(0x7fff, 0x00).await?;
        self.poll_for_answer(1, 0, 0x06, 0xff, 1).await?;

        self.writes(&[
            (0x000E, 0x01),
            (0x7fff, 0x02),
            // Enable FW access
            (0x03, 0x0D),
            (0x7fff, 0x01),
        ])
        .await?;
        self.poll_for_answer(1, 0, 0x21, 0x10, 0x10).await?;
        self.wr_byte(0x7fff, 0x00).await?;
        // Enable host access to GO1
        self.rd_byte(0x7fff).await?;
        self.writes(&[
            (0x0C, 0x01),
            // Power ON status
            (0x7fff, 0x00),
            (0x101, 0x00),
            (0x102, 0x00),
            (0x010A, 0x01),
            (0x4002, 0x01),
            (0x4002, 0x00),
            (0x010A, 0x03),
            (0x103, 0x01),
            (0x400F, 0x00),
            (0x21A, 0x43),
            (0x21A, 0x03),
            (0x21A, 0x01),
            (0x21A, 0x00),
            (0x219, 0x00),
            (0x21B, 0x00),
            // Wake up MCU
            (0x7fff, 0x00),
        ])
        .await?;
        self.rd_byte(0x7fff).await?;
        self.writes(&[(0x0C, 0x00), (0x7fff, 0x01), (0x20, 0x07), (0x20, 0x06)])
            .await?;

        // Download the firmware into the VL53L7CX
        for (i, page) in [0x09u8, 0x0a, 0x0b].into_iter().enumerate() {
            self.wr_byte(0x7fff, page).await?;
            let end = ((i + 1) * 0x8000).min(FIRMWARE.len());
            self.wr_string(0, &FIRMWARE[i * 0x8000..end]).await?;
        }
        // Check that the firmware was downloaded correctly
        self.writes(&[(0x7fff, 0x01), (0x7fff, 0x02), (0x03, 0x0D), (0x7fff, 0x01)])
            .await?;
        self.poll_for_answer(1, 0, 0x21, 0x10, 0x10).await?;

        // Reset the MCU and wait for it to boot
        self.wr_byte(0x7fff, 0x00).await?;
        self.rd_byte(0x7fff).await?;
        self.writes(&[
            (0x0C, 0x01),
            (0x7FFF, 0x00),
            (0x114, 0x00),
            (0x115, 0x00),
            (0x116, 0x42),
            (0x117, 0x00),
            (0x0B, 0x00),
        ])
        .await?;
        self.rd_byte(0x7fff).await?;
        self.writes(&[(0x0C, 0x00), (0x0B, 0x01)]).await?;
        self.poll_for_mcu_boot().await?;

        // Get the offset NVM data and send it
        self.wr_byte(0x7fff, 0x02).await?;
        self.wr_string(0x2fd8, GET_NVM_CMD).await?;
        self.poll_for_answer(4, 0, UI_CMD_STATUS, 0xff, 2).await?;
        self.rd_buf(UI_CMD_START, NVM_DATA_SIZE).await?;
        self.offset.copy_from_slice(&self.tx[B..B + OFFSET_SIZE]);
        self.send_offset_data(RESOLUTION_4X4).await?;

        // Send the default Xtalk shape
        self.xtalk.copy_from_slice(DEFAULT_XTALK);
        self.send_xtalk_data(RESOLUTION_4X4).await?;

        // Send the default configuration
        self.wr_string(0x2c34, DEFAULT_CONFIGURATION).await?;
        self.poll_for_answer(4, 1, UI_CMD_STATUS, 0xff, 0x03).await?;
        self.dci_write(DCI_PIPE_CONTROL, &[NB_TARGETS_ZONE, 0x00, 0x01, 0x00])
            .await?;
        self.dci_write(DCI_SINGLE_RANGE, &1u32.to_le_bytes()).await?;
        self.dci_replace(DCI_GLARE_FILTER, 40, &[1], 0x26).await?;
        self.dci_replace(DCI_GLARE_FILTER, 40, &[1], 0x25).await
    }

    async fn send_offset_data(&mut self, resolution: u8) -> Result<(), Error<I::Error>> {
        let b = &mut self.tx[B..];
        b[..OFFSET_SIZE].copy_from_slice(&self.offset);

        // Data extrapolation is required for 4x4 offset
        if resolution == RESOLUTION_4X4 {
            let dss = [0x0F, 0x04, 0x04, 0x00, 0x08, 0x10, 0x10, 0x07];
            b[0x10..0x18].copy_from_slice(&dss);
            swap(&mut b[..OFFSET_SIZE]);
            average_grid(&mut b[0x3C..], 4);
            average_grid(&mut b[0x140..], 2);
            swap(&mut b[..OFFSET_SIZE]);
        }

        b.copy_within(8..OFFSET_SIZE + 4, 0);
        let footer = [0x00, 0x00, 0x00, 0x0F, 0x03, 0x01, 0x01, 0xE4];
        b[0x1E0..0x1E8].copy_from_slice(&footer);
        self.wr_buf(0x2e18, OFFSET_SIZE).await?;
        self.poll_for_answer(4, 1, UI_CMD_STATUS, 0xff, 0x03).await
    }

    async fn send_xtalk_data(&mut self, resolution: u8) -> Result<(), Error<I::Error>> {
        let b = &mut self.tx[B..];
        b[..XTALK_SIZE].copy_from_slice(&self.xtalk);

        // Data extrapolation is required for 4x4 Xtalk
        if resolution == RESOLUTION_4X4 {
            let res = [0x0F, 0x04, 0x04, 0x17, 0x08, 0x10, 0x10, 0x07];
            let dss = [0x00, 0x78, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08];
            let profile = [0xA0, 0xFC, 0x01, 0x00];
            b[0x08..0x10].copy_from_slice(&res);
            b[0x20..0x28].copy_from_slice(&dss);
            swap(&mut b[..XTALK_SIZE]);
            average_grid(&mut b[0x34..], 4);
            swap(&mut b[..XTALK_SIZE]);
            b[0x134..0x138].copy_from_slice(&profile);
            b[0x78..0x7C].fill(0);
        }

        self.wr_buf(0x2cf8, XTALK_SIZE).await?;
        self.poll_for_answer(4, 1, UI_CMD_STATUS, 0xff, 0x03).await
    }

    /// Reads size bytes of firmware data at index into the start of the
    /// temporary buffer, in host format.
    async fn dci_read(&mut self, index: u16, size: usize) -> Result<(), Error<I::Error>> {
        if size + 12 > BUF_SIZE {
            return Err(Error::BufferSize);
        }
        let [hi, lo] = index.to_be_bytes();
        let cmd = [
            hi,
            lo,
            ((size & 0xff0) >> 4) as u8,
            ((size & 0xf) << 4) as u8,
            0x00,
            0x00,
            0x00,
            0x0f,
            0x00,
            0x02,
            0x00,
            0x08,
        ];
        self.buf()[..cmd.len()].copy_from_slice(&cmd);

        // Request data reading from FW
        self.wr_buf(UI_CMD_END - 11, cmd.len()).await?;
        self.poll_for_answer(4, 1, UI_CMD_STATUS, 0xff, 0x03).await?;

        // Read the data sent (4 bytes header + data + 8 bytes footer)
        self.rd_buf(UI_CMD_START, size + 12).await?;
        let b = self.buf();
        swap(&mut b[..size + 12]);
        b.copy_within(4..4 + size, 0);
        Ok(())
    }

    /// Writes data, in host format, to the firmware at index.
    async fn dci_write(&mut self, index: u16, data: &[u8]) -> Result<(), Error<I::Error>> {
        let size = data.len();
        if size + 12 > BUF_SIZE {
            return Err(Error::BufferSize);
        }
        self.buf()[4..4 + size].copy_from_slice(data);
        self.dci_write_buf(index, size).await
    }

    /// Writes the size bytes at offset 4 of the temporary buffer, in host
    /// format, to the firmware at index.
    async fn dci_write_buf(&mut self, index: u16, size: usize) -> Result<(), Error<I::Error>> {
        let b = self.buf();
        swap(&mut b[4..4 + size]);
        let [hi, lo] = index.to_be_bytes();
        b[0] = hi;
        b[1] = lo;
        b[2] = ((size & 0xff0) >> 4) as u8;
        b[3] = ((size & 0xf) << 4) as u8;
        let footer = [
            0x00,
            0x00,
            0x00,
            0x0f,
            0x05,
            0x01,
            ((size + 8) >> 8) as u8,
            (size + 8) as u8,
        ];
        b[4 + size..12 + size].copy_from_slice(&footer);

        self.wr_buf((usize::from(UI_CMD_END) - (size + 12) + 1) as u16, size + 12)
            .await?;
        self.poll_for_answer(4, 1, UI_CMD_STATUS, 0xff, 0x03).await
    }

    /// Reads size bytes at index, replaces those at pos by data and writes
    /// them back.
    async fn dci_replace(
        &mut self,
        index: u16,
        size: usize,
        data: &[u8],
        pos: usize,
    ) -> Result<(), Error<I::Error>> {
        self.dci_read(index, size).await?;
        let b = self.buf();
        b[pos..pos + data.len()].copy_from_slice(data);
        b.copy_within(0..size, 4);
        self.dci_write_buf(index, size).await
    }

    /// Sets the resolution to [`RESOLUTION_4X4`] or [`RESOLUTION_8X8`]. It
    /// must be called before set_ranging_frequency.
    pub async fn set_resolution(&mut self, resolution: u8) -> Result<(), Error<I::Error>> {
        let (dss, zones, size) = match resolution {
            RESOLUTION_4X4 => (64, 4, 8),
            RESOLUTION_8X8 => (16, 8, 4),
            _ => return Err(Error::Invalid),
        };
        self.dci_read(DCI_DSS_CONFIG, 16).await?;
        let b = self.buf();
        b[0x04] = dss;
        b[0x06] = dss;
        b[0x09] = if resolution == RESOLUTION_8X8 { 1 } else { 4 };
        b.copy_within(0..16, 4);
        self.dci_write_buf(DCI_DSS_CONFIG, 16).await?;
        self.dci_read(DCI_ZONE_CONFIG, 8).await?;
        let b = self.buf();
        b[0x00] = zones;
        b[0x01] = zones;
        b[0x04] = size;
        b[0x05] = size;
        b.copy_within(0..8, 4);
        self.dci_write_buf(DCI_ZONE_CONFIG, 8).await?;
        self.send_offset_data(resolution).await?;
        self.send_xtalk_data(resolution).await
    }

    /// Sets the frame rate: 1 to 60 Hz in resolution 4x4, 1 to 15 Hz in 8x8.
    /// Only the 4x4 range is checked.
    pub async fn set_ranging_frequency(&mut self, hz: u8) -> Result<(), Error<I::Error>> {
        if !(1..=60).contains(&hz) {
            return Err(Error::Invalid);
        }
        self.dci_replace(DCI_FREQ_HZ, 4, &[hz], 0x01).await
    }

    /// Sets the time the VCSEL emits per frame in autonomous mode, from 2 to
    /// 1000 ms.
    pub async fn set_integration_time(&mut self, ms: u32) -> Result<(), Error<I::Error>> {
        if !(2..=1000).contains(&ms) {
            return Err(Error::Invalid);
        }
        self.dci_replace(DCI_INT_TIME, 20, &(ms * 1000).to_le_bytes(), 0x00)
            .await
    }

    /// Sets [`RANGING_CONTINUOUS`] or [`RANGING_AUTONOMOUS`].
    pub async fn set_ranging_mode(&mut self, mode: u8) -> Result<(), Error<I::Error>> {
        let (mode1, mode3, single_range) = match mode {
            RANGING_CONTINUOUS => (0x1, 0x3, 0x00u32),
            RANGING_AUTONOMOUS => (0x3, 0x2, 0x01u32),
            _ => return Err(Error::Invalid),
        };
        self.dci_read(DCI_RANGING_MODE, 8).await?;
        let b = self.buf();
        b[0x01] = mode1;
        b[0x03] = mode3;
        b.copy_within(0..8, 4);
        self.dci_write_buf(DCI_RANGING_MODE, 8).await?;
        self.dci_write(DCI_SINGLE_RANGE, &single_range.to_le_bytes())
            .await
    }

    /// Starts a ranging session. The settings cannot be changed while it
    /// runs.
    pub async fn start_ranging(&mut self) -> Result<(), Error<I::Error>> {
        // The ULD reads the resolution, which only sizes per zone outputs.
        self.dci_read(DCI_ZONE_CONFIG, 8).await?;
        self.data_read_size = 0;
        self.stream_count = 255;

        for (i, &out) in OUTPUTS.iter().enumerate() {
            if out == 0 || OUTPUT_ENABLES[i / 32] & (1 << (i % 32)) == 0 {
                continue;
            }
            let (typ, size) = (out & 0xf, (out >> 4) & 0xfff);
            if (0x1..0x0d).contains(&typ) {
                // Per zone outputs are resized to the resolution; none is
                // enabled.
                return Err(Error::Invalid);
            }
            self.data_read_size += size + 4;
        }
        self.data_read_size += 24;

        let b = self.buf();
        for (k, &v) in OUTPUTS.iter().enumerate() {
            put32(&mut b[4 + 4 * k..], v);
        }
        self.dci_write_buf(DCI_OUTPUT_LIST, 4 * OUTPUTS.len()).await?;
        let size = self.data_read_size;
        let b = self.buf();
        put32(&mut b[4..], size);
        put32(&mut b[8..], OUTPUTS.len() as u32 + 1);
        self.dci_write_buf(DCI_OUTPUT_CONFIG, 8).await?;
        let b = self.buf();
        for (k, &v) in OUTPUT_ENABLES.iter().enumerate() {
            put32(&mut b[4 + 4 * k..], v);
        }
        self.dci_write_buf(DCI_OUTPUT_ENABLES, 4 * OUTPUT_ENABLES.len())
            .await?;

        // Start xshut bypass (interrupt mode)
        self.writes(&[(0x7fff, 0x00), (0x09, 0x05), (0x7fff, 0x02)])
            .await?;

        // Start the ranging session
        let cmd = [0x00, 0x03, 0x00, 0x00];
        self.buf()[..4].copy_from_slice(&cmd);
        self.wr_buf(UI_CMD_END - (4 - 1), cmd.len()).await?;
        self.poll_for_answer(4, 1, UI_CMD_STATUS, 0xff, 0x03).await?;

        // Check that the firmware agrees on the data size
        self.dci_read(0x5440, 12).await?;
        let b = &self.tx[B..];
        if u32::from(u16::from_le_bytes([b[0x8], b[0x9]])) != self.data_read_size {
            return Err(Error::DataSize);
        }
        Ok(())
    }

    /// Stops the ranging session.
    pub async fn stop_ranging(&mut self) -> Result<(), Error<I::Error>> {
        self.rd_buf(0x2FFC, 4).await?;
        if get32(&self.tx[B..]) != 0x4FF {
            self.writes(&[
                (0x7fff, 0x00),
                // Provoke MCU stop
                (0x15, 0x16),
                (0x14, 0x01),
            ])
            .await?;
            // Poll for G02 status 0 MCU stop
            let mut timeout = 0;
            let mut tmp = 0u8;
            while tmp & 0x80 == 0 {
                tmp = self.rd_byte(0x6).await?;
                self.wait(10).await;
                timeout += 1;
                if timeout > 500 {
                    return Err(Error::Timeout);
                }
            }
        }

        // Check GO2 status 1 if status is still OK
        if self.rd_byte(0x6).await? & 0x80 != 0 {
            let tmp = self.rd_byte(0x7).await?;
            if tmp != 0x84 && tmp != 0x85 {
                return Err(Error::Mcu(Some(tmp)));
            }
        }

        // Undo MCU stop, and stop xshut bypass
        self.writes(&[
            (0x7fff, 0x00),
            (0x14, 0x00),
            (0x15, 0x00),
            (0x09, 0x04),
            (0x7fff, 0x02),
        ])
        .await
    }

    /// Reads the motion indicator results of the latest frame.
    pub async fn motion(&mut self) -> Result<Motion, Error<I::Error>> {
        let mut m = Motion::default();
        let size = self.data_read_size as usize;
        if !(24..=BUF_SIZE).contains(&size) {
            return Err(Error::Invalid);
        }
        self.rd_buf(0x0, size).await?;
        let b = &mut self.tx[B..B + size];
        self.stream_count = b[0];
        swap(b);

        // Block headers follow the 16 byte frame header.
        let mut i = 16;
        while i + 4 <= size {
            let bh = get32(&b[i..]);
            let (typ, idx) = (bh & 0xf, (bh >> 16) as u16);
            let mut msize = ((bh >> 4) & 0xfff) as usize;
            if typ > 0x1 && typ < 0xd {
                msize *= typ as usize;
            }
            if idx == MOTION_IDX && i + 4 + 140 <= size {
                let p = &b[i + 4..];
                m.global1 = get32(p);
                m.global2 = get32(&p[4..]);
                m.status = p[8];
                m.nb_detected = p[9];
                m.nb_aggregates = p[10];
                for (k, v) in m.values.iter_mut().enumerate() {
                    *v = get32(&p[12 + 4 * k..]) / 65535;
                }
            }
            i += 4 + msize;
        }

        // Matching header and footer ids detect corrupted frames.
        if b[0x8] != b[size - 4] || b[0x9] != b[size - 3] {
            return Err(Error::Corrupted);
        }
        Ok(m)
    }
}
