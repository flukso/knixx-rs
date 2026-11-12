//! The firmware image: what runs, what is staged, and the update window in
//! which the transfer service (xfer_gatt) takes files.
//!
//! - `fw [status]`: the running image's header, the staged image, the
//!   window, and what the bootloader did on the last boot.
//! - `fw install`: resets into the bootloader, which installs the staged
//!   image.
//! - `fw discard`: removes the staged image.
//! - `fw recovery`: resets into the bootloader's USB recovery.
//! - `upload [window <minutes> | window off]`: the update window.

use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

use knixx_cli::Action;
use knixx_image::stage::{self, Files};
use knixx_image::{self as image, Header, result};

use crate::boot;
use crate::console::Ctx;
use crate::storage::Fs;

mod keys {
    include!(concat!(env!("OUT_DIR"), "/keys.rs"));
}

/// The keys that staged images must be signed with.
pub fn keys() -> image::Keys<'static> {
    image::Keys(keys::KEYS)
}

/// What the bootloader did on this boot, from GPREGRET2.
pub static LAST_RESULT: AtomicU8 = AtomicU8::new(0);

/// At boot: removes the staged image once it runs, or when it is no newer
/// than what runs, so that it could never install.
pub fn tidy() {
    LAST_RESULT.store(boot::last_result(), Ordering::Relaxed);
    if let (Some(s), Some(r)) = (stage::staged(&mut Fs), boot::header())
        && s.counter <= r.counter
    {
        let _ = Fs.remove(image::STAGED_PATH);
    }
}

const USAGE: &str = "usage: fw [status | install | discard | recovery]";
const UPLOAD_USAGE: &str = "usage: upload [window <minutes> | window off]";

/// upload [window <minutes> | window off]: the update window, in which the
/// transfer service takes and gives files without pairing.
pub fn upload_command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] | ["window"] => window_status(w),
        ["window", "off"] => {
            crate::xfer_gatt::set_window(None);
            let _ = writeln!(w, "upload: window shut");
        }
        ["window", m] => match m.parse::<u64>() {
            Ok(m @ 1..=60) => {
                crate::xfer_gatt::set_window(Some(embassy_time::Duration::from_secs(m * 60)));
                let _ = writeln!(
                    w,
                    "upload: window open for {m} min: the transfer service takes signed firmware, scripts and the config, without pairing"
                );
            }
            _ => {
                let _ = writeln!(w, "upload: window takes 1 to 60 minutes, or off");
            }
        },
        _ => {
            let _ = writeln!(w, "{UPLOAD_USAGE}");
        }
    }
    Action::None
}

pub fn command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] | ["status"] => status(w),
        ["install" | "discard"] if crate::xfer_gatt::busy() => {
            let _ = writeln!(w, "fw: a transfer is in progress over BLE");
        }
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

fn window_status(w: &mut dyn Write) {
    match crate::xfer_gatt::window_left() {
        Some(d) => {
            let _ = writeln!(w, "upload: window open, {} s left", d.as_secs());
        }
        None => {
            let _ = writeln!(w, "upload: window shut");
        }
    }
    if let Some(l) = crate::xfer_gatt::last() {
        let secs = |d: embassy_time::Duration| d.as_millis() as f32 / 1000.0;
        let _ = writeln!(
            w,
            "upload: last: {} {} B in {:.1} s, {:.1} s in flash",
            l.kind,
            l.bytes,
            secs(l.total),
            secs(l.flash)
        );
    }
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
    window_status(w);
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

fn install(w: &mut dyn Write) -> Action {
    match stage::staged(&mut Fs) {
        Some(h) => {
            let _ = write!(w, "fw: installing ");
            describe(&h, w);
            let _ = writeln!(w, "fw: rebooting into the bootloader");
            Action::Reboot(boot::INSTALL)
        }
        None => {
            let _ = writeln!(w, "fw: no image staged; push one first (bolt fw push)");
            Action::None
        }
    }
}
