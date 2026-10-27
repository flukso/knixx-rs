//! Driver for the ambient light sensor (ALS) of the Vishay VCNL4040
//! proximity and ambient light sensor. The proximity sensor is kept shut
//! down.
//!
//! The ALS has no single measurement mode: while powered on it measures
//! continuously and draws about 260 µA, against 0.2 µA when shut down. To keep
//! the sensor low-power, read_illuminance powers the ALS on for one
//! measurement and shuts it down again.
//!
//! Datasheet: <https://www.vishay.com/docs/84274/vcnl4040.pdf>

use core::fmt;

use embedded_hal_async::delay::DelayNs;
use embedded_hal_async::i2c::I2c;

/// The fixed I2C address of the VCNL4040.
pub const ADDRESS: u8 = 0x60;

// Command codes of the 16-bit registers, accessed LSB first.
const REG_ALS_CONF: u8 = 0x00; // ALS_CONF in the low byte
const REG_PS_CONF12: u8 = 0x03; // PS_CONF1 low, PS_CONF2 high
const REG_PS_CONF3: u8 = 0x04; // PS_CONF3 low, PS_MS high
const REG_ALS_DATA: u8 = 0x09;
const REG_ID: u8 = 0x0c;

// ALS_CONF bits.
const ALS_IT80MS: u16 = 0x00 << 6; // integration time 80 ms: 0.1 lx/step
const ALS_SD: u16 = 0x01; // shut down

/// PS_SD shuts the proximity sensor down, in PS_CONF1.
const PS_SD: u16 = 0x01;

/// DEVICE_ID is the content of the ID register: ID_L 0x86, version 1.
const DEVICE_ID: u16 = 0x0186;

/// INTEGRATION_MS is the ALS integration time of ALS_IT80MS.
const INTEGRATION_MS: u32 = 80;
/// SETTLE_MS is the wait from powering the ALS on to reading its data. The
/// datasheet does not specify one; two integration times leave margin for
/// the first measurement to complete.
const SETTLE_MS: u32 = 2 * INTEGRATION_MS;

/// The reading of a saturated sensor, 6553.5 lx.
pub const MAX_LUX10: u32 = 65535;

/// A VCNL4040 error: a bus error or a device that is not a VCNL4040.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error<E> {
    /// The I2C bus failed.
    Bus(E),
    /// A device on the address that is not a VCNL4040.
    Id,
}

impl<E: fmt::Debug> fmt::Display for Error<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Bus(e) => write!(f, "{e:?}"),
            Error::Id => f.write_str("vcnl4040: unexpected device id"),
        }
    }
}

impl<E: fmt::Debug> core::error::Error for Error<E> {}

