//! Flashing a .kfw file over USB through the bootloader's recovery: reset
//! the running board into it with the 1200-baud touch (a board that waits
//! there already is used as it is), send the file with `fw put`, boot it,
//! and wait for the board to come back, optionally checking the version it
//! reports.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use knixx_image::crc32;

use crate::image::Image;
use crate::usbserial::{self, Kind};

/// What to flash and how.
pub struct Options<'a> {
    /// The .kfw file.
    pub file: &'a Path,
    /// The serial port of the running board; found by USB ID when None.
    pub port: Option<String>,
    /// Takes an image whose counter is not above the installed one.
    pub force: bool,
    /// The line the board's version command must print after the flash.
    pub expect: Option<String>,
    /// How long to wait for the bootloader and for the board to come back.
    pub wait: Duration,
}

/// Prints a status line.
fn status(msg: impl AsRef<str>) {
    eprintln!("{}", msg.as_ref());
}

/// Flashes a board; see the module.
pub fn kfw(o: Options) -> Result<(), String> {
    let data = fs::read(o.file).map_err(|e| format!("{}: {e}", o.file.display()))?;
    let (_, hdr) = Image::from_kfw(&data).map_err(|e| format!("{}: {e}", o.file.display()))?;

    let boot = match one(Kind::Bootloader) {
        Some(p) => {
            status(format!("{p}: the board waits in its bootloader"));
            p
        }
        None => {
            let p = match &o.port {
                Some(p) => p.clone(),
                None => app_port("double-tap reset for its bootloader, or use --port")?,
            };
            status(format!("resetting the board on {p} into its bootloader"));
            usbserial::reset_1200(&p).map_err(|e| e.to_string())?;
            wait_port(Kind::Bootloader, o.wait)?
        }
    };

    let name = o.file.file_name().unwrap_or_default().to_string_lossy();
    status(format!(
        "sending {name}: {} (counter {}, {} KiB)",
        hdr.version_str(),
        hdr.counter,
        data.len() / 1024
    ));
    let t = Instant::now();
    put(&boot, &data, o.force)?;
    status(format!(
        "written and checked in {:.1} s; booting it",
        t.elapsed().as_secs_f32()
    ));

    match &o.expect {
        Some(expect) => {
            let got = wait_version(o.wait)?;
            if &got != expect {
                return Err(format!("the board reports {got:?}, want {expect:?}"));
            }
            status(format!("the board runs the new firmware: {got}"));
        }
        None => {
            wait_port(Kind::App, o.wait)?;
            status("the board runs the new firmware");
        }
    }
    Ok(())
}

/// The one port of a board running kind, if there is exactly one.
fn one(kind: Kind) -> Option<String> {
    let mut ports = usbserial::board_ports(kind);
    (ports.len() == 1).then(|| ports.remove(0))
}

/// Returns the serial port of the one knixx board on USB running the
/// firmware.
pub fn app_port(hint: &str) -> Result<String, String> {
    let ports = usbserial::board_ports(Kind::App);
    match ports.len() {
        0 => Err(format!("no knixx board on USB; {hint}")),
        1 => Ok(ports.into_iter().next().unwrap()),
        _ => Err(format!(
            "several serial ports ({}); pick one with --port",
            ports.join(", ")
        )),
    }
}

fn wait_port(kind: Kind, wait: Duration) -> Result<String, String> {
    let end = Instant::now() + wait;
    loop {
        if let Some(p) = one(kind) {
            // The port shows before the device takes data.
            sleep(Duration::from_millis(300));
            return Ok(p);
        }
        if Instant::now() > end {
            return Err(format!("no board in {kind:?} mode on USB after {wait:?}"));
        }
        sleep(Duration::from_millis(250));
    }
}

/// A line-based conversation with the bootloader.
struct Line<'p> {
    port: &'p mut serialport::TTYPort,
    name: &'p str,
    buf: Vec<u8>,
}

impl Line<'_> {
    fn send(&mut self, s: &str) -> Result<(), String> {
        self.port
            .write_all(s.as_bytes())
            .map_err(|e| format!("{}: {e}", self.name))
    }

    /// Reads until a line that starts with one of want, and returns it;
    /// a line `error: ...` ends it with that error.
    fn expect(&mut self, want: &[&str], wait: Duration) -> Result<String, String> {
        let end = Instant::now() + wait;
        let mut chunk = [0u8; 256];
        loop {
            while let Some(i) = self.buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = self.buf.drain(..=i).collect();
                let line = usbserial::strip(&String::from_utf8_lossy(&raw));
                let line = line.trim().trim_start_matches(">>> ").to_string();
                if let Some(e) = line.strip_prefix("error: ") {
                    return Err(format!("the bootloader: {e}"));
                }
                if want.iter().any(|w| line.starts_with(w)) {
                    return Ok(line);
                }
            }
            if Instant::now() > end {
                return Err(format!(
                    "{}: no {want:?} from the bootloader after {wait:?}",
                    self.name
                ));
            }
            match self.port.read(&mut chunk) {
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {}
                Err(e) => return Err(format!("{}: {e}", self.name)),
            }
        }
    }
}

/// Sends data with fw put and boots it.
fn put(port: &str, data: &[u8], force: bool) -> Result<(), String> {
    let mut p = usbserial::open(port, 115_200).map_err(|e| e.to_string())?;
    let mut l = Line {
        port: &mut p,
        name: port,
        buf: Vec::new(),
    };
    // Past the banner, to a prompt.
    l.send("\r")?;
    sleep(Duration::from_millis(200));
    l.buf.clear();
    let mut cmd = format!("fw put {} {:08x}", data.len(), crc32(data));
    if force {
        cmd.push_str(" force");
    }
    l.send(&format!("{cmd}\r"))?;
    let ready = l.expect(&["ready "], Duration::from_secs(5))?;
    let chunk: usize = ready["ready ".len()..]
        .trim()
        .parse()
        .map_err(|_| format!("the bootloader: {ready:?}"))?;
    let mut off = 0;
    for piece in data.chunks(chunk) {
        l.port.write_all(piece).map_err(|e| format!("{port}: {e}"))?;
        off += piece.len();
        // Erasing and writing internal flash: up to a few hundred ms.
        if off < data.len() {
            l.expect(&["ok "], Duration::from_secs(5))?;
        }
    }
    // The checks at the end read all of internal flash.
    l.expect(&["done "], Duration::from_secs(20))?;
    l.send("boot\r")?;
    let _ = l.expect(&["booting"], Duration::from_secs(2));
    Ok(())
}

/// Waits for the board's console to answer version and returns the line.
fn wait_version(wait: Duration) -> Result<String, String> {
    let end = Instant::now() + wait;
    let mut last = String::new();
    while Instant::now() < end {
        sleep(Duration::from_secs(1));
        let Some(p) = one(Kind::App) else {
            last = "waiting for the board's serial port".into();
            continue;
        };
        if !Path::new(&p).exists() {
            continue;
        }
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
