//! The messages, little-endian throughout:
//!
//! ```text
//! put    0x01 kind flags size:u32 crc:u32 name_len name   host offers a file
//! data   0x02 offset:u32 bytes                            the sender
//! abort  0x03                                             either end
//! get    0x04 kind name_len name                          host asks for a file
//! file   0x05 kind size:u32 crc:u32 name_len name         board, to a get
//! ack    0x10 offset:u32                                  the receiver
//! rewind 0x11 offset:u32                                  the receiver
//! done   0x20 kind crc:u32 info:u32                       the receiver
//! error  0x21 code crc:u32 reason                         the receiver, or the board to a request
//! ```

use crate::MAX_NAME;

/// What a file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    /// A signed .kfw.
    Firmware = 0,
    /// A script's bytecode, by name.
    Script = 1,
    /// The whole knixx.ini.
    Config = 2,
    /// The scripts on the board: one `<name> <size> <hash>` line each.
    ScriptList = 3,
}

impl Kind {
    pub fn from_u8(b: u8) -> Option<Kind> {
        Some(match b {
            0 => Kind::Firmware,
            1 => Kind::Script,
            2 => Kind::Config,
            3 => Kind::ScriptList,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Firmware => "firmware",
            Kind::Script => "script",
            Kind::Config => "config",
            Kind::ScriptList => "script list",
        }
    }
}

/// put's flag: take a config for another individual address, or an
/// image whose counter is not above the running one (USB recovery).
pub const FORCE: u8 = 0x01;

pub const PUT: u8 = 0x01;
pub const DATA: u8 = 0x02;
pub const ABORT: u8 = 0x03;
pub const GET: u8 = 0x04;
pub const FILE: u8 = 0x05;
pub const ACK: u8 = 0x10;
pub const REWIND: u8 = 0x11;
pub const DONE: u8 = 0x20;
pub const ERROR: u8 = 0x21;

/// The header of data: its type and offset.
pub const DATA_HEADER: usize = 5;

/// The longest reason of an error.
pub const MAX_REASON: usize = 32;

/// A message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg<'a> {
    Put {
        kind: Kind,
        flags: u8,
        size: u32,
        crc: u32,
        name: &'a str,
    },
    Get {
        kind: Kind,
        name: &'a str,
    },
    File {
        kind: Kind,
        size: u32,
        crc: u32,
        name: &'a str,
    },
    Data {
        offset: u32,
        bytes: &'a [u8],
    },
    Abort,
    Ack(u32),
    Rewind(u32),
    Done {
        kind: Kind,
        crc: u32,
        info: u32,
    },
    Error {
        code: u8,
        crc: u32,
        reason: &'a str,
    },
}

/// The buffer is too small for the message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TooSmall;

struct W<'b> {
    b: &'b mut [u8],
    n: usize,
}

