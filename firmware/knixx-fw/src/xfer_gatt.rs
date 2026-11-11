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
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};

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
use knixx_xfer::msg::{DATA_HEADER, FORCE, Kind, MAX_CONTROL, Msg};
use knixx_xfer::receiver::{Fail, Offer, Out, Receiver, Sink};
use knixx_xfer::sender::{Event, Sender};
use knixx_xfer::{MAX_LIST, code};
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
    pub data: heapless::Vec<u8, MAX_DATA>,
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
/// The notifications of data for the central: a pull's data.
static DATA_OUT: Channel<CriticalSectionRawMutex, heapless::Vec<u8, MAX_DATA>, 4> = Channel::new();
/// The largest value of data.
const MAX_DATA: usize = 244;

/// A pull the central asked for, and the link's ATT MTU.
static PULL: Signal<CriticalSectionRawMutex, (Source, u16)> = Signal::new();
/// The central's answers during a pull: ack, rewind, done, error, abort.
static PULL_IN: Channel<CriticalSectionRawMutex, heapless::Vec<u8, CTRL>, 4> = Channel::new();
/// Whether a pull is in progress.
static PULLING: AtomicBool = AtomicBool::new(false);

/// The bytes a pull keeps in flight.
const PULL_WINDOW: u32 = 16 * 1024;
/// How long a pull waits for the central before it goes back to its last
/// ack.
const PULL_STALL: Duration = Duration::from_millis(1500);
/// How often a pull goes back without progress before it gives up: the
/// central went away, or did not subscribe to data.
const PULL_STALLS: u32 = 4;
/// How long a pull waits for the ack of file, and for done.
const PULL_WAIT: Duration = Duration::from_secs(5);
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

/// Whether a transfer is in progress, a push or a pull.
pub fn busy() -> bool {
    PULLING.load(Ordering::Relaxed) || xfer().lock(|x| x.borrow().rx.busy())
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
    if let Some(m) = out.msg() {
        send_ctrl(&m);
    }
}

