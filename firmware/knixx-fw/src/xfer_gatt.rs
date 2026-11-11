//! The transfer service: firmware, scripts and the config, pushed to the
//! board and pulled from it over BLE with the transfer protocol
//! (knixx_xfer), in an update window, without pairing. Firmware must be
//! signed; scripts and the config are taken as they come, which the
//! window guards: it takes a command on the console, or the KNX control
//! plane's open.
//!
//! - **ctrl** (write, notify): the requests (put, get, abort) and the
//!   receiver's answers (ack, rewind, done, error).
//! - **data** (write without response, notify): data messages, the host's
//!   in a push, the board's in a pull.
//!
//! The write handler only feeds the receiver. When the last byte is in, the
//! transfer task sends the last ack first and only then checks the file
//! (for firmware, the signature and a read-back of the whole image: seconds
//! during which the executor blocks on the flash), and then done or error.

use core::cell::RefCell;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::once_lock::OnceLock;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use knixx_cli::LoadError;
use knixx_console::name::Name;
use knixx_device::config;
use knixx_image::stage::{Files, StageError, Stager};
use knixx_image::{self as image, Crc32, Refusal};
use knixx_xfer::code;
use knixx_xfer::msg::{FORCE, Kind, MAX_CONTROL, Msg};
use knixx_xfer::receiver::{Fail, Offer, Out, Receiver, Sink};
use trouble_host::prelude::*;

use crate::storage::{self, FileError, Fs};
use crate::{boot, fw, script};

/// The service UUID, 6b6e7810-1b5d-4c8e-a2f0-3e5b9d8c7a10.
#[gatt_service(uuid = "6b6e7810-1b5d-4c8e-a2f0-3e5b9d8c7a10")]
pub struct XferService {
    #[characteristic(uuid = "6b6e7811-1b5d-4c8e-a2f0-3e5b9d8c7a10", write, notify)]
    pub ctrl: heapless::Vec<u8, CTRL>,
    #[characteristic(
        uuid = "6b6e7812-1b5d-4c8e-a2f0-3e5b9d8c7a10",
        write,
        write_without_response,
        notify
    )]
    pub data: heapless::Vec<u8, 244>,
}

/// The largest value of ctrl: a put with the longest name.
pub const CTRL: usize = 48;

/// The kinds the board takes.
const KINDS: &[Kind] = &[Kind::Firmware, Kind::Script, Kind::Config];

/// After this long without data, a new put replaces a transfer.
const IDLE_MS: u64 = 10_000;

/// Until when the update window is open; None while it is shut.
static WINDOW: Mutex<CriticalSectionRawMutex, RefCell<Option<Instant>>> = Mutex::new(RefCell::new(None));
/// Wakes the BLE task when the window opens or closes.
pub static WINDOW_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The notifications of ctrl for the central.
static OUT: Channel<CriticalSectionRawMutex, heapless::Vec<u8, CTRL>, 8> = Channel::new();
/// The last byte is in: the transfer task checks the file.
static COMPLETE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The receiver and its sink. In a OnceLock, made at run time, so that the
/// stager's buffers lie in .bss: as a plain static their initial value
/// would need an image in flash (.data).
struct Xfer {
    rx: Receiver,
    sink: AppSink,
}

static XFER: OnceLock<Mutex<ThreadModeRawMutex, RefCell<Xfer>>> = OnceLock::new();

fn xfer() -> &'static Mutex<ThreadModeRawMutex, RefCell<Xfer>> {
    XFER.get_or_init(|| {
        Mutex::new(RefCell::new(Xfer {
            rx: Receiver::new(KINDS, IDLE_MS),
            sink: AppSink {
                into: Into::Nothing,
                last_error: 0,
            },
        }))
    })
}

/// Where the board's received files go.
struct AppSink {
    into: Into,
    /// The code of the last failed file, 0 for none.
    last_error: u8,
}

/// The file being received.
#[allow(clippy::large_enum_variant, reason = "one, in a static")]
enum Into {
    Nothing,
    /// A firmware image, into the stager.
    Firmware(Stager),
    /// A script, appended to its .tmp file, which replaces the script once
    /// it reads back right.
    Script {
        name: Name,
        crc: Crc32,
    },
    /// The config, in RAM: it is checked and applied whole.
    Config {
        buf: heapless::Vec<u8, { config::MAX_FILE }>,
        force: bool,
    },
}

const STORAGE: Fail = Fail::new(code::STORAGE, "flash error");
const STATE: Fail = Fail::new(code::STATE, "out of step");

/// The file a script is received into.
fn script_tmp(name: &str) -> heapless::String<64> {
    let mut p = heapless::String::new();
    let _ = core::fmt::Write::write_fmt(&mut p, format_args!("{}/{name}.tmp", script::DIR));
    p
}

