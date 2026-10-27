//! knixx firmware for the knixx Thunder board (Feather nRF52840).
#![no_std]
#![no_main]

use embassy_executor::Spawner;
use embassy_nrf::gpio::{Level, Output, OutputDrive};
use embassy_nrf::interrupt::{self, InterruptExt, Priority};
use embassy_nrf::rng::{self, Rng};
use embassy_nrf::{bind_interrupts, peripherals, usb};
use knixx_ui::led::Mode;
use nrf_mpsl::MultiprotocolServiceLayer;
use static_cell::StaticCell;

mod ble;
mod boot;
mod bus;
mod console;
mod i2c;
mod leds;
mod panic;
mod sensor;
mod storage;
mod switches;
mod tof;
mod usbcdc;

// MPSL takes the radio, its timers and the clock interrupt; USB's VBUS
// detection shares the clock interrupt with it.
bind_interrupts!(struct Irqs {
    USBD => usb::InterruptHandler<peripherals::USBD>;
    CLOCK_POWER => nrf_mpsl::ClockInterruptHandler, usb::vbus_detect::InterruptHandler;
    RNG => rng::InterruptHandler<peripherals::RNG>;
    EGU0_SWI0 => nrf_mpsl::LowPrioInterruptHandler;
    RADIO => nrf_mpsl::HighPrioInterruptHandler;
    TIMER0 => nrf_mpsl::HighPrioInterruptHandler;
    RTC0 => nrf_mpsl::HighPrioInterruptHandler;
    QSPI => embassy_nrf::qspi::InterruptHandler<peripherals::QSPI>;
    UARTE0 => embassy_nrf::buffered_uarte::InterruptHandler<peripherals::UARTE0>;
    TWISPI1 => embassy_nrf::twim::InterruptHandler<peripherals::TWISPI1>;
});

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    boot::sanitize();

    // Interrupt priorities: MPSL keeps P0 (radio, its timers) and P1 to
    // itself and runs its clock and low-priority work at P4. The rest of the
    // application stays at P2 and P3.
    let mut config = embassy_nrf::config::Config::default();
    config.time_interrupt_priority = Priority::P2;
    config.gpiote_interrupt_priority = Priority::P3;
    let p = embassy_nrf::init(config);
    interrupt::USBD.set_priority(Priority::P3);
    interrupt::RNG.set_priority(Priority::P3);
    interrupt::PWM0.set_priority(Priority::P3);
    interrupt::QSPI.set_priority(Priority::P3);
    interrupt::UARTE0.set_priority(Priority::P3);
    interrupt::TIMER1.set_priority(Priority::P3);
    interrupt::TWISPI1.set_priority(Priority::P3);

    // The red LED stays off: the status LEDs show errors.
    let _led = Output::new(p.P1_15, Level::Low, OutputDrive::Standard);
    spawner.spawn(leds::task(p.PWM0, p.P0_28).unwrap());
    leds::signal(Mode::Init);

    // The configuration from the flash, or the built-in default.
    let pins = storage::Pins {
        sck: p.P0_19,
        csn: p.P0_20,
        io0: p.P0_17,
        io1: p.P0_22,
        io2: p.P0_23,
        io3: p.P0_21,
    };
    let (dev, ok) = console::BOOT_LOG.lock(|l| storage::load(p.QSPI, Irqs, pins, &mut *l.borrow_mut()));
    console::DEVICE.lock(|d| d.set(Some(dev)));
    if !ok {
        leds::signal(Mode::Error);
    }

    let mpsl_p = nrf_mpsl::Peripherals::new(p.RTC0, p.TIMER0, p.TEMP, p.PPI_CH19, p.PPI_CH30, p.PPI_CH31);
    static MPSL: StaticCell<MultiprotocolServiceLayer> = StaticCell::new();
    let mpsl = MPSL.init(MultiprotocolServiceLayer::new(mpsl_p, Irqs, ble::lfclk()).expect("mpsl"));
    spawner.spawn(ble::mpsl_task(mpsl).unwrap());

    // USB runs on the crystal oscillator, which MPSL owns: hold it for good.
    core::mem::forget(mpsl.request_hfclk().await.expect("hfclk"));

    let (rx, tx) = usbcdc::start(&spawner, p.USBD, Irqs);
    spawner.spawn(usbcdc::tx_task(tx).unwrap());
    spawner.spawn(console::task(rx).unwrap());

    let uart = bus::uart(
        bus::Peripherals {
            uarte: p.UARTE0,
            timer: p.TIMER1,
            ppi_ch0: p.PPI_CH0,
            ppi_ch1: p.PPI_CH1,
            ppi_group: p.PPI_GROUP0,
            rxd: p.P0_24,
            txd: p.P0_25,
        },
        Irqs,
    );
    spawner.spawn(bus::task(uart).unwrap());

    let i2c = i2c::start(p.TWISPI1, Irqs, p.P0_12, p.P0_11);
    spawner.spawn(sensor::task(i2c).unwrap());
    let tof_pins = tof::Pins {
        int: p.P0_14,
        lpn: p.P0_13,
        rst: p.P0_15,
    };
    spawner.spawn(tof::task(spawner, i2c, tof_pins).unwrap());

    switches::start(
        &spawner,
        [p.P0_26.into(), p.P0_27.into(), p.P0_04.into(), p.P0_05.into()],
    );

    static RNG: StaticCell<Rng<'static, embassy_nrf::mode::Async>> = StaticCell::new();
    let rng = RNG.init(Rng::new(p.RNG, Irqs));
    let sdc_p = nrf_sdc::Peripherals::new(
        p.PPI_CH17, p.PPI_CH18, p.PPI_CH20, p.PPI_CH21, p.PPI_CH22, p.PPI_CH23, p.PPI_CH24, p.PPI_CH25,
        p.PPI_CH26, p.PPI_CH27, p.PPI_CH28, p.PPI_CH29,
    );
    ble::start(&spawner, sdc_p, rng, mpsl);

    boot::check_priorities();
}
