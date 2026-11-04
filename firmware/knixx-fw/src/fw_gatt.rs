//! The fw GATT service: a central, such as `bolt fw push`, stages a signed
//! image while the firmware runs, in an update window, without pairing:
//! the image's signature, not the link, keeps out what may not install.
//!
//! - **ctrl** (write, notify). Writes: `begin` [0x01, size u32, crc32 u32]
//!   starts an image of size bytes, or, for the image coming in already
//!   (after a lost link), goes on with it: the ack says from where;
//!   `abort` [0x03]. Notifications:
//!   `ack` [0x10, offset u32], the offset the board takes next, every
//!   4 KiB and after begin; `rewind` [0x11, offset u32] for data at another
//!   offset; `done` [0x20, counter u32] once the image is checked and
//!   staged; `error` [0x21, code u8, reason…].
//! - **data** (write without response): [offset u32, data…], the image's
//!   bytes from offset. The board takes only the offset it expects; the
//!   host keeps a few KiB in flight and goes back to the offset of a rewind,
//!   or of the last ack when nothing comes.
//!
//! The window is open for a while after `fw window <minutes>` (and, later,
//! the KNX control plane's open); the board advertises meanwhile. Outside
//! it, ctrl and data take nothing.

use core::cell::RefCell;
use core::fmt::Write as _;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::channel::Channel;
use embassy_sync::once_lock::OnceLock;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use knixx_image as image;
use knixx_image::stage::{Files, Stager};
use trouble_host::prelude::*;

use crate::storage::Fs;
use crate::{boot, fw};

/// The service UUID, 6b6e7810-1b5d-4c8e-a2f0-3e5b9d8c7a10.
#[gatt_service(uuid = "6b6e7810-1b5d-4c8e-a2f0-3e5b9d8c7a10")]
pub struct FwService {
    #[characteristic(uuid = "6b6e7811-1b5d-4c8e-a2f0-3e5b9d8c7a10", write, notify)]
    pub ctrl: heapless::Vec<u8, 20>,
    #[characteristic(uuid = "6b6e7812-1b5d-4c8e-a2f0-3e5b9d8c7a10", write, write_without_response)]
    pub data: heapless::Vec<u8, 244>,
}

const BEGIN: u8 = 0x01;
const ABORT: u8 = 0x03;
const ACK: u8 = 0x10;
const REWIND: u8 = 0x11;
const DONE: u8 = 0x20;
const ERROR: u8 = 0x21;

/// The board acks every ACK_EVERY bytes it takes.
const ACK_EVERY: u32 = 4096;
/// A rewind is said at most this often, so that a burst of writes at the
/// wrong offset brings one.
const REWIND_GAP: Duration = Duration::from_millis(200);

/// Error codes of `error`: an image refusal's code (knixx_image), or these.
const E_SPACE: u8 = 0x40;
const E_STORAGE: u8 = 0x41;
const E_STATE: u8 = 0x42;
const E_CRC: u8 = 0x43;
const E_WINDOW: u8 = 0x44;

/// Until when the update window is open; None while it is shut.
static WINDOW: Mutex<CriticalSectionRawMutex, RefCell<Option<Instant>>> = Mutex::new(RefCell::new(None));
/// Wakes the BLE task when the window opens or closes.
pub static WINDOW_CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The notifications for the central.
static OUT: Channel<CriticalSectionRawMutex, heapless::Vec<u8, 20>, 8> = Channel::new();

/// An image coming in over the service.
struct Upload {
    stager: Stager,
    crc: u32,
    last_rewind: Option<Instant>,
}

/// The upload in progress. In a OnceLock, made at run time, so that its
/// 17 KB lie in .bss: as a plain static its None would need an image in
/// flash (.data).
static UPLOAD_CELL: OnceLock<Mutex<ThreadModeRawMutex, RefCell<Option<Upload>>>> = OnceLock::new();

fn upload() -> &'static Mutex<ThreadModeRawMutex, RefCell<Option<Upload>>> {
    UPLOAD_CELL.get_or_init(|| Mutex::new(RefCell::new(None)))
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

/// Whether an image is coming in over the service.
pub fn busy() -> bool {
    upload().lock(|u| u.borrow().is_some())
}

fn notify(bytes: &[u8]) {
    let mut v = heapless::Vec::new();
    let _ = v.extend_from_slice(&bytes[..bytes.len().min(20)]);
    let _ = OUT.try_send(v);
}

fn notify_offset(kind: u8, off: u32) {
    let o = off.to_le_bytes();
    notify(&[kind, o[0], o[1], o[2], o[3]]);
}

