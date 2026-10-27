//! The console on the USB CDC port, or on BLE (the Nordic UART Service, see
//! nus) after nus on: the banner on the first key, then lines edited with
//! history (knixx_console::lineedit) and run as commands: the shared ones of
//! knixx_cli and the firmware's own. The console task alone moves the
//! console between the ports.

use core::cell::{Cell, RefCell};
use core::fmt::{self, Write};
use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

use embassy_futures::select::{Either3, select3};
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
use knixx_console::nus::{IDLE_TIMEOUT, Timer as IdleTimer};
use knixx_device::thunder::Device;
use knixx_ui::led::Mode;
use knixx_ui::switches::HOLD_TIME;

use crate::{boot, leds, nus, panic, storage, usbcdc};

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

/// Whether the console runs on NUS rather than USB.
static ON_NUS: AtomicBool = AtomicBool::new(false);

/// The timer that runs on NUS while no central is connected.
static IDLE: Mutex<CriticalSectionRawMutex, RefCell<IdleTimer>> = Mutex::new(RefCell::new(IdleTimer::new()));

/// The latest NUS request of the wall switch gestures, which the console
/// task carries out: KEEP, WANT_ON or WANT_OFF.
static NUS_REQUEST: AtomicU8 = AtomicU8::new(KEEP);
const KEEP: u8 = 0;
const WANT_ON: u8 = 1;
const WANT_OFF: u8 = 2;

/// Asks the console task for nus on or off, as a wall switch gesture does.
pub fn request_nus(on: bool) {
    NUS_REQUEST.store(if on { WANT_ON } else { WANT_OFF }, Ordering::Relaxed);
}

fn on_nus() -> bool {
    ON_NUS.load(Ordering::Relaxed)
}

/// The time since boot, for the idle timer.
fn now() -> core::time::Duration {
    core::time::Duration::from_micros(Instant::now().as_micros())
}

/// Console output, to the port the console runs on: "\n" becomes "\r\n"
/// for serial terminals. Output that does not fit the queue is dropped.
pub struct Out;

impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        if on_nus() {
            nus::write_crlf(s.as_bytes());
        } else {
            usbcdc::write_crlf(s.as_bytes());
        }
        Ok(())
    }
}

/// The raw console output, such as the line editor's echo and redraws.
struct Echo;

