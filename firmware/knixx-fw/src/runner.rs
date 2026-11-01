//! The script task: it runs the scripts in /scripts, with the scripts
//! feature. At boot it loads each, up to SLOTS, and calls its on_init; then
//! it calls on_telegram for every telegram on the bus, on_time every second,
//! and loads or drops a script when the console uploads or removes one.
//!
//! Everything it does with Rhai, allocating and freeing, it does holding
//! the heap's grant: the heap is the scripts' alone. A failed call is
//! reported on the console and blinks red; after MAX_ERRORS failures in a
//! row the script is stopped. A panic while a script runs names it in the
//! panic record, and the next boot leaves it out.

use alloc::vec;
use core::cell::RefCell;
use core::fmt::Write;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, ThreadModeRawMutex};
use embassy_sync::channel::Channel;
use embassy_time::{Duration, Instant, Ticker};
use knixx_bus::Command;
use knixx_console::upload::Name;
use knixx_knx::tp1::Telegram;
use knixx_script::{Host, Runtime, Script};
use knixx_ui::led::Mode;

use crate::heap::Grant;
use crate::{bus, console, leds, panic, script, storage};

/// The most scripts that run.
pub const SLOTS: usize = 4;

/// The failures in a row of one callback after which a script is stopped:
/// its on_telegram failing on every telegram stops it, even while its
/// on_time works.
const MAX_ERRORS: u32 = 5;

/// The callbacks the task calls after loading, which count their failures
/// apart.
#[derive(Clone, Copy)]
enum Callback {
    Telegram = 0,
    Time = 1,
}

/// The heap in use after a call above which the script that made the call
/// is stopped, freeing what it holds: the limits bound a call, but not what
/// a script keeps in its state from call to call. The rest of the heap is
/// room for one call, which within its operations allocates less.
const HEAP_LIMIT: usize = crate::heap::SIZE * 5 / 8;

/// The heap a load leaves free besides the file and the program: on_info
/// and on_init run then too.
const LOAD_MARGIN: usize = 8 * 1024;

/// The telegrams on the bus, with own, for the scripts.
pub static TELEGRAMS: Channel<CriticalSectionRawMutex, (Telegram, bool), 16> = Channel::new();

/// The telegrams that did not fit TELEGRAMS.
pub static DROPPED: AtomicU32 = AtomicU32::new(0);

/// What the console asks of the script task.
#[derive(Clone, Copy)]
pub enum Control {
    /// Loads the script, anew if it runs.
    Load(Name),
    /// Drops the script.
    Remove(Name),
}

pub static CONTROL: Channel<CriticalSectionRawMutex, Control, 4> = Channel::new();

/// Passes a telegram on to the scripts, if there is room.
pub fn telegram(tg: &Telegram, own: bool) {
    if TELEGRAMS.try_send((*tg, own)).is_err() {
        DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Where a script is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    /// Its file did not load or its on_info or on_init failed.
    Failed,
    /// Stopped after MAX_ERRORS failures in a row.
    Stopped,
    /// Left out at boot: it was running when the board panicked.
    LeftOut,
    /// Not loaded: SLOTS scripts run already, or the heap has no room.
    NoRoom,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Failed => "failed",
            State::Stopped => "stopped",
            State::LeftOut => "left out",
            State::NoRoom => "not loaded",
        }
    }
}

/// What the console shows of a script: plain data, as the scripts' Rhai
/// values stay with the task.
pub struct Status {
    pub name: Name,
    pub state: State,
    pub info: [heapless::String<32>; 6],
    pub calls: u32,
    pub errors: u32,
    pub last_error: heapless::String<160>,
    pub faults: heapless::Vec<u32, 4>,
    /// The longest call, in microseconds.
    pub max_us: u32,
}

/// The status of each script the task knows: SLOTS running and some
/// others.
pub static STATUS: Mutex<ThreadModeRawMutex, RefCell<heapless::Vec<Status, 8>>> =
    Mutex::new(RefCell::new(heapless::Vec::new()));

/// Copies s, cut to the capacity of the string.
fn text<const N: usize>(s: &str) -> heapless::String<N> {
    let mut t = heapless::String::new();
    for c in s.chars() {
        if t.push(c).is_err() {
            break;
        }
    }
    t
}

/// Runs f on the status of name, made first with state if there is none.
fn with_status(name: Name, state: State, f: impl FnOnce(&mut Status)) {
    STATUS.lock(|st| {
        let mut st = st.borrow_mut();
        let i = match st.iter().position(|s| s.name == name) {
            Some(i) => i,
            None => {
                let s = Status {
                    name,
                    state,
                    info: Default::default(),
                    calls: 0,
                    errors: 0,
                    last_error: heapless::String::new(),
                    faults: heapless::Vec::new(),
                    max_us: 0,
                };
                if st.push(s).is_err() {
                    // Full of others: the oldest that does not run goes.
                    if let Some(j) = st.iter().position(|s| s.state != State::Running) {
                        st.remove(j);
                    }
                    let _ = st.push(Status {
                        name,
                        state,
                        info: Default::default(),
                        calls: 0,
                        errors: 0,
                        last_error: heapless::String::new(),
                        faults: heapless::Vec::new(),
                        max_us: 0,
                    });
                }
                st.len() - 1
            }
        };
        f(&mut st[i]);
    })
}

