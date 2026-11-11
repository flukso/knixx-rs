//! The receiving end of a transfer: the board in a push, bolt in a pull.
//! It takes data only at the offset it expects, acks every ACK_EVERY bytes
//! and at the end, asks for a rewind when data comes for another offset,
//! and checks the whole file's CRC; a [`Sink`] stores the bytes and checks
//! what it must (a signature, a config).
//!
//! When the last byte is in, [`Receiver::data`] answers [`Out::Complete`]:
//! the caller sends its ack ([`Out::msg`]) first, and only then calls
//! [`Receiver::finish`], which may take seconds (a firmware's signature
//! and read-back), so that the sender is not left waiting through it.

use knixx_image::Crc32;

use crate::msg::{Kind, Msg};
use crate::{ACK_EVERY, MAX_NAME, cap, code, valid_name};

/// Why a sink, or the receiver, refuses: an error code and a short reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fail {
    pub code: u8,
    pub reason: &'static str,
}

impl Fail {
    pub const fn new(code: u8, reason: &'static str) -> Fail {
        Fail { code, reason }
    }
}

/// Where the received bytes go.
pub trait Sink {
    /// Starts a file of size bytes; flags as put gave them.
    fn open(&mut self, kind: Kind, name: &str, size: u32, flags: u8) -> Result<(), Fail>;
    /// Takes the next bytes, in order.
    fn write(&mut self, data: &[u8]) -> Result<(), Fail>;
    /// Checks and keeps the whole file, its CRC checked already; returns
    /// the info that done carries.
    fn finish(&mut self) -> Result<u32, Fail>;
    /// Drops what was taken.
    fn abort(&mut self);
}

/// A put's offer: what the sender would send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Offer<'a> {
    pub kind: Kind,
    pub flags: u8,
    pub size: u32,
    pub crc: u32,
    pub name: &'a str,
}

impl<'a> Offer<'a> {
    /// The offer of a put message.
    pub fn of(m: &Msg<'a>) -> Option<Offer<'a>> {
        match *m {
            Msg::Put {
                kind,
                flags,
                size,
                crc,
                name,
            } => Some(Offer {
                kind,
                flags,
                size,
                crc,
                name,
            }),
            _ => None,
        }
    }
}

/// What the receiver answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Out {
    None,
    Ack(u32),
    Rewind(u32),
    /// The last byte is in: send `ack size`, then call
    /// [`Receiver::finish`].
    Complete(u32),
    Done {
        kind: Kind,
        crc: u32,
        info: u32,
    },
    Error {
        code: u8,
        crc: u32,
        reason: &'static str,
    },
}