impl Sink for Echo {
    fn write(&mut self, b: &[u8]) {
        if on_nus() {
            nus::write(b);
        } else {
            usbcdc::write(b);
        }
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

const fn commands() -> [Cmd<Ctx>; knixx_cli::NUM_SHARED + 6] {
    let s = knixx_cli::commands::<Ctx>();
    [
        s[0],
        s[1],
        s[2],
        s[3],
        Cmd {
            name: "sensor",
            help: "show the latest temperature, humidity and illuminance reading",
            run: crate::sensor::command,
        },
        Cmd {
            name: "tof",
            help: "show the time-of-flight sensor state and its latest motion",
            run: crate::tof::command,
        },
        Cmd {
            name: "nus",
            help: "nus on|off: move the console to BLE (Nordic UART Service) or back to USB",
            run: nus_cmd,
        },
        Cmd {
            name: "version",
            help: "show the commit and build time of the firmware",
            run: version,
        },
        Cmd {
            name: "sys",
            help: "show the uptime and the console queues",
            run: sys,
        },
        Cmd {
            name: "ota",
            help: "reboot into the bootloader for a BLE firmware update (DFU)",
            run: ota,
        },
    ]
}

static COMMANDS: [Cmd<Ctx>; knixx_cli::NUM_SHARED + 6] = commands();

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
    for (name, q) in [("usb out", &usbcdc::OUT), ("nus out", &nus::OUT)] {
        let (queued, lost) = q.lock(|q| {
            let q = q.borrow();
            (q.buffered(), q.lost())
        });
        let _ = writeln!(
            w,
            "{name:<10} {queued} of {} bytes queued, {lost} lost",
            knixx_console::outq::LEN
        );
    }
    let _ = writeln!(w, "usb dtr    {}", usbcdc::DTR.load(Ordering::Relaxed));
    Action::None
}

/// nus [on|off]: shows the NUS state, or asks the console task to move the
/// console to BLE or back to USB.
fn nus_cmd(_: &mut Ctx, args: &[&str], w: &mut dyn Write) -> Action {
    match args {
        [] => {
            if !on_nus() {
                let _ = writeln!(w, "nus: off");
            } else if nus::connected() {
                let _ = writeln!(w, "nus: on, connected");
            } else {
                let left = IDLE.lock(|t| t.borrow().left(now()));
                let _ = writeln!(w, "nus: on, advertising, {} s left", left.as_secs());
            }
            Action::None
        }
        ["on"] => {
            if on_nus() {
                let _ = writeln!(w, "nus: already on");
                return Action::None;
            }
            let _ = writeln!(
                w,
                "nus: advertising as \"{}\"; the console moves to BLE, back to USB after {} s without a connection, on nus off or on reset",
                nus::NAME,
                IDLE_TIMEOUT.as_secs()
            );
            Action::Nus(true)
        }
        ["off"] => {
            if !on_nus() {
                let _ = writeln!(w, "nus: not on");
                return Action::None;
            }
            let _ = writeln!(w, "nus: off, console back on USB");
            Action::Nus(false)
        }
        _ => {
            let _ = writeln!(w, "usage: nus [on|off]");
            Action::None
        }
    }
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

/// The console task's state.
struct Console {
    editor: Editor,
    /// Whether the banner was shown on the port: it is shown on the first
    /// key, so that it is not lost before a terminal is attached.
    greeted: bool,
    /// The connection state of the central that the console last saw.
    was_connected: bool,
    last_panic: Option<&'static str>,
    ctx: Ctx,
}

impl Console {
    /// Moves the console to NUS (on) or USB, dropping the line being typed.
    /// The banner is shown again on the first key on NUS.
    fn attach(&mut self, on: bool) {
        ON_NUS.store(on, Ordering::Relaxed);
        self.editor.discard();
        self.greeted = !on;
    }

    /// Starts advertising NUS and moves the console to it.
    fn nus_on(&mut self) {
        nus::RX.lock(|r| r.borrow_mut().reset());
        self.was_connected = false;
        IDLE.lock(|t| t.borrow_mut().arm(now()));
        nus::OFF_DONE.reset();
        nus::CONTROL.signal(true);
        self.attach(true);
        leds::signal(Mode::Ble);
    }

    /// Stops advertising, lets the queued output reach the central, drops
    /// it and moves the console back to USB.
    async fn nus_off(&mut self) {
        nus::OFF_DONE.reset();
        nus::CONTROL.signal(false);
        nus::OFF_DONE.wait().await;
        IDLE.lock(|t| t.borrow_mut().disarm());
        leds::cancel(Mode::Ble);
        self.attach(false);
    }

    /// Carries out what a command line asked for.
    async fn act(&mut self, action: Action) {
        match action {
            Action::Reboot(magic) => {
                // Leave the reply time to reach the terminal.
                Timer::after_millis(200).await;
                boot::reset_into(magic);
            }
            Action::Nus(true) => self.nus_on(),
            Action::Nus(false) => {
                self.nus_off().await;
                let _ = writeln!(Out, "nus: off, console back on USB");
            }
            Action::None | Action::Exit => {}
        }
    }

    /// Follows the connection of the central and gives the console back to
    /// USB once no central connected for IDLE_TIMEOUT.
    async fn poll_nus(&mut self) {
        if !on_nus() {
            return;
        }
        let c = nus::connected();
        if c != self.was_connected {
            self.was_connected = c;
            if c {
                IDLE.lock(|t| t.borrow_mut().disarm());
                // The banner follows on the central's first key, once it
                // subscribed to the notifications.
                self.editor.discard();
                self.greeted = false;
            } else {
                IDLE.lock(|t| t.borrow_mut().arm(now()));
            }
        }
        if IDLE.lock(|t| t.borrow().expired(now())) {
            self.nus_off().await;
            let _ = write!(
                Out,
                "\nnus: no BLE connection for {} s, console back on USB\n{PROMPT}",
                IDLE_TIMEOUT.as_secs()
            );
        }
    }

    /// Carries out the NUS request of the wall switches as the nus command
    /// would.
    async fn poll_switches(&mut self) {
        let (on, gesture) = match NUS_REQUEST.swap(KEEP, Ordering::Relaxed) {
            WANT_ON => (true, "A on + B off"),
            WANT_OFF => (false, "A off + B on"),
            _ => return,
        };
        let _ = writeln!(Out, "\nswitches: {gesture} held {} s", HOLD_TIME.as_secs());
        let action = nus_cmd(&mut self.ctx, &[if on { "on" } else { "off" }], &mut Out);
        self.act(action).await;
        Echo.write(PROMPT.as_bytes());
    }

    /// Handles a key of the port the console runs on.
    async fn key(&mut self, b: u8) {
        if !self.greeted {
            self.greeted = true;
            let _ = writeln!(Out, "=== knixx thunder ===\n{VERSION}");
            BOOT_LOG.lock(|l| {
                let _ = Out.write_str(core::str::from_utf8(l.borrow().as_bytes()).unwrap_or(""));
            });
            if let Some(m) = self.last_panic {
                let _ = writeln!(Out, "last panic: {m}");
            }
            Echo.write(PROMPT.as_bytes());
            if b == b'\r' || b == b'\n' {
                // Let the editor see the CR, so that a following LF is taken
                // as part of the pair; the empty line is dropped.
                let _ = self.editor.feed(b, &mut Echo);
                return;
            }
        }
        let Some(line) = self.editor.feed(b, &mut Echo) else {
            return;
        };
        let _ = Out.write_str("\n");
        let action = match core::str::from_utf8(line) {
            Ok(s) => knixx_cli::exec(&COMMANDS, &mut self.ctx, s, &mut Out),
            Err(_) => Action::None,
        };
        self.act(action).await;
        Echo.write(PROMPT.as_bytes());
    }
}

#[embassy_executor::task]
pub async fn task(mut rx: Receiver<'static, Driver<'static, HardwareVbusDetect>>) -> ! {
    static MSG: static_cell::StaticCell<[u8; 200]> = static_cell::StaticCell::new();
    let msg = MSG.init([0; 200]);
    let mut con = Console {
        editor: Editor::new(PROMPT),
        greeted: false,
        was_connected: false,
        last_panic: panic::take_last(msg),
        ctx: Ctx,
    };
    let mut buf = [0u8; 64];
    loop {
        con.poll_nus().await;
        con.poll_switches().await;
        let usb = async {
            rx.wait_connection().await;
            rx.read_packet(&mut buf).await
        };
        match select3(usb, nus::RX_READY.wait(), Timer::after_millis(100)).await {
            Either3::First(Ok(n)) => {
                // Typing on USB while the console runs on NUS is dropped.
                for &b in &buf[..n] {
                    if on_nus() {
                        break;
                    }
                    con.key(b).await;
                }
            }
            Either3::First(Err(_)) => {}
            Either3::Second(()) => {
                while on_nus() {
                    let Some(b) = nus::RX.lock(|r| r.borrow_mut().read_byte()) else {
                        break;
                    };
                    con.key(b).await;
                }
            }
            Either3::Third(()) => {}
        }
    }
}
