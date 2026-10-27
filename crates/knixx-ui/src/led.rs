//! The two status LEDs, a WS2812B string. One task owns the LEDs; the only
//! way in is its channel of triggers. Each trigger queues a mode of LED1, and
//! the modes play one after another, each for its length, unless a cancel
//! trigger ends one early. LED2 shows the BLE console, independently of LED1.
//!
//! [`Player`] holds the logic; the firmware's LED task drives the strip with
//! it. That task writes the strip only when a LED changes and, without a mode
//! to play, only waits for triggers. While a mode plays, it sleeps until the
//! next frame, at most [`BUSY_POLL`] at a time, and takes the triggers that
//! came in between.

use embassy_time::{Duration, Instant};

/// A pattern of LED1, or BLE on LED2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Mode {
    /// Blinks at 5 Hz for at most 10 s, while the device starts.
    Init,
    /// A single 50 ms pulse for a motion of the ToF sensor.
    Occupancy,
    /// Flashes 10 ms every 100 ms for 0.5 s.
    Error,
    /// Pulses LED2 for 50 ms each second until cancelled, while the console
    /// runs on BLE.
    Ble,
}

const NUM_MODES: usize = 4;

/// Queues `mode`, or with `cancel` set ends it: a playing mode stops and
/// queued ones are dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Trigger {
    pub mode: Mode,
    pub cancel: bool,
}

/// The capacity of both the trigger channel and the queue of modes waiting
/// to play.
pub const QUEUE_LEN: usize = 8;

/// Bounds the wait for the next frame while a mode plays, so that a trigger
/// is taken within it.
pub const BUSY_POLL: Duration = Duration::from_millis(10);

/// The colour of a LED.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

/// A dark LED.
pub const OFF: Rgb = Rgb { r: 0, g: 0, b: 0 };

// The colours of the LEDs, dimmed: full brightness is glaring.
pub const YELLOW: Rgb = Rgb {
    r: 0x20,
    g: 0x10,
    b: 0,
};
pub const GREEN: Rgb = Rgb { r: 0, g: 0x20, b: 0 };
pub const RED: Rgb = Rgb { r: 0x20, g: 0, b: 0 };
pub const BLUE: Rgb = Rgb { r: 0, g: 0, b: 0x20 };

/// Lights the LED in `colour` for `on` at the start of every `period`, until
/// `length` has passed; a zero length lasts until cancelled.
#[derive(Debug, Clone, Copy)]
struct Pattern {
    colour: Rgb,
    on: Duration,
    period: Duration,
    length: Duration,
}

const PATTERNS: [Pattern; NUM_MODES] = [
    // Init
    Pattern {
        colour: YELLOW,
        on: Duration::from_millis(100),
        period: Duration::from_millis(200),
        length: Duration::from_secs(10),
    },
    // Occupancy
    Pattern {
        colour: GREEN,
        on: Duration::from_millis(50),
        period: Duration::from_millis(100),
        length: Duration::from_millis(100),
    },
    // Error
    Pattern {
        colour: RED,
        on: Duration::from_millis(10),
        period: Duration::from_millis(100),
        length: Duration::from_millis(500),
    },
    // Ble
    Pattern {
        colour: BLUE,
        on: Duration::from_millis(50),
        period: Duration::from_secs(1),
        length: Duration::from_ticks(0),
    },
];

const fn pattern(m: Mode) -> &'static Pattern {
    &PATTERNS[m as usize]
}

impl Pattern {
    /// Returns the colour of the pattern at `now`, for a start at `start`,
    /// and the time of its next change.
    fn at(&self, start: Instant, now: Instant) -> (Rgb, Instant) {
        let elapsed = now.saturating_duration_since(start).as_ticks();
        let phase = elapsed % self.period.as_ticks();
        let cycle = start + Duration::from_ticks(elapsed - phase);
        if phase < self.on.as_ticks() {
            return (self.colour, cycle + self.on);
        }
        (OFF, cycle + self.period)
    }
}

/// Sequences the modes of LED1 and runs the BLE pulse of LED2. It does not
/// read the clock: the caller passes the time.
#[derive(Debug, Clone)]
pub struct Player {
    // Records that LED2 pulses, since ble_start.
    ble: bool,
    ble_start: Instant,

    playing: bool,
    cur: Mode,
    start: Instant,
    // The modes waiting to play, a ring of n from head.
    pending: [Mode; QUEUE_LEN],
    head: u8,
    n: u8,
}

impl Default for Player {
    fn default() -> Self {
        Self::new()
    }
}

impl Player {
    pub const fn new() -> Self {
        Self {
            ble: false,
            ble_start: Instant::MIN,
            playing: false,
            cur: Mode::Init,
            start: Instant::MIN,
            pending: [Mode::Init; QUEUE_LEN],
            head: 0,
            n: 0,
        }
    }

