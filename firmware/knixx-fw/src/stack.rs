//! The stack's high-water mark. flip-link puts the one stack, which main,
//! the executor and the interrupt handlers share, at the bottom of RAM,
//! from _stack_end up to _stack_start, below the statics; the embassy tasks
//! live in statics. At boot the unused stack is painted with a pattern, and
//! the high-water mark is the lowest word that no longer holds it.

use core::ptr::{addr_of, read_volatile, write_volatile};

const PAINT: u32 = 0x5354_4B21; // "STK!"

/// How much of the stack below the current stack pointer is left unpainted,
/// for the painting itself.
const MARGIN: usize = 64;

unsafe extern "C" {
    static _stack_end: u32;
    static _stack_start: u32;
}

fn bottom() -> usize {
    addr_of!(_stack_end) as usize
}

fn top() -> usize {
    addr_of!(_stack_start) as usize
}

/// Paints the stack from its bottom up to just below the stack pointer.
/// main calls it first thing, while the stack is still shallow.
pub fn paint() {
    let sp = cortex_m::register::msp::read() as usize;
    let end = (sp - MARGIN) & !3;
    let mut p = bottom();
    while p < end {
        // Safety: the stack below the stack pointer is not in use.
        unsafe { write_volatile(p as *mut u32, PAINT) };
        p += 4;
    }
}

/// The stack's size, and the most of it used since boot, in bytes.
pub fn usage() -> (usize, usize) {
    let (bottom, top) = (bottom(), top());
    let mut p = bottom;
    // Safety: the words between the bottom of the stack and its top are
    // RAM; those below the stack pointer are read as plain data.
    while p < top && unsafe { read_volatile(p as *const u32) } == PAINT {
        p += 4;
    }
    (top - bottom, top - p)
}
