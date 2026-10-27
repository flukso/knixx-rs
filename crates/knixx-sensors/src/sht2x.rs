//! Driver for the SHT2x (SHT20, SHT21, SHT25) digital humidity and
//! temperature sensors by Sensirion.
//!
//! Datasheet:
//! <https://sensirion.com/media/documents/CCDE1377/635000A2/Sensirion_Datasheet_Humidity_Sensor_SHT20.pdf>

use core::fmt;

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::I2c;

/// The fixed I2C address of the SHT2x.
pub const ADDRESS: u8 = 0x40;

// Commands in no hold master mode: the sensor releases the bus while it
// measures.
const CMD_TRIGGER_T: u8 = 0xf3;
const CMD_TRIGGER_RH: u8 = 0xf5;
const CMD_SOFT_RESET: u8 = 0xfe;

// Worst case durations from the datasheet in ms, at the default resolution
// of 14 bit for temperature and 12 bit for humidity.
const RESET_TIME_MS: u32 = 15;
const T_TIME_MS: u32 = 85;
const RH_TIME_MS: u32 = 29;

/// An SHT2x error: a bus error or a measurement whose CRC does not match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The I2C bus failed.
    Bus(E),
    /// A measurement whose CRC does not match.
    Checksum,
}

impl<E: fmt::Debug> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bus(e) => write!(f, "{e:?}"),
            Error::Checksum => f.write_str("sht2x: checksum mismatch"),
        }
    }
}

impl<E: fmt::Debug> core::error::Error for Error<E> {}

/// Device wraps an I2C connection to an SHT2x.
pub struct Device<I, D> {
    bus: I,
    delay: D,
    pub address: u8,
}

impl<I: I2c, D: DelayNs> Device<I, D> {
    /// new returns a device on bus, waiting with delay.
    pub fn new(bus: I, delay: D) -> Self {
        Device {
            bus,
            delay,
            address: ADDRESS,
        }
    }

    /// configure soft resets the sensor, restoring the default resolution.
    pub async fn configure(&mut self) -> Result<(), Error<I::Error>> {
        self.bus
            .write(self.address, &[CMD_SOFT_RESET])
            .await
            .map_err(Error::Bus)?;
        self.delay.delay_ms(RESET_TIME_MS).await;
        Ok(())
    }

    /// read_temperature returns the temperature in milli degrees Celsius.
    pub async fn read_temperature(&mut self) -> Result<i32, Error<I::Error>> {
        let raw = self.measure(CMD_TRIGGER_T, T_TIME_MS).await?;
        Ok((-46850 + 175720 * i64::from(raw) / 65536) as i32)
    }

    /// read_humidity returns the relative humidity in hundredths of a
    /// percent, clamped to 0..10000.
    pub async fn read_humidity(&mut self) -> Result<i32, Error<I::Error>> {
        let raw = self.measure(CMD_TRIGGER_RH, RH_TIME_MS).await?;
        let rh = (-600 + 12500 * i64::from(raw) / 65536) as i32;
        Ok(rh.clamp(0, 10000))
    }

    /// measure triggers a measurement, waits for it to complete and returns
    /// the raw value with the status bits cleared.
    async fn measure(&mut self, cmd: u8, wait_ms: u32) -> Result<u16, Error<I::Error>> {
        self.bus.write(self.address, &[cmd]).await.map_err(Error::Bus)?;
        self.delay.delay_ms(wait_ms).await;
        let mut data = [0u8; 3];
        self.bus.read(self.address, &mut data).await.map_err(Error::Bus)?;
        if crc8(&data[..2]) != data[2] {
            return Err(Error::Checksum);
        }
        Ok(u16::from_be_bytes([data[0], data[1]]) & !3)
    }
}

