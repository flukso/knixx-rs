//! The sending end of a transfer: bolt in a push, the board in a pull. It
//! keeps up to a window of bytes in flight, goes on from each ack, goes
//! back on a rewind, and to the last ack when nothing came for a while;
//! once everything is acked it waits for done, without counting stalls.
//! The caller does the I/O: it sends the chunks [`Sender::next_piece`] gives,
//! feeds the receiver's messages to [`Sender::on`], and calls
//! [`Sender::silence`] when nothing came for its stall time.

use crate::msg::{MAX_REASON, Msg};

/// Why a transfer failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SendError {
    /// The receiver refused: its code and reason.
    Refused { code: u8, reason: Reason },
    /// No progress after max_stalls silences.
    Stalled { at: u32 },
    /// The receiver answered out of step: an ack past the end.
    Protocol,
}

/// An error's reason, as the receiver gave it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Reason {
    buf: [u8; MAX_REASON],
    len: usize,
}

impl Reason {
    fn new(s: &str) -> Reason {
        let mut r = Reason {
            buf: [0; MAX_REASON],
            len: s.len().min(MAX_REASON),
        };
        r.buf[..r.len].copy_from_slice(&s.as_bytes()[..r.len]);
        r
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("?")
    }
}

impl core::fmt::Debug for Reason {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            SendError::Refused { code, reason } => write!(f, "{} ({code:#04x})", reason.as_str()),
            SendError::Stalled { at } => write!(f, "no progress at {at}"),
            SendError::Protocol => f.write_str("the receiver answered out of step"),
        }
    }
}

/// What a message of the receiver brought.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    None,
    /// The receiver has this much.
    Progress(u32),
    /// The receiver checked and kept the file: done's info.
    Done(u32),
}

/// The sending end.
pub struct Sender {
    size: u32,
    crc: u32,
    window: u32,
    piece: u32,
    sent: u32,
    acked: u32,
    stalls: u32,
    max_stalls: u32,
}

impl Sender {
    /// A transfer of size bytes with CRC crc, from offset from (what the
    /// receiver's first ack says), with window bytes in flight in pieces
    /// of piece bytes.
    pub fn new(size: u32, crc: u32, from: u32, window: u32, piece: u32, max_stalls: u32) -> Sender {
        let from = from.min(size);
        Sender {
            size,
            crc,
            window: window.max(piece),
            piece: piece.max(1),
            sent: from,
            acked: from,
            stalls: 0,
            max_stalls,
        }
    }

    pub fn acked(&self) -> u32 {
        self.acked
    }

    /// Whether all bytes are acked: what is left is to wait for done.
    pub fn all_acked(&self) -> bool {
        self.acked == self.size
    }

    /// The next piece to send, offset and length, while the window has
    /// room.
    pub fn next_piece(&mut self) -> Option<(u32, u32)> {
        if self.sent >= self.size || self.sent - self.acked >= self.window {
            return None;
        }
        let len = self.piece.min(self.size - self.sent);
        let at = self.sent;
        self.sent += len;
        Some((at, len))
    }

    /// Takes a message of the receiver.
    pub fn on(&mut self, m: &Msg) -> Result<Event, SendError> {
        match *m {
            Msg::Ack(off) if off > self.size => Err(SendError::Protocol),
            Msg::Ack(off) if off > self.acked => {
                self.acked = off;
                self.sent = self.sent.max(off);
                self.stalls = 0;
                Ok(Event::Progress(off))
            }
            Msg::Rewind(off) if off <= self.size => {
                self.acked = off;
                self.sent = off;
                Ok(Event::Progress(off))
            }
            Msg::Done { crc, info, .. } if crc == self.crc => Ok(Event::Done(info)),
            Msg::Error { code, crc, reason } if crc == self.crc || crc == 0 => Err(SendError::Refused {
                code,
                reason: Reason::new(reason),
            }),
            // An old ack, a stale done or error of another file.
            _ => Ok(Event::None),
        }
    }

    /// Nothing came for the stall time: go back to the last ack. Once all
    /// is acked, this only waits for done, and does not count.
    pub fn silence(&mut self) -> Result<(), SendError> {
        if self.all_acked() {
            return Ok(());
        }
        self.stalls += 1;
        if self.stalls > self.max_stalls {
            return Err(SendError::Stalled { at: self.acked });
        }
        self.sent = self.acked;
        Ok(())
    }
}
