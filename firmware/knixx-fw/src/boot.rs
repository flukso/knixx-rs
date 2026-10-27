//! Resets into the Adafruit bootloader: GPREGRET tells it what to do after
//! the reset. The SoftDevice is never enabled, so the register is written
//! directly.

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
