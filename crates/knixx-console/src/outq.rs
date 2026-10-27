//! Queues console output, so that writers never wait for a terminal: a slow,
//! absent or unplugged one only loses output. One sender task per port
//! drains the queue to the port and is the only one that waits.

use crate::lineedit::Sink;

/// The capacity of the firmware's queues: a few dozen lines of sniffer
/// trace.
pub const LEN: usize = 2048;

/// A byte queue of N bytes for writers and one sender. N is a power of two
/// of at least 64 bytes.
pub struct Queue<const N: usize = LEN> {
    buf: [u8; N],
    /// head counts the bytes sent, tail the bytes queued; both wrap.
    head: u32,
    tail: u32,
    /// dropped counts the bytes dropped since the last `dropped`, lost all
    /// the bytes dropped.
    dropped: u32,
    lost: u32,
}

impl<const N: usize> Default for Queue<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> Queue<N> {
    const VALID: () = assert!(
        N.is_power_of_two() && N >= 64,
        "N must be a power of two of at least 64"
    );

    /// Returns an empty queue.
    pub const fn new() -> Self {
        let () = Self::VALID;
        Queue {
            buf: [0; N],
            head: 0,
            tail: 0,
            dropped: 0,
            lost: 0,
        }
    }

    /// Returns the room left.
    pub fn free(&self) -> usize {
        N - self.buffered()
    }

    /// Returns the number of bytes queued.
    pub fn buffered(&self) -> usize {
        self.tail.wrapping_sub(self.head) as usize
    }

    /// Queues p whole, or drops it whole when it does not fit, so that
    /// output is not cut inside a write. It always reports p as written.
    pub fn write(&mut self, p: &[u8]) -> usize {
        if p.len() > self.free() {
            self.drop(p.len() as u32);
            return p.len();
        }
        self.put(p);
        p.len()
    }

    /// Queues p with each "\n" turned into "\r\n", for serial terminals,
    /// whole or not at all like `write`.
    pub fn write_crlf(&mut self, p: &[u8]) -> usize {
        let n = p.len() + p.iter().filter(|&&b| b == b'\n').count();
        if n > self.free() {
            self.drop(n as u32);
            return p.len();
        }
        for (i, part) in p.split(|&b| b == b'\n').enumerate() {
            if i > 0 {
                self.put(b"\r\n");
            }
            self.put(part);
        }
        p.len()
    }

    fn drop(&mut self, n: u32) {
        self.dropped = self.dropped.wrapping_add(n);
        self.lost = self.lost.wrapping_add(n);
    }

    fn put(&mut self, p: &[u8]) {
        for &b in p {
            self.buf[self.tail as usize % N] = b;
            self.tail = self.tail.wrapping_add(1);
        }
    }

    /// Returns the oldest queued bytes that are contiguous in the buffer,
    /// without removing them. It is empty when nothing is queued.
    pub fn peek(&self) -> &[u8] {
        let start = self.head as usize % N;
        let end = (start + self.buffered()).min(N);
        &self.buf[start..end]
    }

    /// Removes the n oldest queued bytes, after the sender sent them.
    pub fn discard(&mut self, n: usize) {
        self.head = self.head.wrapping_add(n.min(self.buffered()) as u32);
    }

    /// Drops the queued bytes without counting them.
    pub fn reset(&mut self) {
        self.head = self.tail;
    }

    /// Returns the number of bytes dropped since its last call and clears
    /// it.
    pub fn dropped(&mut self) -> u32 {
        core::mem::take(&mut self.dropped)
    }

    /// Returns the number of bytes dropped since the queue was made. Unlike
    /// `dropped` it does not clear the count, so that it can be read for a
    /// report while the sender uses `dropped`.
    pub fn lost(&self) -> u32 {
        self.lost
    }

    /// Returns the next part to send, as `peek` does. Once the queue ran
    /// empty after output was dropped, it first queues a note with the
    /// number of bytes lost, so that the gap is visible on the terminal. A
    /// sender calls it, sends the part and then calls `discard` with its
    /// length; it is empty when there is nothing to send.
    pub fn pending(&mut self) -> &[u8] {
        if self.buffered() == 0 {
            let n = self.dropped();
            if n != 0 {
                self.note(n);
            }
        }
        self.peek()
    }

