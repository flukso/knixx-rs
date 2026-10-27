//! knixx firmware for the knixx Thunder board (Feather nRF52840).
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::{bind_interrupts, peripherals, usb};

mod boot;
mod console;
mod panic;
mod usbcdc;

bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => usb::vbus_detect::InterruptHandler;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let mut config = embassy_nrf::config::Config::default();
    // USB needs the crystal oscillator.
    config.hfclk_source = embassy_nrf::config::HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);

    // The red LED stays off: it is for errors later on.
    let _led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard);

    let (rx, tx) = usbcdc::start(&spawner, p.USBD, Irqs);
    spawner.spawn(usbcdc::tx_task(tx).unwrap());
    spawner.spawn(console::task(rx).unwrap());
}
