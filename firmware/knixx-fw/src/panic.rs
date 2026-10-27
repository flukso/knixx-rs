//! A panic records its message in RAM that the startup code leaves alone,
//! and resets the chip; the next boot shows the record in its banner.

use core::fmt::Write;
use core::mem::MaybeUninit;
use core::panic::PanicInfo;
use core::ptr::addr_of_mut;

const MAGIC: u32 = 0x5041_4E43; // "PANC"

#[repr(C)]
struct Record {
    magic: u32,
    len: u32,
    msg: [u8; 200],
}

#[unsafe(link_section = ".uninit.PANIC")]
static mut RECORD: MaybeUninit<Record> = MaybeUninit::uninit();

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
    rec.magic = MAGIC;
    cortex_m::peripheral::SCB::sys_reset()
}

/// Returns the message of the panic before the last reset, once, into buf.
pub fn take_last(buf: &mut [u8; 200]) -> Option<&str> {
    // Safety: called once at startup, before anything can panic; the
    // record is read as plain data whatever RAM held.
    let rec = unsafe { &mut *addr_of_mut!(RECORD).cast::<Record>() };
    if rec.magic != MAGIC {
        return None;
    }
    rec.magic = 0;
    let n = (rec.len as usize).min(buf.len());
    buf[..n].copy_from_slice(&rec.msg[..n]);
    Some(match core::str::from_utf8(&buf[..n]) {
        Ok(s) => s,
        Err(e) => core::str::from_utf8(&buf[..e.valid_up_to()]).unwrap(),
    })
}
