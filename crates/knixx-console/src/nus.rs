//! The transport logic of the console on the Nordic UART Service (NUS) of
//! BLE: the receive ring that the BLE host fills, the writer that splits
//! console output into notifications, and the idle timer that gives the
//! console back to USB when no central connects.

use core::time::Duration;

/// The capacity of `Ring`. It holds several writes of a central, each at
/// most 20 bytes with the default ATT MTU.
pub const RING_LEN: usize = 256;

/// A byte queue for the writes of a central.
pub struct Ring {
    buf: [u8; RING_LEN],
    /// head counts the bytes read, tail the bytes written; both wrap.
    head: u32,
    tail: u32,
}

impl Default for Ring {
    fn default() -> Self {
        Self::new()
    }
}

impl Ring {
    /// Returns an empty ring.
    pub const fn new() -> Self {
        Ring {
            buf: [0; RING_LEN],
            head: 0,
            tail: 0,
        }
    }

    /// Queues p and reports how much of it fit.
    pub fn put(&mut self, p: &[u8]) -> usize {
        let free = RING_LEN - self.buffered();
        let n = p.len().min(free);
        for &b in &p[..n] {
            self.buf[self.tail as usize % RING_LEN] = b;
            self.tail = self.tail.wrapping_add(1);
        }
        n
    }

    /// Returns the number of bytes queued.
    pub fn buffered(&self) -> usize {
        self.tail.wrapping_sub(self.head) as usize
    }

    /// Returns the oldest queued byte, or None when there is none.
    pub fn read_byte(&mut self) -> Option<u8> {
        if self.head == self.tail {
            return None;
        }
        let b = self.buf[self.head as usize % RING_LEN];
        self.head = self.head.wrapping_add(1);
        Some(b)
    }

    /// Drops the queued bytes.
    pub fn reset(&mut self) {
        self.head = self.tail;
    }
}

/// The payload of a notification with the default ATT MTU of 23.
pub const CHUNK: usize = 20;

/// Separates the attempts to queue a notification while the BLE stack has
/// no free transmit buffer.
pub const RETRY_DELAY: Duration = Duration::from_millis(5);
/// Bounds the wait for a transmit buffer per notification.
pub const RETRY_LIMIT: Duration = Duration::from_secs(1);

/// The BLE link that `write` sends notifications on.
#[allow(async_fn_in_trait)]
pub trait Link {
    type Error;
    /// Queues one notification.
    async fn send(&mut self, p: &[u8]) -> Result<(), Self::Error>;
    /// Reports whether a central is connected.
    fn connected(&self) -> bool;
    /// Waits for d.
    async fn sleep(&mut self, d: Duration);
}

/// Sends console output p on link as notifications of at most `CHUNK`
/// bytes, and always reports p as written: console output is best effort.
/// It never blocks for long: output is dropped while no central is
/// connected, and the rest of a write once a notification could not be
/// queued within `RETRY_LIMIT`.
pub async fn write<L: Link + ?Sized>(link: &mut L, p: &[u8]) -> usize {
    let mut rest = p;
    while !rest.is_empty() && link.connected() {
        let n = split(rest);
        if !send(link, &rest[..n]).await {
            break;
        }
        rest = &rest[n..];
    }
    p.len()
}

/// Returns the length of the next notification of p: at most `CHUNK`
/// bytes, ending before a UTF-8 sequence that would not fit whole, so that a
/// central that decodes each notification by itself sees whole characters.
pub fn split(p: &[u8]) -> usize {
    const UTF_MAX: usize = 4;
    if p.len() <= CHUNK {
        return p.len();
    }
    let mut n = CHUNK;
    // Back off over continuation bytes to the start of the sequence that
    // p[CHUNK] continues.
    while n > CHUNK - UTF_MAX && p[n] & 0xc0 == 0x80 {
        n -= 1;
    }
    if p[n] & 0xc0 == 0x80 {
        // Not UTF-8; split anywhere.
        return CHUNK;
    }
    n
}

/// Queues chunk, retrying while the transmit buffers are full.
async fn send<L: Link + ?Sized>(link: &mut L, chunk: &[u8]) -> bool {
    let mut waited = Duration::ZERO;
    loop {
        if link.send(chunk).await.is_ok() {
            return true;
        }
        if waited >= RETRY_LIMIT || !link.connected() {
            return false;
        }
        link.sleep(RETRY_DELAY).await;
        waited += RETRY_DELAY;
    }
}

/// How long NUS waits for a central before the console goes back to USB.
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// The idle timer: it runs while no central is connected. Instants are
/// durations since an arbitrary epoch, such as boot.
#[derive(Default)]
pub struct Timer {
    armed: bool,
    deadline: Duration,
}

impl Timer {
    /// Returns a disarmed timer.
    pub const fn new() -> Self {
        Timer {
            armed: false,
            deadline: Duration::ZERO,
        }
    }

    /// Starts the timer at now.
    pub fn arm(&mut self, now: Duration) {
        self.armed = true;
        self.deadline = now + IDLE_TIMEOUT;
    }

    /// Stops the timer.
    pub fn disarm(&mut self) {
        self.armed = false;
    }

    /// Reports whether the armed timer has run out at now.
    pub fn expired(&self, now: Duration) -> bool {
        self.armed && now >= self.deadline
    }

