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
//!
//! Besides the configuration, the littlefs holds other files, such as the
//! scripts: [`read`], [`write_atomic`], [`remove`], [`mkdir`], [`stat`] and
//! [`list`] work on whole files by path. Each mounts the littlefs and
//! unmounts it again, and none formats it.
//!
//! A [`Session`] keeps the littlefs mounted, and a file that is appended to
//! open, for the firmware: the free functions are each a session of one
//! call, for the bootloader.

#![no_std]

#[cfg(test)]
extern crate std;

use core::ffi::{c_int, c_void};
use core::fmt;
use core::marker::PhantomPinned;
use core::pin::Pin;

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

/// The longest path, in bytes.
pub const MAX_PATH: usize = 63;

/// The suffix of the file that write_atomic writes before it renames it.
const TMP_SUFFIX: &[u8] = b".tmp";

/// A path with its C terminator, and room for TMP_SUFFIX.
#[derive(Clone, Copy)]
struct CPath {
    buf: [u8; MAX_PATH + TMP_SUFFIX.len() + 1],
    len: usize,
}

impl CPath {
    /// Checks path: not empty, at most MAX_PATH bytes, without NUL.
    fn new(path: &str) -> Result<CPath, FsError> {
        let b = path.as_bytes();
        if b.is_empty() || b.len() > MAX_PATH || b.contains(&0) {
            return Err(FsError::Path);
        }
        let mut p = CPath {
            buf: [0; MAX_PATH + TMP_SUFFIX.len() + 1],
            len: b.len(),
        };
        p.buf[..b.len()].copy_from_slice(b);
        Ok(p)
    }

    /// The path with its terminator.
    fn c(&self) -> &[u8] {
        &self.buf[..self.len + 1]
    }

    /// The path with TMP_SUFFIX, and its terminator.
    fn tmp(&self) -> CPath {
        let mut t = CPath {
            buf: self.buf,
            len: self.len + TMP_SUFFIX.len(),
        };
        t.buf[self.len..t.len].copy_from_slice(TMP_SUFFIX);
        t.buf[t.len] = 0;
        t
    }
}

/// Why a file operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsError {
    /// The littlefs does not mount.
    Mount(Code),
    /// littlefs failed, such as LFS_ERR_NOENT for a missing file.
    Fs(Code),
    /// The path is empty, longer than MAX_PATH or holds a NUL.
    Path,
    /// The file does not fit the buffer.
    TooLarge,
}

impl FsError {
    /// Reports whether the file or directory does not exist.
    pub fn not_found(&self) -> bool {
        *self == FsError::Fs(Code(ll::lfs_error_LFS_ERR_NOENT))
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            FsError::Mount(e) => write!(f, "littlefs mount: {e}"),
            FsError::Fs(e) => e.fmt(f),
            FsError::Path => write!(f, "bad path (1 to {MAX_PATH} bytes)"),
            FsError::TooLarge => f.write_str("file too large"),
        }
    }
}

/// An entry of a directory, or of a path, as stat and list report them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry<'a> {
    pub name: &'a str,
    pub size: u32,
    pub dir: bool,
}

