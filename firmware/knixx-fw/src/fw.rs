//! The firmware image: what runs, what is staged, and staging a new one
//! while the firmware runs, for the bootloader to install.
//!
//! - `fw [status]`: the running image's header, the staged image, and what
//!   the bootloader did on the last boot.
//! - `fw put <size> <crc32>`: takes a .kfw file over the console, in chunks
//!   as `script put` does: `ready <chunk>` (4096 bytes on USB, 2048 on
//!   NUS); the host sends each chunk as a frame, [offset][CRC][data], and
//!   waits for `ok <offset>`, or `resend <offset>` for a damaged frame; a
//!   frame that stalls halfway is dropped, and the host sends it again when
//!   no answer came. After the last chunk's ok: `done <size>` once the file
//!   is checked and staged (knixx_image::stage), or `error: <reason>`.
//! - `fw install`: resets into the bootloader, which installs it.
//! - `fw discard`: removes the staged image.
//! - `fw recovery`: resets into the bootloader's USB recovery.

use core::cell::Cell;
use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};
use core::time::Duration;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use knixx_cli::Action;
use knixx_image::stage::{self, Files, Frame, Frames, StageError, Stager};
use knixx_image::{self as image, Header, result};

use crate::boot;
use crate::console::Ctx;
use crate::storage::{FileError, Fs};

mod keys {
    include!(concat!(env!("OUT_DIR"), "/keys.rs"));
}

/// The keys that staged images must be signed with.
pub fn keys() -> image::Keys<'static> {
    image::Keys(keys::KEYS)
}

/// The chunks of fw put: USB has flow control, NUS has the ring of
/// knixx_console::nus.
pub const CHUNK_USB: usize = 4096;
pub const CHUNK_NUS: usize = 2048;

/// How long an upload waits for the next byte.
const TIMEOUT: Duration = Duration::from_secs(5);

/// How long a frame may stall halfway before the board drops it: a USB
/// packet goes missing now and then, and the host sends the frame again.
const STALL: Duration = Duration::from_millis(500);

/// Counters of fw put, for the probe: packets fed, chunks taken, resends,
/// frames dropped halfway, timeouts.
#[unsafe(no_mangle)]
pub static FW_STATS: [core::sync::atomic::AtomicU32; 5] =
    [const { core::sync::atomic::AtomicU32::new(0) }; 5];

fn count(i: usize) {
    FW_STATS[i].fetch_add(1, Ordering::Relaxed);
}

/// What the bootloader did on this boot, from GPREGRET2.
pub static LAST_RESULT: AtomicU8 = AtomicU8::new(0);

/// The fw put that the command asked for, which the console starts.
static PENDING: Mutex<ThreadModeRawMutex, Cell<Option<(u32, u32)>>> = Mutex::new(Cell::new(None));

/// Takes the fw put that the command asked for: size and CRC-32.
pub fn take_pending() -> Option<(u32, u32)> {
    PENDING.lock(|p| p.take())
}

/// At boot: removes the staged image once it runs.
pub fn tidy() {
    LAST_RESULT.store(boot::last_result(), Ordering::Relaxed);
    if let (Some(s), Some(r)) = (stage::staged(&mut Fs), boot::header())
        && s == r
    {
        let _ = Fs.remove(image::STAGED_PATH);
    }
}

const USAGE: &str = "usage: fw [status | put <size> <crc32> | install | discard | recovery]";

pub fn command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] | ["status"] => status(w),
        ["put", size, crc] => return put(size, crc, w),
        ["install"] => return install(w),
        ["discard"] => match Fs.remove(image::STAGED_PATH) {
            Ok(()) => {
                let _ = writeln!(w, "fw: no image staged");
            }
            Err(e) => {
                let _ = writeln!(w, "fw: {e}");
            }
        },
        ["recovery"] => {
            let _ = writeln!(w, "fw: rebooting into the bootloader's USB recovery");
            return Action::Reboot(boot::RECOVERY);
        }
        _ => {
            let _ = writeln!(w, "{USAGE}");
        }
    }
    Action::None
}

fn describe(h: &Header, w: &mut dyn Write) {
    let _ = writeln!(
        w,
        "{} (counter {}, {} bytes, key {})",
        h.version_str(),
        h.counter,
        h.len,
        h.key
    );
}

