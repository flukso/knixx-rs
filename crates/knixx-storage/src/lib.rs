//! The configuration file, /knixx.ini, on a littlefs on the board's 2 MiB
//! SPI flash, with the geometry and the on-disk version of the Go firmware's
//! littlefs (tinyfs, on the GD25Q16C), so that a board keeps its
//! configuration when it moves between the Go and the Rust firmware:
//! 256-byte pages are read and programmed, 4 KiB sectors are the blocks,
//! and the on-disk version is 2.1.
//!
//! It drives the littlefs C library (littlefs2-sys) directly: the littlefs2
//! crate pins the on-disk version to 2.0, and littlefs does not mount a
//! filesystem of a later minor version, as the Go firmware's are.
//!
//! Unlike the Go firmware, a flash that holds a littlefs superblock but does
//! not mount is never formatted: the configuration on it may still be
//! recoverable, and a mistake in the geometry would otherwise wipe it.

#![no_std]

#[cfg(test)]
extern crate std;

use core::ffi::{c_int, c_void};
use core::fmt;

use knixx_device::config;
use littlefs2_sys as ll;

#[cfg(any(test, target_os = "none"))]
mod c_stubs;

/// The path of the configuration file, with its C terminator.
const CONFIG_PATH: &[u8] = b"/knixx.ini\0";

/// The path of the configuration file, for messages.
pub const CONFIG_NAME: &str = "/knixx.ini";

/// The size of a page, the unit of reads and writes.
pub const PAGE_SIZE: usize = 256;
/// The size of a sector, the unit of erases and littlefs' block.
pub const SECTOR_SIZE: usize = 4096;
/// The number of sectors of the 2 MiB flash.
pub const SECTOR_COUNT: usize = 512;

/// The caches and the lookahead buffer, as the Go firmware's.
const CACHE_SIZE: usize = 512;
const LOOKAHEAD_SIZE: usize = 512;
const BLOCK_CYCLES: i32 = 100;
/// The on-disk version: 2.1, as littlefs 2.8 of the Go firmware writes.
const DISK_VERSION: u32 = 0x0002_0001;

/// A NOR flash: reads anywhere, writes within erased pages and erases
/// sectors.
pub trait BlockDevice {
    type Error;
    /// Reads buf.len() bytes at off.
    fn read(&mut self, off: u32, buf: &mut [u8]) -> Result<(), Self::Error>;
    /// Writes data at off; off and data.len() are multiples of PAGE_SIZE.
    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), Self::Error>;
    /// Erases the sector at off, a multiple of SECTOR_SIZE.
    fn erase_sector(&mut self, off: u32) -> Result<(), Self::Error>;
}

/// Reports whether sector 0 or 1, the superblock pair, holds a littlefs
/// metadata block: "littlefs" after the revision count and the first tag.
pub fn has_superblock<D: BlockDevice>(dev: &mut D) -> bool {
    (0..2).any(|block| {
        let mut head = [0u8; 16];
        dev.read((block * SECTOR_SIZE) as u32, &mut head).is_ok() && &head[8..16] == b"littlefs"
    })
}

/// A littlefs error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Code(pub i32);

impl fmt::Display for Code {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let name = match self.0 {
            ll::lfs_error_LFS_ERR_IO => "input/output error",
            ll::lfs_error_LFS_ERR_CORRUPT => "corrupted",
            ll::lfs_error_LFS_ERR_NOENT => "no such file or directory",
            ll::lfs_error_LFS_ERR_NOSPC => "no space left on device",
            ll::lfs_error_LFS_ERR_INVAL => "invalid parameter",
            _ => return write!(f, "error {}", self.0),
        };
        f.write_str(name)
    }
}

/// Why the configuration could not be loaded or saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Formatting a blank flash failed.
    Format(Code),
    /// The littlefs does not mount: after formatting a blank flash, or, not
    /// formatted, one that holds a superblock.
    Mount(Code, bool),
    /// Reading or writing the file failed.
    File(Code),
    /// The file is longer than a configuration can be.
    TooLarge,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Format(e) => write!(f, "littlefs format: {e}"),
            Error::Mount(e, false) => write!(f, "littlefs mount: {e}"),
            Error::Mount(e, true) => write!(f, "littlefs mount: {e}; the flash is not formatted"),
            Error::File(e) => write!(f, "{CONFIG_NAME}: {e}"),
            Error::TooLarge => write!(f, "{CONFIG_NAME}: larger than {} bytes", config::MAX_FILE),
        }
    }
}

/// Turns a littlefs return value into a result.
fn check(r: c_int) -> Result<c_int, Code> {
    if r < 0 { Err(Code(r)) } else { Ok(r) }
}