/// Checks the received script against what came, puts it in place and has
/// the script task load it.
fn finish_script(name: Name, crc: u32) -> Result<u32, Fail> {
    let tmp = script_tmp(name.as_str());
    let mut back = Crc32::new();
    let mut buf = [0u8; 256];
    let mut off = 0u32;
    loop {
        let n = Fs.read_at(&tmp, off, &mut buf).map_err(|_| STORAGE)?;
        if n == 0 {
            break;
        }
        back.update(&buf[..n]);
        off += n as u32;
    }
    if back.finish() != crc {
        return Err(Fail::new(code::CRC, "read-back mismatch"));
    }
    Fs.rename(&tmp, &script::path(name.as_str()))
        .map_err(|_| STORAGE)?;
    #[cfg(feature = "scripts")]
    let _ = crate::runner::CONTROL.try_send(crate::runner::Control::Load(name));
    Ok(off)
}

/// Checks, applies and saves a received config, as config_load does.
fn finish_config(data: &[u8], force: bool) -> Result<u32, Fail> {
    match knixx_cli::config_load(&mut crate::console::Ctx, data, force) {
        Ok(_) => Ok(data.len() as u32),
        Err(LoadError::Parse) => Err(Fail::new(code::CONFIG, "does not parse")),
        Err(LoadError::Guard) => Err(Fail::new(code::GUARD, "another individual address")),
        Err(LoadError::Check(why)) => Err(Fail::new(code::CONFIG, why)),
        Err(LoadError::QueueFull) => Err(Fail::new(code::BUSY, "bus queue full")),
        Err(LoadError::Save(_)) => Err(Fail::new(code::STORAGE, "applied, not saved")),
    }
}

fn refusal(r: Refusal) -> Fail {
    let reason = match r {
        Refusal::Header(_) => "bad header",
        Refusal::Length => "length mismatch",
        Refusal::Crc => "body CRC mismatch",
        Refusal::UnknownKey(_) => "unknown key",
        Refusal::Signature => "bad signature",
        Refusal::Downgrade { .. } => "downgrade",
        Refusal::TooLarge(_) => "too large",
    };
    Fail::new(r.code(), reason)
}

fn stage_fail(e: StageError<FileError>) -> Fail {
    match e {
        StageError::Refused(r) => refusal(r),
        StageError::Space { .. } => Fail::new(code::SPACE, "no room on the flash"),
        StageError::TransportCrc | StageError::ReadBack => Fail::new(code::CRC, "read-back mismatch"),
        StageError::Files(_) => STORAGE,
        StageError::Overflow | StageError::SizeMismatch { .. } | StageError::Incomplete { .. } => STATE,
    }
}

