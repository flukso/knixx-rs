//! Flashing a UF2 file through the Adafruit UF2 bootloader: reset the
//! running board into its bootloader with the 1200-baud touch, copy the
//! file to the bootloader's drive, and wait for the board to come back,
//! optionally checking the version it reports.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, Instant};

use crate::{drive, uf2, usbserial};

/// The name of the Feather nRF52840's bootloader drive.
pub const VOLUME: &str = "FTHR840BOOT";

/// What to flash and how.
pub struct Options<'a> {
    /// The UF2 file.
    pub file: &'a Path,
    /// The serial port of the running board; found by USB ID when None.
    pub port: Option<String>,
    /// The name of the bootloader's drive.
    pub volume: &'a str,
    /// The line the board's version command must print after the flash.
    pub expect: Option<String>,
    /// How long to wait for the drive and for the board to come back.
    pub wait: Duration,
}

/// Prints a status line.
fn status(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

/// Flashes a board; see the module.
pub fn uf2(o: Options) -> Result<(), String> {
    let data = fs::read(o.file).map_err(|e| format!("{}: {e}", o.file.display()))?;
    uf2::check(&data, uf2::FAMILY_NRF52840).map_err(|e| format!("{}: {e}", o.file.display()))?;
    let names = [o.volume];

    let mut port = o.port.clone();
    let mount = match find_drive(&names) {
        Some(m) => {
            status(format!(
                "{} is mounted: the board is in its bootloader",
                m.display()
            ));
            m
        }
        None => {
            let p = match &port {
                Some(p) => p.clone(),
                None => app_port("double-tap reset for its bootloader, or use --port")?,
            };
            status(format!("resetting the board on {p} into its bootloader"));
            usbserial::reset_1200(&p).map_err(|e| e.to_string())?;
            port.get_or_insert(p);
            sleep(Duration::from_secs(3));
            wait_drive(&names, o.wait)?
        }
    };
    if let Some(id) = drive::board_id(&mount) {
        status(format!("bootloader: {id}"));
    }

    let name = o.file.file_name().unwrap_or_default().to_string_lossy();
    status(format!(
        "writing {name} ({} KiB) to {}",
        data.len() / 1024,
        mount.display()
    ));
    let werr = fs::write(mount.join("flash.uf2"), &data).err();
    if let Some(e) = &werr {
        status(format!(
            "the write ended with {e}; the bootloader resets the board after the last block"
        ));
    }

    if let Some(expect) = &o.expect {
        let got = wait_version(port.as_deref(), o.wait)?;
        if &got != expect {
            return Err(format!("the board reports {got:?}, want {expect:?}"));
        }
        status(format!("the board runs the new firmware: {got}"));
        return Ok(());
    }
    if let Err(e) = wait_gone(&mount, o.wait) {
        return Err(match werr {
            Some(w) => format!("write: {w}"),
            None => e,
        });
    }
    status("the board left its bootloader");
    Ok(())
}

/// Returns the serial port of the one knixx board on USB.
pub fn app_port(hint: &str) -> Result<String, String> {
    let ports = usbserial::ports(usbserial::APP_VID, usbserial::APP_PID);
    match ports.len() {
        0 => Err(format!("no knixx board on USB; {hint}")),
        1 => Ok(ports.into_iter().next().unwrap()),
        _ => Err(format!(
            "several serial ports ({}); pick one with --port",
            ports.join(", ")
        )),
    }
}

/// Finds the drive, mounting it first on Linux when nothing did.
fn find_drive(names: &[&str]) -> Option<PathBuf> {
    if let Some(m) = drive::find(names) {
        return Some(m);
    }
    match drive::mount(names) {
        Ok(Some(m)) => {
            status(format!("mounted {}", m.display()));
            drive::find(names).or(Some(m))
        }
        Ok(None) => None,
        Err(e) => {
            status(format!("{e}; mount it by hand"));
            None
        }
    }
}

fn wait_drive(names: &[&str], wait: Duration) -> Result<PathBuf, String> {
    let end = Instant::now() + wait;
    loop {
        if let Some(m) = find_drive(names) {
            return Ok(m);
        }
        if Instant::now() > end {
            return Err(format!(
                "no UF2 drive named {} after {wait:?}",
                names.join(" or ")
            ));
        }
        sleep(Duration::from_millis(500));
    }
}

/// Waits for the board's console to answer version and returns the line.
fn wait_version(port: Option<&str>, wait: Duration) -> Result<String, String> {
    let end = Instant::now() + wait;
    let mut last = String::new();
    while Instant::now() < end {
        sleep(Duration::from_secs(1));
        let p = match port {
            Some(p) if Path::new(p).exists() => p.to_string(),
            _ => {
                let ports = usbserial::ports(usbserial::APP_VID, usbserial::APP_PID);
                if ports.len() != 1 {
                    last = format!("waiting for the board's serial port ({} found)", ports.len());
                    continue;
                }
                ports[0].clone()
            }
        };
        match usbserial::query(&p, "version", Duration::from_secs(1)) {
            Ok(out) => match out.lines().find(|l| l.starts_with("version ")) {
                Some(l) => return Ok(l.trim().to_string()),
                None => last = "no answer to version on the console".into(),
            },
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("the board did not come back after {wait:?}: {last}"))
}

/// Waits for the drive to go away: the bootloader has started the new
/// firmware.
fn wait_gone(mount: &Path, wait: Duration) -> Result<(), String> {
    let end = Instant::now() + wait;
    while mount.join(uf2::INFO_FILE).exists() {
        if Instant::now() > end {
            return Err(format!("{} is still there after {wait:?}", mount.display()));
        }
        sleep(Duration::from_millis(500));
    }
    Ok(())
}
