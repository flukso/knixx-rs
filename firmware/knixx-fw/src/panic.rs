//! A panic records its message in RAM that neither the startup code nor
//! the bootloader touches (PANIC in memory.x), and resets the chip; the
//! next boot shows the record in its banner.
//!
//! The record also names the script that was running, if one was: the next
//! boot leaves that script out, so that a script that brings the board down
//! cannot keep it down.

use core::cell::Cell;
use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;
use core::ptr::addr_of_mut;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use knixx_console::name::{MAX_NAME, Name};
use static_cell::StaticCell;

const MAGIC: u32 = 0x5041_4E43; // "PANC"

/// The longest message kept.
const MSG_LEN: usize = 200;

#[repr(C)]
struct Record {
    magic: u32,
    len: u32,
    msg: [u8; MSG_LEN],
    script_len: u32,
    script: [u8; MAX_NAME],
}

#[unsafe(link_section = ".panic")]
static mut RECORD: MaybeUninit<Record> = MaybeUninit::uninit();

/// The script running, which a panic names.
static RUNNING: Mutex<CriticalSectionRawMutex, Cell<Option<Name>>> = Mutex::new(Cell::new(None));

/// Records the script that is about to run, or that none is.
#[cfg_attr(not(feature = "scripts"), allow(dead_code))]
pub fn set_running(name: Option<Name>) {
    RUNNING.lock(|r| r.set(name));
}

/// The script running, if one is.
#[cfg_attr(not(feature = "scripts"), allow(dead_code))]
pub fn running() -> Option<Name> {
    RUNNING.lock(|r| r.get())
}

/// The panic before the last reset, as latch found it.
#[derive(Clone, Copy)]
pub struct Last {
    pub msg: &'static str,
    /// The script that was running.
    pub script: Option<Name>,
}

static LAST: Mutex<CriticalSectionRawMutex, Cell<Option<Last>>> = Mutex::new(Cell::new(None));

/// Writes into a fixed buffer, cutting off what does not fit.
struct Cut<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl Write for Cut<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let n = s.len().min(self.buf.len() - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Safety: nothing else runs any more, and the record is plain data.
    let rec = unsafe { &mut *addr_of_mut!(RECORD).cast::<Record>() };
    let mut w = Cut {
        buf: &mut rec.msg,
        len: 0,
    };
    let _ = write!(w, "{info}");
    rec.len = w.len as u32;
    let running = RUNNING.lock(|r| r.get());
    let name = running.as_ref().map_or("", |n| n.as_str());
    rec.script[..name.len()].copy_from_slice(name.as_bytes());
    rec.script_len = name.len() as u32;
    rec.magic = MAGIC;
    cortex_m::peripheral::SCB::sys_reset()
}

/// Reads the record of the panic before the last reset and clears it;
/// main calls it once, before the tasks that ask for it start.
pub fn latch() {
    // Safety: called once at startup, before anything can panic; the
    // record is read as plain data whatever RAM held.
    let rec = unsafe { &mut *addr_of_mut!(RECORD).cast::<Record>() };
    if rec.magic != MAGIC {
        return;
    }
    rec.magic = 0;
    static MSG: StaticCell<[u8; MSG_LEN]> = StaticCell::new();
    let buf = MSG.init([0; MSG_LEN]);
    let n = (rec.len as usize).min(MSG_LEN);
    buf[..n].copy_from_slice(&rec.msg[..n]);
    let msg = match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&buf[..e.valid_up_to()]).unwrap(),
    };
    let sn = (rec.script_len as usize).min(MAX_NAME);
    let script = core::str::from_utf8(&rec.script[..sn]).ok().and_then(Name::new);
    LAST.lock(|l| l.set(Some(Last { msg, script })));
}

/// The panic before the last reset, if there was one.
pub fn last() -> Option<Last> {
    LAST.lock(|l| l.get())
}
