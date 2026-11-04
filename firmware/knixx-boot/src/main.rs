//! The bootloader of knixx thunder, at the start of flash.
//!
//! After a reset it decides, before it sets up any peripheral, what to do
//! (knixx_image::first_step):
//!
//! - **Boot:** the application's header and CRC are good and nothing asks
//!   for more. It write-protects itself with an ACL region and jumps to the
//!   application, which so starts from a chip as a reset leaves it.
//! - **Install:** GPREGRET asks for it, or the application is not valid.
//!   It checks the staged image on the QSPI flash, copies it into internal
//!   flash and resets (install).
//! - **Recovery:** GPREGRET asks for it, a double tap of reset, or no
//!   application and nothing to install. It takes an image over USB
//!   (recovery).
//!
//! The Feather's NeoPixel and red LED show what it does, as the Adafruit
//! bootloader's do (led).
//!
//! It keeps out of the bottom 0x108 bytes of RAM, so that the double tap's
//! word and the application's panic record survive it.

#![no_std]
#![no_main]

use core::ptr;

use cortex_m_rt::entry;
use embassy_nrf::{bind_interrupts, pac, peripherals, qspi, usb};
use knixx_image::{self as image, Header, Step};

mod app;
mod install;
mod led;
mod recovery;
mod writer;

mod keys {
    include!(concat!(env!("OUT_DIR"), "/keys.rs"));
}

bind_interrupts!(pub struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
    QSPI => qspi::InterruptHandler<peripherals::QSPI>;
});

/// The pins of the QSPI flash, from the peripherals p.
#[macro_export]
macro_rules! qspi_pins {
    ($p:ident) => {
        knixx_qspi::Pins {
            sck: $p.P0_19,
            csn: $p.P0_20,
            io0: $p.P0_17,
            io1: $p.P0_22,
            io2: $p.P0_23,
            io3: $p.P0_21,
        }
    };
}

/// The bootloader's version, as cargo xtask build stamps it.
pub const VERSION: &str = env!("KNIXX_COMMIT");
pub const BUILT: &str = env!("KNIXX_BUILD_DATE");

/// The keys that images must be signed with.
pub fn keys() -> image::Keys<'static> {
    image::Keys(keys::KEYS)
}

/// How long a first reset waits for a second one, in CPU cycles at the
/// 64 MHz of the reset: 500 ms.
const DOUBLE_TAP_CYCLES: u32 = 32_000_000;

#[entry]
fn main() -> ! {
    let power = pac::POWER;
    let reasons = power.resetreas().read();
    power.resetreas().write_value(reasons);
    let gpregret = power.gpregret().read().gpregret();
    power.gpregret().write(|w| w.set_gpregret(0));

    let double_tap = double_tap(reasons.resetpin());
    let app = app::header();
    match image::first_step(gpregret, double_tap, app.is_some()) {
        Step::Boot => app::boot(),
        Step::Install => install::run(app),
        Step::Recovery(why) => recovery::run(why, app),
    }
}

/// Reports a double tap of reset: after a reset by the pin, it marks the
/// double tap's word and waits; a second reset by the pin within the wait
/// finds the mark. Other resets clear it, so that RAM left over from before
/// power-up never counts.
fn double_tap(pin: bool) -> bool {
    let word = image::DOUBLE_TAP_ADDR as *mut u32;
    // Safety: the word lies below the bootloader's and the application's
    // RAM (memory.x), reserved for this.
    unsafe {
        if !pin {
            ptr::write_volatile(word, 0);
            return false;
        }
        if ptr::read_volatile(word) == image::DOUBLE_TAP_MAGIC {
            ptr::write_volatile(word, 0);
            return true;
        }
        ptr::write_volatile(word, image::DOUBLE_TAP_MAGIC);
        // As the Adafruit bootloader: the NeoPixel red and the red LED
        // breathing during the wait.
        led::start(led::State::Started);
        cortex_m::asm::delay(DOUBLE_TAP_CYCLES);
        led::off();
        ptr::write_volatile(word, 0);
    }
    false
}

/// Resets the chip, leaving the result of an install for the application
/// in GPREGRET2 and asking for the next step in GPREGRET.
pub fn reset(gpregret: u8, result: u8) -> ! {
    led::off();
    let power = pac::POWER;
    power.gpregret().write(|w| w.set_gpregret(gpregret));
    power.gpregret2().write(|w| w.set_gpregret(result));
    cortex_m::peripheral::SCB::sys_reset()
}

/// The running application's header, if it is valid.
pub fn running() -> Option<Header> {
    app::header()
}

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    // The red LED (P1.15) on, and wait for a reset: a reset loop would
    // hide the fault, and the double tap still reaches recovery.
    let p1 = pac::P1;
    p1.pin_cnf(15).write(|w| w.set_dir(pac::gpio::vals::Dir::Output));
    p1.outset().write(|w| w.0 = 1 << 15);
    loop {
        cortex_m::asm::wfi();
    }
}
