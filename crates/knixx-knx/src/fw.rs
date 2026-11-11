//! The firmware update's control plane on the bus: requests in
//! A_UserManufacturerInfo_Read (APCI 0x2C5), answers in
//! A_UserManufacturerInfo_Response (0x2C6), the two services KNX keeps for
//! the manufacturer.
//!
//! A scan goes to the broadcast 0/0/0, and each thunder answers its sender,
//! 1-1, from its individual address; the rest goes 1-1 to a board's
//! individual address. The telegram's data, as Telegram holds it (from the
//! APCI octet on):
//!
//! ```text
//! [0x05 | 0x06]  the APCI's low six bits: read or response
//! 0x6B           the protocol tag ('k'): other makers' answers differ
//! op             a request's or an answer's op
//! body           up to 12 bytes, little-endian
//! ```

use crate::tp1::{AddrType, Apci, MAX_PAYLOAD, Telegram};

/// The 10-bit APCIs, and the low six bits that the data's first octet
/// carries after the 4-bit UserMessage.
pub const APCI_READ: u16 = 0x2C5;
pub const APCI_RESPONSE: u16 = 0x2C6;
const READ_LOW: u8 = (APCI_READ & 0x3F) as u8;
const RESPONSE_LOW: u8 = (APCI_RESPONSE & 0x3F) as u8;

/// The protocol tag.
pub const TAG: u8 = 0x6B;

/// The longest body.
pub const MAX_BODY: usize = MAX_PAYLOAD - 3;

/// What a board does with an image, as it answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Idle = 0,
    /// An image is coming in.
    Receiving = 1,
    /// An image is staged, checked.
    Staged = 2,
    /// The last image failed.
    Failed = 3,
}

impl State {
    fn from_u8(b: u8) -> Option<State> {
        Some(match b {
            0 => State::Idle,
            1 => State::Receiving,
            2 => State::Staged,
            3 => State::Failed,
            _ => return None,
        })
    }
}

/// A request, from the proxy's central.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Request {
    /// Who is there: to the broadcast.
    Scan,
    /// What runs and what is staged.
    Status,
    /// Open the update window for the image of counter, whose body has the
    /// CRC-32 crc, for minutes.
    Open { counter: u32, crc: u32, minutes: u8 },
    /// Install the staged image of counter.
    Install { counter: u32 },
    /// Shut the update window, and drop a transfer in progress.
    Close,
    /// Open the update window for minutes, for scripts and the config:
    /// no image to check.
    Window { minutes: u8 },
}

/// An answer, from a board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Response {
    /// To scan: the running counter, the state, the board's BLE address.
    Info {
        counter: u32,
        state: State,
        ble: [u8; 6],
    },
    /// To status: the running and the staged counter (0: none), the state,
    /// the last error's code and the staged image's progress in percent.
    Status {
        running: u32,
        staged: u32,
        state: State,
        error: u8,
        progress: u8,
    },
    /// To open or window: the window is open; connect to the BLE address.
    Ready { ble: [u8; 6] },
    /// To open: this image runs already.
    Current,
    /// To open or install: refused, with a reason code.
    Refused { reason: u8 },
    /// To install: the board resets into its bootloader.
    Installing,
    /// To close.
    Closed,
}

/// Reasons of Refused.
pub mod reason {
    /// The image is older than, or the same as, the one that runs.
    pub const DOWNGRADE: u8 = 1;
    /// Install of an image that is not staged.
    pub const NOT_STAGED: u8 = 2;
    /// Asked too often.
    pub const RATE: u8 = 3;
    /// A transfer is in progress.
    pub const BUSY: u8 = 4;
}

const OP_SCAN: u8 = 0x01;
const OP_STATUS: u8 = 0x02;
const OP_OPEN: u8 = 0x03;
const OP_INSTALL: u8 = 0x04;
const OP_CLOSE: u8 = 0x05;
const OP_WINDOW: u8 = 0x06;

const OP_INFO: u8 = 0x81;
const OP_STATUS_R: u8 = 0x82;
const OP_READY: u8 = 0x83;
const OP_CURRENT: u8 = 0x84;
const OP_REFUSED: u8 = 0x85;
const OP_INSTALLING: u8 = 0x86;
const OP_CLOSED: u8 = 0x87;

/// A message's bytes: a telegram's data.
pub struct Data {
    buf: [u8; MAX_PAYLOAD],
    len: usize,
}