/// Forgets the status of name.
fn forget(name: Name) {
    STATUS.lock(|st| st.borrow_mut().retain(|s| s.name != name));
}

/// Reports whether name runs.
pub fn running(name: &str) -> Option<State> {
    STATUS.lock(|st| {
        st.borrow()
            .iter()
            .find(|s| s.name.as_str() == name)
            .map(|s| s.state)
    })
}

/// The number of scripts that run.
pub fn count() -> usize {
    STATUS.lock(|st| st.borrow().iter().filter(|s| s.state == State::Running).count())
}

/// What the scripts reach: the bus, through its scripts' queue, and the
/// console.
struct Firmware;

impl Host for Firmware {
    fn send(&mut self, tg: &Telegram) -> bool {
        let ok = bus::SCRIPT_CMDS.try_send(Command::Send(*tg)).is_ok();
        if !ok {
            leds::signal(Mode::Error);
        }
        ok
    }

    fn print(&mut self, line: &str) {
        let name = panic::running();
        console::write_crlf(name.as_ref().map_or("script", |n| n.as_str()).as_bytes());
        console::write_crlf(b": ");
        console::write_crlf(line.as_bytes());
        console::write_crlf(b"\n");
    }
}

/// A running script.
struct Slot {
    name: Name,
    script: Script,
    /// The failures in a row, of each Callback.
    in_row: [u32; 2],
}

/// Reports a failure of name on the console and blinks red.
fn report(name: Name, what: &str, err: &dyn core::fmt::Display) {
    let mut w = console::Out;
    let _ = writeln!(w, "script {}: {what}{err}", name.as_str());
    leds::signal(Mode::Error);
}

/// Loads the script name into a free slot and calls its on_info and
/// on_init.
fn load(rt: &Runtime, slots: &mut [Option<Slot>; SLOTS], name: Name) {
    let Some(free) = slots.iter().position(Option::is_none) else {
        with_status(name, State::NoRoom, |s| s.state = State::NoRoom);
        let mut w = console::Out;
        let _ = writeln!(
            w,
            "script {}: not loaded, {SLOTS} scripts run already",
            name.as_str()
        );
        return;
    };
    let path = script::path(name.as_str());
    let size = match storage::file_size(&path) {
        Ok(Some(n)) => n as usize,
        Ok(None) => return,
        Err(e) => {
            with_status(name, State::Failed, |s| s.state = State::Failed);
            report(name, "", &e);
            return;
        }
    };
    // Loading holds the file and the program it becomes; the limits keep
    // what runs small, but four large programs would fill the heap.
    let room = crate::heap::SIZE - crate::heap::usage().0;
    if 2 * size + LOAD_MARGIN > room {
        with_status(name, State::NoRoom, |s| s.state = State::NoRoom);
        let mut w = console::Out;
        let _ = writeln!(
            w,
            "script {}: not loaded, {size} bytes do not fit the {room} bytes free on the heap",
            name.as_str()
        );
        leds::signal(Mode::Error);
        return;
    }
    let g = Grant::take();
    panic::set_running(Some(name));
    // The error is a short message; boxing it would allocate.
    #[allow(clippy::result_large_err)]
    let loaded = (|| -> Result<(Script, knixx_script::Info), heapless::String<160>> {
        let mut bytes = vec![0u8; size];
        storage::read_file(&path, &mut bytes).map_err(|e| text(&alloc::format!("{e}")))?;
        let mut script = Script::load(&bytes).map_err(|e| text(&alloc::format!("{e}")))?;
        drop(bytes);
        let info = script.info(rt).map_err(|e| text(&alloc::format!("{e}")))?;
        script.init(rt).map_err(|e| text(&alloc::format!("{e}")))?;
        Ok((script, info))
    })();
    panic::set_running(None);
    match loaded {
        Ok((script, info)) => {
            with_status(name, State::Running, |s| {
                s.state = State::Running;
                s.info = [
                    text(&info.name),
                    text(&info.author),
                    text(&info.date),
                    text(&info.compiled),
                    text(&info.version),
                    text(&info.hash),
                ];
                s.calls = 2;
                s.errors = 0;
                s.last_error.clear();
                s.faults.clear();
                s.max_us = 0;
            });
            drop(info);
            slots[free] = Some(Slot {
                name,
                script,
                in_row: [0; 2],
            });
            let mut w = console::Out;
            let _ = writeln!(w, "script {}: running", name.as_str());
        }
        Err(e) => {
            with_status(name, State::Failed, |s| {
                s.state = State::Failed;
                s.last_error = e.clone();
            });
            report(name, "", &e);
        }
    }
    drop(g);
}

