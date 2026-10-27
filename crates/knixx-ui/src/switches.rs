//! The logic of the wall switches that does not touch the hardware: press
//! detection from interrupt counters and the hold gestures.

use embassy_time::{Duration, Instant};

/// How long a gesture must be held before it fires.
pub const HOLD_TIME: Duration = Duration::from_secs(5);

/// Follows a counter that an interrupt handler increments on each press.
#[derive(Debug, Default, Clone, Copy)]
pub struct Edges {
    seen: u32,
}

impl Edges {
    /// Reports whether the counter moved to `n` since the last call. A
    /// counter that wrapped around counts as changed.
    pub fn changed(&mut self, n: u32) -> bool {
        if n == self.seen {
            return false;
        }
        self.seen = n;
        true
    }
}

/// Detects a gesture that is held for longer than [`HOLD_TIME`]. It fires
/// once per hold, and again only after the gesture was released.
#[derive(Debug, Clone, Copy)]
pub struct Hold {
    held: bool,
    fired: bool,
    since: Instant,
}

impl Default for Hold {
    fn default() -> Self {
        Self::new()
    }
}

impl Hold {
    pub const fn new() -> Self {
        Self {
            held: false,
            fired: false,
            since: Instant::MIN,
        }
    }

    /// Records whether the gesture is active at `now` and reports whether
    /// it fires.
    pub fn update(&mut self, now: Instant, active: bool) -> bool {
        if !active {
            self.held = false;
            self.fired = false;
            return false;
        }
        if !self.held {
            self.held = true;
            self.since = now;
        }
        if self.fired || now.saturating_duration_since(self.since) <= HOLD_TIME {
            return false;
        }
        self.fired = true;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edges() {
        let mut e = Edges::default();
        for (n, changed) in [
            (0, false),
            (1, true),
            (1, false),
            (3, true),
            (u32::MAX, true),
            (0, true),
            (0, false),
        ] {
            assert_eq!(e.changed(n), changed, "changed({n})");
        }
    }

    #[test]
    fn hold() {
        let t0 = Instant::from_secs(1000);
        let at = |ms: u64| t0 + Duration::from_millis(ms);

        let mut h = Hold::new();
        for (ms, active, fire) in [
            (0, false, false),
            // A hold released before HOLD_TIME does not fire and restarts.
            (0, true, false),
            (4900, true, false),
            (4920, false, false),
            (5100, true, false),
            (10100, true, false),
            // More than HOLD_TIME fires, once.
            (10120, true, true),
            (10140, true, false),
            (20000, true, false),
            // After a release it fires again.
            (20020, false, false),
            (20040, true, false),
            (25060, true, true),
        ] {
            assert_eq!(h.update(at(ms), active), fire, "update({ms} ms, {active})");
        }
    }
}
