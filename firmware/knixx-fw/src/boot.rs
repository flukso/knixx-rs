//! Resets into the Adafruit bootloader: GPREGRET tells it what to do after
//! the reset. The SoftDevice is never enabled, so the register is written
//! directly.

use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::pac;

/// Starts the UF2 bootloader: its USB drive (the 1200-baud touch).
pub const UF2: u8 = 0x57;
/// Starts the bootloader's BLE DFU (the ota command).
pub const OTA: u8 = 0xA8;

/// Resets the chip into the bootloader in mode magic.
pub fn reset_into(magic: u8) -> ! {
    pac::POWER.gpregret().write(|w| w.set_gpregret(magic));
    cortex_m::peripheral::SCB::sys_reset()
}

/// Clears what the bootloader leaves of POWER and CLOCK when it jumps to
/// the application without a reset: the USB and clock events and their
/// interrupt enables. Left set, they fire the clock interrupt before MPSL
/// and the USB driver are ready for it, which hangs a board that runs both
/// (nrf-sdc issue #103). Called first thing in main.
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