/// Device wraps an I2C connection to a VCNL4040.
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

    /// configure checks the device id and shuts both the proximity and the
    /// ambient light sensor down, with the ALS interrupt disabled. The sensor
    /// keeps its registers across a reset of the host, so they are all
    /// written.
    pub async fn configure(&mut self) -> Result<(), Error<I::Error>> {
        if self.read(REG_ID).await? != DEVICE_ID {
            return Err(Error::Id);
        }
        self.write(REG_PS_CONF12, PS_SD).await?;
        self.write(REG_PS_CONF3, 0).await?;
        self.write(REG_ALS_CONF, ALS_IT80MS | ALS_SD).await
    }

    /// read_illuminance powers the ALS on, waits for a measurement, reads it
    /// and shuts the ALS down again, also when the read fails. It returns the
    /// illuminance in tenths of a lux, at most MAX_LUX10.
    pub async fn read_illuminance(&mut self) -> Result<u32, Error<I::Error>> {
        if let Err(e) = self.write(REG_ALS_CONF, ALS_IT80MS).await {
            // The ALS may have been powered on all the same.
            let _ = self.write(REG_ALS_CONF, ALS_IT80MS | ALS_SD).await;
            return Err(e);
        }
        self.delay.delay_ms(SETTLE_MS).await;
        let raw = self.read(REG_ALS_DATA).await;
        let sd = self.write(REG_ALS_CONF, ALS_IT80MS | ALS_SD).await;
        let raw = raw?;
        sd?;
        // At 80 ms one step is 0.1 lx.
        Ok(u32::from(raw))
    }

    /// read reads a 16-bit register.
    async fn read(&mut self, reg: u8) -> Result<u16, Error<I::Error>> {
        let mut data = [0u8; 2];
        self.bus
            .write_read(self.address, &[reg], &mut data)
            .await
            .map_err(Error::Bus)?;
        Ok(u16::from_le_bytes(data))
    }

    /// write writes a 16-bit register.
    async fn write(&mut self, reg: u8, v: u16) -> Result<(), Error<I::Error>> {
        let [lo, hi] = v.to_le_bytes();
        self.bus
            .write(self.address, &[reg, lo, hi])
            .await
            .map_err(Error::Bus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::block_on;
    use core::cell::RefCell;
    use embedded_hal_async::i2c::{ErrorKind, ErrorType, NoAcknowledgeSource, Operation};
    use std::collections::HashMap;
    use std::format;
    use std::rc::Rc;
    use std::string::String;
    use std::vec::Vec;

    const NACK: ErrorKind = ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown);

    type Log = Rc<RefCell<Vec<String>>>;

    /// FakeBus logs the transactions and answers register reads from regs.
    #[derive(Default)]
    struct FakeBus {
        log: Log,
        regs: HashMap<u8, u16>,
        /// fail_read fails the reads of a register.
        fail_read: HashMap<u8, bool>,
        err: Option<ErrorKind>,
    }

    impl ErrorType for FakeBus {
        type Error = ErrorKind;
    }

    impl I2c for FakeBus {
        async fn transaction(&mut self, addr: u8, ops: &mut [Operation<'_>]) -> Result<(), ErrorKind> {
            assert_eq!(addr, ADDRESS, "address {addr:#x}");
            if let Some(e) = self.err {
                return Err(e);
            }
            let [w, rest @ ..] = ops else {
                panic!("empty transaction")
            };
            let Operation::Write(w) = w else {
                panic!("transaction starts with a read")
            };
            let w: &[u8] = w;
            match rest {
                [] => {
                    let data: Vec<String> = w[1..].iter().map(|b| format!("{b:02x}")).collect();
                    self.log
                        .borrow_mut()
                        .push(format!("W {:02x} {}", w[0], data.join(" ")));
                    Ok(())
                }
                [Operation::Read(r)] => {
                    self.log.borrow_mut().push(format!("R {:02x}", w[0]));
                    if self.fail_read.get(&w[0]).copied().unwrap_or(false) {
                        return Err(NACK);
                    }
                    let v = self.regs.get(&w[0]).copied().unwrap_or(0);
                    r[0] = v as u8;
                    r[1] = (v >> 8) as u8;
                    Ok(())
                }
                _ => panic!("unexpected transaction"),
            }
        }
    }

    /// FakeDelay logs the waits in ms to the bus log.
    struct FakeDelay(Log);

    impl DelayNs for FakeDelay {
        async fn delay_ns(&mut self, ns: u32) {
            self.0.borrow_mut().push(format!("D {}", ns / 1_000_000));
        }
    }

    fn new_device(f: &mut FakeBus) -> Device<&mut FakeBus, FakeDelay> {
        let delay = FakeDelay(f.log.clone());
        Device::new(f, delay)
    }

    #[test]
    fn configure() {
        let mut f = FakeBus {
            regs: HashMap::from([(REG_ID, 0x0186)]),
            ..Default::default()
        };
        block_on(new_device(&mut f).configure()).unwrap();
        assert_eq!(
            *f.log.borrow(),
            ["R 0c", "W 03 01 00", "W 04 00 00", "W 00 01 00"]
        );
    }

    #[test]
    fn configure_id() {
        let mut f = FakeBus {
            regs: HashMap::from([(REG_ID, 0x0086)]),
            ..Default::default()
        };
        assert_eq!(block_on(new_device(&mut f).configure()), Err(Error::Id));
        assert_eq!(
            f.log.borrow().len(),
            1,
            "wrote to an unknown device: {:?}",
            f.log.borrow()
        );
        assert_eq!(
            format!("{}", Error::<ErrorKind>::Id),
            "vcnl4040: unexpected device id"
        );
    }

    #[test]
    fn read_illuminance() {
        let mut f = FakeBus {
            regs: HashMap::from([(REG_ALS_DATA, 0x1234)]),
            ..Default::default()
        };
        let lux10 = block_on(new_device(&mut f).read_illuminance()).unwrap();
        assert_eq!(lux10, 0x1234);
        assert_eq!(*f.log.borrow(), ["W 00 00 00", "D 160", "R 09", "W 00 01 00"]);
    }

    #[test]
    fn read_error_shuts_down() {
        let mut f = FakeBus {
            fail_read: HashMap::from([(REG_ALS_DATA, true)]),
            ..Default::default()
        };
        assert_eq!(
            block_on(new_device(&mut f).read_illuminance()),
            Err(Error::Bus(NACK))
        );
        assert_eq!(f.log.borrow().last().map(String::as_str), Some("W 00 01 00"));
    }

    #[test]
    fn bus_error() {
        let mut f = FakeBus {
            err: Some(NACK),
            ..Default::default()
        };
        let mut d = new_device(&mut f);
        assert_eq!(block_on(d.configure()), Err(Error::Bus(NACK)));
        assert_eq!(block_on(d.read_illuminance()), Err(Error::Bus(NACK)));
    }
}
