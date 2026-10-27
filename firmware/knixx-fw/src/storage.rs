//! The board's 2 MiB QSPI flash (GD25Q16C), driven in single-line SPI mode
//! as the Go firmware did, with the configuration file on its littlefs
//! (knixx_storage).
//!
//! QSPI's EasyDMA reads from and writes to word-aligned RAM only, while
//! littlefs passes any buffer, also data in flash such as the default
//! configuration, so all transfers go through an aligned bounce buffer.
//!
//! The flash is behind a thread-mode mutex, not a critical section: an
//! erase takes tens of milliseconds, too long to keep interrupts off with
//! the BLE controller running. Loading and saving block the executor
//! meanwhile, at boot and on config changes only.

use core::cell::RefCell;
use core::fmt::{self, Write};

use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::peripherals::{P0_17, P0_19, P0_20, P0_21, P0_22, P0_23, QSPI};
use embassy_nrf::qspi::{self, AddressMode, Frequency, Qspi, ReadOpcode, WriteOpcode, WritePageSize};
use embassy_nrf::{Peri, interrupt};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use knixx_device::config;
use knixx_storage::{BlockDevice, PAGE_SIZE, SECTOR_COUNT, SECTOR_SIZE};

/// The flash, once set up.
static FLASH: Mutex<ThreadModeRawMutex, RefCell<Option<QspiFlash>>> = Mutex::new(RefCell::new(None));

/// Whether the littlefs mounted at boot, so that saving can use it.
static MOUNTED: Mutex<ThreadModeRawMutex, core::cell::Cell<bool>> = Mutex::new(core::cell::Cell::new(false));

/// The QSPI flash as a knixx_storage block device.
pub struct QspiFlash {
    qspi: Qspi<'static>,
    bounce: Bounce,
}

#[repr(C, align(4))]
struct Bounce([u8; PAGE_SIZE]);

/// A QSPI transfer failed.
pub struct FlashError;

impl BlockDevice for QspiFlash {
    type Error = FlashError;

    fn read(&mut self, off: u32, buf: &mut [u8]) -> Result<(), FlashError> {
        let mut off = off;
        for chunk in buf.chunks_mut(PAGE_SIZE) {
            // QSPI reads whole words.
            let n = chunk.len().next_multiple_of(4);
            self.qspi
                .blocking_read(off, &mut self.bounce.0[..n])
                .map_err(|_| FlashError)?;
            chunk.copy_from_slice(&self.bounce.0[..chunk.len()]);
            off += chunk.len() as u32;
        }
        Ok(())
    }

    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), FlashError> {
        let mut off = off;
        for chunk in data.chunks(PAGE_SIZE) {
            self.bounce.0[..chunk.len()].copy_from_slice(chunk);
            self.qspi
                .blocking_write(off, &self.bounce.0[..chunk.len()])
                .map_err(|_| FlashError)?;
            off += chunk.len() as u32;
        }
        Ok(())
    }

    fn erase_sector(&mut self, off: u32) -> Result<(), FlashError> {
        self.qspi.blocking_erase(off).map_err(|_| FlashError)
    }
}

/// The pins of the flash: SCK, CSN, IO0..IO3 (WP# and HOLD#, held high).
pub struct Pins {
    pub sck: Peri<'static, P0_19>,
    pub csn: Peri<'static, P0_20>,
    pub io0: Peri<'static, P0_17>,
    pub io1: Peri<'static, P0_22>,
    pub io2: Peri<'static, P0_23>,
    pub io3: Peri<'static, P0_21>,
}

/// Sets up the flash and loads the configuration from it, writing progress
/// to log. It falls back to the built-in default, and reports it on log,
/// when the flash or the file is unusable; the returned flag tells.
pub fn load(
    qspi: Peri<'static, QSPI>,
    irq: impl Binding<interrupt::typelevel::QSPI, qspi::InterruptHandler<QSPI>> + 'static,
    pins: Pins,
    log: &mut dyn Write,
) -> (knixx_device::thunder::Device, bool) {
    let mut cfg = qspi::Config::default();
    cfg.read_opcode = ReadOpcode::Fastread;
    cfg.write_opcode = WriteOpcode::Pp;
    cfg.write_page_size = WritePageSize::_256bytes;
    cfg.address_mode = AddressMode::_24bit;
    cfg.frequency = Frequency::M8;
    cfg.capacity = (SECTOR_SIZE * SECTOR_COUNT) as u32;
    let qspi = Qspi::new(
        qspi, irq, pins.sck, pins.csn, pins.io0, pins.io1, pins.io2, pins.io3, cfg,
    );
    let mut flash = QspiFlash {
        qspi,
        bounce: Bounce([0; PAGE_SIZE]),
    };
    // Wake the flash in case it was left in deep power-down.
    let _ = flash.qspi.blocking_custom_instruction(0xAB, &[], &mut []);

    let mut buf = [0u8; config::MAX_FILE];
    let loaded = knixx_storage::load(&mut flash, &mut buf, log);
    MOUNTED.lock(|m| {
        m.set(!matches!(
            loaded,
            Err(knixx_storage::Error::Mount(..) | knixx_storage::Error::Format(_))
        ))
    });
    FLASH.lock(|f| *f.borrow_mut() = Some(flash));
    let parsed = match loaded {
        Ok(n) => config::parse(&buf[..n]).map_err(|e| {
            let _ = writeln!(log, "{}: {e}", knixx_storage::CONFIG_NAME);
        }),
        Err(e) => {
            let _ = writeln!(log, "{e}");
            Err(())
        }
    };
    match parsed {
        Ok(dev) => (dev, true),
        Err(()) => {
            let _ = writeln!(log, "using the built-in default configuration");
            (config::parse(config::DEFAULT).unwrap_or_default(), false)
        }
    }
}

/// Why a configuration was not saved.
pub enum SaveError {
    Unavailable,
    Storage(knixx_storage::Error),
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            SaveError::Unavailable => f.write_str("flash unavailable"),
            SaveError::Storage(e) => write!(f, "{e}"),
        }
    }
}

/// Saves data as the configuration file.
pub fn save(data: &[u8]) -> Result<(), SaveError> {
    if !MOUNTED.lock(|m| m.get()) {
        return Err(SaveError::Unavailable);
    }
    FLASH.lock(|f| match f.borrow_mut().as_mut() {
        Some(flash) => knixx_storage::save(flash, data).map_err(SaveError::Storage),
        None => Err(SaveError::Unavailable),
    })
}
