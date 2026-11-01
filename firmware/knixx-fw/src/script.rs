//! The scripts on the flash: `/scripts/<name>.rhgn`, the bytecode that
//! `bolt script upload` builds and sends. The `script` command lists and
//! removes them, and `script put` starts the upload of one, which the
//! console then receives raw (knixx_console::upload).

use core::cell::Cell;
use core::fmt::Write;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use knixx_cli::Action;
use knixx_console::upload::{Name, Upload};

use crate::console::Ctx;
use crate::storage::{self, FileError};

/// The directory of the scripts.
pub const DIR: &str = "/scripts";
/// The extension of a script's file.
pub const EXT: &str = ".rhgn";

/// The largest script.
pub const MAX_SIZE: usize = 16 * 1024;

/// The upload that `script put` asked for, which the console starts.
static PENDING: Mutex<ThreadModeRawMutex, Cell<Option<Upload>>> = Mutex::new(Cell::new(None));

/// Takes the upload that `script put` asked for.
pub fn take_pending() -> Option<Upload> {
    PENDING.lock(|p| p.take())
}

/// The path of the script named name.
pub fn path(name: &str) -> heapless::String<64> {
    let mut p = heapless::String::new();
    let _ = write!(p, "{DIR}/{name}{EXT}");
    p
}

/// Writes data as the script named name.
pub fn store(name: &str, data: &[u8]) -> Result<(), FileError> {
    storage::write_file(DIR, &path(name), data)
}

const USAGE: &str = "usage: script [ls] | info <name> | rm <name> | put <name> <size> <crc32>";

/// script [ls] | info <name> | rm <name> | put <name> <size> <crc32>
pub fn command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] | ["ls"] => list(w),
        ["info", name] => info(name, w),
        ["rm", name] => rm(name, w),
        ["put", name, size, crc] => return put(name, size, crc, w),
        _ => {
            let _ = writeln!(w, "{USAGE}");
        }
    }
    Action::None
}

fn list(w: &mut dyn Write) {
    let mut n = 0;
    let r = storage::list_dir(DIR, |file, size| {
        if let Some(name) = file.strip_suffix(EXT) {
            let _ = writeln!(w, "{name:<24} {size:>6} bytes  {}", state(name));
            n += 1;
        }
    });
    match r {
        Err(e) => {
            let _ = writeln!(w, "script: {e}");
        }
        Ok(()) if n == 0 => {
            let _ = writeln!(w, "no scripts");
        }
        Ok(()) => {}
    }
}

fn rm(name: &str, w: &mut dyn Write) {
    if Name::new(name).is_none() {
        let _ = writeln!(w, "script: bad name {name:?}");
        return;
    }
    let _ = match storage::remove_file(&path(name)) {
        Ok(true) => {
            #[cfg(feature = "scripts")]
            if let Some(n) = Name::new(name) {
                let _ = crate::runner::CONTROL.try_send(crate::runner::Control::Remove(n));
            }
            writeln!(w, "removed {name}")
        }
        Ok(false) => writeln!(w, "no script {name}"),
        Err(e) => writeln!(w, "script: {e}"),
    };
}

/// The state of the script name, as the script task has it.
fn state(name: &str) -> &'static str {
    #[cfg(feature = "scripts")]
    return crate::runner::running(name).map_or("not loaded", |s| s.as_str());
    #[cfg(not(feature = "scripts"))]
    {
        let _ = name;
        "scripts are off in this build"
    }
}

/// Shows what the script task knows of the script name.
fn info(name: &str, w: &mut dyn Write) {
    #[cfg(feature = "scripts")]
    {
        let shown = crate::runner::STATUS.lock(|st| {
            let st = st.borrow();
            let Some(s) = st.iter().find(|s| s.name.as_str() == name) else {
                return false;
            };
            for (key, v) in ["name", "author", "date", "compiled", "version", "hash"]
                .iter()
                .zip(&s.info)
            {
                let _ = writeln!(w, "{key:<10} {v}");
            }
            let _ = writeln!(
                w,
                "state      {}, {} calls, {} failed, longest call took {} us",
                s.state.as_str(),
                s.calls,
                s.errors,
                s.max_us
            );
            if !s.last_error.is_empty() {
                let _ = write!(w, "last error {}", s.last_error);
                if !s.faults.is_empty() {
                    let _ = write!(w, " (at");
                    for a in &s.faults {
                        let _ = write!(w, " {a:#x}");
                    }
                    let _ = write!(w, ")");
                }
                let _ = writeln!(w);
            }
            true
        });
        if shown {
            return;
        }
    }
    let _ = match storage::file_size(&path(name)) {
        Ok(Some(n)) => writeln!(w, "{name}: {n} bytes, {}", state(name)),
        Ok(None) => writeln!(w, "no script {name}"),
        Err(e) => writeln!(w, "script: {e}"),
    };
}

fn put(name: &str, size: &str, crc: &str, w: &mut dyn Write) -> Action {
    let Some(n) = Name::new(name) else {
        let _ = writeln!(
            w,
            "error: bad name {name:?}: 1 to {} letters, digits, - and _",
            knixx_console::upload::MAX_NAME
        );
        return Action::None;
    };
    let size = match size.parse::<usize>() {
        Ok(s) if (1..=MAX_SIZE).contains(&s) => s,
        _ => {
            let _ = writeln!(w, "error: size {size:?} is not 1 to {MAX_SIZE} bytes");
            return Action::None;
        }
    };
    let Ok(crc) = u32::from_str_radix(crc.trim_start_matches("0x"), 16) else {
        let _ = writeln!(w, "error: crc32 {crc:?} is not hexadecimal");
        return Action::None;
    };
    let up = Upload::new(n, size, crc, crate::console::now());
    PENDING.lock(|p| p.set(Some(up)));
    Action::Upload
}