    /// Returns the time left at now, or 0 when the timer is not armed.
    pub fn left(&self, now: Duration) -> Duration {
        if !self.armed {
            return Duration::ZERO;
        }
        self.deadline.saturating_sub(now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::future::Future;
    use core::pin::pin;
    use core::task::{Context, Poll, Waker};
    use std::vec;
    use std::vec::Vec;

    /// Runs f, whose futures are all ready, to completion.
    fn block_on<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
                return v;
            }
        }
    }

    #[test]
    fn ring() {
        let mut r = Ring::new();
        assert_eq!(r.read_byte(), None, "empty ring");
        // Wrap around a few times.
        for round in 0..3 * RING_LEN / 100 {
            let input: Vec<u8> = (0..100).map(|i| (round + i) as u8).collect();
            assert_eq!(r.put(&input), input.len());
            assert_eq!(r.buffered(), input.len());
            for (i, &want) in input.iter().enumerate() {
                assert_eq!(r.read_byte(), Some(want), "round {round} byte {i}");
            }
        }
    }

    #[test]
    fn ring_full() {
        let mut r = Ring::new();
        assert_eq!(r.put(&[0; RING_LEN + 10]), RING_LEN);
        assert_eq!(r.put(&[1]), 0, "put into a full ring");
        r.read_byte();
        assert_eq!(r.put(&[1, 2]), 1);
        r.reset();
        assert_eq!(r.buffered(), 0, "buffered after reset");
    }

    /// Records the notifications and fails the first fails sends.
    #[derive(Default)]
    struct FakeLink {
        sent: Vec<Vec<u8>>,
        fails: u32,
        connected: bool,
        slept: Duration,
    }

    /// No tx buffers.
    struct Busy;

    impl Link for FakeLink {
        type Error = Busy;
        async fn send(&mut self, p: &[u8]) -> Result<(), Busy> {
            if self.fails > 0 {
                self.fails -= 1;
                return Err(Busy);
            }
            self.sent.push(p.to_vec());
            Ok(())
        }
        fn connected(&self) -> bool {
            self.connected
        }
        async fn sleep(&mut self, d: Duration) {
            self.slept += d;
        }
    }

    #[test]
    fn writer_chunks() {
        let mut f = FakeLink {
            connected: true,
            fails: 3,
            ..Default::default()
        };
        let p = b"0123456789".repeat(5);
        assert_eq!(block_on(write(&mut f, &p)), p.len());
        assert_eq!(f.sent.len(), 3);
        assert_eq!((f.sent[0].len(), f.sent[2].len()), (CHUNK, 10));
        assert_eq!(f.sent.concat(), p);
        assert_eq!(f.slept, 3 * RETRY_DELAY);
    }

    #[test]
    fn writer_gives_up() {
        let mut f = FakeLink {
            connected: true,
            fails: 1 << 30,
            ..Default::default()
        };
        assert_eq!(block_on(write(&mut f, &[0; 3 * CHUNK])), 3 * CHUNK);
        assert_eq!(f.sent.len(), 0);
        assert_eq!(f.slept, RETRY_LIMIT);
    }

    #[test]
    fn writer_disconnected() {
        let mut f = FakeLink::default();
        assert_eq!(block_on(write(&mut f, b"hello")), 5);
        assert!(f.sent.is_empty(), "sent {:?} while disconnected", f.sent);
    }

    #[test]
    fn timer() {
        // 2026-01-01 as seconds since the Unix epoch.
        let t0 = Duration::from_secs(1_767_225_600);
        let hour = Duration::from_secs(3600);
        let mut tm = Timer::new();
        assert!(
            !tm.expired(t0 + hour) && tm.left(t0) == Duration::ZERO,
            "an unarmed timer expired"
        );
        tm.arm(t0);
        assert!(
            !tm.expired(t0 + IDLE_TIMEOUT - Duration::from_millis(1)) && tm.expired(t0 + IDLE_TIMEOUT),
            "expiry not at IdleTimeout"
        );
        assert_eq!(tm.left(t0 + IDLE_TIMEOUT / 4), IDLE_TIMEOUT - IDLE_TIMEOUT / 4);
        assert_eq!(
            tm.left(t0 + IDLE_TIMEOUT + Duration::from_secs(1)),
            Duration::ZERO,
            "left after expiry"
        );
        tm.disarm();
        assert!(!tm.expired(t0 + hour), "a disarmed timer expired");
    }

    #[test]
    fn writer_keeps_utf8_whole() {
        // The first ° starts at bytes 18 to 21, around the end of a chunk.
        for pad in 0..4 {
            let mut f = FakeLink {
                connected: true,
                ..Default::default()
            };
            let mut p = vec![b'x'; 12 + pad];
            p.extend_from_slice("20.44 °C, 5 °C".as_bytes());
            block_on(write(&mut f, &p));
            for (i, c) in f.sent.iter().enumerate() {
                assert!(
                    core::str::from_utf8(c).is_ok() && c.len() <= CHUNK,
                    "pad {pad}: chunk {i} {c:?}"
                );
            }
            assert_eq!(f.sent.concat(), p, "pad {pad}");
        }
        // Bytes that are not UTF-8 are still split at CHUNK.
        assert_eq!(split(&[0x80; 30]), CHUNK);
    }
}