    /// Handles the trigger `t` received at `now`. A mode that arrives with
    /// the queue full is dropped.
    pub fn apply(&mut self, t: Trigger, now: Instant) {
        if t.mode == Mode::Ble {
            if t.cancel {
                self.ble = false;
            } else if !self.ble {
                self.ble = true;
                self.ble_start = now;
            }
            return;
        }
        self.settle(now);
        if t.cancel {
            self.cancel(t.mode, now);
            return;
        }
        if !self.playing {
            self.playing = true;
            self.cur = t.mode;
            self.start = now;
            return;
        }
        if self.n as usize == QUEUE_LEN {
            return;
        }
        self.pending[(self.head + self.n) as usize % QUEUE_LEN] = t.mode;
        self.n += 1;
    }

    /// Returns the colours of LED1 and LED2 at `now` and the time of the
    /// next change of either, or busy false when neither has anything to
    /// play.
    pub fn frame(&mut self, now: Instant) -> ([Rgb; 2], Instant, bool) {
        self.settle(now);
        let mut leds = [OFF; 2];
        let mut next = Instant::MIN;
        if self.playing {
            let pat = pattern(self.cur);
            (leds[0], next) = pat.at(self.start, now);
            let end = self.start + pat.length;
            if next > end {
                next = end;
            }
        }
        if self.ble {
            let next2;
            (leds[1], next2) = pattern(Mode::Ble).at(self.ble_start, now);
            if !self.playing || next2 < next {
                next = next2;
            }
        }
        (leds, next, self.playing || self.ble)
    }

    /// Moves past the modes that ended by `now`. Each next mode starts when
    /// the previous one ended, so late calls keep the sequence gapless.
    fn settle(&mut self, now: Instant) {
        while self.playing {
            let end = self.start + pattern(self.cur).length;
            if now < end {
                return;
            }
            self.advance(end);
        }
    }

    /// Starts the next queued mode at `at`, if any.
    fn advance(&mut self, at: Instant) {
        if self.n == 0 {
            self.playing = false;
            return;
        }
        self.cur = self.pending[self.head as usize];
        self.start = at;
        self.head = (self.head + 1) % QUEUE_LEN as u8;
        self.n -= 1;
    }

