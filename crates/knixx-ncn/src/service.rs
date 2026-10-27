use core::fmt;

/// Service identifies an indication from the NCN5121 to the host controller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    /// A standard frame from the bus, see [`Parser::frame`]. The transceiver
    /// also echoes the frames it sends.
    LDataInd,
    LDataCon,
    UResetInd,
    UStateInd,
    UFrameStateInd,
    USystemStatInd,
}

const L_DATA_STANDARD_MASK: u8 = 0xd3;
const L_DATA_STANDARD: u8 = 0x90;
const L_DATA_CON_MASK: u8 = 0x7f;
const L_DATA_CON: u8 = 0x0b;
const L_DATA_CON_POSITIVE: u8 = 0x80;
const U_RESET_IND: u8 = 0x03;
const U_STATE_MASK: u8 = 0x07;
const U_STATE: u8 = 0x07;
const U_FRAME_STATE_MASK: u8 = 0x17;
const U_FRAME_STATE: u8 = 0x13;
const U_SYSTEM_STAT: u8 = 0x4b;

const NPCI_INDEX: usize = 5;
const LENGTH_MASK: u8 = 0x0f;
const FRAME_OVERHEAD: usize = 8;
const MAX_STD_FRAME: usize = FRAME_OVERHEAD + LENGTH_MASK as usize;

/// Parser splits the byte stream from the NCN5121 into KNX frames and service
/// indications. Bytes belonging to a standard frame are collected apart, so
/// data octets are not mistaken for services. Extended frames are not
/// recognised.
#[derive(Debug, Clone, Default)]
pub struct Parser {
    /// Octets of the current frame seen so far.
    n: usize,
    /// Total frame length, 0 until the NPCI octet is seen.
    length: usize,
    system_hdr: bool,
    frame: [u8; MAX_STD_FRAME],
    /// Length of the last completed frame.
    done: usize,
}

impl Parser {
    pub const fn new() -> Self {
        Parser {
            n: 0,
            length: 0,
            system_hdr: false,
            frame: [0; MAX_STD_FRAME],
            done: 0,
        }
    }

    /// Returns the frame of the last L_Data.ind, checksum included. It is
    /// valid until the next call of feed.
    pub fn frame(&self) -> &[u8] {
        &self.frame[..self.done]
    }

    /// Processes one received byte. It returns the service it completes,
    /// together with its status byte, or None.
    pub fn feed(&mut self, b: u8) -> Option<(Service, u8)> {
        if self.system_hdr {
            self.system_hdr = false;
            return Some((Service::USystemStatInd, b));
        }
        if self.n > 0 {
            self.frame[self.n] = b;
            self.n += 1;
            if self.n == NPCI_INDEX + 1 {
                self.length = FRAME_OVERHEAD + (b & LENGTH_MASK) as usize;
            }
            if self.length > 0 && self.n == self.length {
                self.done = self.length;
                self.n = 0;
                self.length = 0;
                return Some((Service::LDataInd, 0));
            }
            return None;
        }
        if b & L_DATA_STANDARD_MASK == L_DATA_STANDARD {
            self.frame[0] = b;
            self.n = 1;
            return None;
        }
        let s = match b {
            U_SYSTEM_STAT => {
                self.system_hdr = true;
                return None;
            }
            U_RESET_IND => Service::UResetInd,
            _ if b & L_DATA_CON_MASK == L_DATA_CON => Service::LDataCon,
            _ if b & U_STATE_MASK == U_STATE => Service::UStateInd,
            _ if b & U_FRAME_STATE_MASK == U_FRAME_STATE => Service::UFrameStateInd,
            _ => return None,
        };
        Some((s, b))
    }
}

/// Reports whether an L_Data.con status byte confirms delivery.
pub fn con_positive(b: u8) -> bool {
    b & L_DATA_CON_POSITIVE != 0
}

/// Renders a service returned by [`Parser::feed`].
pub fn describe(s: Service, b: u8) -> Describe {
    Describe(s, b)
}

/// The rendering of a service, see [`describe`].
#[derive(Debug, Clone, Copy)]
pub struct Describe(pub Service, pub u8);

