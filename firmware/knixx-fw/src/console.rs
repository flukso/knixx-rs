//! The console on the USB CDC port: a banner on the first key, then command
//! lines after the prompt. The line editor of the Go firmware, with its
//! history, follows with the shared console crate.

use core::fmt::{self, Write};

use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_time::{Instant, Timer};
use embassy_usb::class::cdc_acm::Receiver;

use crate::{boot, panic, usbcdc};

const PROMPT: &str = ">>> ";

/// The version line, also checked by bolt uf2 -expect after a flash.
pub const VERSION: &str = concat!(
    "version ",
    env!("KNIXX_COMMIT"),
    " built ",
    env!("KNIXX_BUILD_DATE")
);

/// Console output: "\n" becomes "\r\n" for serial terminals. Output that
/// does not fit the USB queue is dropped.
struct Out;

impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut parts = s.split('\n');
        if let Some(first) = parts.next() {
            usbcdc::write(first.as_bytes());
        }
        for part in parts {
            usbcdc::write(b"\r\n");
            usbcdc::write(part.as_bytes());
        }
        Ok(())
    }
}

/// What a command asks of the console after it ran.
enum Action {
    None,
    Reboot(u8),
}

struct Cmd {
    name: &'static str,
    help: &'static str,
    run: fn(&[&str], &mut Out) -> Action,
}

const CMDS: &[Cmd] = &[
    Cmd {
        name: "help",
        help: "list the commands",
        run: help,
    },
    Cmd {
        name: "version",
        help: "show the commit and build time of the firmware",
        run: version,
    },
    Cmd {
        name: "sys",
        help: "show the uptime and the console queue",
        run: sys,
    },
    Cmd {
        name: "ota",
        help: "reboot into the bootloader for a BLE firmware update (DFU)",
        run: ota,
    },
];

fn help(_: &[&str], w: &mut Out) -> Action {
    for c in CMDS {
        let _ = writeln!(w, "{:<8} {}", c.name, c.help);
    }
    Action::None
}

fn version(_: &[&str], w: &mut Out) -> Action {
    let _ = writeln!(w, "{VERSION}");
    Action::None
}

fn sys(_: &[&str], w: &mut Out) -> Action {
    let _ = writeln!(w, "uptime     {}", Uptime(Instant::now().as_secs()));
    let _ = writeln!(
        w,
        "usb out    {} of {} bytes queued",
        usbcdc::OUT.len(),
        usbcdc::OUT.capacity()
    );
    let _ = writeln!(
        w,
        "usb dtr    {}",
        usbcdc::DTR.load(core::sync::atomic::Ordering::Relaxed)
    );
    Action::None
}

fn ota(_: &[&str], w: &mut Out) -> Action {
    let _ = writeln!(w, "rebooting into the bootloader for a BLE firmware update");
    Action::Reboot(boot::OTA)
}

/// A duration in seconds, formatted as Go's time.Duration: 45s, 1m0s,
/// 2h26m30s.
struct Uptime(u64);

impl fmt::Display for Uptime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (h, m, s) = (self.0 / 3600, self.0 / 60 % 60, self.0 % 60);
        if h > 0 {
            write!(f, "{h}h{m}m{s}s")
        } else if m > 0 {
            write!(f, "{m}m{s}s")
        } else {
            write!(f, "{s}s")
        }
    }
}

/// Runs the command line.
fn exec(line: &str, w: &mut Out) -> Action {
    let mut args = [""; 8];
    let mut n = 0;
    for a in line.split_ascii_whitespace() {
        if n == args.len() {
            let _ = writeln!(w, "too many arguments");
            return Action::None;
        }
        args[n] = a;
        n += 1;
    }
    let Some(&name) = args[..n].first() else {
        return Action::None;
    };
    if name == "panic" {
        // Hidden: tests the panic record.
        panic!("console panic command");
    }
    match CMDS.iter().find(|c| c.name == name) {
        Some(c) => (c.run)(&args[1..n], w),
        None => {
            let _ = writeln!(w, "unknown command {name:?}, try help");
            Action::None
        }
    }
}

#[embassy_executor::task]
pub async fn task(mut rx: Receiver<'static, Driver<'static, HardwareVbusDetect>>) -> ! {
    let mut msg = [0u8; 200];
    let last_panic = panic::take_last(&mut msg);
    let mut greeted = false;
    let mut line = [0u8; 255];
    let mut len = 0;
    let mut cr = false;
    let mut buf = [0u8; 64];
    let mut w = Out;
    loop {
        rx.wait_connection().await;
        let Ok(n) = rx.read_packet(&mut buf).await else {
            continue;
        };
        for &b in &buf[..n] {
            if !greeted {
                greeted = true;
                let _ = writeln!(w, "=== knixx thunder ===\n{VERSION}");
                if let Some(m) = last_panic {
                    let _ = writeln!(w, "last panic: {m}");
                }
                let _ = w.write_str(PROMPT);
                if b == b'\r' || b == b'\n' {
                    cr = b == b'\r';
                    continue;
                }
            }
            match b {
                b'\n' if cr => {}
                b'\r' | b'\n' => {
                    let _ = w.write_str("\n");
                    let action = match core::str::from_utf8(&line[..len]) {
                        Ok(s) => exec(s, &mut w),
                        Err(_) => Action::None,
                    };
                    len = 0;
                    if let Action::Reboot(magic) = action {
                        // Leave the message time to reach the host.
                        Timer::after_millis(200).await;
                        boot::reset_into(magic);
                    }
                    let _ = w.write_str(PROMPT);
                }
                0x08 | 0x7f => {
                    if len > 0 {
                        len -= 1;
                        usbcdc::write(b"\x08 \x08");
                    }
                }
                0x20..=0x7e if len < line.len() => {
                    line[len] = b;
                    len += 1;
                    usbcdc::write(&[b]);
                }
                _ => {}
            }
            cr = b == b'\r';
        }
    }
}