    /// Queues "\n[n bytes of output dropped]\n".
    fn note(&mut self, n: u32) {
        let mut b = [0u8; 40];
        let mut len = 0;
        for &c in b"\n[" {
            b[len] = c;
            len += 1;
        }
        let mut digits = [0u8; 10];
        let mut d = 0;
        let mut v = n;
        loop {
            digits[d] = b'0' + (v % 10) as u8;
            d += 1;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        for i in (0..d).rev() {
            b[len] = digits[i];
            len += 1;
        }
        for &c in b" bytes of output dropped]\n" {
            b[len] = c;
            len += 1;
        }
        self.write_crlf(&b[..len]);
    }

    /// Writes the queued bytes to w, one contiguous part per call of
    /// `w.write`, until the queue is empty, with the note of `pending`. It
    /// reports whether anything was sent.
    pub fn send<W: Sink + ?Sized>(&mut self, w: &mut W) -> bool {
        let mut sent = false;
        loop {
            let p = self.pending();
            if p.is_empty() {
                return sent;
            }
            let n = p.len();
            w.write(p);
            self.discard(n);
            sent = true;
        }
    }
}

/// The raw output of a line editor.
impl<const N: usize> Sink for Queue<N> {
    fn write(&mut self, b: &[u8]) {
        Queue::write(self, b);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::String;
    use std::vec::Vec;

    const L: usize = LEN;

    /// Records the writes it gets.
    #[derive(Default)]
    struct ChunkWriter {
        buf: Vec<u8>,
        writes: usize,
    }

    impl Sink for ChunkWriter {
        fn write(&mut self, b: &[u8]) {
            self.writes += 1;
            self.buf.extend_from_slice(b);
        }
    }

    impl ChunkWriter {
        fn string(&self) -> String {
            String::from_utf8(self.buf.clone()).unwrap()
        }
    }

    #[test]
    fn write_send() {
        let mut q: Queue = Queue::new();
        q.write(b"hello ");
        q.write_crlf(b"a\nb\n");
        assert_eq!((q.buffered(), q.free()), (12, L - 12));
        let mut w = ChunkWriter::default();
        assert!(q.send(&mut w), "Send reported nothing sent");
        assert_eq!(w.string(), "hello a\r\nb\r\n");
        assert_eq!(q.buffered(), 0);
        assert!(!q.send(&mut w));
    }

    #[test]
    fn drop_whole() {
        let mut q: Queue = Queue::new();
        let fill = [b'x'; L - 4];
        q.write(&fill);
        // Neither fits whole; neither is cut.
        assert_eq!(q.write(b"12345"), 5);
        q.write_crlf(b"ab\n\n"); // 6 bytes with the CRs
        assert_eq!(q.buffered(), L - 4, "Buffered after drops");
        q.write(b"1234"); // fits exactly
        assert_eq!(q.free(), 0);

        let mut w = ChunkWriter::default();
        q.send(&mut w);
        let mut want = fill.to_vec();
        want.extend_from_slice(b"1234\r\n[11 bytes of output dropped]\r\n");
        assert_eq!(w.buf, want);
        assert_eq!(q.lost(), 11, "Lost after the note");
        assert_eq!(q.dropped(), 0, "dropped count not cleared by the note");
    }

    /// Sends across the end of the buffer: in two contiguous parts, in
    /// order.
    #[test]
    fn wrap_around() {
        let mut q: Queue = Queue::new();
        let mut w = ChunkWriter::default();
        q.write(&[b'-'; L - 3]);
        q.send(&mut w);
        let mut w = ChunkWriter::default();

        q.write(b"abcdefgh");
        assert_eq!(q.peek(), b"abc");
        q.send(&mut w);
        assert_eq!((w.string().as_str(), w.writes), ("abcdefgh", 2));
    }

    /// Writes to the queue while it is being sent, as a writer task would
    /// while the sender waits for the port.
    #[test]
    fn write_while_sending() {
        let mut q: Queue = Queue::new();
        q.write(b"one ");
        let mut more: &[&[u8]] = &[b"two ", b"three"];
        let mut out = Vec::new();
        loop {
            let p = q.pending();
            if p.is_empty() {
                break;
            }
            let n = p.len();
            out.extend_from_slice(p);
            if let Some((m, rest)) = more.split_first() {
                q.write(m);
                more = rest;
            }
            q.discard(n);
        }
        assert_eq!(out, b"one two three");
    }

    #[test]
    fn reset() {
        let mut q: Queue = Queue::new();
        q.write(b"stale");
        q.reset();
        let mut w = ChunkWriter::default();
        assert!(!q.send(&mut w) && w.buf.is_empty(), "sent after Reset");
    }

    #[test]
    fn note_large_count() {
        let mut q: Queue<64> = Queue::new();
        q.drop(u32::MAX);
        let mut w = ChunkWriter::default();
        q.send(&mut w);
        assert_eq!(w.string(), "\r\n[4294967295 bytes of output dropped]\r\n");
    }
}
