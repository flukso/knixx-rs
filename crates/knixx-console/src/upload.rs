//! The upload of a file over the console, as `script put` starts it: the
//! host sends the file's bytes raw, in chunks of CHUNK bytes, and waits for
//! the board's `ok <offset>` after each, so that a link without
//! back-pressure, such as BLE NUS, never overruns the board; the board
//! checks the size and the CRC-32 at the end, and gives up after TIMEOUT
//! without a byte.
//!
//! The board's lines, each ending in "\n":
//!
//! ```text
//! ready                     after `script put`, waiting for the bytes
//! ok <offset>               after each chunk but the last
//! done <size>               the file is complete and its CRC matches
//! error: <reason>           the upload is over, nothing written
//! ```

use core::time::Duration;

/// The bytes the host sends before it waits for an ok.
pub const CHUNK: usize = 128;

/// How long the board waits for the next byte.
pub const TIMEOUT: Duration = Duration::from_secs(5);

/// The longest name of a file.
pub const MAX_NAME: usize = 24;

/// Reports whether name is a valid file name: 1 to MAX_NAME letters, digits,
/// `-` and `_`.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The CRC-32 of data, as zlib's crc32 and `crc32` of the shells compute it
/// (IEEE 802.3, reflected, polynomial 0xEDB88320).
pub fn crc32(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xEDB8_8320 & (crc & 1).wrapping_neg());
        }
    }
    !crc
}

/// A valid name, in a fixed buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Name {
    buf: [u8; MAX_NAME],
    len: u8,
}

impl Name {
    /// Returns name if it is valid.
    pub fn new(name: &str) -> Option<Name> {
        if !valid_name(name) {
            return None;
        }
        let mut buf = [0; MAX_NAME];
        buf[..name.len()].copy_from_slice(name.as_bytes());
        Some(Name {
            buf,
            len: name.len() as u8,
        })
    }

    pub fn as_str(&self) -> &str {
        // The bytes are checked ASCII.
        core::str::from_utf8(&self.buf[..self.len as usize]).unwrap_or("")
    }
}

/// What feeding an upload brings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    /// A chunk is in: the host may send the next one.
    Ack(usize),
    /// All the bytes are in, and their CRC matches.
    Done(usize),
    /// All the bytes are in, but their CRC does not match.
    BadCrc,
}

/// An upload in progress, into a buffer of the caller.
#[derive(Clone, Copy, Debug)]
pub struct Upload {
    pub name: Name,
    pub size: usize,
    crc: u32,
    got: usize,
    last: Duration,
}

impl Upload {
    /// Starts the upload of size bytes with the given CRC-32 at now. The
    /// caller has checked that size fits its buffer.
    pub fn new(name: Name, size: usize, crc: u32, now: Duration) -> Upload {
        Upload {
            name,
            size,
            crc,
            got: 0,
            last: now,
        }
    }

    /// The bytes received.
    pub fn received(&self) -> usize {
        self.got
    }

    /// Takes the bytes of data at now into buf, which holds the file, and
    /// reports the chunk they completed or the end. Bytes past the size, such
    /// as a stray line end, are dropped.
    pub fn feed(&mut self, data: &[u8], buf: &mut [u8], now: Duration) -> Option<Event> {
        self.last = now;
        let n = data.len().min(self.size - self.got);
        buf[self.got..self.got + n].copy_from_slice(&data[..n]);
        let before = self.got / CHUNK;
        self.got += n;
        if self.got == self.size {
            if n == 0 {
                return None;
            }
            return Some(if crc32(&buf[..self.size]) == self.crc {
                Event::Done(self.size)
            } else {
                Event::BadCrc
            });
        }
        (self.got / CHUNK > before).then_some(Event::Ack(self.got / CHUNK * CHUNK))
    }

    /// Reports whether the upload timed out at now.
    pub fn timed_out(&self, now: Duration) -> bool {
        now.saturating_sub(self.last) >= TIMEOUT
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    #[test]
    fn crc() {
        assert_eq!(crc32(b""), 0);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn names() {
        for (name, ok) in [
            ("light", true),
            ("A-b_9", true),
            ("x".repeat(MAX_NAME).as_str(), true),
            ("", false),
            ("x".repeat(MAX_NAME + 1).as_str(), false),
            ("a.b", false),
            ("a/b", false),
            ("a b", false),
        ] {
            assert_eq!(valid_name(name), ok, "{name:?}");
            assert_eq!(Name::new(name).is_some(), ok, "{name:?}");
        }
        assert_eq!(Name::new("light").unwrap().as_str(), "light");
    }

    fn t(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn chunks_and_done() {
        let file: Vec<u8> = (0..300u32).map(|i| i as u8).collect();
        let mut buf = [0u8; 512];
        let mut up = Upload::new(Name::new("s").unwrap(), file.len(), crc32(&file), t(0));
        let mut events = Vec::new();
        // As the host sends it: a chunk, then a wait for its ok.
        for piece in file.chunks(CHUNK) {
            events.extend(up.feed(piece, &mut buf, t(10)));
        }
        assert_eq!(events, [Event::Ack(128), Event::Ack(256), Event::Done(300)]);
        assert_eq!(&buf[..300], &file[..]);

        // In pieces that do not line up with the chunks, as a link may
        // split them: the end comes before the ok of the chunk it crosses.
        let mut buf2 = [0u8; 512];
        let mut up2 = Upload::new(Name::new("s").unwrap(), file.len(), crc32(&file), t(0));
        let events: Vec<_> = file
            .chunks(50)
            .filter_map(|p| up2.feed(p, &mut buf2, t(10)))
            .collect();
        assert_eq!(events, [Event::Ack(128), Event::Done(300)]);
        // A stray byte after the end changes nothing.
        assert_eq!(up.feed(b"\r", &mut buf, t(20)), None);
        assert_eq!(up.received(), 300);
    }

    #[test]
    fn exact_chunk_and_bad_crc() {
        let file = [7u8; CHUNK];
        let mut buf = [0u8; CHUNK];
        let mut up = Upload::new(Name::new("s").unwrap(), CHUNK, crc32(&file), t(0));
        // The last chunk ends it, without an ok.
        assert_eq!(up.feed(&file, &mut buf, t(1)), Some(Event::Done(CHUNK)));
        let mut up = Upload::new(Name::new("s").unwrap(), CHUNK, 0, t(0));
        assert_eq!(up.feed(&file, &mut buf, t(1)), Some(Event::BadCrc));
    }

    #[test]
    fn timeout() {
        let mut buf = [0u8; 8];
        let mut up = Upload::new(Name::new("s").unwrap(), 8, 0, t(0));
        assert!(!up.timed_out(t(4_999)));
        up.feed(b"ab", &mut buf, t(4_000));
        assert!(!up.timed_out(t(8_999)));
        assert!(up.timed_out(t(9_000)));
    }
}
