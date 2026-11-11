//! The firmware update's control plane on the bus (knixx_knx::fw): the
//! board answers the requests addressed to it, 1-1 or to the broadcast,
//! opens and shuts its update window, and installs; as the proxy, it sends
//! its central's requests (the knixx service's knx characteristic) and
//! hands the answers addressed to it back.
//!
//! A request for the board itself from its own central is answered here,
//! without the bus: so the proxy is a board like the others, and a scan
//! finds it too.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Timer};
use knixx_bus::{Command, Tag};
use knixx_image::stage;
use knixx_knx::fw::{self, Act, Board, Request, Response, State, reason};
use knixx_knx::tp1::Telegram;

use crate::storage::Fs;
use crate::{boot, console, xfer_gatt};

/// Who asked.
#[derive(Clone, Copy)]
enum From {
    /// A board on the bus, by its individual address.
    Bus(u16),
    /// The proxy's own central (the knixx GATT service).
    #[cfg_attr(not(feature = "web-ble"), allow(dead_code))]
    Central,
}

/// The requests to answer.
static REQUESTS: Channel<CriticalSectionRawMutex, (From, Request), 4> = Channel::new();

/// The answers for the proxy's central: the sender's individual address and
/// the telegram's data.
pub static ANSWERS: Channel<CriticalSectionRawMutex, (u16, heapless::Vec<u8, 15>), 8> = Channel::new();

/// When the last open was taken, in ms since boot, for the rate limit.
static LAST_OPEN: AtomicU32 = AtomicU32::new(u32::MAX);
/// Opens closer than this apart are refused.
const OPEN_GAP: Duration = Duration::from_secs(2);

/// The longest a board waits before it answers a scan, so that the answers
/// of many boards spread out.
const SCAN_SPREAD_MS: u64 = 2000;

fn own() -> u16 {
    console::device().individual
}

/// Takes a telegram of the bus, from the bus engine.
pub fn telegram(tg: &Telegram, own_tg: bool) {
    if own_tg || !fw::is_user_message(tg) {
        return;
    }
    let me = own();
    if let Some(req) = Request::decode(tg.data())
        && (fw::is_broadcast(tg) || fw::is_for(tg, me))
    {
        let _ = REQUESTS.try_send((From::Bus(tg.src), req));
        return;
    }
    if Response::decode(tg.data()).is_some() && fw::is_for(tg, me) {
        let mut d = heapless::Vec::new();
        let _ = d.extend_from_slice(tg.data());
        let _ = ANSWERS.try_send((tg.src, d));
    }
}

/// Takes a request of the proxy's central for dest (0: the broadcast):
/// it goes on the bus, and is answered here too when it is for this board.
/// Returns false when the bus queue is full.
#[cfg(feature = "web-ble")]
pub fn from_central(seq: u8, dest: u16, data: &[u8]) -> bool {
    let me = own();
    if (dest == 0 || dest == me)
        && let Some(req) = Request::decode(data)
    {
        let _ = REQUESTS.try_send((From::Central, req));
    }
    if dest == me {
        crate::knixx_gatt::sent(Tag::Fw(seq), knixx_bus::Status::Confirmed);
        return true;
    }
    let tg = fw::telegram(dest, &fw::Data::from_slice(data));
    console::CMDS
        .try_send(Command::Send {
            tg,
            tag: Tag::Fw(seq),
        })
        .is_ok()
}

/// What the board knows, for an answer.
fn board() -> Board {
    let running = boot::header().map_or(0, |h| h.counter);
    // A staged image no newer than what runs never installs.
    let staged = stage::staged(&mut Fs).map(|h| h.counter).filter(|&c| c > running);
    let receiving = xfer_gatt::busy();
    let state = if receiving {
        State::Receiving
    } else if staged.is_some() {
        State::Staged
    } else if xfer_gatt::last_error() != 0 {
        State::Failed
    } else {
        State::Idle
    };
    Board {
        running,
        staged,
        state,
        ble: crate::ble::address(),
        error: xfer_gatt::last_error(),
        progress: xfer_gatt::progress(),
        busy: receiving,
    }
}

/// A delay of up to SCAN_SPREAD_MS, the same for a board each time and
/// different from board to board: from its device ID.
fn scan_delay() -> Duration {
    let ficr = embassy_nrf::pac::FICR;
    let id = ficr.deviceid(0).read() ^ ficr.deviceid(1).read().rotate_left(13);
    Duration::from_millis(u64::from(id.wrapping_mul(2_654_435_761)) % SCAN_SPREAD_MS)
}

#[embassy_executor::task]
pub async fn task() -> ! {
    loop {
        let (from, req) = REQUESTS.receive().await;
        let (mut resp, mut act) = fw::answer(&req, &board());
        if let Request::Open { .. } | Request::Window { .. } = req {
            let now = Instant::now().as_millis() as u32;
            let last = LAST_OPEN.load(Ordering::Relaxed);
            if last != u32::MAX && now.wrapping_sub(last) < OPEN_GAP.as_millis() as u32 {
                resp = Response::Refused { reason: reason::RATE };
                act = Act::None;
            } else if act != Act::None {
                LAST_OPEN.store(now, Ordering::Relaxed);
            }
        }
        if let (From::Bus(_), Request::Scan) = (from, req) {
            Timer::after(scan_delay()).await;
        }
        let data = resp.encode();
        match from {
            From::Bus(src) => {
                let tg = fw::telegram(src, &data);
                let _ = console::CMDS.try_send(Command::Send { tg, tag: Tag::Fw(0) });
            }
            From::Central => {
                let mut d = heapless::Vec::new();
                let _ = d.extend_from_slice(data.as_slice());
                let _ = ANSWERS.try_send((own(), d));
            }
        }
        match act {
            Act::None => {}
            Act::Open(minutes) => xfer_gatt::set_window(Some(Duration::from_secs(u64::from(minutes) * 60))),
            Act::Close => {
                xfer_gatt::abort();
                xfer_gatt::set_window(None)
            }
            Act::Install => {
                // Leave the answer time to go out.
                Timer::after_millis(500).await;
                boot::reset_into(boot::INSTALL);
            }
        }
    }
}
