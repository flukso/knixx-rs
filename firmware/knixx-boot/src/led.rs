//! The Feather's NeoPixel (P0.16) and red LED (P1.15), as the Adafruit
//! nRF52 bootloader shows them, so that a board in its bootloader looks
//! the same as it always did:
//!
//! | state                              | NeoPixel | red LED breathes every |
//! |------------------------------------|----------|------------------------|
//! | started, USB not (yet) connected   | red      | 300 ms                 |
//! | USB connected                      | green    | 3 s                    |
//! | writing an image                   | red      | 100 ms                 |
//!
//! At the Adafruit bootloader's brightness: 0x04 of 0xFF for the NeoPixel,
//! up to 0x4F of 0xFF for the red LED. The NeoPixel's power switch, P1.14,
//! is switched on as the Adafruit bootloader does.
//!
//! The NeoPixel plays a WS2812 sequence on PWM1 (each bit one 1.25 µs
//! period); the red LED is PWM2, its duty set from SysTick every
//! millisecond. [`off`] leaves both, and the pins, as a reset does, before
//! the jump to the application.

use core::ptr::addr_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use cortex_m::peripheral::syst::SystClkSource;
use cortex_m_rt::exception;
use embassy_nrf::pac;
use embassy_nrf::pac::gpio::vals::Dir;

/// The NeoPixel's colours, as RGB.
const RED: [u8; 3] = [0x04, 0, 0];
const GREEN: [u8; 3] = [0, 0x04, 0];
const OFF: [u8; 3] = [0, 0, 0];

/// What the bootloader is doing.
#[derive(Clone, Copy)]
pub enum State {
    /// Started, or USB not connected.
    Started,
    /// USB is connected.
    Usb,
    /// Writing an image.
    Writing,
}

/// The red LED's breathing period, in ms.
static CYCLE: AtomicU32 = AtomicU32::new(300);
/// Milliseconds since SysTick started.
static MILLIS: AtomicU32 = AtomicU32::new(0);
/// Whether start set the LEDs up.
static STARTED: AtomicBool = AtomicBool::new(false);

/// The red LED's PWM value, which PWM2 plays from RAM.
static mut RED_DUTY: u16 = 0x8000;

/// The WS2812 bits of one pixel and the reset gap.
static mut PIXEL: [u16; 24 + 2] = [0x8000; 26];

const NEOPIXEL: usize = 16; // P0.16
const NEOPIXEL_POWER: usize = 14; // P1.14
const RED_LED: usize = 15; // P1.15

/// Sets up the LEDs in state, with SysTick at 1 kHz from the 64 MHz clock.
pub fn start(state: State) {
    pac::P1.pin_cnf(NEOPIXEL_POWER).write(|w| w.set_dir(Dir::Output));
    pac::P1.outset().write(|w| w.0 = 1 << NEOPIXEL_POWER);
    pac::P0.pin_cnf(NEOPIXEL).write(|w| w.set_dir(Dir::Output));
    pac::P0.outclr().write(|w| w.0 = 1 << NEOPIXEL);
    pac::P1.pin_cnf(RED_LED).write(|w| w.set_dir(Dir::Output));
    pac::P1.outclr().write(|w| w.0 = 1 << RED_LED);

    // PWM1: the NeoPixel, 16 MHz, 20 ticks a bit.
    let pwm = pac::PWM1;
    pwm.psel()
        .out(0)
        .write_value(pac::shared::regs::Psel(NEOPIXEL as u32));
    pwm.prescaler()
        .write(|w| w.set_prescaler(pac::pwm::vals::Prescaler::Div1));
    pwm.countertop().write(|w| w.set_countertop(20));
    pwm.loop_()
        .write(|w| w.set_cnt(pac::pwm::vals::LoopCnt::from_bits(0)));
    pwm.decoder().write(|w| {
        w.set_load(pac::pwm::vals::Load::Common);
        w.set_mode(pac::pwm::vals::Mode::RefreshCount);
    });
    pwm.dma().seq(0).refresh().write(|w| w.0 = 0);
    pwm.dma().seq(0).enddelay().write(|w| w.0 = 0);
    pwm.enable().write(|w| w.set_enable(true));

    // PWM2: the red LED, 16 MHz / 8, 256 ticks: about 7.8 kHz.
    let pwm = pac::PWM2;
    pwm.psel()
        .out(0)
        .write_value(pac::shared::regs::Psel((32 + RED_LED) as u32));
    pwm.prescaler()
        .write(|w| w.set_prescaler(pac::pwm::vals::Prescaler::Div8));
    pwm.countertop().write(|w| w.set_countertop(0xFF));
    pwm.loop_()
        .write(|w| w.set_cnt(pac::pwm::vals::LoopCnt::from_bits(0)));
    pwm.decoder().write(|w| {
        w.set_load(pac::pwm::vals::Load::Common);
        w.set_mode(pac::pwm::vals::Mode::RefreshCount);
    });
    pwm.dma().seq(0).ptr().write_value(addr_of!(RED_DUTY) as u32);
    pwm.dma().seq(0).maxcnt().write(|w| w.0 = 1);
    pwm.dma().seq(0).refresh().write(|w| w.0 = 0);
    pwm.dma().seq(0).enddelay().write(|w| w.0 = 0);
    pwm.enable().write(|w| w.set_enable(true));

    STARTED.store(true, Ordering::Relaxed);
    set(state);

    // Safety: the bootloader owns SysTick; it is stopped again in off.
    let mut syst = unsafe { cortex_m::Peripherals::steal() }.SYST;
    syst.set_clock_source(SystClkSource::Core);
    syst.set_reload(64_000 - 1);
    syst.clear_current();
    syst.enable_interrupt();
    syst.enable_counter();
}