impl W<'_> {
    fn put(&mut self, d: &[u8]) -> Result<(), TooSmall> {
        let end = self.n + d.len();
        self.b.get_mut(self.n..end).ok_or(TooSmall)?.copy_from_slice(d);
        self.n = end;
        Ok(())
    }

    fn name(&mut self, s: &str) -> Result<(), TooSmall> {
        let s = &s.as_bytes()[..s.len().min(MAX_NAME)];
        self.put(&[s.len() as u8])?;
        self.put(s)
    }
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    let s = b.get(i..i + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn name_at(b: &[u8], i: usize) -> Option<(&str, usize)> {
    let n = *b.get(i)? as usize;
    if n > MAX_NAME {
        return None;
    }
    let s = core::str::from_utf8(b.get(i + 1..i + 1 + n)?).ok()?;
    Some((s, i + 1 + n))
}

impl<'a> Msg<'a> {
    /// Encodes the message into out and returns its length.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, TooSmall> {
        let mut w = W { b: out, n: 0 };
        match *self {
            Msg::Put {
                kind,
                flags,
                size,
                crc,
                name,
            } => {
                w.put(&[PUT, kind as u8, flags])?;
                w.put(&size.to_le_bytes())?;
                w.put(&crc.to_le_bytes())?;
                w.name(name)?;
            }
            Msg::Get { kind, name } => {
                w.put(&[GET, kind as u8])?;
                w.name(name)?;
            }
            Msg::File {
                kind,
                size,
                crc,
                name,
            } => {
                w.put(&[FILE, kind as u8])?;
                w.put(&size.to_le_bytes())?;
                w.put(&crc.to_le_bytes())?;
                w.name(name)?;
            }
            Msg::Data { offset, bytes } => {
                w.put(&[DATA])?;
                w.put(&offset.to_le_bytes())?;
                w.put(bytes)?;
            }
            Msg::Abort => w.put(&[ABORT])?,
            Msg::Ack(o) => {
                w.put(&[ACK])?;
                w.put(&o.to_le_bytes())?;
            }
            Msg::Rewind(o) => {
                w.put(&[REWIND])?;
                w.put(&o.to_le_bytes())?;
            }
            Msg::Done { kind, crc, info } => {
                w.put(&[DONE, kind as u8])?;
                w.put(&crc.to_le_bytes())?;
                w.put(&info.to_le_bytes())?;
            }
            Msg::Error { code, crc, reason } => {
                w.put(&[ERROR, code])?;
                w.put(&crc.to_le_bytes())?;
                let r = reason.as_bytes();
                let mut n = r.len().min(MAX_REASON);
                // Cut on a character boundary.
                while !reason.is_char_boundary(n) {
                    n -= 1;
                }
                w.put(&r[..n])?;
            }
        }
        Ok(w.n)
    }

    /// Decodes a message, or None for one that is malformed.
    pub fn decode(b: &'a [u8]) -> Option<Msg<'a>> {
        let kind_at = |i| b.get(i).copied().and_then(Kind::from_u8);
        Some(match *b.first()? {
            PUT => {
                let (name, end) = name_at(b, 11)?;
                if end != b.len() {
                    return None;
                }
                Msg::Put {
                    kind: kind_at(1)?,
                    flags: *b.get(2)?,
                    size: u32_at(b, 3)?,
                    crc: u32_at(b, 7)?,
                    name,
                }
            }
            GET => {
                let (name, end) = name_at(b, 2)?;
                if end != b.len() {
                    return None;
                }
                Msg::Get {
                    kind: kind_at(1)?,
                    name,
                }
            }
            FILE => {
                let (name, end) = name_at(b, 10)?;
                if end != b.len() {
                    return None;
                }
                Msg::File {
                    kind: kind_at(1)?,
                    size: u32_at(b, 2)?,
                    crc: u32_at(b, 6)?,
                    name,
                }
            }
            DATA => Msg::Data {
                offset: u32_at(b, 1)?,
                bytes: b.get(DATA_HEADER..)?,
            },
            ABORT if b.len() == 1 => Msg::Abort,
            ACK if b.len() == 5 => Msg::Ack(u32_at(b, 1)?),
            REWIND if b.len() == 5 => Msg::Rewind(u32_at(b, 1)?),
            DONE if b.len() == 10 => Msg::Done {
                kind: kind_at(1)?,
                crc: u32_at(b, 2)?,
                info: u32_at(b, 6)?,
            },
            ERROR => Msg::Error {
                code: *b.get(1)?,
                crc: u32_at(b, 2)?,
                reason: core::str::from_utf8(b.get(6..)?).ok()?,
            },
            _ => return None,
        })
    }
}

/// The longest message other than data: put or file with the longest name,
/// error with the longest reason.
pub const MAX_CONTROL: usize = 11 + 1 + MAX_NAME + 6 + MAX_REASON;

#[cfg(test)]
mod tests {
    use super::*;

    fn round(m: Msg) {
        let mut buf = [0u8; 300];
        let n = m.encode(&mut buf).unwrap();
        assert_eq!(Msg::decode(&buf[..n]), Some(m), "{m:?}");
        // Every prefix is malformed, but data and error carry tails.
        if !matches!(m, Msg::Data { .. } | Msg::Error { .. }) {
            for k in 0..n {
                assert_eq!(Msg::decode(&buf[..k]), None, "{m:?} cut at {k}");
            }
        }
    }

    #[test]
    fn round_trips() {
        round(Msg::Put {
            kind: Kind::Script,
            flags: FORCE,
            size: 1234,
            crc: 0xdead_beef,
            name: "light_1",
        });
        round(Msg::Put {
            kind: Kind::Firmware,
            flags: 0,
            size: 800_000,
            crc: 1,
            name: "",
        });
        round(Msg::Get {
            kind: Kind::Config,
            name: "",
        });
        round(Msg::File {
            kind: Kind::ScriptList,
            size: 0,
            crc: 0,
            name: "",
        });
        round(Msg::Data {
            offset: 4096,
            bytes: &[1, 2, 3],
        });
        round(Msg::Abort);
        round(Msg::Ack(8192));
        round(Msg::Rewind(4096));
        round(Msg::Done {
            kind: Kind::Firmware,
            crc: 7,
            info: 1_793_818_601,
        });
        round(Msg::Error {
            code: crate::code::BUSY,
            crc: 9,
            reason: "busy",
        });
    }

    #[test]
    fn sizes() {
        let name = "x".repeat(MAX_NAME);
        let mut buf = [0u8; MAX_CONTROL];
        let put = Msg::Put {
            kind: Kind::Script,
            flags: 0,
            size: 1,
            crc: 1,
            name: &name,
        };
        // A put with the longest name fits ctrl's 48 bytes.
        assert!(put.encode(&mut buf).unwrap() <= 48);
        let long = "y".repeat(100);
        let n = Msg::Error {
            code: 1,
            crc: 0,
            reason: &long,
        }
        .encode(&mut buf)
        .unwrap();
        assert_eq!(n, 6 + MAX_REASON);
        assert_eq!(put.encode(&mut [0u8; 8]), Err(TooSmall));
    }

    #[test]
    fn malformed() {
        assert_eq!(Msg::decode(&[]), None);
        assert_eq!(Msg::decode(&[0x7f]), None);
        // An unknown kind, a name longer than allowed, trailing bytes.
        assert_eq!(Msg::decode(&[GET, 9, 0]), None);
        let mut b = std::vec![GET, 1, 30];
        b.extend([b'a'; 30]);
        assert_eq!(Msg::decode(&b), None);
        assert_eq!(Msg::decode(&[GET, 2, 0, 0]), None);
        assert_eq!(Msg::decode(&[ACK, 1, 2, 3]), None);
    }
}
