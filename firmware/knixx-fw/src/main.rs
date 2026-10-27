//! knixx firmware for the knixx Thunder board (Feather nRF52840).
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_time::Timer;

mod panic;

#[embassy_executor::main]
async fn main(_spawner: Spawner) {
    let p = embassy_nrf::init(Default::default());
    // The Feather's red LED (D3), active high.
    let mut led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard);
    loop {
        led.toggle();
        Timer::after_millis(500).await;
    }
}
