//! The scripts on the flash: `/scripts/<name>.rhgn`, the bytecode that
//! `bolt script push` builds and sends over the transfer service
//! (xfer_gatt). The `script` command lists and removes them.

use core::fmt::Write;

use knixx_cli::Action;
use knixx_console::name::Name;

use crate::console::Ctx;
use crate::storage;

/// The directory of the scripts.
pub const DIR: &str = "/scripts";
/// The extension of a script's file.
pub const EXT: &str = ".rhgn";

/// The path of the script named name.
pub fn path(name: &str) -> heapless::String<64> {
    let mut p = heapless::String::new();
    let _ = write!(p, "{DIR}/{name}{EXT}");
    p
}

const USAGE: &str = "usage: script [ls] | info <name> | rm <name>";

/// script [ls] | info <name> | rm <name>
pub fn command(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] | ["ls"] => list(w),
        ["info", name] => info(name, w),
        ["rm", name] => rm(name, w),
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
