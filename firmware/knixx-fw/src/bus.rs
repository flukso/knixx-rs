//! The KNX bus: the NCN5121 transceiver on UARTE0 (TX D0/P0.25, RX D1/P0.24)
//! at 38400 baud, 8N1, driven by knixx_bus's engine and runner. The
//! console, the switches and the sensors queue commands on console::CMDS;
//! the trace and the errors go to the console, and an error blinks red.
//!
//! BufferedUarte receives into its RAM ring with EasyDMA while the executor
//! is busy elsewhere, so no byte is lost while, say, the flash erases.

use embassy_nrf::buffered_uarte::{self, BufferedUarte};
use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::peripherals::{P0_24, P0_25, PPI_CH0, PPI_CH1, PPI_GROUP0, TIMER1, UARTE0};
use embassy_nrf::uarte::{Baudrate, Config, Parity};
use embassy_nrf::{Peri, interrupt};
use knixx_bus::{Engine, Output};
use knixx_ui::led::Mode;
use static_cell::StaticCell;

use crate::{console, leds};

/// The peripherals of the bus.
pub struct Peripherals {
    pub uarte: Peri<'static, UARTE0>,
    pub timer: Peri<'static, TIMER1>,
    pub ppi_ch0: Peri<'static, PPI_CH0>,
    pub ppi_ch1: Peri<'static, PPI_CH1>,
    pub ppi_group: Peri<'static, PPI_GROUP0>,
    pub rxd: Peri<'static, P0_24>,
    pub txd: Peri<'static, P0_25>,
}

/// The bus engine's output: lines to the console, on USB or BLE, errors to
/// the red LED.
struct Console;

impl Output for Console {
    fn write(&mut self, b: &[u8]) {
        console::write_crlf(b);
    }

    fn failed(&mut self) {
        leds::signal(Mode::Error);
    }
}

/// Sets up the UART and returns it for the bus task.
pub fn uart(
    p: Peripherals,
    irq: impl Binding<interrupt::typelevel::UARTE0, buffered_uarte::InterruptHandler<UARTE0>> + 'static,
) -> BufferedUarte<'static> {
    let mut config = Config::default();
    config.baudrate = Baudrate::Baud38400;
    config.parity = Parity::Excluded;
    static RX: StaticCell<[u8; 256]> = StaticCell::new();
    static TX: StaticCell<[u8; 64]> = StaticCell::new();
    BufferedUarte::new(
        p.uarte,
        p.timer,
        p.ppi_ch0,
        p.ppi_ch1,
        p.ppi_group,
        p.rxd,
        p.txd,
        irq,
        config,
        RX.init([0; 256]),
        TX.init([0; 64]),
    )
}

#[embassy_executor::task]
pub async fn task(mut uart: BufferedUarte<'static>) -> ! {
    let mut engine = Engine::new(console::device(), &console::FLAGS);
    knixx_bus::run(&mut uart, &mut engine, &console::CMDS.receiver(), &mut Console).await
}