impl Out {
    /// The message to send, if any.
    pub fn msg(&self) -> Option<Msg<'static>> {
        Some(match *self {
            Out::None => return None,
            Out::Ack(o) | Out::Complete(o) => Msg::Ack(o),
            Out::Rewind(o) => Msg::Rewind(o),
            Out::Done { kind, crc, info } => Msg::Done { kind, crc, info },
            Out::Error { code, crc, reason } => Msg::Error { code, crc, reason },
        })
    }

    fn error(f: Fail, crc: u32) -> Out {
        Out::Error {
            code: f.code,
            crc,
            reason: f.reason,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Idle,
    Receiving,
    Complete,
}

/// A rewind is asked for at most this often, in ms: a burst of data at the
/// wrong offset brings one.
pub const REWIND_GAP_MS: u64 = 200;

/// The receiving end.
pub struct Receiver {
    /// The kinds this place takes.
    kinds: &'static [Kind],
    /// After this long without data, in ms, a new put replaces the transfer.
    idle_ms: u64,
    state: State,
    kind: Kind,
    name: [u8; MAX_NAME],
    name_len: usize,
    size: u32,
    crc: u32,
    next: u32,
    running: Crc32,
    last_rewind: Option<u64>,
    last_activity: u64,
}

impl Receiver {
    pub const fn new(kinds: &'static [Kind], idle_ms: u64) -> Receiver {
        Receiver {
            kinds,
            idle_ms,
            state: State::Idle,
            kind: Kind::Firmware,
            name: [0; MAX_NAME],
            name_len: 0,
            size: 0,
            crc: 0,
            next: 0,
            running: Crc32::new(),
            last_rewind: None,
            last_activity: 0,
        }
    }

    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len]).unwrap_or("")
    }

    /// Whether a transfer is in progress.
    pub fn busy(&self) -> bool {
        self.state != State::Idle
    }

    /// The file in progress: kind, name, size, and the bytes in.
    pub fn progress(&self) -> Option<(Kind, &str, u32, u32)> {
        self.busy()
            .then(|| (self.kind, self.name(), self.size, self.next))
    }

    /// Whether the transfer in progress saw nothing since idle_ms at now.
    pub fn idle(&self, now: u64) -> bool {
        now.saturating_sub(self.last_activity) >= self.idle_ms
    }

    /// Takes a put: starts the file, resumes it (firmware, after a lost
    /// link), or refuses it.
    pub fn put(&mut self, sink: &mut impl Sink, o: &Offer, now: u64) -> Out {
        let Offer {
            kind,
            flags,
            size,
            crc,
            name,
        } = *o;
        if !self.kinds.contains(&kind) {
            return Out::error(Fail::new(code::KIND, "not taken here"), crc);
        }
        if !valid_name(kind, name) {
            return Out::error(Fail::new(code::NAME, "bad name"), crc);
        }
        if size == 0 || size > cap(kind) {
            return Out::error(Fail::new(code::SIZE, "bad size"), crc);
        }
        if self.state == State::Receiving
            && kind == Kind::Firmware
            && self.kind == kind
            && self.size == size
            && self.crc == crc
            && self.name() == name
        {
            self.last_activity = now;
            return Out::Ack(self.next);
        }
        if self.busy() {
            if self.state == State::Complete || !self.idle(now) {
                return Out::error(Fail::new(code::BUSY, "busy"), crc);
            }
            sink.abort();
            self.state = State::Idle;
        }
        if let Err(f) = sink.open(kind, name, size, flags) {
            return Out::error(f, crc);
        }
        self.start(kind, name, size, crc, now);
        Out::Ack(0)
    }

    fn start(&mut self, kind: Kind, name: &str, size: u32, crc: u32, now: u64) {
        self.state = State::Receiving;
        self.kind = kind;
        self.name_len = name.len().min(MAX_NAME);
        self.name[..self.name_len].copy_from_slice(&name.as_bytes()[..self.name_len]);
        self.size = size;
        self.crc = crc;
        self.next = 0;
        self.running = Crc32::new();
        self.last_rewind = None;
        self.last_activity = now;
    }

    /// Starts a file that this end asked for with get, as file announced
    /// it: a pull, on bolt's side. Answers `ack 0`.
    pub fn take(
        &mut self,
        sink: &mut impl Sink,
        kind: Kind,
        size: u32,
        crc: u32,
        name: &str,
        now: u64,
    ) -> Out {
        if size > cap(kind) {
            return Out::error(Fail::new(code::SIZE, "bad size"), crc);
        }
        if let Err(f) = sink.open(kind, name, size, 0) {
            return Out::error(f, crc);
        }
        self.start(kind, name, size, crc, now);
        if size == 0 {
            self.state = State::Complete;
            return Out::Complete(0);
        }
        Out::Ack(0)
    }

    /// Takes data at offset.
    pub fn data(&mut self, sink: &mut impl Sink, offset: u32, bytes: &[u8], now: u64) -> Out {
        if self.state != State::Receiving {
            return Out::None;
        }
        self.last_activity = now;
        if offset != self.next {
            if self
                .last_rewind
                .is_none_or(|t| now.saturating_sub(t) >= REWIND_GAP_MS)
            {
                self.last_rewind = Some(now);
                return Out::Rewind(self.next);
            }
            return Out::None;
        }
        let room = (self.size - self.next) as usize;
        let bytes = &bytes[..bytes.len().min(room)];
        if bytes.is_empty() {
            return Out::None;
        }
        self.running.update(bytes);
        if let Err(f) = sink.write(bytes) {
            sink.abort();
            self.state = State::Idle;
            return Out::error(f, self.crc);
        }
        self.last_rewind = None;
        let before = self.next / ACK_EVERY;
        self.next += bytes.len() as u32;
        if self.next == self.size {
            self.state = State::Complete;
            return Out::Complete(self.size);
        }
        if self.next / ACK_EVERY > before {
            return Out::Ack(self.next);
        }
        Out::None
    }

    /// Checks and keeps the complete file: the CRC, then the sink.
    pub fn finish(&mut self, sink: &mut impl Sink) -> Out {
        if self.state != State::Complete {
            return Out::None;
        }
        self.state = State::Idle;
        if self.running.finish() != self.crc {
            sink.abort();
            return Out::error(Fail::new(code::CRC, "CRC mismatch"), self.crc);
        }
        match sink.finish() {
            Ok(info) => Out::Done {
                kind: self.kind,
                crc: self.crc,
                info,
            },
            Err(f) => Out::error(f, self.crc),
        }
    }

    /// Takes an abort: drops the transfer in progress, unless it is
    /// complete already.
    pub fn abort(&mut self, sink: &mut impl Sink) {
        if self.state == State::Receiving {
            sink.abort();
            self.state = State::Idle;
        }
    }
}
