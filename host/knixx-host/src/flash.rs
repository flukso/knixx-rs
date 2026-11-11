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

use std::sync::mpsc;

use knixx_image::crc32;
use knixx_xfer::frame::{self, Decoder, Fed};
use knixx_xfer::msg::{FORCE, Kind as XKind, Msg};
use knixx_xfer::sender::{Event, Sender};

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

/// How long the host waits for an answer to a frame before it sends again.
const FRAME_WAIT: Duration = Duration::from_secs(2);
/// How often a frame is sent again before the flash gives up.
const MAX_STALLS: u32 = 8;
/// How long the bootloader may take to check and write the header.
const DONE_WAIT: Duration = Duration::from_secs(30);

/// Sends msg to the bootloader, framed.
fn send(port: &mut serialport::TTYPort, name: &str, m: &Msg) -> Result<(), String> {
    let mut msg = vec![0u8; frame::MAX_MSG];
    let n = m.encode(&mut msg).map_err(|_| "message too large".to_string())?;
    let mut buf = vec![0u8; n + frame::OVERHEAD];
    let n = frame::encode(&msg[..n], &mut buf).ok_or("frame too large")?;
    port.write_all(&buf[..n]).map_err(|e| format!("{name}: {e}"))
}

/// Sends data, a .kfw, to the bootloader's recovery over the transfer
/// protocol (knixx_xfer): put, then data in 4 KiB frames, each sent again
/// when no answer comes, then done. Boots it after.
fn put(port: &str, data: &[u8], force: bool) -> Result<(), String> {
    let mut p = usbserial::open(port, 115_200).map_err(|e| e.to_string())?;
    let mut reader = p.try_clone_native().map_err(|e| format!("{port}: {e}"))?;
    // A reader thread finds the frames among the console's text.
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut dec = Decoder::new();
        let mut buf = [0u8; 512];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    for &b in &buf[..n] {
                        if dec.feed(b) == Fed::Frame && tx.send(dec.message().to_vec()).is_err() {
                            return;
                        }
                    }
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
                Err(_) => return,
            }
        }
    });
    let size = data.len() as u32;
    let crc = crc32(data);
    let put = Msg::Put {
        kind: XKind::Firmware,
        flags: if force { FORCE } else { 0 },
        size,
        crc,
        name: "",
    };
    // put, until the bootloader answers it.
    let mut from = None;
    for _ in 0..MAX_STALLS {
        send(&mut p, port, &put)?;
        let end = Instant::now() + FRAME_WAIT;
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            let Ok(b) = rx.recv_timeout(left) else { break };
            match Msg::decode(&b) {
                Some(Msg::Ack(o)) => {
                    from = Some(o);
                    break;
                }
                Some(Msg::Error { reason, crc: c, .. }) if c == crc || c == 0 => {
                    return Err(format!("the bootloader: {reason}"));
                }
                _ => {}
            }
        }
        if from.is_some() {
            break;
        }
    }
    let from = from.ok_or("the bootloader does not answer the transfer protocol")?;
    if from > 0 {
        status(format!(
            "the bootloader has {} KiB of it already: going on from there",
            from / 1024
        ));
    }
    let chunk = knixx_xfer::ACK_EVERY;
    let mut sender = Sender::new(size, crc, from, chunk, chunk, MAX_STALLS);
    loop {
        while let Some((at, len)) = sender.next_piece() {
            let bytes = &data[at as usize..(at + len) as usize];
            send(&mut p, port, &Msg::Data { offset: at, bytes })?;
        }
        let wait = if sender.all_acked() { DONE_WAIT } else { FRAME_WAIT };
        match rx.recv_timeout(wait) {
            Ok(b) => {
                let Some(m) = Msg::decode(&b) else { continue };
                match sender.on(&m).map_err(|e| format!("the bootloader: {e}"))? {
                    Event::Done(_) => break,
                    Event::Progress(_) | Event::None => {}
                }
            }
            Err(_) if sender.all_acked() => return Err("no done from the bootloader".into()),
            Err(_) => sender.silence().map_err(|e| format!("the bootloader: {e}"))?,
        }
    }
    p.write_all(b"boot\r").map_err(|e| format!("{port}: {e}"))?;
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
