//! The console's BLE port: the Nordic UART Service (NUS), as the TinyGo
//! firmware offered it. The console task moves the console here with nus on
//! (or the wall switch gesture) and back to USB with nus off, after
//! IDLE_TIMEOUT without a central, or on reset; the BLE task advertises,
//! accepts a central and moves the bytes, both ways.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use knixx_console::nus::Ring;
use knixx_console::outq::Queue;

/// The advertised name. With the flags and the 128-bit NUS UUID the
/// advertising packet has room for a name of 8 bytes.
pub const NAME: &str = "knixx";

/// The console output to the central.
pub static OUT: Mutex<CriticalSectionRawMutex, RefCell<Queue>> = Mutex::new(RefCell::new(Queue::new()));

/// Wakes the BLE task's sender after a write to OUT.
pub static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// The central's writes, for the console.
pub static RX: Mutex<CriticalSectionRawMutex, RefCell<Ring>> = Mutex::new(RefCell::new(Ring::new()));

/// Wakes the console after a write of the central.
pub static RX_READY: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Whether a central is connected.
pub static CONNECTED: AtomicBool = AtomicBool::new(false);

/// Tells the BLE task to advertise (true) or to stop advertising and drop
/// the central (false).
pub static CONTROL: Signal<CriticalSectionRawMutex, bool> = Signal::new();

/// The BLE task's answer to CONTROL false: the output is flushed, the
/// central gone and the advertising stopped.
pub static OFF_DONE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Queues raw bytes for the central, whole or not at all. Without a central
/// the output is dropped, as the central gets the banner on its first key.
pub fn write(b: &[u8]) {
    if !connected() {
        return;
    }
    OUT.lock(|q| q.borrow_mut().write(b));
    WAKE.signal(());
}

/// Queues bytes for the central with "\n" turned into "\r\n", whole or not
/// at all, or dropped without a central.
pub fn write_crlf(b: &[u8]) {
    if !connected() {
        return;
    }
    OUT.lock(|q| q.borrow_mut().write_crlf(b));
    WAKE.signal(());
}

/// Whether a central is connected.
pub fn connected() -> bool {
    CONNECTED.load(Ordering::Relaxed)
}
