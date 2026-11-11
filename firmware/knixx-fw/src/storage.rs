//! The board's 2 MiB QSPI flash (knixx_qspi), with the configuration file
//! and the other files on its littlefs (knixx_storage).
//!
//! The flash is behind a thread-mode mutex, not a critical section: an
//! erase takes tens of milliseconds, too long to keep interrupts off with
//! the BLE controller running. Loading and saving block the executor
//! meanwhile, at boot and on config changes only.

use core::cell::RefCell;
use core::fmt::{self, Write};

use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::peripherals::QSPI;
use embassy_nrf::{Peri, interrupt, qspi};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use knixx_device::config;
pub use knixx_qspi::Pins;
use knixx_qspi::QspiFlash;

/// The flash, once set up.
static FLASH: Mutex<ThreadModeRawMutex, RefCell<Option<QspiFlash>>> = Mutex::new(RefCell::new(None));

/// Whether the littlefs mounted at boot, so that saving can use it.
static MOUNTED: Mutex<ThreadModeRawMutex, core::cell::Cell<bool>> = Mutex::new(core::cell::Cell::new(false));

/// Sets up the flash and loads the configuration from it, writing progress
/// to log. It falls back to the built-in default, and reports it on log,
/// when the flash or the file is unusable; the returned flag tells.
pub fn load(
    qspi: Peri<'static, QSPI>,
    irq: impl Binding<interrupt::typelevel::QSPI, qspi::InterruptHandler<QSPI>> + 'static,
    pins: Pins,
    log: &mut dyn Write,
) -> (knixx_device::thunder::Device, bool) {
    let mut flash = QspiFlash::new(qspi, irq, pins);

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

/// Why a file operation failed.
pub enum FileError {
    /// The flash or its littlefs is not available.
    Unavailable,
    Fs(knixx_storage::FsError),
}

impl fmt::Display for FileError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FileError::Unavailable => f.write_str("flash unavailable"),
            FileError::Fs(e) => write!(f, "{e}"),
        }
    }
}

/// Runs f on the flash, if its littlefs mounted at boot. It blocks the
/// executor while the flash works.
fn with_flash<R>(
    f: impl FnOnce(&mut QspiFlash) -> Result<R, knixx_storage::FsError>,
) -> Result<R, FileError> {
    if !MOUNTED.lock(|m| m.get()) {
        return Err(FileError::Unavailable);
    }
    FLASH.lock(|fl| match fl.borrow_mut().as_mut() {
        Some(flash) => f(flash).map_err(FileError::Fs),
        None => Err(FileError::Unavailable),
    })
}

/// Reads the file at path into buf and returns its length.
pub fn read_file(path: &str, buf: &mut [u8]) -> Result<usize, FileError> {
    with_flash(|fl| knixx_storage::read(fl, path, buf))
}

/// Writes data as the file at path, in its directory, which it makes if
/// need be: whole, or not at all.
pub fn write_file(dir: &str, path: &str, data: &[u8]) -> Result<(), FileError> {
    with_flash(|fl| {
        knixx_storage::mkdir(fl, dir)?;
        knixx_storage::write_atomic(fl, path, data)
    })
}

/// Makes the directory dir, if it is not there.
pub fn make_dir(dir: &str) -> Result<(), FileError> {
    with_flash(|fl| knixx_storage::mkdir(fl, dir))
}

/// Removes the file at path and reports whether it was there.
pub fn remove_file(path: &str) -> Result<bool, FileError> {
    with_flash(|fl| knixx_storage::remove(fl, path))
}

/// Runs f on the name and size of each file in the directory at path; a
/// missing directory has none.
pub fn list_dir(path: &str, mut f: impl FnMut(&str, u32)) -> Result<(), FileError> {
    match with_flash(|fl| {
        knixx_storage::list(fl, path, |e| {
            if !e.dir {
                f(e.name, e.size)
            }
        })
    }) {
        Err(FileError::Fs(e)) if e.not_found() => Ok(()),
        r => r,
    }
}

/// The size of the file at path, or None when there is none.
#[cfg_attr(not(feature = "scripts"), allow(dead_code))]
pub fn file_size(path: &str) -> Result<Option<u32>, FileError> {
    with_flash(|fl| knixx_storage::stat(fl, path, |e| e.filter(|e| !e.dir).map(|e| e.size)))
}

/// The littlefs as the files a staged firmware goes to
/// (knixx_image::stage).
pub struct Fs;

impl knixx_image::stage::Files for Fs {
    type Error = FileError;

    fn create(&mut self, path: &str) -> Result<(), FileError> {
        with_flash(|fl| {
            knixx_storage::mkdir(fl, knixx_image::STAGED_DIR)?;
            knixx_storage::create(fl, path)
        })
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<u32, FileError> {
        with_flash(|fl| knixx_storage::append(fl, path, data))
    }

    fn read_at(&mut self, path: &str, off: u32, buf: &mut [u8]) -> Result<usize, FileError> {
        with_flash(|fl| knixx_storage::read_at(fl, path, off, buf))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), FileError> {
        with_flash(|fl| knixx_storage::rename(fl, from, to))
    }

    fn remove(&mut self, path: &str) -> Result<(), FileError> {
        with_flash(|fl| knixx_storage::remove(fl, path).map(|_| ()))
    }

    fn free(&mut self) -> Result<u32, FileError> {
        with_flash(knixx_storage::free)
    }
}