/// The buffers littlefs works in: no allocation.
#[repr(C, align(4))]
struct Buffers {
    read: [u8; CACHE_SIZE],
    prog: [u8; CACHE_SIZE],
    lookahead: [u8; LOOKAHEAD_SIZE],
    file: [u8; CACHE_SIZE],
}

/// A littlefs on a block device. It holds pointers into itself once set up,
/// so it stays in place: it is made, used and dropped within one function.
struct Lfs<'d, D: BlockDevice> {
    lfs: ll::lfs_t,
    cfg: ll::lfs_config,
    bufs: Buffers,
    dev: &'d mut D,
    mounted: bool,
}

unsafe extern "C" fn read_cb<D: BlockDevice>(
    c: *const ll::lfs_config,
    block: ll::lfs_block_t,
    off: ll::lfs_off_t,
    buf: *mut c_void,
    size: ll::lfs_size_t,
) -> c_int {
    // Safety: context is the device, set by setup; littlefs passes a buffer
    // of size bytes.
    let (dev, buf) = unsafe {
        (
            &mut *((*c).context as *mut D),
            core::slice::from_raw_parts_mut(buf as *mut u8, size as usize),
        )
    };
    match dev.read(block * SECTOR_SIZE as u32 + off, buf) {
        Ok(()) => 0,
        Err(_) => ll::lfs_error_LFS_ERR_IO,
    }
}

unsafe extern "C" fn prog_cb<D: BlockDevice>(
    c: *const ll::lfs_config,
    block: ll::lfs_block_t,
    off: ll::lfs_off_t,
    buf: *const c_void,
    size: ll::lfs_size_t,
) -> c_int {
    // Safety: as in read_cb.
    let (dev, data) = unsafe {
        (
            &mut *((*c).context as *mut D),
            core::slice::from_raw_parts(buf as *const u8, size as usize),
        )
    };
    match dev.write(block * SECTOR_SIZE as u32 + off, data) {
        Ok(()) => 0,
        Err(_) => ll::lfs_error_LFS_ERR_IO,
    }
}

unsafe extern "C" fn erase_cb<D: BlockDevice>(c: *const ll::lfs_config, block: ll::lfs_block_t) -> c_int {
    // Safety: as in read_cb.
    let dev = unsafe { &mut *((*c).context as *mut D) };
    match dev.erase_sector(block * SECTOR_SIZE as u32) {
        Ok(()) => 0,
        Err(_) => ll::lfs_error_LFS_ERR_IO,
    }
}

unsafe extern "C" fn sync_cb(_: *const ll::lfs_config) -> c_int {
    0
}

impl<'d, D: BlockDevice> Lfs<'d, D> {
    fn new(dev: &'d mut D) -> Self {
        Lfs {
            lfs: ll::lfs_t::default(),
            cfg: ll::lfs_config::default(),
            bufs: Buffers {
                read: [0; CACHE_SIZE],
                prog: [0; CACHE_SIZE],
                lookahead: [0; LOOKAHEAD_SIZE],
                file: [0; CACHE_SIZE],
            },
            dev,
            mounted: false,
        }
    }

    /// Points the configuration at the device and the buffers; called after
    /// the Lfs reached its place, before any littlefs call.
    fn setup(&mut self) {
        self.cfg = ll::lfs_config {
            context: self.dev as *mut D as *mut c_void,
            read: Some(read_cb::<D>),
            prog: Some(prog_cb::<D>),
            erase: Some(erase_cb::<D>),
            sync: Some(sync_cb),
            read_size: PAGE_SIZE as u32,
            prog_size: PAGE_SIZE as u32,
            block_size: SECTOR_SIZE as u32,
            block_count: SECTOR_COUNT as u32,
            block_cycles: BLOCK_CYCLES,
            cache_size: CACHE_SIZE as u32,
            lookahead_size: LOOKAHEAD_SIZE as u32,
            read_buffer: self.bufs.read.as_mut_ptr() as *mut c_void,
            prog_buffer: self.bufs.prog.as_mut_ptr() as *mut c_void,
            lookahead_buffer: self.bufs.lookahead.as_mut_ptr() as *mut c_void,
            disk_version: DISK_VERSION,
            ..ll::lfs_config::default()
        };
    }

    fn format(&mut self) -> Result<(), Code> {
        // Safety: setup pointed cfg at live buffers and the device.
        check(unsafe { ll::lfs_format(&mut self.lfs, &self.cfg) }).map(|_| ())
    }

    fn mount(&mut self) -> Result<(), Code> {
        // Safety: as in format; self stays in place until dropped.
        check(unsafe { ll::lfs_mount(&mut self.lfs, &self.cfg) })?;
        self.mounted = true;
        Ok(())
    }