fn status(w: &mut dyn Write) {
    let _ = write!(w, "fw: running ");
    match boot::header() {
        Some(h) => describe(&h, w),
        None => {
            let _ = writeln!(w, "without an image header");
        }
    }
    let _ = write!(w, "fw: staged ");
    match stage::staged(&mut Fs) {
        Some(h) => describe(&h, w),
        None => {
            let _ = writeln!(w, "none");
        }
    }
    let last = LAST_RESULT.load(Ordering::Relaxed);
    let what = match last {
        result::NONE => return,
        result::INSTALLED => "installed the staged image",
        result::WRITE => "failed writing internal flash",
        result::READ => "could not read the staged image",
        r if r & 0xF0 == result::REFUSED => "refused the staged image",
        _ => "did something unknown",
    };
    let _ = writeln!(w, "fw: on this boot the bootloader {what} ({last:#04x})");
}

fn put(size: &str, crc: &str, w: &mut dyn Write) -> Action {
    let Ok(size) = size.parse::<u32>() else {
        let _ = writeln!(w, "error: size {size:?} is not a number");
        return Action::None;
    };
    let Ok(crc) = u32::from_str_radix(crc.trim_start_matches("0x"), 16) else {
        let _ = writeln!(w, "error: crc32 {crc:?} is not hexadecimal");
        return Action::None;
    };
    PENDING.lock(|p| p.set(Some((size, crc))));
    Action::Upload
}

fn install(w: &mut dyn Write) -> Action {
    match stage::staged(&mut Fs) {
        Some(h) => {
            let _ = write!(w, "fw: installing ");
            describe(&h, w);
            let _ = writeln!(w, "fw: rebooting into the bootloader");
            Action::Reboot(boot::INSTALL)
        }
        None => {
            let _ = writeln!(w, "fw: no image staged; fw put one first");
            Action::None
        }
    }
}

/// An fw put in progress: framed chunks (knixx_image::stage::Frames) into
/// the stager.
pub struct Upload {
    stager: Stager,
    frames: Frames,
    crc: u32,
    chunk: u32,
    last: Duration,
}

/// What feeding an upload brings: a line for the host.
pub enum Fed {
    None,
    /// `ok <offset>`: the chunk is in, the next one may come.
    Ack(u32),
    /// `resend <offset>`: the frame was damaged or for another offset.
    Resend(u32),
    Done(Header),
    Failed(StageError<FileError>),
}

impl Upload {
    /// Starts an fw put of size bytes with the given CRC-32, at now.
    pub fn begin(size: u32, crc: u32, chunk: usize, now: Duration) -> Result<Upload, StageError<FileError>> {
        Ok(Upload {
            stager: Stager::begin(&mut Fs, size)?,
            frames: Frames::new(size, chunk),
            crc,
            chunk: chunk as u32,
            last: now,
        })
    }

    pub fn chunk(&self) -> u32 {
        self.chunk
    }

    /// Takes data, received at now.
    pub fn feed(&mut self, data: &[u8], now: Duration) -> Fed {
        self.last = now;
        count(0);
        match self.frames.feed(data) {
            Frame::Partial => Fed::None,
            Frame::Resend(at) => {
                count(2);
                Fed::Resend(at)
            }
            Frame::Chunk(_, chunk) => {
                count(1);
                let running = boot::header();
                match self.stager.feed(&mut Fs, chunk, keys(), running.as_ref()) {
                    Ok(()) => Fed::Ack(self.stager.received()),
                    Err(e) => Fed::Failed(e),
                }
            }
        }
    }

    /// Whether all the bytes are in.
    pub fn complete(&self) -> bool {
        self.stager.received() == self.stager.size()
    }

    /// Checks and stages the file, once complete.
    pub fn finish(self) -> Fed {
        match self.stager.finish(&mut Fs, Some(self.crc)) {
            Ok(h) => Fed::Done(h),
            Err(e) => {
                let _ = Fs.remove(image::STAGED_TMP);
                Fed::Failed(e)
            }
        }
    }

    pub fn received(&self) -> u32 {
        self.stager.received()
    }

    pub fn size(&self) -> u32 {
        self.stager.size()
    }

    pub fn timed_out(&self, now: Duration) -> bool {
        let t = now.saturating_sub(self.last) >= TIMEOUT;
        if t {
            count(4);
        }
        t
    }

    /// Drops a frame whose rest stalled at now, so that the host's next
    /// send of it starts afresh.
    pub fn poll(&mut self, now: Duration) {
        if self.frames.partial() && now.saturating_sub(self.last) >= STALL {
            count(3);
            self.frames.drop_partial();
        }
    }
}
