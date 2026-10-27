//! The USB CDC serial port of a board: finding it by USB ID, the 1200-baud
//! touch that resets it into its bootloader, and a console command with
//! its output.

use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The USB ID of the knixx firmware (as of the TinyGo firmware's).
pub const APP_VID: u16 = 0x239A;
pub const APP_PID: u16 = 0x802A;

/// An error on the serial port.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: {1}")]
    Port(String, serialport::Error),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
}

/// Returns the serial ports of the USB devices with ID vid:pid.
pub fn ports(vid: u16, pid: u16) -> Vec<String> {
    if cfg!(target_os = "linux") {
        return sysfs_ports(Path::new("/"), vid, pid);
    }
    let Ok(all) = serialport::available_ports() else {
        return Vec::new();
    };
    let mut out: Vec<String> = all
        .into_iter()
        .filter(|p| matches!(&p.port_type, serialport::SerialPortType::UsbPort(u) if u.vid == vid && u.pid == pid))
        // macOS lists each port twice; cu.* is the one to open.
        .filter(|p| !p.port_name.starts_with("/dev/tty."))
        .map(|p| p.port_name)
        .collect();
    out.sort();
    out
}

/// Returns the /dev/ttyACM* ports under root whose USB device has ID
/// vid:pid, from sysfs.
pub fn sysfs_ports(root: &Path, vid: u16, pid: u16) -> Vec<String> {
    let class = root.join("sys/class/tty");
    let Ok(entries) = fs::read_dir(&class) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("ttyACM") {
            continue;
        }
        // device is the USB interface; its parent the USB device.
        let Ok(intf) = fs::canonicalize(e.path().join("device")) else {
            continue;
        };
        let read = |f: &str| fs::read_to_string(intf.join("..").join(f)).ok();
        let (Some(v), Some(p)) = (read("idVendor"), read("idProduct")) else {
            continue;
        };
        let hex = |s: &str| u16::from_str_radix(s.trim(), 16).ok();
        if hex(&v) == Some(vid) && hex(&p) == Some(pid) {
            out.push(format!("/dev/{name}"));
        }
    }
    out.sort();
    out
}

/// Opens port name at baud, exclusively (serialport's default: TIOCEXCL
/// and a flock), so that others cannot open it meanwhile. Asking for it a
/// second time fails with EBUSY on macOS.
fn open(name: &str, baud: u32) -> Result<serialport::TTYPort, Error> {
    serialport::new(name, baud)
        .exclusive(true)
        .timeout(Duration::from_millis(50))
        .open_native()
        .map_err(|e| Error::Port(name.into(), e))
}

/// Opens port name at 1200 baud and drops DTR, which makes a board with an
/// Arduino-style USB stack reset into its bootloader. It tries 3 times, 1 s
/// apart.
pub fn reset_1200(name: &str) -> Result<(), Error> {
    let mut last = None;
    for attempt in 0..3 {
        if attempt > 0 {
            sleep(Duration::from_secs(1));
        }
        let r = open(name, 1200).and_then(|mut p| {
            use serialport::SerialPort;
            p.write_data_terminal_ready(false)
                .map_err(|e| Error::Port(name.into(), e))
        });
        match r {
            Ok(()) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

/// Sends a CR, to start the console, then cmd to the console on port name,
/// and returns what came back within wait, without escape sequences and
/// CRs.
pub fn query(name: &str, cmd: &str, wait: Duration) -> Result<String, Error> {
    let mut p = open(name, 115_200)?;
    let mut out = Vec::new();
    let io = |e| Error::Io(name.into(), e);
    p.write_all(b"\r").map_err(io)?;
    read_for(&mut p, &mut out, Duration::from_millis(300)).map_err(io)?;
    p.write_all(format!("{cmd}\r").as_bytes()).map_err(io)?;
    read_for(&mut p, &mut out, wait).map_err(io)?;
    Ok(strip(&String::from_utf8_lossy(&out)))
}

/// Reads from r into out for d.
fn read_for(r: &mut impl Read, out: &mut Vec<u8>, d: Duration) -> std::io::Result<()> {
    let end = Instant::now() + d;
    let mut buf = [0u8; 256];
    while Instant::now() < end {
        match r.read(&mut buf) {
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == ErrorKind::TimedOut || e.kind() == ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Removes CSI escape sequences (ESC [ params letter) and CRs.
pub fn strip(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {}
            '\x1b' if chars.peek() == Some(&'[') => {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn sysfs_by_usb_id() {
        let root = std::env::temp_dir().join(format!("knixx-sysfs-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let dev = |n: &str, vid: &str, pid: &str| {
            let d = root.join(format!("sys/devices/usb1/{n}"));
            fs::create_dir_all(d.join("1-1:1.0")).unwrap();
            fs::write(d.join("idVendor"), format!("{vid}\n")).unwrap();
            fs::write(d.join("idProduct"), format!("{pid}\n")).unwrap();
            d.join("1-1:1.0")
        };
        let tty = root.join("sys/class/tty");
        fs::create_dir_all(&tty).unwrap();
        for (name, intf) in [
            ("ttyACM0", dev("1-1", "239a", "802A")),
            ("ttyACM1", dev("1-2", "1234", "5678")),
            ("ttyS0", dev("1-3", "239a", "802a")),
        ] {
            fs::create_dir_all(tty.join(name)).unwrap();
            symlink(&intf, tty.join(name).join("device")).unwrap();
        }
        assert_eq!(
            sysfs_ports(&root, APP_VID, APP_PID),
            vec!["/dev/ttyACM0".to_string()]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn strip_escapes() {
        assert_eq!(
            strip("\x1b[2K\r>>> version\r\nversion abc\r\n"),
            ">>> version\nversion abc\n"
        );
        assert_eq!(strip("a\x1b[1;32mb"), "ab");
    }
}