/// Drops the script in slot i, under the grant.
fn unload(slots: &mut [Option<Slot>; SLOTS], i: usize) {
    let g = Grant::take();
    slots[i] = None;
    drop(g);
}

/// Calls f on the script in slot i, timing it and counting its failures;
/// the script stops after MAX_ERRORS in a row.
fn call(
    rt: &Runtime,
    slots: &mut [Option<Slot>; SLOTS],
    i: usize,
    cb: Callback,
    f: impl FnOnce(&mut Script, &Runtime) -> Result<(), knixx_script::Error>,
) {
    let Some(slot) = slots[i].as_mut() else {
        return;
    };
    let name = slot.name;
    let g = Grant::take();
    panic::set_running(Some(name));
    let t0 = Instant::now();
    let r = f(&mut slot.script, rt);
    let us = Instant::now().duration_since(t0).as_micros() as u32;
    panic::set_running(None);
    let failed = match &r {
        Ok(()) => {
            slot.in_row[cb as usize] = 0;
            None
        }
        Err(e) => {
            slot.in_row[cb as usize] += 1;
            let mut msg: heapless::String<160> = text(e.call);
            let _ = msg.push_str(": ");
            for c in e.text.chars().map(|c| if c == '\n' { ' ' } else { c }) {
                if msg.push(c).is_err() {
                    break;
                }
            }
            let faults: heapless::Vec<u32, 4> = e.faults.iter().take(4).map(|&a| a as u32).collect();
            Some((msg, faults))
        }
    };
    drop(r);
    let full = crate::heap::usage().0 > HEAP_LIMIT;
    let stop = slot.in_row[cb as usize] >= MAX_ERRORS || full;
    drop(g);
    with_status(name, State::Running, |s| {
        s.calls += 1;
        s.max_us = s.max_us.max(us);
        if let Some((msg, faults)) = &failed {
            s.errors += 1;
            s.last_error = msg.clone();
            s.faults = faults.clone();
        }
        if full {
            s.last_error = text("the heap is full: what the script keeps grows too large");
        }
        if stop {
            s.state = State::Stopped;
        }
    });
    if let Some((msg, _)) = failed {
        report(name, "", &msg.as_str());
    }
    if stop {
        let used = crate::heap::usage().0;
        unload(slots, i);
        let mut w = console::Out;
        let _ = if full {
            writeln!(
                w,
                "script {}: stopped, {used} bytes of the heap in use, over {HEAP_LIMIT}",
                name.as_str()
            )
        } else {
            writeln!(
                w,
                "script {}: stopped after {MAX_ERRORS} failures in a row of {}",
                name.as_str(),
                match cb {
                    Callback::Telegram => "on_telegram",
                    Callback::Time => "on_time",
                }
            )
        };
        leds::signal(Mode::Error);
    }
}

#[embassy_executor::task]
pub async fn task() -> ! {
    let g = Grant::take();
    let rt = Runtime::new(Firmware);
    drop(g);
    let mut slots: [Option<Slot>; SLOTS] = [const { None }; SLOTS];

    // The scripts on the flash, but the one the last panic names.
    let left_out = panic::last().and_then(|l| l.script);
    let mut names: heapless::Vec<Name, 16> = heapless::Vec::new();
    let _ = storage::list_dir(script::DIR, |file, _| {
        if let Some(n) = file.strip_suffix(script::EXT).and_then(Name::new) {
            let _ = names.push(n);
        }
    });
    rt.set_now(Instant::now().as_secs() as u32);
    for name in names {
        if Some(name) == left_out {
            with_status(name, State::LeftOut, |s| s.state = State::LeftOut);
            continue;
        }
        load(&rt, &mut slots, name);
    }

    let mut ticker = Ticker::every(Duration::from_secs(1));
    loop {
        let event = select3(TELEGRAMS.receive(), ticker.next(), CONTROL.receive()).await;
        // The second the scripts count their sends in.
        rt.set_now(Instant::now().as_secs() as u32);
        match event {
            Either3::First((tg, own)) => {
                for i in 0..SLOTS {
                    call(&rt, &mut slots, i, Callback::Telegram, |s, rt| {
                        s.telegram(rt, &tg, own)
                    });
                }
            }
            Either3::Second(()) => {
                let uptime = Instant::now().as_secs() as u32;
                for i in 0..SLOTS {
                    call(&rt, &mut slots, i, Callback::Time, |s, rt| s.time(rt, uptime));
                }
            }
            Either3::Third(c) => {
                let (Control::Load(name) | Control::Remove(name)) = c;
                if let Some(i) = slots
                    .iter()
                    .position(|s| s.as_ref().is_some_and(|s| s.name == name))
                {
                    unload(&mut slots, i);
                }
                forget(name);
                if let Control::Load(name) = c {
                    load(&rt, &mut slots, name);
                }
            }
        }
    }
}
