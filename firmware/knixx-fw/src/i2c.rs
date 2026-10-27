//! I2C1 (TWIM1, SDA P0.12, SCL P0.11, 400 kHz), which the SHT20, the
//! VCNL4040 and the VL53L7CX share. Each driver gets an I2cDevice of the
//! shared bus, which holds the bus for one transaction at a time, as the Go
//! firmware's i2cbus.Shared did.

use embassy_embedded_hal::shared_bus::asynch::i2c::I2cDevice;
use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::peripherals::{P0_11, P0_12, TWISPI1};
use embassy_nrf::twim::{self, Frequency, Twim};
use embassy_nrf::{Peri, interrupt};
use embassy_sync::blocking_mutex::raw::NoopRawMutex;
use embassy_sync::mutex::Mutex;
use static_cell::StaticCell;

/// A device on the shared bus.
pub type Device = I2cDevice<'static, NoopRawMutex, Twim<'static>>;

/// The shared bus: the drivers' devices take it in turn.
pub type Bus = Mutex<NoopRawMutex, Twim<'static>>;

/// Sets up I2C1 and returns the shared bus.
pub fn start(
    twim: Peri<'static, TWISPI1>,
    irq: impl Binding<interrupt::typelevel::TWISPI1, twim::InterruptHandler<TWISPI1>> + 'static,
    sda: Peri<'static, P0_12>,
    scl: Peri<'static, P0_11>,
) -> &'static Bus {
    let mut config = twim::Config::default();
    config.frequency = Frequency::K400;
    // The internal pull-ups, as TinyGo enables them: with the sensor board
    // and its pull-ups unplugged, the lines would float, and a transfer would
    // wait forever for SCL instead of failing with a NACK. The transfer
    // cannot be given a timeout instead: cancelling it would not stop the
    // TWIM's EasyDMA.
    config.sda_pullup = true;
    config.scl_pullup = true;
    // Writes of data in flash (the VL53L7CX firmware) go through this RAM
    // buffer, as EasyDMA reads RAM only.
    static TX: StaticCell<[u8; 256]> = StaticCell::new();
    let twim = Twim::new(twim, irq, sda, scl, config, TX.init([0; 256]));
    static BUS: StaticCell<Bus> = StaticCell::new();
    BUS.init(Mutex::new(twim))
}