/// Handles a write of ctrl; mtu is the link's ATT MTU.
pub fn ctrl(data: &[u8], mtu: u16) {
    let Some(m) = Msg::decode(data) else {
        return;
    };
    let pulling = PULLING.load(Ordering::Relaxed);
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
            if pulling {
                notify(Out::Error {
                    code: code::BUSY,
                    crc,
                    reason: "busy",
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
        Msg::Get { kind, name } => match get(kind, name) {
            Ok(src) => {
                PULLING.store(true, Ordering::Relaxed);
                PULL.signal((src, mtu));
            }
            Err(f) => notify(Out::Error {
                code: f.code,
                crc: 0,
                reason: f.reason,
            }),
        },
        Msg::Abort => {
            if pulling {
                to_pull(data);
            }
            abort();
        }
        Msg::Ack(_) | Msg::Rewind(_) | Msg::Done { .. } | Msg::Error { .. } if pulling => to_pull(data),
        _ => {}
    }
}

/// Passes a message of the central to the pull.
fn to_pull(data: &[u8]) {
    let mut v = heapless::Vec::new();
    if v.extend_from_slice(data).is_ok() {
        let _ = PULL_IN.try_send(v);
    }
}

/// What a pull sends.
#[derive(Clone, Copy)]
enum Source {
    Config,
    Script(Name),
    List,
}

impl Source {
    fn kind(self) -> Kind {
        match self {
            Source::Config => Kind::Config,
            Source::Script(_) => Kind::Script,
            Source::List => Kind::ScriptList,
        }
    }
}

/// Takes a get, or refuses it.
fn get(kind: Kind, name: &str) -> Result<Source, Fail> {
    if !window_open() {
        return Err(Fail::new(code::WINDOW, "no update window"));
    }
    if busy() {
        return Err(Fail::new(code::BUSY, "busy"));
    }
    if !knixx_xfer::valid_name(kind, name) {
        return Err(Fail::new(code::NAME, "bad name"));
    }
    match kind {
        Kind::Config => Ok(Source::Config),
        Kind::ScriptList => Ok(Source::List),
        Kind::Script => Name::new(name)
            .map(Source::Script)
            .ok_or(Fail::new(code::NAME, "bad name")),
        Kind::Firmware => Err(Fail::new(code::KIND, "not given here")),
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

/// The transfer task: checks a complete file once its last ack went out,
/// outside the BLE task's write handler, and sends the files the central
/// pulls.
#[embassy_executor::task]
pub async fn task() -> ! {
    loop {
        match embassy_futures::select::select(COMPLETE.wait(), PULL.wait()).await {
            embassy_futures::select::Either::First(()) => complete().await,
            embassy_futures::select::Either::Second((src, mtu)) => {
                PULL_IN.clear();
                if let Err((f, crc)) = pull(src, mtu).await {
                    notify(Out::Error {
                        code: f.code,
                        crc,
                        reason: f.reason,
                    });
                }
                PULLING.store(false, Ordering::Relaxed);
            }
        }
    }
}

async fn complete() {
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

/// Where a pull's bytes come from: a file, or the script list made for it.
#[allow(clippy::large_enum_variant, reason = "one, in the transfer task")]
enum Bytes {
    File(heapless::String<64>),
    Made(heapless::Vec<u8, { MAX_LIST as usize }>),
}

const NOT_FOUND: Fail = Fail::new(code::NOT_FOUND, "no such file");

impl Bytes {
    fn of(src: Source) -> Result<Bytes, Fail> {
        Ok(match src {
            Source::Config => {
                let mut p = heapless::String::new();
                let _ = p.push_str(knixx_storage::CONFIG_NAME);
                Bytes::File(p)
            }
            Source::Script(name) => Bytes::File(script::path(name.as_str())),
            Source::List => Bytes::Made(script_list()?),
        })
    }

    /// Reads at off into buf.
    fn read(&self, off: u32, buf: &mut [u8]) -> Result<usize, Fail> {
        match self {
            Bytes::File(p) => Fs.read_at(p, off, buf).map_err(|_| STORAGE),
            Bytes::Made(v) => {
                let rest = v.get(off as usize..).unwrap_or(&[]);
                let n = rest.len().min(buf.len());
                buf[..n].copy_from_slice(&rest[..n]);
                Ok(n)
            }
        }
    }

    /// The size and the CRC.
    fn measure(&self) -> Result<(u32, u32), Fail> {
        if let Bytes::File(p) = self
            && storage::file_size(p).map_err(|_| STORAGE)?.is_none()
        {
            return Err(NOT_FOUND);
        }
        let mut crc = Crc32::new();
        let mut buf = [0u8; 256];
        let mut size = 0u32;
        loop {
            let n = self.read(size, &mut buf)?;
            if n == 0 {
                return Ok((size, crc.finish()));
            }
            crc.update(&buf[..n]);
            size += n as u32;
        }
    }
}

/// The scripts on the flash, one `<name> <size> <hash>` line each: the hash
/// is the source's, as the script reports it in on_info, `-` when it is not
/// loaded or has none.
fn script_list() -> Result<heapless::Vec<u8, { MAX_LIST as usize }>, Fail> {
    let mut out: heapless::String<{ MAX_LIST as usize }> = heapless::String::new();
    let mut full = false;
    storage::list_dir(script::DIR, |file, size| {
        let Some(name) = file.strip_suffix(script::EXT) else {
            return;
        };
        let mut hash: heapless::String<32> = heapless::String::new();
        #[cfg(feature = "scripts")]
        crate::runner::STATUS.lock(|st| {
            if let Some(s) = st.borrow().iter().find(|s| s.name.as_str() == name) {
                hash = s.info[5].clone();
            }
        });
        if hash.is_empty() || hash.contains(' ') {
            hash.clear();
            let _ = hash.push('-');
        }
        full |= writeln!(out, "{name} {size} {hash}").is_err();
    })
    .map_err(|_| STORAGE)?;
    if full {
        return Err(Fail::new(code::SIZE, "too many scripts"));
    }
    Ok(heapless::Vec::from_slice(out.as_bytes()).unwrap_or_default())
}

/// The central's next message in a pull, within wait.
async fn pull_note(wait: Duration) -> Option<heapless::Vec<u8, CTRL>> {
    with_timeout(wait, PULL_IN.receive()).await.ok()
}

/// Sends the file src to the central: file, then data in notifications of
/// mtu − 3 bytes, until the central says done. On a failure, the reason
/// and the CRC of the file (0 before file).
async fn pull(src: Source, mtu: u16) -> Result<(), (Fail, u32)> {
    let bytes = Bytes::of(src).map_err(|f| (f, 0))?;
    let (size, crc) = bytes.measure().map_err(|f| (f, 0))?;
    let name = match &src {
        Source::Script(n) => n.as_str(),
        _ => "",
    };
    let file = Msg::File {
        kind: src.kind(),
        size,
        crc,
        name,
    };
    send_ctrl(&file);
    // The central acks 0 when it is ready; nothing comes when it did not
    // subscribe to data, or went away.
    match pull_note(PULL_WAIT).await.as_deref().and_then(Msg::decode) {
        Some(Msg::Ack(0)) => {}
        _ => return Ok(()),
    }
    let piece = (usize::from(mtu).saturating_sub(3).min(MAX_DATA) - DATA_HEADER).max(16) as u32;
    let mut s = Sender::new(size, crc, 0, PULL_WINDOW, piece, PULL_STALLS);
    let mut done_by = None;
    loop {
        while let Some((at, len)) = s.next_piece() {
            let mut buf = [0u8; MAX_DATA];
            let n = bytes
                .read(at, &mut buf[DATA_HEADER..DATA_HEADER + len as usize])
                .map_err(|f| (f, crc))?;
            if n != len as usize {
                return Err((STORAGE, crc));
            }
            let mut v = heapless::Vec::new();
            let m = Msg::Data {
                offset: at,
                bytes: &buf[DATA_HEADER..DATA_HEADER + n],
            };
            let mut enc = [0u8; MAX_DATA];
            let k = m.encode(&mut enc).map_err(|_| (STATE, crc))?;
            let _ = v.extend_from_slice(&enc[..k]);
            if with_timeout(Duration::from_secs(2), DATA_OUT.send(v))
                .await
                .is_err()
            {
                // The link is gone.
                return Ok(());
            }
        }
        if s.all_acked() && done_by.is_none() {
            done_by = Some(Instant::now() + PULL_WAIT);
        }
        match pull_note(PULL_STALL).await {
            Some(n) => match Msg::decode(&n) {
                Some(Msg::Abort) => return Ok(()),
                Some(m) => match s.on(&m) {
                    Ok(Event::Done(_)) => return Ok(()),
                    Ok(_) => {}
                    // The central refused, or answered out of step: it
                    // knows.
                    Err(_) => return Ok(()),
                },
                None => {}
            },
            None => {
                if done_by.is_some_and(|t| Instant::now() >= t) || s.silence().is_err() {
                    return Ok(());
                }
            }
        }
    }
}

/// Queues a control message for the central.
fn send_ctrl(m: &Msg) {
    let mut buf = [0u8; MAX_CONTROL];
    if let Ok(n) = m.encode(&mut buf) {
        let mut v = heapless::Vec::new();
        let _ = v.extend_from_slice(&buf[..n.min(CTRL)]);
        let _ = OUT.try_send(v);
    }
}

/// Drops what was queued for an earlier central.
pub fn reset() {
    OUT.clear();
    DATA_OUT.clear();
}

/// Notifies the central of ctrl's answers and a pull's data until one
/// fails.
pub async fn serve<P: PacketPool>(svc: &XferService, conn: &GattConnection<'_, '_, P>) {
    loop {
        let r = match embassy_futures::select::select(OUT.receive(), DATA_OUT.receive()).await {
            embassy_futures::select::Either::First(n) => svc.ctrl.notify_raw(conn, &n, false).await,
            embassy_futures::select::Either::Second(d) => svc.data.notify_raw(conn, &d, false).await,
        };
        if r.is_err() {
            return;
        }
    }
}
