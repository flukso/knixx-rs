//! Resets into the bootloader (firmware/knixx-boot): GPREGRET tells it what
//! to do after the reset, GPREGRET2 tells what it did.

use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::pac;
use knixx_image::{self as image, HEADER_SIZE, Header};

/// Waits for an image over USB: the 1200-baud touch, `fw recovery`.
pub const RECOVERY: u8 = image::GPREGRET_RECOVERY;
/// Installs the staged image: `fw install`.
pub const INSTALL: u8 = image::GPREGRET_INSTALL;

/// Resets the chip into the bootloader in mode magic.
pub fn reset_into(magic: u8) -> ! {
    pac::POWER.gpregret().write(|w| w.set_gpregret(magic));
    cortex_m::peripheral::SCB::sys_reset()
}

/// The header of the running firmware, which the bootloader checked.
pub fn header() -> Option<Header> {
    // Safety: internal flash below the application, memory-mapped.
    let raw: &[u8; HEADER_SIZE] = unsafe { &*(image::HEADER_ADDR as *const [u8; HEADER_SIZE]) };
    Header::parse(raw).ok()
}

/// What the bootloader did on the last boot (knixx_image::result), read
/// once at boot.
pub fn last_result() -> u8 {
    pac::POWER.gpregret2().read().gpregret()
}

/// Clears what a bootloader may leave of POWER and CLOCK when it jumps to
/// the application without a reset: the USB and clock events and their
/// interrupt enables. Left set, they fire the clock interrupt before MPSL
/// and the USB driver are ready for it, which hangs a board that runs both
/// (nrf-sdc issue #103). Called first thing in main. Our bootloader jumps
/// before it touches them; this stays for one release.
pub fn sanitize() {
    let power = pac::POWER;
    power.events_usbdetected().write_value(0);
    power.events_usbremoved().write_value(0);
    power.events_usbpwrrdy().write_value(0);
    power.intenclr().write(|w| w.0 = 0xFFFF_FFFF);
    let clock = pac::CLOCK;
    clock.events_hfclkstarted().write_value(0);
    clock.events_lfclkstarted().write_value(0);
    clock.events_done().write_value(0);
    clock.events_ctto().write_value(0);
    clock.intenclr().write(|w| w.0 = 0xFFFF_FFFF);
    interrupt::CLOCK_POWER.unpend();
}

/// Checks that no application interrupt sits at P0 or P1, which MPSL keeps
/// for the radio; a mistake there breaks BLE timing in ways hard to trace.
pub fn check_priorities() {
    for (name, irq) in [
        ("USBD", interrupt::USBD),
        ("RNG", interrupt::RNG),
        ("GPIOTE", interrupt::GPIOTE),
        ("RTC1", interrupt::RTC1),
        ("CLOCK_POWER", interrupt::CLOCK_POWER),
        ("PWM0", interrupt::PWM0),
        ("QSPI", interrupt::QSPI),
        ("UARTE0", interrupt::UARTE0),
        ("TIMER1", interrupt::TIMER1),
        ("TWISPI1", interrupt::TWISPI1),
    ] {
        let prio = irq.get_priority();
        assert!(
            prio != Priority::P0 && prio != Priority::P1,
            "interrupt {name} at {prio:?}, which MPSL keeps for itself"
        );
    }
}