    /// Drops the queued entries of `m` and ends `m` if it is playing.
    fn cancel(&mut self, m: Mode, now: Instant) {
        let mut kept = 0u8;
        for i in 0..self.n {
            let q = self.pending[(self.head + i) as usize % QUEUE_LEN];
            if q != m {
                self.pending[(self.head + kept) as usize % QUEUE_LEN] = q;
                kept += 1;
            }
        }
        self.n = kept;
        if self.playing && self.cur == m {
            self.advance(now);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    const T0: Instant = Instant::from_secs(1_000_000);

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A change of LED1 at a time after T0.
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct Edge {
        at: Duration,
        on: bool,
    }

    const fn edge(at: Duration, on: bool) -> Edge {
        Edge { at, on }
    }

    fn trig(mode: Mode) -> Trigger {
        Trigger { mode, cancel: false }
    }

    fn stop(mode: Mode) -> Trigger {
        Trigger { mode, cancel: true }
    }

    /// Follows the frames of p from T0 until it is idle and returns the
    /// changes of LED1.
    fn play(p: &mut Player) -> Vec<Edge> {
        let mut edges: Vec<Edge> = Vec::new();
        let mut now = T0;
        for i in 0.. {
            assert!(i <= 1000, "player does not end");
            let (leds, next, busy) = p.frame(now);
            let led1 = leds[0];
            assert_eq!(leds[1], OFF, "LED2 lit");
            if !busy {
                assert_eq!(led1, OFF, "idle LED1");
                edges.push(edge(now - T0, false));
                break;
            }
            let on = led1 != OFF;
            if on {
                assert_eq!(led1, pattern(p.cur).colour, "mode {:?} lit", p.cur);
            }
            if edges.last().is_none_or(|e| e.on != on) {
                edges.push(edge(now - T0, on));
            }
            assert!(next > now, "next {next:?} not after {now:?}");
            now = next;
        }
        edges
    }

    /// Returns the edges of n cycles of on and period from start.
    fn blinks(start: Duration, on: Duration, period: Duration, n: u32) -> Vec<Edge> {
        let mut e = Vec::new();
        for i in 0..n {
            let c = start + period * i;
            e.push(edge(c, true));
            e.push(edge(c + on, false));
        }
        e
    }

    fn with(mut e: Vec<Edge>, tail: &[Edge]) -> Vec<Edge> {
        e.extend_from_slice(tail);
        e
    }

    #[test]
    fn modes() {
        for (mode, want) in [
            (
                Mode::Occupancy,
                std::vec![edge(ms(0), true), edge(ms(50), false), edge(ms(100), false)],
            ),
            (
                Mode::Error,
                with(blinks(ms(0), ms(10), ms(100), 5), &[edge(ms(500), false)]),
            ),
            (
                Mode::Init,
                with(blinks(ms(0), ms(100), ms(200), 50), &[edge(ms(10_000), false)]),
            ),
        ] {
            let mut p = Player::new();
            p.apply(trig(mode), T0);
            assert_eq!(play(&mut p), want, "mode {mode:?}");
        }
    }

    #[test]
    fn sequence() {
        let mut p = Player::new();
        for m in [Mode::Error, Mode::Occupancy, Mode::Occupancy] {
            p.apply(trig(m), T0);
        }
        let mut want = blinks(ms(0), ms(10), ms(100), 5);
        want.extend(blinks(ms(500), ms(50), ms(100), 2));
        want.push(edge(ms(700), false));
        assert_eq!(play(&mut p), want);
    }

    #[test]
    fn late_frame() {
        let mut p = Player::new();
        p.apply(trig(Mode::Occupancy), T0);
        p.apply(trig(Mode::Error), T0);
        // Error started when Occupancy ended, at 100 ms: at 205 ms it is in
        // its second flash.
        let (leds, next, busy) = p.frame(T0 + ms(205));
        assert_eq!((leds[0], next, busy), (RED, T0 + ms(210), true));
    }

    #[test]
    fn cancel() {
        let mut p = Player::new();
        p.apply(trig(Mode::Init), T0);
        p.apply(trig(Mode::Occupancy), T0);
        p.apply(trig(Mode::Init), T0);
        p.apply(stop(Mode::Init), T0 + ms(150));
        // The queued Init is dropped as well; Occupancy starts at once.
        let (leds, next, busy) = p.frame(T0 + ms(150));
        assert_eq!((leds[0], next, busy), (GREEN, T0 + ms(200), true));
        let (_, _, busy) = p.frame(T0 + ms(250));
        assert!(!busy, "busy after the occupancy pulse");
    }

    #[test]
    fn cancel_other() {
        let mut p = Player::new();
        p.apply(trig(Mode::Error), T0);
        p.apply(stop(Mode::Init), T0);
        let want = with(blinks(ms(0), ms(10), ms(100), 5), &[edge(ms(500), false)]);
        assert_eq!(play(&mut p), want);
    }

    #[test]
    fn queue_full() {
        let mut p = Player::new();
        // One playing and QUEUE_LEN queued; the rest is dropped.
        for _ in 0..QUEUE_LEN + 5 {
            p.apply(trig(Mode::Occupancy), T0);
        }
        let got = play(&mut p);
        let end = got.last().unwrap().at;
        assert_eq!(end, ms(100) * (QUEUE_LEN as u32 + 1));
    }

    #[test]
    fn ble() {
        let mut p = Player::new();
        p.apply(trig(Mode::Ble), T0);
        for (at, next, on) in [
            (ms(0), ms(50), true),
            (ms(49), ms(50), true),
            (ms(50), ms(1000), false),
            (ms(999), ms(1000), false),
            (ms(1000), ms(1050), true),
            (
                Duration::from_secs(3600) + ms(10),
                Duration::from_secs(3600) + ms(50),
                true,
            ),
        ] {
            let (leds, got_next, busy) = p.frame(T0 + at);
            let want = [OFF, if on { BLUE } else { OFF }];
            assert_eq!((leds, got_next, busy), (want, T0 + next, true), "at {at:?}");
        }
        // A repeat keeps the phase.
        p.apply(trig(Mode::Ble), T0 + ms(500));
        let (leds, _, _) = p.frame(T0 + ms(510));
        assert_eq!(leds[1], OFF, "a repeated BLE restarted the pulse");
        p.apply(stop(Mode::Ble), T0 + ms(2000));
        let (leds, _, busy) = p.frame(T0 + ms(2000));
        assert_eq!((leds[1], busy), (OFF, false), "after cancel");
    }

    #[test]
    fn ble_independent() {
        let mut p = Player::new();
        p.apply(trig(Mode::Ble), T0);
        p.apply(trig(Mode::Error), T0 + ms(20));
        for (at, next, leds) in [
            // Error flashes 20-30 ms; the BLE pulse runs 0-50 ms.
            (ms(20), ms(30), [RED, BLUE]),
            (ms(30), ms(50), [OFF, BLUE]),
            (ms(50), ms(120), [OFF, OFF]),
            (ms(120), ms(130), [RED, OFF]),
            // Error ends at 520 ms; LED2 goes on at 1 s.
            (ms(500), ms(520), [OFF, OFF]),
            (ms(520), ms(1000), [OFF, OFF]),
        ] {
            assert_eq!(p.frame(T0 + at), (leds, T0 + next, true), "at {at:?}");
        }
    }
}