/// crc8 computes the SHT2x checksum: polynomial x^8+x^5+x^4+1, initial 0.
fn crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x31;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::block_on;
    use embedded_hal_async::i2c::{ErrorKind, ErrorType, NoAcknowledgeSource, Operation};
    use std::vec::Vec;

    const NACK: ErrorKind = ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown);

    /// FakeBus records the bytes written and answers reads with reply.
    #[derive(Default)]
    struct FakeBus {
        addr: u8,
        written: Vec<u8>,
        reply: Vec<u8>,
        err: Option<ErrorKind>,
    }

    impl ErrorType for FakeBus {
        type Error = ErrorKind;
    }

    impl I2c for FakeBus {
        async fn transaction(&mut self, addr: u8, ops: &mut [Operation<'_>]) -> Result<(), ErrorKind> {
            self.addr = addr;
            for op in ops {
                match op {
                    Operation::Write(w) => self.written.extend_from_slice(w),
                    Operation::Read(r) => {
                        let n = r.len().min(self.reply.len());
                        r[..n].copy_from_slice(&self.reply[..n]);
                    }
                }
            }
            self.err.map_or(Ok(()), Err)
        }
    }

    /// FakeDelay records the waits in ms.
    #[derive(Default)]
    struct FakeDelay(Vec<u32>);

    impl DelayNs for FakeDelay {
        async fn delay_ns(&mut self, ns: u32) {
            self.0.push(ns / 1_000_000);
        }
    }

    #[test]
    fn crc() {
        // Example from the datasheet.
        assert_eq!(crc8(&[0x68, 0x3a]), 0x7c);
    }

    #[test]
    fn read_temperature() {
        // The low status bits (0x17 vs 0x14) must not change the result.
        for reply in [[0x66, 0x14, 0x88], [0x66, 0x17, 0xdb]] {
            let mut bus = FakeBus {
                reply: reply.to_vec(),
                ..Default::default()
            };
            let mut delay = FakeDelay::default();
            let got = block_on(Device::new(&mut bus, &mut delay).read_temperature()).unwrap();
            assert_eq!(got, 23217, "{reply:02x?}: temperature {got} m°C, want 23217");
            assert_eq!(bus.addr, ADDRESS);
            assert_eq!(bus.written, [CMD_TRIGGER_T]);
            assert_eq!(delay.0, [85]);
        }
    }

    #[test]
    fn read_humidity() {
        let mut bus = FakeBus {
            reply: std::vec![0x7c, 0x80, 0xf5],
            ..Default::default()
        };
        let mut delay = FakeDelay::default();
        let got = block_on(Device::new(&mut bus, &mut delay).read_humidity()).unwrap();
        assert_eq!(got, 5479);
        assert_eq!(bus.written, [CMD_TRIGGER_RH]);
        assert_eq!(delay.0, [29]);
    }

    #[test]
    fn humidity_clamped() {
        let mut bus = FakeBus {
            reply: std::vec![0x00, 0x00, 0x00],
            ..Default::default()
        }; // crc8 of 00 00 is 0
        let got = block_on(Device::new(&mut bus, FakeDelay::default()).read_humidity());
        assert_eq!(got, Ok(0));
    }

    #[test]
    fn checksum_error() {
        let mut bus = FakeBus {
            reply: std::vec![0x66, 0x14, 0x00],
            ..Default::default()
        };
        let got = block_on(Device::new(&mut bus, FakeDelay::default()).read_temperature());
        assert_eq!(got, Err(Error::Checksum));
        assert_eq!(
            std::format!("{}", Error::<ErrorKind>::Checksum),
            "sht2x: checksum mismatch"
        );
    }

    #[test]
    fn bus_error() {
        let mut bus = FakeBus {
            err: Some(NACK),
            ..Default::default()
        };
        let mut d = Device::new(&mut bus, FakeDelay::default());
        assert_eq!(block_on(d.read_humidity()), Err(Error::Bus(NACK)));
        assert_eq!(block_on(d.configure()), Err(Error::Bus(NACK)));
    }
}