impl fmt::Display for Describe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Describe(s, b) = *self;
        match s {
            Service::LDataInd => f.write_str("L_Data.ind"),
            Service::LDataCon if con_positive(b) => f.write_str("L_Data.con: positive"),
            Service::LDataCon => f.write_str("L_Data.con: negative"),
            Service::UResetInd => f.write_str("U_Reset.ind"),
            Service::UStateInd => write!(f, "U_State.ind: {}", State(b)),
            Service::UFrameStateInd => write!(f, "U_FrameState.ind: {}", FrameState(b)),
            Service::USystemStatInd => write!(f, "U_SystemStat.ind: {}", SystemStat(b)),
        }
    }
}

struct Flag {
    mask: u8,
    text: &'static str,
}

/// Writes the texts of the flags set in b, separated by commas, or none when
/// no flag is set.
fn write_flags(f: &mut fmt::Formatter<'_>, b: u8, flags: &[Flag], none: &str) -> fmt::Result {
    let mut first = true;
    for flag in flags.iter().filter(|flag| b & flag.mask != 0) {
        if !first {
            f.write_str(", ")?;
        }
        f.write_str(flag.text)?;
        first = false;
    }
    if first {
        f.write_str(none)?;
    }
    Ok(())
}

const STATE_FLAGS: [Flag; 5] = [
    Flag {
        mask: 0x80,
        text: "slave collision",
    },
    Flag {
        mask: 0x40,
        text: "receive error (corrupted bytes from host)",
    },
    Flag {
        mask: 0x20,
        text: "transmit error (sent 0, read 1)",
    },
    Flag {
        mask: 0x10,
        text: "protocol error (invalid command sequence from host)",
    },
    Flag {
        mask: 0x08,
        text: "thermal warning",
    },
];

/// Describes the flags of a U_State.ind byte.
pub fn decode_state(b: u8) -> State {
    State(b)
}

/// The flags of a U_State.ind byte, see [`decode_state`].
#[derive(Debug, Clone, Copy)]
pub struct State(pub u8);

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_flags(f, self.0, &STATE_FLAGS, "no errors")
    }
}

const FRAME_STATE_FLAGS: [Flag; 3] = [
    Flag {
        mask: 0x80,
        text: "parity or bit error",
    },
    Flag {
        mask: 0x40,
        text: "checksum or length error",
    },
    Flag {
        mask: 0x20,
        text: "timing error",
    },
];

/// Describes the flags of a U_FrameState.ind byte.
pub fn decode_frame_state(b: u8) -> FrameState {
    FrameState(b)
}

/// The flags of a U_FrameState.ind byte, see [`decode_frame_state`].
#[derive(Debug, Clone, Copy)]
pub struct FrameState(pub u8);

impl fmt::Display for FrameState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_flags(f, self.0, &FRAME_STATE_FLAGS, "ok")
    }
}

const SYSTEM_STAT_BITS: [Flag; 5] = [
    Flag {
        mask: 0x80,
        text: "V20V",
    },
    Flag {
        mask: 0x40,
        text: "VDD2",
    },
    Flag {
        mask: 0x20,
        text: "VBUS",
    },
    Flag {
        mask: 0x10,
        text: "VFILT",
    },
    Flag {
        mask: 0x08,
        text: "XTAL",
    },
];

const MODES: [&str; 4] = ["Power-Up", "Sync", "Stop", "Normal"];

/// Describes the second byte of a U_SystemStat.ind.
pub fn decode_system_stat(b: u8) -> SystemStat {
    SystemStat(b)
}

/// The second byte of a U_SystemStat.ind, see [`decode_system_stat`].
#[derive(Debug, Clone, Copy)]
pub struct SystemStat(pub u8);

impl fmt::Display for SystemStat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = self.0;
        for bit in &SYSTEM_STAT_BITS {
            f.write_str(bit.text)?;
            f.write_str(if b & bit.mask != 0 {
                " ok, "
            } else {
                " out of range, "
            })?;
        }
        f.write_str(if b & 0x04 != 0 {
            "thermal warning"
        } else {
            "no thermal warning"
        })?;
        f.write_str(", mode ")?;
        f.write_str(MODES[(b & 0x03) as usize])
    }
}