impl Data {
    fn new(low: u8, op: u8) -> Data {
        let mut d = Data {
            buf: [0; MAX_PAYLOAD],
            len: 3,
        };
        d.buf[0] = low;
        d.buf[1] = TAG;
        d.buf[2] = op;
        d
    }

    fn put(&mut self, b: &[u8]) -> &mut Data {
        self.buf[self.len..self.len + b.len()].copy_from_slice(b);
        self.len += b.len();
        self
    }

    pub fn as_slice(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// A message's bytes as given, up to MAX_PAYLOAD.
    pub fn from_slice(b: &[u8]) -> Data {
        let n = b.len().min(MAX_PAYLOAD);
        let mut d = Data {
            buf: [0; MAX_PAYLOAD],
            len: n,
        };
        d.buf[..n].copy_from_slice(&b[..n]);
        d
    }
}

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    let s = b.get(i..i + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn ble_at(b: &[u8], i: usize) -> Option<[u8; 6]> {
    b.get(i..i + 6)?.try_into().ok()
}

/// The body of data, if it is a message of ours with the APCI low bits low.
fn body(data: &[u8], low: u8) -> Option<(u8, &[u8])> {
    match data {
        [l, TAG, op, rest @ ..] if l & 0x3F == low => Some((*op, rest)),
        _ => None,
    }
}

impl Request {
    pub fn encode(&self) -> Data {
        let mut d;
        match *self {
            Request::Scan => d = Data::new(READ_LOW, OP_SCAN),
            Request::Status => d = Data::new(READ_LOW, OP_STATUS),
            Request::Open {
                counter,
                crc,
                minutes,
            } => {
                d = Data::new(READ_LOW, OP_OPEN);
                d.put(&counter.to_le_bytes())
                    .put(&crc.to_le_bytes())
                    .put(&[minutes]);
            }
            Request::Install { counter } => {
                d = Data::new(READ_LOW, OP_INSTALL);
                d.put(&counter.to_le_bytes());
            }
            Request::Close => d = Data::new(READ_LOW, OP_CLOSE),
            Request::Window { minutes } => {
                d = Data::new(READ_LOW, OP_WINDOW);
                d.put(&[minutes]);
            }
        }
        d
    }

    /// The request in a telegram's data, if it is one.
    pub fn decode(data: &[u8]) -> Option<Request> {
        let (op, b) = body(data, READ_LOW)?;
        Some(match op {
            OP_SCAN => Request::Scan,
            OP_STATUS => Request::Status,
            OP_OPEN => Request::Open {
                counter: u32_at(b, 0)?,
                crc: u32_at(b, 4)?,
                minutes: *b.get(8)?,
            },
            OP_INSTALL => Request::Install {
                counter: u32_at(b, 0)?,
            },
            OP_CLOSE => Request::Close,
            OP_WINDOW => Request::Window { minutes: *b.first()? },
            _ => return None,
        })
    }
}

impl Response {
    pub fn encode(&self) -> Data {
        let mut d;
        match *self {
            Response::Info { counter, state, ble } => {
                d = Data::new(RESPONSE_LOW, OP_INFO);
                d.put(&counter.to_le_bytes()).put(&[state as u8]).put(&ble);
            }
            Response::Status {
                running,
                staged,
                state,
                error,
                progress,
            } => {
                d = Data::new(RESPONSE_LOW, OP_STATUS_R);
                d.put(&running.to_le_bytes())
                    .put(&staged.to_le_bytes())
                    .put(&[state as u8, error, progress]);
            }
            Response::Ready { ble } => {
                d = Data::new(RESPONSE_LOW, OP_READY);
                d.put(&ble);
            }
            Response::Current => d = Data::new(RESPONSE_LOW, OP_CURRENT),
            Response::Refused { reason } => {
                d = Data::new(RESPONSE_LOW, OP_REFUSED);
                d.put(&[reason]);
            }
            Response::Installing => d = Data::new(RESPONSE_LOW, OP_INSTALLING),
            Response::Closed => d = Data::new(RESPONSE_LOW, OP_CLOSED),
        }
        d
    }

    /// The answer in a telegram's data, if it is one of ours.
    pub fn decode(data: &[u8]) -> Option<Response> {
        let (op, b) = body(data, RESPONSE_LOW)?;
        Some(match op {
            OP_INFO => Response::Info {
                counter: u32_at(b, 0)?,
                state: State::from_u8(*b.get(4)?)?,
                ble: ble_at(b, 5)?,
            },
            OP_STATUS_R => Response::Status {
                running: u32_at(b, 0)?,
                staged: u32_at(b, 4)?,
                state: State::from_u8(*b.get(8)?)?,
                error: *b.get(9)?,
                progress: *b.get(10)?,
            },
            OP_READY => Response::Ready { ble: ble_at(b, 0)? },
            OP_CURRENT => Response::Current,
            OP_REFUSED => Response::Refused { reason: *b.first()? },
            OP_INSTALLING => Response::Installing,
            OP_CLOSED => Response::Closed,
            _ => return None,
        })
    }
}

/// What a board knows when it answers a request.
#[derive(Clone, Copy, Debug)]
pub struct Board {
    /// The running image's counter.
    pub running: u32,
    /// The staged image's counter, if one is staged.
    pub staged: Option<u32>,
    pub state: State,
    /// The board's BLE address.
    pub ble: [u8; 6],
    /// The last error's code, 0 for none.
    pub error: u8,
    /// How much of an incoming image is in, in percent.
    pub progress: u8,
    /// A transfer is in progress.
    pub busy: bool,
}

/// What the board does about a request, besides answering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    None,
    /// Open the update window for minutes.
    Open(u8),
    /// Shut the update window.
    Close,
    /// Reset into the bootloader to install the staged image.
    Install,
}

/// The answer to req, and what to do: the board's side of the protocol.
pub fn answer(req: &Request, b: &Board) -> (Response, Act) {
    match *req {
        Request::Scan => (
            Response::Info {
                counter: b.running,
                state: b.state,
                ble: b.ble,
            },
            Act::None,
        ),
        Request::Status => (
            Response::Status {
                running: b.running,
                staged: b.staged.unwrap_or(0),
                state: b.state,
                error: b.error,
                progress: b.progress,
            },
            Act::None,
        ),
        Request::Open { counter, minutes, .. } => {
            if counter == b.running {
                (Response::Current, Act::None)
            } else if counter < b.running {
                (
                    Response::Refused {
                        reason: reason::DOWNGRADE,
                    },
                    Act::None,
                )
            } else if b.busy {
                (Response::Refused { reason: reason::BUSY }, Act::None)
            } else {
                (Response::Ready { ble: b.ble }, Act::Open(minutes.clamp(1, 60)))
            }
        }
        Request::Install { counter } => {
            if b.staged == Some(counter) && counter > b.running {
                (Response::Installing, Act::Install)
            } else {
                (
                    Response::Refused {
                        reason: reason::NOT_STAGED,
                    },
                    Act::None,
                )
            }
        }
        Request::Close => (Response::Closed, Act::Close),
        Request::Window { minutes } => {
            if b.busy {
                (Response::Refused { reason: reason::BUSY }, Act::None)
            } else {
                (Response::Ready { ble: b.ble }, Act::Open(minutes.clamp(1, 60)))
            }
        }
    }
}

/// Whether tg is a user message: the control plane's telegrams are.
pub fn is_user_message(tg: &Telegram) -> bool {
    tg.apci == Apci::UserMessage
}

/// Whether tg goes to the broadcast 0/0/0.
pub fn is_broadcast(tg: &Telegram) -> bool {
    tg.npci.addr_type == AddrType::Group && tg.dest == 0
}

/// Whether tg goes 1-1 to individual address addr.
pub fn is_for(tg: &Telegram, addr: u16) -> bool {
    tg.npci.addr_type == AddrType::Individual && tg.dest == addr
}

/// A telegram of data to dest: 1-1, or to the broadcast when dest is 0.
/// The engine sets the source, the hop count and the rest.
pub fn telegram(dest: u16, data: &Data) -> Telegram {
    let mut tg = Telegram {
        dest,
        apci: Apci::UserMessage,
        ..Telegram::default()
    };
    tg.npci.addr_type = if dest == 0 {
        AddrType::Group
    } else {
        AddrType::Individual
    };
    let _ = tg.set_data(data.as_slice());
    tg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_round_trip() {
        for r in [
            Request::Scan,
            Request::Status,
            Request::Open {
                counter: 1_793_818_601,
                crc: 0xdead_beef,
                minutes: 10,
            },
            Request::Install { counter: 7 },
            Request::Close,
            Request::Window { minutes: 15 },
        ] {
            let d = r.encode();
            assert!(d.as_slice().len() <= MAX_PAYLOAD);
            assert_eq!(d.as_slice()[..2], [0x05, TAG]);
            assert_eq!(Request::decode(d.as_slice()), Some(r));
            // Not an answer.
            assert_eq!(Response::decode(d.as_slice()), None);
        }
    }

    #[test]
    fn responses_round_trip() {
        let ble = [1, 2, 3, 4, 5, 0xc6];
        for r in [
            Response::Info {
                counter: 9,
                state: State::Staged,
                ble,
            },
            Response::Status {
                running: 1,
                staged: 2,
                state: State::Receiving,
                error: 5,
                progress: 99,
            },
            Response::Ready { ble },
            Response::Current,
            Response::Refused {
                reason: reason::DOWNGRADE,
            },
            Response::Installing,
            Response::Closed,
        ] {
            let d = r.encode();
            assert!(d.as_slice().len() <= MAX_PAYLOAD, "{r:?}");
            assert_eq!(d.as_slice()[..2], [0x06, TAG]);
            assert_eq!(Response::decode(d.as_slice()), Some(r));
        }
    }

    #[test]
    fn others_are_ignored() {
        // Another maker's answer: its manufacturer code instead of our tag.
        assert_eq!(Response::decode(&[0x06, 0x00, 0x83, 1, 2]), None);
        // Too short, an unknown op, a bad state.
        assert_eq!(Response::decode(&[0x06, TAG]), None);
        assert_eq!(Request::decode(&[0x05, TAG, 0x7f]), None);
        assert_eq!(
            Response::decode(&[0x06, TAG, OP_INFO, 0, 0, 0, 0, 9, 1, 2, 3, 4, 5, 6]),
            None
        );
        // A truncated open.
        assert_eq!(Request::decode(&[0x05, TAG, OP_OPEN, 1, 2]), None);
    }

    #[test]
    fn answers() {
        let b = Board {
            running: 100,
            staged: None,
            state: State::Idle,
            ble: [9; 6],
            error: 0,
            progress: 0,
            busy: false,
        };
        let open = |counter| Request::Open {
            counter,
            crc: 0,
            minutes: 90,
        };
        assert_eq!(
            answer(&open(101), &b),
            (Response::Ready { ble: [9; 6] }, Act::Open(60))
        );
        assert_eq!(answer(&open(100), &b).0, Response::Current);
        assert_eq!(
            answer(&open(99), &b).0,
            Response::Refused {
                reason: reason::DOWNGRADE
            }
        );
        let busy = Board { busy: true, ..b };
        assert_eq!(
            answer(&open(101), &busy).0,
            Response::Refused { reason: reason::BUSY }
        );
        // Install only the staged, newer image.
        assert_eq!(answer(&Request::Install { counter: 101 }, &b).1, Act::None);
        let staged = Board {
            staged: Some(101),
            state: State::Staged,
            ..b
        };
        assert_eq!(
            answer(&Request::Install { counter: 101 }, &staged),
            (Response::Installing, Act::Install)
        );
        assert_eq!(answer(&Request::Install { counter: 102 }, &staged).1, Act::None);
        assert_eq!(answer(&Request::Close, &b), (Response::Closed, Act::Close));
        // A window for scripts and the config: no image to check.
        assert_eq!(
            answer(&Request::Window { minutes: 0 }, &b),
            (Response::Ready { ble: [9; 6] }, Act::Open(1))
        );
        assert_eq!(
            answer(&Request::Window { minutes: 5 }, &busy).0,
            Response::Refused { reason: reason::BUSY }
        );
        assert!(matches!(
            answer(&Request::Status, &staged).0,
            Response::Status { staged: 101, .. }
        ));
    }

    #[test]
    fn telegrams() {
        let t = telegram(0, &Request::Scan.encode());
        assert!(is_broadcast(&t) && is_user_message(&t));
        let t = telegram(0x1105, &Request::Status.encode());
        assert!(is_for(&t, 0x1105) && !is_broadcast(&t));
        // Through a frame and back, as the bus carries it.
        let mut frame = [0u8; crate::tp1::MAX_FRAME];
        let f = t.pack_to(&mut frame).unwrap().to_vec();
        let mut back = Telegram::default();
        back.unpack(&f).unwrap();
        assert_eq!(back.apci, Apci::UserMessage);
        assert_eq!(Request::decode(back.data()), Some(Request::Status));
    }
}
