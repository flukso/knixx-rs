//! Messages on a byte stream (USB): `0xA5 0x5A, len u16, message,
//! crc32(message)`, little-endian. A receiver resyncs on the magic after a
//! bad length or CRC; len is capped at the largest message, so that the
//! magic, which a .kfw holds here and there, fails at once when it is not a
//! frame's. A frame starts with 0xA5, which no one types: text commands
//! and frames share a stream.

use crate::ACK_EVERY;
use crate::msg::DATA_HEADER;

pub const MAGIC: [u8; 2] = [0xA5, 0x5A];
/// The largest message in a frame: data with ACK_EVERY bytes.
pub const MAX_MSG: usize = DATA_HEADER + ACK_EVERY as usize;
/// The bytes a frame adds to its message.
pub const OVERHEAD: usize = 2 + 2 + 4;

/// Frames msg into out and returns the frame's length.
pub fn encode(msg: &[u8], out: &mut [u8]) -> Option<usize> {
    if msg.len() > MAX_MSG || out.len() < msg.len() + OVERHEAD {
        return None;
    }
    out[..2].copy_from_slice(&MAGIC);
    out[2..4].copy_from_slice(&(msg.len() as u16).to_le_bytes());
    out[4..4 + msg.len()].copy_from_slice(msg);
    let crc = crate::crc32(msg).to_le_bytes();
    out[4 + msg.len()..OVERHEAD + msg.len()].copy_from_slice(&crc);
    Some(msg.len() + OVERHEAD)
}

/// What a byte brings the decoder.
#[derive(Debug, PartialEq, Eq)]
pub enum Fed {
    /// Nothing yet.
    None,
    /// A byte outside a frame: text, for the caller's console.
    Text(u8),
    /// A whole frame: its message is in [`Decoder::message`].
    Frame,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    Magic,
    Len(u8),
    Body,
}

/// Takes a stream byte by byte and finds the frames in it.
pub struct Decoder {
    state: State,
    len: usize,
    got: usize,
    buf: [u8; MAX_MSG + 4],
}

impl Default for Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder {
    pub const fn new() -> Decoder {
        Decoder {
            state: State::Idle,
            len: 0,
            got: 0,
            buf: [0; MAX_MSG + 4],
        }
    }

    /// The message of the last frame.
    pub fn message(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Whether a frame is partly in.
    pub fn in_frame(&self) -> bool {
        self.state != State::Idle
    }

    /// Drops a frame partly in: its rest is not coming.
    pub fn reset(&mut self) {
        self.state = State::Idle;
    }

    pub fn feed(&mut self, b: u8) -> Fed {
        match self.state {
            State::Idle => {
                if b == MAGIC[0] {
                    self.state = State::Magic;
                    return Fed::None;
                }
                Fed::Text(b)
            }
            State::Magic => {
                self.state = if b == MAGIC[1] {
                    State::Len(0)
                } else if b == MAGIC[0] {
                    State::Magic
                } else {
                    State::Idle
                };
                Fed::None
            }
            State::Len(0) => {
                self.len = b as usize;
                self.state = State::Len(1);
                Fed::None
            }
            State::Len(_) => {
                self.len |= (b as usize) << 8;
                if self.len == 0 || self.len > MAX_MSG {
                    self.state = State::Idle;
                } else {
                    self.got = 0;
                    self.state = State::Body;
                }
                Fed::None
            }
            State::Body => {
                self.buf[self.got] = b;
                self.got += 1;
                if self.got < self.len + 4 {
                    return Fed::None;
                }
                self.state = State::Idle;
                let crc = u32::from_le_bytes([
                    self.buf[self.len],
                    self.buf[self.len + 1],
                    self.buf[self.len + 2],
                    self.buf[self.len + 3],
                ]);
                if crate::crc32(&self.buf[..self.len]) == crc {
                    Fed::Frame
                } else {
                    Fed::None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::vec::Vec;

    fn framed(m: &[u8]) -> Vec<u8> {
        let mut out = std::vec![0u8; m.len() + OVERHEAD];
        let n = encode(m, &mut out).unwrap();
        out.truncate(n);
        out
    }

    fn run(d: &mut Decoder, s: &[u8]) -> (Vec<Vec<u8>>, Vec<u8>) {
        let (mut frames, mut text) = (Vec::new(), Vec::new());
        for &b in s {
            match d.feed(b) {
                Fed::Frame => frames.push(d.message().to_vec()),
                Fed::Text(t) => text.push(t),
                Fed::None => {}
            }
        }
        (frames, text)
    }

    #[test]
    fn frames_among_text() {
        let mut s = b"info\r".to_vec();
        s.extend(framed(&[0x10, 0, 16, 0, 0]));
        s.extend(b"boot\r");
        s.extend(framed(&[0x03]));
        let (frames, text) = run(&mut Decoder::new(), &s);
        assert_eq!(frames, [std::vec![0x10, 0, 16, 0, 0], std::vec![0x03]]);
        assert_eq!(text, b"info\rboot\r");
    }

    #[test]
    fn resyncs() {
        let good = framed(&[0x02, 0, 0, 0, 0, 0xA5, 0x5A, 0xA5]);
        let mut bad = good.clone();
        bad[6] ^= 1; // a damaged frame
        // Garbage, a false magic with a huge length, the damaged frame, then
        // the good one.
        let mut s = std::vec![0x00, 0xA5, 0x5A, 0xff, 0xff];
        s.extend(&bad);
        s.extend(&good);
        let (frames, _) = run(&mut Decoder::new(), &s);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0], &good[4..good.len() - 4]);
        // The largest message fits; one more byte does not frame.
        assert!(encode(&std::vec![0u8; MAX_MSG], &mut std::vec![0u8; MAX_MSG + OVERHEAD]).is_some());
        assert!(
            encode(
                &std::vec![0u8; MAX_MSG + 1],
                &mut std::vec![0u8; MAX_MSG + 1 + OVERHEAD]
            )
            .is_none()
        );
    }

    #[test]
    fn reset_drops_a_partial_frame() {
        let mut d = Decoder::new();
        let f = framed(&[0x03]);
        run(&mut d, &f[..5]);
        assert!(d.in_frame());
        d.reset();
        let (frames, _) = run(&mut d, &f);
        assert_eq!(frames.len(), 1);
    }
}
