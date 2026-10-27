//! The two WS2812B status LEDs on A3 (P0.28), driven by PWM0: each bit is
//! one 1.25 µs PWM period, high for 0.8 µs (1) or 0.4 µs (0), from a
//! sequence in RAM that EasyDMA plays without the CPU. Bit-banging would
//! need interrupts off for longer than the BLE controller allows.
//!
//! The LED task plays the modes of knixx_ui::led::Player; signal queues a
//! mode without waiting.

use embassy_nrf::Peri;
use embassy_nrf::peripherals::{P0_28, PWM0};
use embassy_nrf::pwm::{
    Config, Prescaler, SequenceConfig, SequenceLoad, SequencePwm, SingleSequenceMode, SingleSequencer,
};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_time::{Instant, Timer};
use knixx_ui::led::{self, Mode, Player, Rgb, Trigger};

/// The triggers of the LED task.
static TRIGGERS: Channel<CriticalSectionRawMutex, Trigger, { led::QUEUE_LEN }> = Channel::new();

/// Queues mode for the LEDs; it is dropped when the queue is full.
pub fn signal(mode: Mode) {
    let _ = TRIGGERS.try_send(Trigger { mode, cancel: false });
}

/// PWM ticks at 16 MHz: a period of 20 ticks is 1.25 µs.
const PERIOD: u16 = 20;
/// The high time of a 1 and a 0 bit; bit 15 makes the pin start high.
const T1H: u16 = 0x8000 | 13;
const T0H: u16 = 0x8000 | 6;
/// A low period, for the reset gap that latches the colours.
const RES: u16 = 0x8000;

const LEDS: usize = 2;
const RESET_SLOTS: usize = 40;

/// Encodes the colours as WS2812B bits, green, red, blue, MSB first, then
/// the reset gap.
fn encode(leds: &[Rgb; LEDS], words: &mut [u16; LEDS * 24 + RESET_SLOTS]) {
    let mut i = 0;
    for c in leds {
        for byte in [c.g, c.r, c.b] {
            for bit in (0..8).rev() {
                words[i] = if byte >> bit & 1 == 1 { T1H } else { T0H };
                i += 1;
            }
        }
    }
    for w in &mut words[i..] {
        *w = RES;
    }
}

#[embassy_executor::task]
pub async fn task(pwm: Peri<'static, PWM0>, pin: Peri<'static, P0_28>) -> ! {
    let mut config = Config::default();
    config.max_duty = PERIOD;
    config.prescaler = Prescaler::Div1;
    config.sequence_load = SequenceLoad::Common;
    let mut pwm = SequencePwm::new_1ch(pwm, pin, config).expect("pwm");
    let mut words = [RES; LEDS * 24 + RESET_SLOTS];

    let mut player = Player::new();
    let mut shown: Option<[Rgb; LEDS]> = None;
    loop {
        let now = Instant::now();
        let (leds, next, busy) = player.frame(now);
        if shown != Some(leds) {
            shown = Some(leds);
            encode(&leds, &mut words);
            let seq = SingleSequencer::new(&mut pwm, &words, SequenceConfig::default());
            if seq.start(SingleSequenceMode::Times(1)).is_ok() {
                // 88 periods of 1.25 µs: done well within a millisecond.
                Timer::after_millis(1).await;
            }
            seq.stop();
        }
        if !busy {
            let t = TRIGGERS.receive().await;
            player.apply(t, Instant::now());
            continue;
        }
        let wait = next.saturating_duration_since(now).min(led::BUSY_POLL);
        Timer::after(wait).await;
        while let Ok(t) = TRIGGERS.try_receive() {
            player.apply(t, Instant::now());
        }
    }
}