/// Shows state, once the LEDs are started.
pub fn set(state: State) {
    if !STARTED.load(Ordering::Relaxed) {
        return;
    }
    let (color, cycle) = match state {
        State::Started => (RED, 300),
        State::Usb => (GREEN, 3000),
        State::Writing => (RED, 100),
    };
    CYCLE.store(cycle, Ordering::Relaxed);
    pixel(color);
}

/// Writes the NeoPixel's colour and waits until it is sent: about 35 µs.
fn pixel(rgb: [u8; 3]) {
    let grb = [rgb[1], rgb[0], rgb[2]];
    // Safety: PIXEL is only used here, from thread mode, and the previous
    // sequence has ended (this waits for it).
    let words = unsafe { &mut *core::ptr::addr_of_mut!(PIXEL) };
    let mut i = 0;
    for byte in grb {
        for bit in (0..8).rev() {
            words[i] = 0x8000 | if byte >> bit & 1 == 1 { 13 } else { 6 };
            i += 1;
        }
    }
    let pwm = pac::PWM1;
    pwm.dma().seq(0).ptr().write_value(words.as_ptr() as u32);
    pwm.dma().seq(0).maxcnt().write(|w| w.0 = words.len() as u32);
    pwm.events_seqend(0).write_value(0);
    pwm.tasks_dma().seq(0).start().write_value(1);
    while pwm.events_seqend(0).read() == 0 {}
    pwm.events_seqend(0).write_value(0);
}

#[exception]
fn SysTick() {
    let ms = MILLIS.fetch_add(1, Ordering::Relaxed);
    let cycle = CYCLE.load(Ordering::Relaxed).max(2);
    let half = cycle / 2;
    let mut t = ms % cycle;
    if t > half {
        t = cycle - t;
    }
    let duty = (0x4F * t / half) as u16;
    // Safety: PWM2 reads the value by EasyDMA when a sequence starts; this
    // is its only writer.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(RED_DUTY), 0x8000 | duty) };
    pac::PWM2.tasks_dma().seq(0).start().write_value(1);
}

/// Turns both LEDs off and leaves SysTick, the PWMs and the pins as a
/// reset does, for the application. A WS2812 keeps its colour through a
/// reset of the chip, so this comes before every reset and jump.
pub fn off() {
    if !STARTED.swap(false, Ordering::Relaxed) {
        return;
    }
    // Safety: as in start.
    let mut syst = unsafe { cortex_m::Peripherals::steal() }.SYST;
    syst.disable_interrupt();
    syst.disable_counter();
    pixel(OFF);
    for pwm in [pac::PWM1, pac::PWM2] {
        pwm.tasks_stop().write_value(1);
        pwm.enable().write(|w| w.set_enable(false));
        pwm.psel()
            .out(0)
            .write_value(pac::shared::regs::Psel(0xFFFF_FFFF));
    }
    for (port, pin) in [(pac::P0, NEOPIXEL), (pac::P1, RED_LED), (pac::P1, NEOPIXEL_POWER)] {
        port.outclr().write(|w| w.0 = 1 << pin);
        port.pin_cnf(pin).write_value(pac::gpio::regs::PinCnf(2));
    }
}