fn notify_error(code: u8, reason: &dyn core::fmt::Display) {
    let mut s: heapless::String<18> = heapless::String::new();
    let _ = write!(s, "{reason}");
    let mut b: heapless::Vec<u8, 20> = heapless::Vec::new();
    let _ = b.push(ERROR);
    let _ = b.push(code);
    let _ = b.extend_from_slice(s.as_bytes());
    notify(&b);
}

fn error_code(e: &image::stage::StageError<crate::storage::FileError>) -> u8 {
    use image::stage::StageError::*;
    match e {
        Refused(r) => r.code(),
        Space { .. } => E_SPACE,
        TransportCrc | ReadBack => E_CRC,
        Files(_) => E_STORAGE,
        Overflow | SizeMismatch { .. } | Incomplete { .. } => E_STATE,
    }
}

/// Handles a write of ctrl.
pub fn ctrl(data: &[u8]) {
    if !window_open() {
        notify_error(E_WINDOW, &"no update window");
        return;
    }
    match data {
        [BEGIN, s @ ..] if s.len() >= 8 => {
            let size = u32::from_le_bytes([s[0], s[1], s[2], s[3]]);
            let crc = u32::from_le_bytes([s[4], s[5], s[6], s[7]]);
            begin(size, crc);
        }
        [ABORT, ..] => {
            if upload().lock(|u| u.borrow_mut().take()).is_some() {
                let _ = Fs.remove(image::STAGED_TMP);
            }
        }
        _ => notify_error(E_STATE, &"bad ctrl"),
    }
}

fn begin(size: u32, crc: u32) {
    if fw::console_busy() {
        notify_error(E_STATE, &"console upload");
        return;
    }
    // The same image again, after a lost link: on from where it got to.
    let resumed = upload().lock(|u| {
        let u = u.borrow();
        u.as_ref()
            .filter(|up| up.stager.size() == size && up.crc == crc)
            .map(|up| up.stager.received())
    });
    if let Some(at) = resumed {
        notify_offset(ACK, at);
        return;
    }
    upload().lock(|u| *u.borrow_mut() = None);
    match Stager::begin(&mut Fs, size) {
        Ok(stager) => {
            upload().lock(|u| {
                *u.borrow_mut() = Some(Upload {
                    stager,
                    crc,
                    last_rewind: None,
                })
            });
            notify_offset(ACK, 0);
        }
        Err(e) => notify_error(error_code(&e), &e),
    }
}

/// Handles a write of data.
pub fn data(write: &[u8]) {
    if write.len() < 4 || !window_open() {
        return;
    }
    let off = u32::from_le_bytes([write[0], write[1], write[2], write[3]]);
    let body = &write[4..];
    let mut finished = None;
    upload().lock(|u| {
        let mut u = u.borrow_mut();
        let Some(up) = u.as_mut() else {
            return;
        };
        let next = up.stager.received();
        if off != next {
            let now = Instant::now();
            if up.last_rewind.is_none_or(|t| now - t >= REWIND_GAP) {
                up.last_rewind = Some(now);
                notify_offset(REWIND, next);
            }
            return;
        }
        let room = (up.stager.size() - next) as usize;
        let body = &body[..body.len().min(room)];
        let running = boot::header();
        if let Err(e) = up.stager.feed(&mut Fs, body, fw::keys(), running.as_ref()) {
            notify_error(error_code(&e), &e);
            finished = u.take().map(|u| (u, false));
            return;
        }
        up.last_rewind = None;
        let got = up.stager.received();
        if got == up.stager.size() {
            notify_offset(ACK, got);
            finished = u.take().map(|u| (u, true));
        } else if got / ACK_EVERY > next / ACK_EVERY {
            notify_offset(ACK, got);
        }
    });
    let Some((up, complete)) = finished else {
        return;
    };
    if !complete {
        let _ = Fs.remove(image::STAGED_TMP);
        return;
    }
    match up.stager.finish(&mut Fs, Some(up.crc)) {
        Ok(h) => {
            let c = h.counter.to_le_bytes();
            notify(&[DONE, c[0], c[1], c[2], c[3]]);
        }
        Err(e) => {
            let _ = Fs.remove(image::STAGED_TMP);
            notify_error(error_code(&e), &e);
        }
    }
}

/// Drops what was queued for an earlier central.
pub fn reset() {
    OUT.clear();
}

/// Notifies the central of ctrl's notifications until one fails.
pub async fn serve<P: PacketPool>(svc: &FwService, conn: &GattConnection<'_, '_, P>) {
    loop {
        let n = OUT.receive().await;
        if svc.ctrl.notify_raw(conn, &n, false).await.is_err() {
            return;
        }
    }
}