impl<'a> Entry<'a> {
    fn of(info: &'a ll::lfs_info) -> Entry<'a> {
        // Safety: c_char and u8 have the same layout.
        let name: &[u8; 256] = unsafe { &*(&info.name as *const _ as *const [u8; 256]) };
        let n = name.iter().position(|&b| b == 0).unwrap_or(name.len());
        Entry {
            name: core::str::from_utf8(&name[..n]).unwrap_or("?"),
            size: info.size,
            dir: u32::from(info.type_) == ll::lfs_type_LFS_TYPE_DIR,
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
    /// The cache of a file opened within one call.
    file: [u8; CACHE_SIZE],
    /// The cache of the stream.
    stream: [u8; CACHE_SIZE],
}

/// The file a session keeps open between appends.
struct Stream {
    file: ll::lfs_file_t,
    cfg: ll::lfs_file_config,
    path: CPath,
}

/// A littlefs on a block device, mounted until dropped.
///
/// A file that is appended to stays open from one [`append`](Self::append)
/// to the next, the stream, so that an append costs what its bytes cost:
/// not a mount, a scan of the free blocks and a commit, as with the free
/// functions. The stream is committed when it is closed: by
/// [`close_stream`](Self::close_stream), by an append to another file, by
/// an operation on its own path, and by the drop. After a reset, an
/// uncommitted stream's file has the size it had when it was opened.
///
/// It holds pointers into itself once mounted, so it is used pinned: in a
/// static (`Pin::static_mut`) or on the stack (`core::pin::pin!`).
pub struct Session<D: BlockDevice> {
    lfs: ll::lfs_t,
    cfg: ll::lfs_config,
    bufs: Buffers,
    dev: D,
    mounted: bool,
    stream: Option<Stream>,
    _pin: PhantomPinned,
}

// Safety: the raw pointers littlefs keeps point into the session itself
// (its buffers, its stream) and at its device, which it owns: the whole
// moves between threads together, as the device can.
unsafe impl<D: BlockDevice + Send> Send for Session<D> {}

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

/// A mounted session's error, for a session that is not.
const NOT_MOUNTED: FsError = FsError::Mount(Code(ll::lfs_error_LFS_ERR_INVAL));

impl<D: BlockDevice> Session<D> {
    /// A session on dev, not mounted yet.
    pub fn new(dev: D) -> Self {
        Session {
            lfs: ll::lfs_t::default(),
            cfg: ll::lfs_config::default(),
            bufs: Buffers {
                read: [0; CACHE_SIZE],
                prog: [0; CACHE_SIZE],
                lookahead: [0; LOOKAHEAD_SIZE],
                file: [0; CACHE_SIZE],
                stream: [0; CACHE_SIZE],
            },
            dev,
            mounted: false,
            stream: None,
            _pin: PhantomPinned,
        }
    }

    /// The session, in place: nothing moves out of it.
    fn pinned(self: Pin<&mut Self>) -> &mut Self {
        // Safety: the callers only use fields in place; nothing is moved
        // out or swapped.
        unsafe { self.get_unchecked_mut() }
    }

    /// The session, mounted.
    fn fs(self: Pin<&mut Self>) -> Result<&mut Self, FsError> {
        let s = self.pinned();
        if s.mounted { Ok(s) } else { Err(NOT_MOUNTED) }
    }

    /// Whether the littlefs is mounted.
    pub fn is_mounted(&self) -> bool {
        self.mounted
    }

    /// Points the configuration at the device and the buffers, in place.
    fn setup(&mut self) {
        self.cfg = ll::lfs_config {
            context: &mut self.dev as *mut D as *mut c_void,
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

    fn format_in_place(&mut self) -> Result<(), Code> {
        self.setup();
        // Safety: setup pointed cfg at live buffers and the device, in place.
        check(unsafe { ll::lfs_format(&mut self.lfs, &self.cfg) }).map(|_| ())
    }

    fn mount_in_place(&mut self) -> Result<(), Code> {
        if self.mounted {
            return Ok(());
        }
        self.setup();
        // Safety: as in format_in_place; the session stays in place until
        // dropped, which unmounts.
        check(unsafe { ll::lfs_mount(&mut self.lfs, &self.cfg) })?;
        self.mounted = true;
        Ok(())
    }

    /// Mounts the littlefs, without formatting it.
    pub fn mount(self: Pin<&mut Self>) -> Result<(), Code> {
        self.pinned().mount_in_place()
    }

    /// Formats the device; the session must not be mounted.
    pub fn format(self: Pin<&mut Self>) -> Result<(), Error> {
        let s = self.pinned();
        if s.mounted {
            return Err(Error::Format(Code(ll::lfs_error_LFS_ERR_INVAL)));
        }
        s.format_in_place().map_err(Error::Format)
    }

    /// Mounts the littlefs and reads the configuration file into buf,
    /// returning its length; progress goes to log, line by line. A blank
    /// flash is formatted, and a missing file is written with the default
    /// configuration, as the Go firmware does. The session stays mounted.
    pub fn load(
        self: Pin<&mut Self>,
        buf: &mut [u8; config::MAX_FILE],
        log: &mut dyn fmt::Write,
    ) -> Result<usize, Error> {
        let s = self.pinned();
        match s.mount_in_place() {
            Ok(()) => {
                let _ = writeln!(log, "mounted littlefs");
            }
            Err(e) if has_superblock(&mut s.dev) => return Err(Error::Mount(e, true)),
            Err(_) => {
                let _ = writeln!(log, "no littlefs found, formatting flash");
                s.format_in_place().map_err(Error::Format)?;
                s.mount_in_place().map_err(|e| Error::Mount(e, false))?;
            }
        }
        if !s.exists(CONFIG_PATH).map_err(Error::File)? {
            s.write_file(CONFIG_PATH, config::DEFAULT).map_err(Error::File)?;
            let _ = writeln!(log, "wrote default {CONFIG_NAME}");
        }
        let n = s.read_file(CONFIG_PATH, buf).map_err(Error::File)?;
        if n == buf.len() {
            return Err(Error::TooLarge);
        }
        Ok(n)
    }

    /// Writes data as the configuration file, replacing it.
    pub fn save(self: Pin<&mut Self>, data: &[u8]) -> Result<(), Error> {
        let s = self.pinned();
        if !s.mounted {
            return Err(Error::Mount(Code(ll::lfs_error_LFS_ERR_INVAL), false));
        }
        s.write_file(CONFIG_PATH, data).map_err(Error::File)
    }

    /// Reads the file at path into buf and returns its length; a file
    /// larger than buf is an error, so is a directory.
    pub fn read(self: Pin<&mut Self>, path: &str, buf: &mut [u8]) -> Result<usize, FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        s.release(&[&p]).map_err(FsError::Fs)?;
        let info = s
            .stat_info(p.c())
            .map_err(FsError::Fs)?
            .ok_or(FsError::Fs(Code(ll::lfs_error_LFS_ERR_NOENT)))?;
        if u32::from(info.type_) == ll::lfs_type_LFS_TYPE_DIR {
            return Err(FsError::Fs(Code(ll::lfs_error_LFS_ERR_ISDIR)));
        }
        if info.size as usize > buf.len() {
            return Err(FsError::TooLarge);
        }
        s.read_file(p.c(), &mut buf[..info.size as usize])
            .map_err(FsError::Fs)
    }

    /// Writes data as the file at path, replacing it as a whole: the data
    /// goes to path.tmp first, which is then renamed, so that a reset
    /// halfway leaves the old file, not part of the new one.
    pub fn write_atomic(self: Pin<&mut Self>, path: &str, data: &[u8]) -> Result<(), FsError> {
        let p = CPath::new(path)?;
        let tmp = p.tmp();
        let s = self.fs()?;
        s.release(&[&p, &tmp]).map_err(FsError::Fs)?;
        s.write_file(tmp.c(), data).map_err(FsError::Fs)?;
        // Safety: mounted; both paths are C strings.
        check(unsafe { ll::lfs_rename(&mut s.lfs, tmp.c().as_ptr() as _, p.c().as_ptr() as _) })
            .map_err(FsError::Fs)?;
        Ok(())
    }

    /// Removes the file or the empty directory at path and reports whether
    /// it was there.
    pub fn remove(self: Pin<&mut Self>, path: &str) -> Result<bool, FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        s.release(&[&p]).map_err(FsError::Fs)?;
        // Safety: mounted; path is a C string.
        match check(unsafe { ll::lfs_remove(&mut s.lfs, p.c().as_ptr() as _) }) {
            Ok(_) => Ok(true),
            Err(Code(ll::lfs_error_LFS_ERR_NOENT)) => Ok(false),
            Err(e) => Err(FsError::Fs(e)),
        }
    }

    /// Makes the directory at path, unless it is there already.
    pub fn mkdir(self: Pin<&mut Self>, path: &str) -> Result<(), FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        // Safety: mounted; path is a C string.
        match check(unsafe { ll::lfs_mkdir(&mut s.lfs, p.c().as_ptr() as _) }) {
            Ok(_) | Err(Code(ll::lfs_error_LFS_ERR_EXIST)) => Ok(()),
            Err(e) => Err(FsError::Fs(e)),
        }
    }

    /// Creates the file at path empty, replacing it if it is there, for a
    /// file that [`append`](Self::append) then fills piece by piece.
    pub fn create(self: Pin<&mut Self>, path: &str) -> Result<(), FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        s.release(&[&p]).map_err(FsError::Fs)?;
        s.write_file(p.c(), &[]).map_err(FsError::Fs)
    }

    /// Appends data to the file at path, which must exist, and returns its
    /// new size. The file stays open as the stream, uncommitted, until it
    /// is closed (see [`Session`]).
    pub fn append(self: Pin<&mut Self>, path: &str, data: &[u8]) -> Result<u32, FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        s.append_stream(&p, data).map_err(FsError::Fs)
    }