    fn exists(&mut self, path: &[u8]) -> Result<bool, Code> {
        let mut info = ll::lfs_info::default();
        // Safety: mounted; path is a C string.
        match check(unsafe { ll::lfs_stat(&mut self.lfs, path.as_ptr() as _, &mut info) }) {
            Ok(_) => Ok(true),
            Err(Code(ll::lfs_error_LFS_ERR_NOENT)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Opens path with flags, runs f on the file and closes it.
    fn with_file<R>(
        &mut self,
        path: &[u8],
        flags: i32,
        f: impl FnOnce(*mut ll::lfs_t, *mut ll::lfs_file_t) -> Result<R, Code>,
    ) -> Result<R, Code> {
        let mut file = ll::lfs_file_t::default();
        let fcfg = ll::lfs_file_config {
            buffer: self.bufs.file.as_mut_ptr() as *mut c_void,
            ..ll::lfs_file_config::default()
        };
        // Safety: mounted; path is a C string; fcfg and its buffer outlive
        // the open file.
        check(unsafe { ll::lfs_file_opencfg(&mut self.lfs, &mut file, path.as_ptr() as _, flags, &fcfg) })?;
        let r = f(&mut self.lfs, &mut file);
        // Safety: the file is open; closing writes it out.
        let closed = check(unsafe { ll::lfs_file_close(&mut self.lfs, &mut file) });
        let r = r?;
        closed?;
        Ok(r)
    }

    fn read_file(&mut self, path: &[u8], buf: &mut [u8]) -> Result<usize, Code> {
        self.with_file(path, ll::lfs_open_flags_LFS_O_RDONLY as c_int, |lfs, file| {
            let mut n = 0;
            while n < buf.len() {
                let rest = &mut buf[n..];
                // Safety: rest is a live buffer of its length.
                let m = check(unsafe {
                    ll::lfs_file_read(lfs, file, rest.as_mut_ptr() as *mut c_void, rest.len() as u32)
                })?;
                if m == 0 {
                    break;
                }
                n += m as usize;
            }
            Ok(n)
        })
    }

    fn write_file(&mut self, path: &[u8], data: &[u8]) -> Result<(), Code> {
        let flags = (ll::lfs_open_flags_LFS_O_WRONLY
            | ll::lfs_open_flags_LFS_O_CREAT
            | ll::lfs_open_flags_LFS_O_TRUNC) as c_int;
        self.with_file(path, flags, |lfs, file| {
            // Safety: data is a live buffer of its length.
            let n = check(unsafe {
                ll::lfs_file_write(lfs, file, data.as_ptr() as *const c_void, data.len() as u32)
            })?;
            if n as usize != data.len() {
                return Err(Code(ll::lfs_error_LFS_ERR_NOSPC));
            }
            Ok(())
        })
    }
}

impl<D: BlockDevice> Drop for Lfs<'_, D> {
    fn drop(&mut self) {
        if self.mounted {
            // Safety: mounted, and still in place.
            unsafe { ll::lfs_unmount(&mut self.lfs) };
        }
    }
}

/// Mounts the littlefs on dev and reads the configuration file into buf,
/// returning its length; progress goes to log, line by line. A blank flash
/// is formatted, and a missing file is written with the default
/// configuration, as the Go firmware does.
pub fn load<D: BlockDevice>(
    dev: &mut D,
    buf: &mut [u8; config::MAX_FILE],
    log: &mut dyn fmt::Write,
) -> Result<usize, Error> {
    let mut fs = Lfs::new(dev);
    fs.setup();
    match fs.mount() {
        Ok(()) => {
            let _ = writeln!(log, "mounted littlefs");
        }
        Err(e) if has_superblock(fs.dev) => return Err(Error::Mount(e, true)),
        Err(_) => {
            let _ = writeln!(log, "no littlefs found, formatting flash");
            fs.format().map_err(Error::Format)?;
            fs.mount().map_err(|e| Error::Mount(e, false))?;
        }
    }
    if !fs.exists(CONFIG_PATH).map_err(Error::File)? {
        fs.write_file(CONFIG_PATH, config::DEFAULT).map_err(Error::File)?;
        let _ = writeln!(log, "wrote default {CONFIG_NAME}");
    }
    let n = fs.read_file(CONFIG_PATH, buf).map_err(Error::File)?;
    if n == buf.len() {
        return Err(Error::TooLarge);
    }
    Ok(n)
}

/// Writes data as the configuration file, replacing it.
pub fn save<D: BlockDevice>(dev: &mut D, data: &[u8]) -> Result<(), Error> {
    let mut fs = Lfs::new(dev);
    fs.setup();
    fs.mount().map_err(|e| Error::Mount(e, false))?;
    fs.write_file(CONFIG_PATH, data).map_err(Error::File)
}

/// Formats dev, for tests and a later format command.
pub fn format<D: BlockDevice>(dev: &mut D) -> Result<(), Error> {
    let mut fs = Lfs::new(dev);
    fs.setup();
    fs.format().map_err(Error::Format)
}

#[cfg(test)]
mod tests;