impl Sink for AppSink {
    fn open(&mut self, kind: Kind, name: &str, size: u32, flags: u8) -> Result<(), Fail> {
        self.abort();
        self.into = match kind {
            Kind::Firmware => Into::Firmware(Stager::begin(&mut Fs, size).map_err(stage_fail)?),
            Kind::Script => {
                let name = Name::new(name).ok_or(Fail::new(code::NAME, "bad name"))?;
                storage::make_dir(script::DIR).map_err(|_| STORAGE)?;
                let tmp = script_tmp(name.as_str());
                let free = Fs.free().map_err(|_| STORAGE)?;
                if free < size + 4096 {
                    return Err(Fail::new(code::SPACE, "no room on the flash"));
                }
                Fs.create(&tmp).map_err(|_| STORAGE)?;
                Into::Script {
                    name,
                    crc: Crc32::new(),
                }
            }
            Kind::Config => Into::Config {
                buf: heapless::Vec::new(),
                force: flags & FORCE != 0,
            },
            Kind::ScriptList => return Err(Fail::new(code::KIND, "not taken here")),
        };
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Fail> {
        match &mut self.into {
            Into::Nothing => Err(STATE),
            Into::Firmware(s) => {
                let running = boot::header();
                s.feed(&mut Fs, data, fw::keys(), running.as_ref())
                    .map_err(stage_fail)
            }
            Into::Script { name, crc } => {
                crc.update(data);
                Fs.append(&script_tmp(name.as_str()), data)
                    .map(|_| ())
                    .map_err(|_| STORAGE)
            }
            Into::Config { buf, .. } => buf
                .extend_from_slice(data)
                .map_err(|_| Fail::new(code::SIZE, "too large")),
        }
    }

    fn finish(&mut self) -> Result<u32, Fail> {
        match core::mem::replace(&mut self.into, Into::Nothing) {
            Into::Nothing => Err(STATE),
            Into::Firmware(s) => match s.finish(&mut Fs, None) {
                Ok(h) => Ok(h.counter),
                Err(e) => {
                    let _ = Fs.remove(image::STAGED_TMP);
                    Err(stage_fail(e))
                }
            },
            Into::Script { name, crc } => finish_script(name, crc.finish()).inspect_err(|_| {
                let _ = Fs.remove(&script_tmp(name.as_str()));
            }),
            Into::Config { buf, force } => finish_config(&buf, force),
        }
    }

    fn abort(&mut self) {
        match core::mem::replace(&mut self.into, Into::Nothing) {
            Into::Firmware(_) => {
                let _ = Fs.remove(image::STAGED_TMP);
            }
            Into::Script { name, .. } => {
                let _ = Fs.remove(&script_tmp(name.as_str()));
            }
            Into::Nothing | Into::Config { .. } => {}
        }
    }
}

/// Opens the update window for d, or shuts it with None.
pub fn set_window(d: Option<Duration>) {
    WINDOW.lock(|w| *w.borrow_mut() = d.map(|d| Instant::now() + d));
    WINDOW_CHANGED.signal(());
}

/// Whether the update window is open.
pub fn window_open() -> bool {
    WINDOW.lock(|w| w.borrow().is_some_and(|t| Instant::now() < t))
}

/// The time left in the window.
pub fn window_left() -> Option<Duration> {
    WINDOW.lock(|w| w.borrow().and_then(|t| t.checked_duration_since(Instant::now())))
}

/// Waits until the window opens or closes.
pub async fn window_change() {
    let close = WINDOW.lock(|w| *w.borrow());
    match close {
        Some(t) if Instant::now() < t => {
            embassy_futures::select::select(WINDOW_CHANGED.wait(), Timer::at(t)).await;
        }
        _ => WINDOW_CHANGED.wait().await,
    }
}

/// Whether a transfer is in progress.
pub fn busy() -> bool {
    xfer().lock(|x| x.borrow().rx.busy())
}

/// How much of the incoming file is in, in percent; 0 without one.
pub fn progress() -> u8 {
    xfer().lock(|x| {
        x.borrow().rx.progress().map_or(0, |(_, _, size, got)| {
            (u64::from(got) * 100 / u64::from(size.max(1))) as u8
        })
    })
}

/// The code of the last failed file, 0 for none.
pub fn last_error() -> u8 {
    xfer().lock(|x| x.borrow().sink.last_error)
}

/// Drops a transfer in progress: the KNX control plane's close.
pub fn abort() {
    xfer().lock(|x| {
        let x = &mut *x.borrow_mut();
        x.rx.abort(&mut x.sink);
    });
}

fn now_ms() -> u64 {
    Instant::now().as_millis()
}

/// Queues an answer for the central.
fn notify(out: Out) {
    let Some(m) = out.msg() else {
        return;
    };
    let mut buf = [0u8; MAX_CONTROL];
    if let Ok(n) = m.encode(&mut buf) {
        let mut v = heapless::Vec::new();
        let _ = v.extend_from_slice(&buf[..n.min(CTRL)]);
        let _ = OUT.try_send(v);
    }
}

/// Handles a write of ctrl.
pub fn ctrl(data: &[u8]) {
    let Some(m) = Msg::decode(data) else {
        return;
    };
    match m {
        Msg::Put { crc, .. } => {
            if !window_open() {
                notify(Out::Error {
                    code: code::WINDOW,
                    crc,
                    reason: "no update window",
                });
                return;
            }
            let Some(offer) = Offer::of(&m) else {
                return;
            };
            let out = xfer().lock(|x| {
                let x = &mut *x.borrow_mut();
                x.rx.put(&mut x.sink, &offer, now_ms())
            });
            notify(out);
        }
        Msg::Abort => abort(),
        Msg::Get { .. } => notify(Out::Error {
            code: code::KIND,
            crc: 0,
            reason: "no pulls yet",
        }),
        _ => {}
    }
}

/// Handles a write of data.
pub fn data(write: &[u8]) {
    let Some(Msg::Data { offset, bytes }) = Msg::decode(write) else {
        return;
    };
    let out = xfer().lock(|x| {
        let x = &mut *x.borrow_mut();
        x.rx.data(&mut x.sink, offset, bytes, now_ms())
    });
    if let Out::Error { code, .. } = out {
        xfer().lock(|x| x.borrow_mut().sink.last_error = code);
    }
    let complete = matches!(out, Out::Complete(_));
    notify(out);
    if complete {
        COMPLETE.signal(());
    }
}

/// Checks a complete file once its last ack went out: the board's side of
/// the end of a transfer, outside the BLE task's write handler.
#[embassy_executor::task]
pub async fn task() -> ! {
    loop {
        COMPLETE.wait().await;
        // Let the BLE task send the last ack before the check blocks.
        let _ = with_timeout(Duration::from_millis(300), async {
            while !OUT.is_empty() {
                Timer::after_millis(10).await;
            }
        })
        .await;
        Timer::after_millis(20).await;
        let out = xfer().lock(|x| {
            let x = &mut *x.borrow_mut();
            let out = x.rx.finish(&mut x.sink);
            x.sink.last_error = match out {
                Out::Error { code, .. } => code,
                _ => 0,
            };
            out
        });
        notify(out);
    }
}

/// Drops what was queued for an earlier central.
pub fn reset() {
    OUT.clear();
}

/// Notifies the central of ctrl's answers until one fails.
pub async fn serve<P: PacketPool>(svc: &XferService, conn: &GattConnection<'_, '_, P>) {
    loop {
        let n = OUT.receive().await;
        if svc.ctrl.notify_raw(conn, &n, false).await.is_err() {
            return;
        }
    }
}