    /// Commits the stream and closes it, if there is one.
    pub fn close_stream(self: Pin<&mut Self>) -> Result<(), FsError> {
        self.pinned().close_stream_in_place().map_err(FsError::Fs)
    }

    /// Reads from the file at path, from offset off, into buf, and returns
    /// the bytes read: fewer than buf.len() only at the end of the file.
    pub fn read_at(self: Pin<&mut Self>, path: &str, off: u32, buf: &mut [u8]) -> Result<usize, FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        s.release(&[&p]).map_err(FsError::Fs)?;
        s.with_file(p.c(), ll::lfs_open_flags_LFS_O_RDONLY as c_int, |lfs, file| {
            // Safety: the file is open.
            check(unsafe {
                ll::lfs_file_seek(lfs, file, off as i32, ll::lfs_whence_flags_LFS_SEEK_SET as c_int)
            })?;
            read_all(lfs, file, buf)
        })
        .map_err(FsError::Fs)
    }

    /// Renames the file at from to to, replacing to if it is there.
    pub fn rename(self: Pin<&mut Self>, from: &str, to: &str) -> Result<(), FsError> {
        let f = CPath::new(from)?;
        let t = CPath::new(to)?;
        let s = self.fs()?;
        s.release(&[&f, &t]).map_err(FsError::Fs)?;
        // Safety: mounted; both paths are C strings.
        check(unsafe { ll::lfs_rename(&mut s.lfs, f.c().as_ptr() as _, t.c().as_ptr() as _) })
            .map(|_| ())
            .map_err(FsError::Fs)
    }

    /// The free space of the littlefs, in bytes: the blocks not in use.
    pub fn free(self: Pin<&mut Self>) -> Result<u32, FsError> {
        let s = self.fs()?;
        // Safety: mounted.
        let used = check(unsafe { ll::lfs_fs_size(&mut s.lfs) }).map_err(FsError::Fs)? as u32;
        Ok((SECTOR_COUNT as u32).saturating_sub(used) * SECTOR_SIZE as u32)
    }

    /// Runs f on the entry of path, or None when there is none.
    pub fn stat<R>(
        self: Pin<&mut Self>,
        path: &str,
        f: impl FnOnce(Option<Entry<'_>>) -> R,
    ) -> Result<R, FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        s.release(&[&p]).map_err(FsError::Fs)?;
        let info = s.stat_info(p.c()).map_err(FsError::Fs)?;
        Ok(f(info.as_ref().map(Entry::of)))
    }

    /// Runs f on each entry of the directory at path, in littlefs' order,
    /// without "." and "..". The stream's file shows the size it had when
    /// it was opened.
    pub fn list(self: Pin<&mut Self>, path: &str, mut f: impl FnMut(Entry<'_>)) -> Result<(), FsError> {
        let p = CPath::new(path)?;
        let s = self.fs()?;
        let mut dir = ll::lfs_dir_t::default();
        // Safety: mounted; path is a C string; dir stays in place until
        // closed.
        check(unsafe { ll::lfs_dir_open(&mut s.lfs, &mut dir, p.c().as_ptr() as _) }).map_err(FsError::Fs)?;
        let mut info = ll::lfs_info::default();
        let r = loop {
            // Safety: the directory is open.
            match check(unsafe { ll::lfs_dir_read(&mut s.lfs, &mut dir, &mut info) }) {
                Ok(0) => break Ok(()),
                Ok(_) => {
                    let e = Entry::of(&info);
                    if e.name != "." && e.name != ".." {
                        f(e);
                    }
                }
                Err(e) => break Err(FsError::Fs(e)),
            }
        };
        // Safety: the directory is open.
        let closed = check(unsafe { ll::lfs_dir_close(&mut s.lfs, &mut dir) });
        r?;
        closed.map_err(FsError::Fs)?;
        Ok(())
    }

    fn close_stream_in_place(&mut self) -> Result<(), Code> {
        let Some(st) = self.stream.as_mut() else {
            return Ok(());
        };
        // Safety: the stream is open; closing commits it.
        let r = check(unsafe { ll::lfs_file_close(&mut self.lfs, &mut st.file) });
        self.stream = None;
        r.map(|_| ())
    }

    /// Closes the stream when it is on one of paths, so that an operation
    /// on them sees the file whole.
    fn release(&mut self, paths: &[&CPath]) -> Result<(), Code> {
        match &self.stream {
            Some(st) if paths.iter().any(|p| p.c() == st.path.c()) => self.close_stream_in_place(),
            _ => Ok(()),
        }
    }

    fn append_stream(&mut self, p: &CPath, data: &[u8]) -> Result<u32, Code> {
        if !self.stream.as_ref().is_some_and(|st| st.path.c() == p.c()) {
            self.close_stream_in_place()?;
            let st = self.stream.insert(Stream {
                file: ll::lfs_file_t::default(),
                cfg: ll::lfs_file_config {
                    buffer: self.bufs.stream.as_mut_ptr() as *mut c_void,
                    ..ll::lfs_file_config::default()
                },
                path: *p,
            });
            let flags = (ll::lfs_open_flags_LFS_O_WRONLY | ll::lfs_open_flags_LFS_O_APPEND) as c_int;
            // Safety: mounted; the path is a C string; the file, its config
            // and its buffer stay in place in the session while it is open.
            let opened = check(unsafe {
                ll::lfs_file_opencfg(
                    &mut self.lfs,
                    &mut st.file,
                    st.path.c().as_ptr() as _,
                    flags,
                    &st.cfg,
                )
            });
            if let Err(e) = opened {
                self.stream = None;
                return Err(e);
            }
        }
        let Some(st) = self.stream.as_mut() else {
            return Err(Code(ll::lfs_error_LFS_ERR_INVAL));
        };
        // Safety: the stream is open; data is a live buffer of its length.
        let r = check(unsafe {
            ll::lfs_file_write(
                &mut self.lfs,
                &mut st.file,
                data.as_ptr() as *const c_void,
                data.len() as u32,
            )
        })
        .and_then(|n| {
            if n as usize != data.len() {
                return Err(Code(ll::lfs_error_LFS_ERR_NOSPC));
            }
            // Safety: the stream is open.
            check(unsafe { ll::lfs_file_size(&mut self.lfs, &mut st.file) }).map(|s| s as u32)
        });
        if r.is_err() {
            let _ = self.close_stream_in_place();
        }
        r
    }

    fn exists(&mut self, path: &[u8]) -> Result<bool, Code> {
        self.stat_info(path).map(|i| i.is_some())
    }

    fn stat_info(&mut self, path: &[u8]) -> Result<Option<ll::lfs_info>, Code> {
        let mut info = ll::lfs_info::default();
        // Safety: mounted; path is a C string.
        match check(unsafe { ll::lfs_stat(&mut self.lfs, path.as_ptr() as _, &mut info) }) {
            Ok(_) => Ok(Some(info)),
            Err(Code(ll::lfs_error_LFS_ERR_NOENT)) => Ok(None),
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
            read_all(lfs, file, buf)
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

/// Reads an open file from where it stands into buf, up to its end.
fn read_all(lfs: *mut ll::lfs_t, file: *mut ll::lfs_file_t, buf: &mut [u8]) -> Result<usize, Code> {
    let mut n = 0;
    while n < buf.len() {
        let rest = &mut buf[n..];
        // Safety: the file is open; rest is a live buffer of its length.
        let m = check(unsafe {
            ll::lfs_file_read(lfs, file, rest.as_mut_ptr() as *mut c_void, rest.len() as u32)
        })?;
        if m == 0 {
            break;
        }
        n += m as usize;
    }
    Ok(n)
}

impl<D: BlockDevice> Drop for Session<D> {
    fn drop(&mut self) {
        if self.mounted {
            let _ = self.close_stream_in_place();
            // Safety: mounted, and still in place.
            unsafe { ll::lfs_unmount(&mut self.lfs) };
        }
    }
}

/// A device borrowed by a session, as the free functions use.
impl<D: BlockDevice> BlockDevice for &mut D {
    type Error = D::Error;
    fn read(&mut self, off: u32, buf: &mut [u8]) -> Result<(), Self::Error> {
        (**self).read(off, buf)
    }
    fn write(&mut self, off: u32, data: &[u8]) -> Result<(), Self::Error> {
        (**self).write(off, data)
    }
    fn erase_sector(&mut self, off: u32) -> Result<(), Self::Error> {
        (**self).erase_sector(off)
    }
}

/// Mounts the littlefs on dev, without formatting it, runs f on it and
/// unmounts it: each free function is a session of one call.
fn with_session<D: BlockDevice, R>(
    dev: &mut D,
    f: impl FnOnce(Pin<&mut Session<&mut D>>) -> Result<R, FsError>,
) -> Result<R, FsError> {
    let mut s = core::pin::pin!(Session::new(dev));
    s.as_mut().mount().map_err(FsError::Mount)?;
    f(s)
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
    core::pin::pin!(Session::new(dev)).load(buf, log)
}

/// Writes data as the configuration file, replacing it.
pub fn save<D: BlockDevice>(dev: &mut D, data: &[u8]) -> Result<(), Error> {
    let mut s = core::pin::pin!(Session::new(dev));
    s.as_mut().mount().map_err(|e| Error::Mount(e, false))?;
    s.save(data)
}

/// Formats dev, for tests and a later format command.
pub fn format<D: BlockDevice>(dev: &mut D) -> Result<(), Error> {
    core::pin::pin!(Session::new(dev)).format()
}

/// Reads the file at path into buf and returns its length; a file larger
/// than buf is an error, so is a directory.
pub fn read<D: BlockDevice>(dev: &mut D, path: &str, buf: &mut [u8]) -> Result<usize, FsError> {
    with_session(dev, |s| s.read(path, buf))
}

/// Writes data as the file at path, replacing it as a whole (see
/// [`Session::write_atomic`]).
pub fn write_atomic<D: BlockDevice>(dev: &mut D, path: &str, data: &[u8]) -> Result<(), FsError> {
    with_session(dev, |s| s.write_atomic(path, data))
}

/// Removes the file or the empty directory at path and reports whether it
/// was there.
pub fn remove<D: BlockDevice>(dev: &mut D, path: &str) -> Result<bool, FsError> {
    with_session(dev, |s| s.remove(path))
}

/// Makes the directory at path, unless it is there already.
pub fn mkdir<D: BlockDevice>(dev: &mut D, path: &str) -> Result<(), FsError> {
    with_session(dev, |s| s.mkdir(path))
}

/// Creates the file at path empty, replacing it if it is there.
pub fn create<D: BlockDevice>(dev: &mut D, path: &str) -> Result<(), FsError> {
    with_session(dev, |s| s.create(path))
}

/// Appends data to the file at path, which must exist, and returns its new
/// size. The file is committed before it returns; a session keeps it open
/// instead, which is much faster for a file filled piece by piece.
pub fn append<D: BlockDevice>(dev: &mut D, path: &str, data: &[u8]) -> Result<u32, FsError> {
    with_session(dev, |s| s.append(path, data))
}

/// Reads from the file at path, from offset off, into buf, and returns the
/// bytes read: fewer than buf.len() only at the end of the file.
pub fn read_at<D: BlockDevice>(dev: &mut D, path: &str, off: u32, buf: &mut [u8]) -> Result<usize, FsError> {
    with_session(dev, |s| s.read_at(path, off, buf))
}

/// Renames the file at from to to, replacing to if it is there.
pub fn rename<D: BlockDevice>(dev: &mut D, from: &str, to: &str) -> Result<(), FsError> {
    with_session(dev, |s| s.rename(from, to))
}

/// The free space of the littlefs, in bytes: the blocks not in use.
pub fn free<D: BlockDevice>(dev: &mut D) -> Result<u32, FsError> {
    with_session(dev, |s| s.free())
}

/// Runs f on the entry of path, or None when there is none.
pub fn stat<D: BlockDevice, R>(
    dev: &mut D,
    path: &str,
    f: impl FnOnce(Option<Entry<'_>>) -> R,
) -> Result<R, FsError> {
    with_session(dev, |s| s.stat(path, f))
}

/// Runs f on each entry of the directory at path, in littlefs' order,
/// without "." and "..".
pub fn list<D: BlockDevice>(dev: &mut D, path: &str, f: impl FnMut(Entry<'_>)) -> Result<(), FsError> {
    with_session(dev, |s| s.list(path, f))
}

#[cfg(test)]
mod tests;
