//! The heap, for Rhai Grain only, with the scripts feature. The rest of
//! the firmware does not allocate: the allocator serves only in thread mode
//! while a Grant is held, which the script code takes around everything it
//! does with Rhai, allocating and freeing alike, and panics otherwise.
//!
//! It is a TLSF allocator (rlsf): constant time and without a lock, as only
//! thread-mode code under a Grant reaches it.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;
use core::ptr::{NonNull, null_mut};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use cortex_m::peripheral::SCB;
use cortex_m::peripheral::scb::VectActive;
use rlsf::Tlsf;

/// The size of the heap.
pub const SIZE: usize = 48 * 1024;

type Pool = Tlsf<'static, u16, u16, 12, 16>;

/// A cell that only thread-mode code holding the Grant reaches.
struct Granted<T>(UnsafeCell<T>);

// Safety: only thread-mode code holding the Grant reaches the cell, and
// there is one thread.
unsafe impl<T> Sync for Granted<T> {}

static TLSF: Granted<Pool> = Granted(UnsafeCell::new(Tlsf::new()));

/// The heap's memory, named for cargo xtask build, which reports its size,
/// and kept even before anything allocates.
#[used]
#[unsafe(no_mangle)]
static KNIXX_HEAP: Granted<[MaybeUninit<u8>; SIZE]> = Granted(UnsafeCell::new([MaybeUninit::uninit(); SIZE]));

static READY: AtomicBool = AtomicBool::new(false);
static GRANTED: AtomicBool = AtomicBool::new(false);
static USED: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The right to allocate, held around all work with Rhai. There is one at
/// a time.
#[allow(dead_code)] // taken by the script task
pub struct Grant(());

#[allow(dead_code)] // taken by the script task
impl Grant {
    /// Takes the grant. Panics when it is held already.
    pub fn take() -> Grant {
        assert!(in_thread_mode(), "heap grant outside thread mode");
        assert!(!GRANTED.swap(true, Ordering::Relaxed), "heap grant held twice");
        if !READY.swap(true, Ordering::Relaxed) {
            // Safety: the grant is held, so nothing else touches the heap.
            let (tlsf, mem) = unsafe { (&mut *TLSF.0.get(), &mut *KNIXX_HEAP.0.get()) };
            tlsf.insert_free_block(mem);
        }
        Grant(())
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        GRANTED.store(false, Ordering::Relaxed);
    }
}

/// The bytes in use and the most in use since boot.
pub fn usage() -> (usize, usize) {
    (USED.load(Ordering::Relaxed), PEAK.load(Ordering::Relaxed))
}

fn in_thread_mode() -> bool {
    SCB::vect_active() == VectActive::ThreadMode
}

fn check() {
    if !GRANTED.load(Ordering::Relaxed) || !in_thread_mode() {
        panic!("alloc outside Rhai Grain");
    }
}

struct Allocator;

unsafe impl GlobalAlloc for Allocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        check();
        // Safety: the grant is held in thread mode.
        let tlsf = unsafe { &mut *TLSF.0.get() };
        match tlsf.allocate(layout) {
            Some(p) => {
                let used = USED.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
                PEAK.fetch_max(used, Ordering::Relaxed);
                p.as_ptr()
            }
            None => null_mut(),
        }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        check();
        let tlsf = unsafe { &mut *TLSF.0.get() };
        // Safety: ptr came from allocate with this layout's alignment.
        unsafe { tlsf.deallocate(NonNull::new_unchecked(ptr), layout.align()) };
        USED.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        check();
        let tlsf = unsafe { &mut *TLSF.0.get() };
        let new = Layout::from_size_align(new_size, layout.align()).unwrap();
        // Safety: ptr came from allocate with this layout.
        match unsafe { tlsf.reallocate(NonNull::new_unchecked(ptr), new) } {
            Some(p) => {
                let used = USED.fetch_add(new_size, Ordering::Relaxed) + new_size - layout.size();
                USED.fetch_sub(layout.size(), Ordering::Relaxed);
                PEAK.fetch_max(used, Ordering::Relaxed);
                p.as_ptr()
            }
            None => null_mut(),
        }
    }
}

#[global_allocator]
static ALLOCATOR: Allocator = Allocator;
