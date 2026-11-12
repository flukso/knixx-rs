//! The board's 2 MiB QSPI flash (knixx_qspi), with the configuration file
//! and the other files on its littlefs (knixx_storage).
//!
//! The littlefs is mounted once, at boot, and stays mounted: a session
//! (knixx_storage::Session), in which a file being received stays open
//! from one append to the next.
//!
//! The session is behind a thread-mode mutex, not a critical section: an
//! erase takes tens of milliseconds, too long to keep interrupts off with
//! the BLE controller running. File operations block the executor
//! meanwhile.

use core::cell::RefCell;
use core::fmt::{self, Write};
use core::pin::Pin;

use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::peripherals::QSPI;
use embassy_nrf::{Peri, interrupt, qspi};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use knixx_device::config;
pub use knixx_qspi::Pins;
use knixx_qspi::QspiFlash;
use knixx_storage::{FsError, Session};
use static_cell::StaticCell;

/// The session's place: it holds pointers into itself once mounted.
static PLACE: StaticCell<Session<QspiFlash>> = StaticCell::new();

/// The littlefs, once it mounted at boot.
static SESSION: Mutex<ThreadModeRawMutex, RefCell<Option<Pin<&'static mut Session<QspiFlash>>>>> =
    Mutex::new(RefCell::new(None));

/// Sets up the flash and loads the configuration from it, writing progress
/// to log. It falls back to the built-in default, and reports it on log,
/// when the flash or the file is unusable; the returned flag tells.
pub fn load(
    qspi: Peri<'static, QSPI>,
    irq: impl Binding<interrupt::typelevel::QSPI, qspi::InterruptHandler<QSPI>> + 'static,
    pins: Pins,
    log: &mut dyn Write,
) -> (knixx_device::thunder::Device, bool) {
    let flash = QspiFlash::new(qspi, irq, pins);
    let mut session = Pin::static_mut(PLACE.init(Session::new(flash)));

    let mut buf = [0u8; config::MAX_FILE];
    let loaded = session.as_mut().load(&mut buf, log);
    if session.is_mounted() {
        SESSION.lock(|s| *s.borrow_mut() = Some(session));
    }
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
    SESSION.lock(|s| match s.borrow_mut().as_mut() {
        Some(s) => s.as_mut().save(data).map_err(SaveError::Storage),
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

/// Runs f on the littlefs, if it mounted at boot. It blocks the executor
/// while the flash works.
fn with_flash<R>(f: impl FnOnce(Pin<&mut Session<QspiFlash>>) -> Result<R, FsError>) -> Result<R, FileError> {
    SESSION.lock(|s| match s.borrow_mut().as_mut() {
        Some(s) => f(s.as_mut()).map_err(FileError::Fs),
        None => Err(FileError::Unavailable),
    })
}

/// Reads the file at path into buf and returns its length.
pub fn read_file(path: &str, buf: &mut [u8]) -> Result<usize, FileError> {
    with_flash(|s| s.read(path, buf))
}

/// Writes data as the file at path, in its directory, which it makes if
/// need be: whole, or not at all.
pub fn write_file(dir: &str, path: &str, data: &[u8]) -> Result<(), FileError> {
    with_flash(|mut s| {
        s.as_mut().mkdir(dir)?;
        s.write_atomic(path, data)
    })
}

/// Makes the directory dir, if it is not there.
pub fn make_dir(dir: &str) -> Result<(), FileError> {
    with_flash(|s| s.mkdir(dir))
}

/// Removes the file at path and reports whether it was there.
pub fn remove_file(path: &str) -> Result<bool, FileError> {
    with_flash(|s| s.remove(path))
}

/// Runs f on the name and size of each file in the directory at path; a
/// missing directory has none.
pub fn list_dir(path: &str, mut f: impl FnMut(&str, u32)) -> Result<(), FileError> {
    match with_flash(|s| {
        s.list(path, |e| {
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
    with_flash(|s| s.stat(path, |e| e.filter(|e| !e.dir).map(|e| e.size)))
}

/// The littlefs as the files a staged firmware and a received script go
/// to (knixx_image::stage): an append leaves the file open for the next.
pub struct Fs;

impl knixx_image::stage::Files for Fs {
    type Error = FileError;

    fn create(&mut self, path: &str) -> Result<(), FileError> {
        with_flash(|mut s| {
            s.as_mut().mkdir(knixx_image::STAGED_DIR)?;
            s.create(path)
        })
    }

    fn append(&mut self, path: &str, data: &[u8]) -> Result<u32, FileError> {
        with_flash(|s| s.append(path, data))
    }

    fn read_at(&mut self, path: &str, off: u32, buf: &mut [u8]) -> Result<usize, FileError> {
        with_flash(|s| s.read_at(path, off, buf))
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<(), FileError> {
        with_flash(|s| s.rename(from, to))
    }

    fn remove(&mut self, path: &str) -> Result<(), FileError> {
        with_flash(|s| s.remove(path).map(|_| ()))
    }

    fn free(&mut self) -> Result<u32, FileError> {
        with_flash(|s| s.free())
    }
}
