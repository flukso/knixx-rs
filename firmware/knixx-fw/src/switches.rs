//! The wall switches: four contacts, A on/off and B on/off, active low with
//! the internal pull-ups. Each contact is debounced in software: a press
//! counts once the contact stayed closed for DEBOUNCE after it closed, and
//! the contact counts again only after it stayed open for DEBOUNCE, so the
//! bounces when it closes and opens, and glitches shorter than DEBOUNCE,
//! count nothing. Every 20 ms a changed count switches the lights as the
//! switch command does.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_nrf::Peri;
use embassy_nrf::gpio::{AnyPin, Input, Pull};
use embassy_time::Timer;
use knixx_ui::switches::Edges;

use crate::console::{Ctx, Out};

/// How often the press counts are looked at.
const POLL_MS: u64 = 20;

/// How long a contact must stay closed, or open, to count as such: longer
/// than the bounces of a wall switch, short enough not to be noticed.
const DEBOUNCE_MS: u64 = 30;

struct Contact {
    name: &'static str,
    on: bool,
}

const CONTACTS: [Contact; 4] = [
    Contact { name: "A", on: true },
    Contact { name: "A", on: false },
    Contact { name: "B", on: true },
    Contact { name: "B", on: false },
];

/// The presses counted on each contact.
static PRESSES: [AtomicU32; 4] = [const { AtomicU32::new(0) }; 4];

/// Starts the switch tasks on the contact pins, in the order of CONTACTS:
/// A on (D9, P0.26), A off (D10, P0.27), B on (A0, P0.04), B off (A1,
/// P0.05).
pub fn start(spawner: &Spawner, pins: [Peri<'static, AnyPin>; 4]) {
    for (i, pin) in pins.into_iter().enumerate() {
        spawner.spawn(edges(Input::new(pin, Pull::Up), i).unwrap());
    }
    spawner.spawn(task().unwrap());
}

/// Counts the debounced presses of contact i.
#[embassy_executor::task(pool_size = 4)]
async fn edges(mut pin: Input<'static>, i: usize) -> ! {
    loop {
        pin.wait_for_low().await;
        Timer::after_millis(DEBOUNCE_MS).await;
        if pin.is_high() {
            // A glitch, or the bounce of a release.
            continue;
        }
        PRESSES[i].fetch_add(1, Ordering::Relaxed);
        // Wait until the contact stays open.
        loop {
            pin.wait_for_high().await;
            Timer::after_millis(DEBOUNCE_MS).await;
            if pin.is_high() {
                break;
            }
        }
    }
}

#[embassy_executor::task]
async fn task() -> ! {
    let mut edges = [Edges::default(); 4];
    loop {
        Timer::after_millis(POLL_MS).await;
        for (i, c) in CONTACTS.iter().enumerate() {
            if edges[i].changed(PRESSES[i].load(Ordering::Relaxed)) {
                knixx_cli::switch(&mut Ctx, c.name, c.on, &mut Out);
            }
        }
    }
}
