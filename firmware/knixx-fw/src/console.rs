//! The console on the USB CDC port: the banner on the first key, then lines
//! edited with history (knixx_console::lineedit) and run as commands: the
//! shared ones of knixx_cli and the firmware's own.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};
use core::sync::atomic::Ordering;

use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Instant, Timer};
use embassy_usb::class::cdc_acm::Receiver;
use knixx_bus::{Command, Flags, QUEUE_LEN};
use knixx_cli::{Action, Cmd};
use knixx_console::lineedit::{Editor, Sink};
use knixx_device::thunder::Device;

use crate::{boot, panic, storage, usbcdc};

const PROMPT: &str = ">>> ";

/// The version line, also checked by bolt uf2 --expect after a flash.
pub const VERSION: &str = concat!(
    "version ",
    env!("KNIXX_COMMIT"),
    " built ",
    env!("KNIXX_BUILD_DATE")
);

/// The device configuration that the console, the switches and the
/// sensors read and the config command changes.
pub static DEVICE: Mutex<CriticalSectionRawMutex, Cell<Option<Device>>> = Mutex::new(Cell::new(None));

/// The commands for the bus engine.
pub static CMDS: Channel<CriticalSectionRawMutex, Command, QUEUE_LEN> = Channel::new();

/// The flags shared with the bus engine.
pub static FLAGS: Flags = Flags::new();

/// Returns the device configuration.
pub fn device() -> Device {
    DEVICE.lock(|d| d.get()).unwrap_or_default()
}

/// Console output: "\n" becomes "\r\n" for serial terminals. Output that
/// does not fit the queue is dropped.
pub struct Out;

impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        usbcdc::write_crlf(s.as_bytes());
        Ok(())
    }
}

/// The line editor's raw output: its echo and redraws.
struct Echo;

impl Sink for Echo {
    fn write(&mut self, b: &[u8]) {
        usbcdc::write(b);
    }
}

/// The context of the console commands: the device configuration, the bus
/// engine's command queue and flags, and the configuration store.
pub struct Ctx;

/// The boot log, shown under the banner: what loading the configuration
/// reported.
pub static BOOT_LOG: Mutex<CriticalSectionRawMutex, RefCell<Log>> = Mutex::new(RefCell::new(Log::new()));

/// A log of lines in a fixed buffer; what does not fit is cut off.
pub struct Log {
    buf: [u8; 512],
    len: usize,
}

impl Log {
    pub const fn new() -> Log {
        Log {
            buf: [0; 512],
            len: 0,
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl Write for Log {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

impl knixx_cli::Ctx for Ctx {
    type SaveError = storage::SaveError;

    fn device(&self) -> Device {
        device()
    }

    fn set_device(&mut self, dev: Device) {
        DEVICE.lock(|d| d.set(Some(dev)));
    }

    fn send(&mut self, cmd: Command) -> bool {
        CMDS.try_send(cmd).is_ok()
    }

    fn flags(&self) -> &Flags {
        &FLAGS
    }

    fn save(&mut self, data: &[u8]) -> Result<(), storage::SaveError> {
        storage::save(data)
    }
}

const fn commands() -> [Cmd<Ctx>; knixx_cli::NUM_SHARED + 3] {
    let s = knixx_cli::commands::<Ctx>();
    [
        s[0],
        s[1],
        s[2],
        s[3],
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
    ]
}

static COMMANDS: [Cmd<Ctx>; knixx_cli::NUM_SHARED + 3] = commands();

fn version(_: &mut Ctx, _: &[&str], w: &mut dyn Write) -> Action {
    let _ = writeln!(w, "{VERSION}");
    Action::None
}

fn sys(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    if !args.is_empty() {
        let _ = writeln!(w, "usage: sys");
        return Action::None;
    }
    let _ = writeln!(w, "uptime     {}", Uptime(Instant::now().as_secs()));
    let (queued, lost) = usbcdc::OUT.lock(|q| {
        let q = q.borrow();
        (q.buffered(), q.lost())
    });
    let _ = writeln!(
        w,
        "{:<10} {queued} of {} bytes queued, {lost} lost",
        "usb out",
        knixx_console::outq::LEN
    );
    let _ = writeln!(w, "usb dtr    {}", usbcdc::DTR.load(Ordering::Relaxed));
    Action::None
}

fn ota(_: &mut Ctx, _: &[&str], w: &mut dyn Write) -> Action {
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

#[embassy_executor::task]
pub async fn task(mut rx: Receiver<'static, Driver<'static, HardwareVbusDetect>>) -> ! {
    let mut msg = [0u8; 200];
    let last_panic = panic::take_last(&mut msg);
    let mut editor = Editor::new(PROMPT);
    let mut ctx = Ctx;
    let mut greeted = false;
    let mut buf = [0u8; 64];
    loop {
        rx.wait_connection().await;
        let Ok(n) = rx.read_packet(&mut buf).await else {
            continue;
        };
        for &b in &buf[..n] {
            if !greeted {
                greeted = true;
                let _ = writeln!(Out, "=== knixx thunder ===\n{VERSION}");
                BOOT_LOG.lock(|l| usbcdc::write_crlf(l.borrow().as_bytes()));
                if let Some(m) = last_panic {
                    let _ = writeln!(Out, "last panic: {m}");
                }
                usbcdc::write(PROMPT.as_bytes());
                if b == b'\r' || b == b'\n' {
                    // Let the editor see the CR, so that a following LF is
                    // taken as part of the pair; the empty line is dropped.
                    let _ = editor.feed(b, &mut Echo);
                    continue;
                }
            }
            let Some(line) = editor.feed(b, &mut Echo) else {
                continue;
            };
            usbcdc::write_crlf(b"\n");
            let action = match core::str::from_utf8(line) {
                Ok(s) => knixx_cli::exec(&COMMANDS, &mut ctx, s, &mut Out),
                Err(_) => Action::None,
            };
            if let Action::Reboot(magic) = action {
                // Leave the reply time to reach the host.
                Timer::after_millis(200).await;
                boot::reset_into(magic);
            }
            usbcdc::write(PROMPT.as_bytes());
        }
    }
}
