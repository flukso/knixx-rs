//! Host-side support for knixx boards: firmware images in the formats the
//! Adafruit nRF52 bootloader takes, the bootloader's drive, the board's USB
//! serial port, and flashing over USB.

pub mod drive;
pub mod flash;
pub mod image;
pub mod uf2;
pub mod usbserial;
