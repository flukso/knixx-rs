//! KNX TP1 standard frames.

use core::fmt;

const CTRL: usize = 0;
const SRC1: usize = 1;
const DEST1: usize = 3;
const NPCI: usize = 5;
const TPCI: usize = 6;
const APCI: usize = 7;

const MAX_PAYLOAD_LENGTH: usize = 15;

/// MAX_PAYLOAD is the longest data of a standard frame, and MAX_FRAME the
/// longest frame, checksum included.
pub const MAX_PAYLOAD: usize = MAX_PAYLOAD_LENGTH;
pub const MAX_FRAME: usize = MAX_PAYLOAD_LENGTH + 8;

const CTRL_DEFAULT: u8 = 0x90;
const PRIORITY_OFFSET: u8 = 2;
const REPEAT_OFFSET: u8 = 5;
const ADDR_TYPE_OFFSET: u8 = 7;
const ROUTING_OFFSET: u8 = 4;
const ROUTING_MASK: u8 = 0x07;
const TRANSPORT_OFFSET: u8 = 6;
const SEQUENCE_OFFSET: u8 = 2;
const SEQUENCE_MASK: u8 = 0x0f;
const APCI1_NEG_OFFSET: u8 = 2;
const APCI2_OFFSET: u8 = 6;

/// Error is an error packing or unpacking a frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Payload,
    Transport,
    Short,
    Checksum,
    Standard,
    Length,
}

impl Error {
    pub const fn as_str(self) -> &'static str {
        match self {
            Error::Payload => "tp1: max payload length exceeded",
            Error::Transport => "tp1: control data transports not yet supported",
            Error::Short => "tp1: frame too short",
            Error::Checksum => "tp1: checksum mismatch",
            Error::Standard => "tp1: not a standard frame",
            Error::Length => "tp1: length does not match the frame",
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl core::error::Error for Error {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Priority {
    #[default]
    System,
    High,
    Alarm,
    Low,
}

impl Priority {
    pub const ALL: [Priority; 4] = [Priority::System, Priority::High, Priority::Alarm, Priority::Low];

    /// from_bits returns the priority in the low two bits of b.
    pub const fn from_bits(b: u8) -> Priority {
        Priority::ALL[(b & 0x03) as usize]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Priority::System => "system",
            Priority::High => "high",
            Priority::Alarm => "alarm",
            Priority::Low => "low",
        }
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ctrl {
    pub repeat: bool,
    pub priority: Priority,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum AddrType {
    #[default]
    Individual,
    Group,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Npci {
    pub addr_type: AddrType,
    pub routing: u8,
    pub length: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Transport {
    #[default]
    UnnumberedDataPacket,
    NumberedDataPacket,
    UnnumberedControlData,
    NumberedControlData,
}

impl Transport {
    /// from_bits returns the transport in the low two bits of b.
    pub const fn from_bits(b: u8) -> Transport {
        match b & 0x03 {
            0 => Transport::UnnumberedDataPacket,
            1 => Transport::NumberedDataPacket,
            2 => Transport::UnnumberedControlData,
            _ => Transport::NumberedControlData,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tpci {
    pub transport: Transport,
    pub sequence: u8,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum Apci {
    #[default]
    GroupValueRead,
    GroupValueResponse,
    GroupValueWrite,
    IndividualAddrWrite,
    IndividualAddrRequest,
    IndividualAddrResponse,
    AdcRead,
    AdcResponse,
    MemoryRead,
    MemoryResponse,
    MemoryWrite,
    UserMessage,
    MaskVersionRead,
    MaskVersionResponse,
    Restart,
    Escape,
}

impl Apci {
    pub const ALL: [Apci; 16] = [
        Apci::GroupValueRead,
        Apci::GroupValueResponse,
        Apci::GroupValueWrite,
        Apci::IndividualAddrWrite,
        Apci::IndividualAddrRequest,
        Apci::IndividualAddrResponse,
        Apci::AdcRead,
        Apci::AdcResponse,
        Apci::MemoryRead,
        Apci::MemoryResponse,
        Apci::MemoryWrite,
        Apci::UserMessage,
        Apci::MaskVersionRead,
        Apci::MaskVersionResponse,
        Apci::Restart,
        Apci::Escape,
    ];

    /// from_bits returns the APCI in the low four bits of b.
    pub const fn from_bits(b: u8) -> Apci {
        Apci::ALL[(b & 0x0f) as usize]
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Apci::GroupValueRead => "GroupValueRead",
            Apci::GroupValueResponse => "GroupValueResponse",
            Apci::GroupValueWrite => "GroupValueWrite",
            Apci::IndividualAddrWrite => "IndividualAddrWrite",
            Apci::IndividualAddrRequest => "IndividualAddrRequest",
            Apci::IndividualAddrResponse => "IndividualAddrResponse",
            Apci::AdcRead => "AdcRead",
            Apci::AdcResponse => "AdcResponse",
            Apci::MemoryRead => "MemoryRead",
            Apci::MemoryResponse => "MemoryResponse",
            Apci::MemoryWrite => "MemoryWrite",
            Apci::UserMessage => "UserMessage",
            Apci::MaskVersionRead => "MaskVersionRead",
            Apci::MaskVersionResponse => "MaskVersionResponse",
            Apci::Restart => "Restart",
            Apci::Escape => "Escape",
        }
    }
}

impl fmt::Display for Apci {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Telegram is a standard frame. Its data is the first npci.length octets
/// of the data array; set_data keeps the two in step.
#[derive(Clone, Copy, Debug, Default)]
pub struct Telegram {
    pub ctrl: Ctrl,
    pub src: u16,
    pub dest: u16,
    pub npci: Npci,
    pub tpci: Tpci,
    pub apci: Apci,
    pub data: [u8; MAX_PAYLOAD],
}

impl PartialEq for Telegram {
    fn eq(&self, other: &Self) -> bool {
        self.ctrl == other.ctrl
            && self.src == other.src
            && self.dest == other.dest
            && self.npci == other.npci
            && self.tpci == other.tpci
            && self.apci == other.apci
            && self.data() == other.data()
    }
}

impl Eq for Telegram {}

impl Telegram {
    /// data returns the payload, from the APCI octet on. A length over
    /// MAX_PAYLOAD gives the whole data array.
    pub fn data(&self) -> &[u8] {
        &self.data[..(self.npci.length as usize).min(MAX_PAYLOAD)]
    }

    /// set_data copies data into the telegram and sets npci.length to its
    /// length.
    pub fn set_data(&mut self, data: &[u8]) -> Result<(), Error> {
        if data.len() > MAX_PAYLOAD_LENGTH {
            return Err(Error::Payload);
        }
        self.data = [0; MAX_PAYLOAD];
        self.data[..data.len()].copy_from_slice(data);
        self.npci.length = data.len() as u8;
        Ok(())
    }

    /// pack_to encodes tg as a standard frame, checksum included, into dst
    /// and returns the part of dst it fills.
    pub fn pack_to<'a>(&self, dst: &'a mut [u8; MAX_FRAME]) -> Result<&'a [u8], Error> {
        let length = self.npci.length as usize;
        if length > MAX_PAYLOAD_LENGTH {
            return Err(Error::Payload);
        }
        if self.tpci.transport as u8 > Transport::NumberedDataPacket as u8 {
            return Err(Error::Transport);
        }
        let buffer = &mut dst[..length + 8];
        buffer.fill(0);
        buffer[CTRL] = CTRL_DEFAULT
            | (repeat2uint8(self.ctrl.repeat) << REPEAT_OFFSET)
            | ((self.ctrl.priority as u8) << PRIORITY_OFFSET);
        buffer[SRC1..DEST1].copy_from_slice(&self.src.to_be_bytes());
        buffer[DEST1..NPCI].copy_from_slice(&self.dest.to_be_bytes());
        buffer[NPCI] = ((self.npci.addr_type as u8) << ADDR_TYPE_OFFSET)
            | ((self.npci.routing & ROUTING_MASK) << ROUTING_OFFSET)
            | length as u8;
        buffer[TPCI] = ((self.tpci.transport as u8) << TRANSPORT_OFFSET)
            | ((self.tpci.sequence & SEQUENCE_MASK) << SEQUENCE_OFFSET)
            | ((self.apci as u8) >> APCI1_NEG_OFFSET);
        buffer[APCI..APCI + length].copy_from_slice(&self.data[..length]);
        buffer[APCI] |= (self.apci as u8) << APCI2_OFFSET;
        let last = buffer.len() - 1;
        buffer[last] = checksum(&buffer[..last]);
        Ok(buffer)
    }

    /// unpack decodes a standard frame as built by pack_to, checksum
    /// included, into tg. The data gets the octets from the APCI octet on,
    /// with the APCI bits of its first octet cleared; npci.length is set to
    /// its length. tg is left untouched on an error.
    pub fn unpack(&mut self, buffer: &[u8]) -> Result<(), Error> {
        if buffer.len() < APCI + 1 {
            return Err(Error::Short);
        }
        let last = buffer.len() - 1;
        if checksum(&buffer[..last]) != buffer[last] {
            return Err(Error::Checksum);
        }
        let flags = (1 << REPEAT_OFFSET) | (3 << PRIORITY_OFFSET);
        if buffer[CTRL] & !flags != CTRL_DEFAULT & !(1 << REPEAT_OFFSET) {
            return Err(Error::Standard);
        }
        let length = (buffer[NPCI] & MAX_PAYLOAD_LENGTH as u8) as usize;
        if buffer.len() != length + 8 {
            return Err(Error::Length);
        }
        let transport = Transport::from_bits(buffer[TPCI] >> TRANSPORT_OFFSET);
        if transport as u8 > Transport::NumberedDataPacket as u8 {
            return Err(Error::Transport);
        }

        self.ctrl = Ctrl {
            repeat: buffer[CTRL] & (1 << REPEAT_OFFSET) == 0,
            priority: Priority::from_bits(buffer[CTRL] >> PRIORITY_OFFSET),
        };
        self.src = u16::from_be_bytes([buffer[SRC1], buffer[SRC1 + 1]]);
        self.dest = u16::from_be_bytes([buffer[DEST1], buffer[DEST1 + 1]]);
        self.npci = Npci {
            addr_type: if buffer[NPCI] >> ADDR_TYPE_OFFSET == 0 {
                AddrType::Individual
            } else {
                AddrType::Group
            },
            routing: buffer[NPCI] >> ROUTING_OFFSET & ROUTING_MASK,
            length: length as u8,
        };
        self.tpci = Tpci {
            transport,
            sequence: buffer[TPCI] >> SEQUENCE_OFFSET & SEQUENCE_MASK,
        };
        let mut apci = (buffer[TPCI] & 0x03) << APCI1_NEG_OFFSET | buffer[APCI] >> APCI2_OFFSET;
        self.data = [0; MAX_PAYLOAD];
        if length == 0 {
            // The APCI octet is the checksum: pack_to drops the low APCI bits.
            apci &= !0x03;
        } else {
            self.data[..length].copy_from_slice(&buffer[APCI..APCI + length]);
            self.data[0] &= !(0x03 << APCI2_OFFSET);
        }
        self.apci = Apci::from_bits(apci);
        Ok(())
    }
}

fn repeat2uint8(r: bool) -> u8 {
    if r { 0 } else { 1 }
}

fn checksum(buffer: &[u8]) -> u8 {
    buffer.iter().fold(0xff, |c, x| c ^ x)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::format;
    use std::string::{String, ToString};
    use std::vec::Vec;

    /// group_write is the telegram of a DPT 1.001 on from 7.8.1 to 0/0/1, and
    /// GROUP_WRITE_FRAME its frame.
    fn group_write() -> Telegram {
        let mut tg = Telegram {
            ctrl: Ctrl {
                repeat: false,
                priority: Priority::Low,
            },
            src: 0x7801,
            dest: 0x0001,
            npci: Npci {
                addr_type: AddrType::Group,
                routing: 0,
                length: 1,
            },
            tpci: Tpci {
                transport: Transport::UnnumberedDataPacket,
                sequence: 0,
            },
            apci: Apci::GroupValueWrite,
            data: [0; MAX_PAYLOAD],
        };
        tg.data[0] = 0x01;
        tg
    }

    const GROUP_WRITE_FRAME: [u8; 9] = [0xbc, 0x78, 0x01, 0x00, 0x01, 0x81, 0x00, 0x81, 0x3b];

    fn pack(tg: &Telegram) -> Result<Vec<u8>, Error> {
        let mut buf = [0; MAX_FRAME];
        tg.pack_to(&mut buf).map(|b| b.to_vec())
    }

    #[test]
    fn pack_frame() {
        let buf = pack(&group_write()).unwrap();
        assert_eq!(buf, GROUP_WRITE_FRAME);
    }

    #[test]
    fn unpack_frame() {
        let mut tg = Telegram::default();
        tg.unpack(&GROUP_WRITE_FRAME).unwrap();
        assert_eq!(tg, group_write());
    }

    /// with returns group_write changed by f.
    fn with(f: impl FnOnce(&mut Telegram)) -> Telegram {
        let mut tg = group_write();
        f(&mut tg);
        tg
    }

    #[test]
    fn pack_unpack() {
        let mut tests: Vec<(String, Telegram)> = Vec::from([
            ("group write 1 bit".to_string(), group_write()),
            (
                "group write off".to_string(),
                with(|tg| tg.set_data(&[0x00]).unwrap()),
            ),
            (
                "group write 6 bit".to_string(),
                with(|tg| tg.set_data(&[0x3f]).unwrap()),
            ),
            (
                "group write 2 octets".to_string(),
                // DPT 9.001 21.0 °C
                with(|tg| tg.set_data(&[0x00, 0x0c, 0x1a]).unwrap()),
            ),
            (
                "max payload".to_string(),
                with(|tg| {
                    tg.set_data(&[0x00, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 0xff])
                        .unwrap()
                }),
            ),
            (
                "group read".to_string(),
                with(|tg| {
                    tg.apci = Apci::GroupValueRead;
                    tg.set_data(&[0x00]).unwrap();
                }),
            ),
            ("repeated".to_string(), with(|tg| tg.ctrl.repeat = true)),
            (
                "individual destination, routing 6".to_string(),
                with(|tg| {
                    tg.npci.addr_type = AddrType::Individual;
                    tg.npci.routing = 6;
                    tg.dest = 0x1105;
                }),
            ),
            (
                "numbered data, sequence 9".to_string(),
                with(|tg| {
                    tg.tpci.transport = Transport::NumberedDataPacket;
                    tg.tpci.sequence = 9;
                }),
            ),
            (
                "extreme addresses".to_string(),
                with(|tg| {
                    tg.src = 0xffff;
                    tg.dest = 0x0000;
                }),
            ),
        ]);
        for p in Priority::ALL {
            tests.push((format!("priority {}", p as u8), with(|tg| tg.ctrl.priority = p)));
        }
        for a in Apci::ALL {
            tests.push((
                format!("apci {}", (b'a' + a as u8) as char),
                with(|tg| {
                    tg.apci = a;
                    tg.set_data(&[0x2a, 0xc3]).unwrap();
                }),
            ));
        }

        for (name, want) in tests {
            let buf = pack(&want).unwrap_or_else(|e| panic!("{name}: pack: {e}"));
            let mut got = Telegram::default();
            got.unpack(&buf)
                .unwrap_or_else(|e| panic!("{name}: unpack({buf:02x?}): {e}"));
            assert_eq!(got, want, "{name}: unpack(pack())");
            // Packing the result again gives the same frame.
            assert_eq!(pack(&got), Ok(buf), "{name}: pack(unpack())");
        }
    }

    /// unpack_no_data covers a frame without APCI octet: pack_to drops the
    /// low APCI bits, which unpack returns cleared.
    #[test]
    fn unpack_no_data() {
        let mut tg = with(|tg| {
            tg.apci = Apci::MemoryRead;
            tg.set_data(&[]).unwrap();
        });
        let buf = pack(&tg).unwrap();
        assert_eq!(buf.len(), 8, "pack = {buf:02x?}, want 8 octets");
        let mut got = Telegram::default();
        got.unpack(&buf).unwrap();
        tg.apci = Apci::from_bits(Apci::MemoryRead as u8 & !0x03);
        assert_eq!(got, tg);
    }

    #[test]
    fn unpack_copies_data() {
        let mut buf = GROUP_WRITE_FRAME;
        let mut tg = Telegram::default();
        tg.unpack(&buf).unwrap();
        buf[APCI] = 0x80;
        assert_eq!(tg.data()[0], 0x01, "data follows the frame buffer: {buf:02x?}");
    }

    #[test]
    fn unpack_errors() {
        // frame returns GROUP_WRITE_FRAME changed by f, with its checksum fixed.
        let frame = |f: fn(&mut [u8])| {
            let mut buf = GROUP_WRITE_FRAME.to_vec();
            f(&mut buf);
            let last = buf.len() - 1;
            buf[last] = checksum(&buf[..last]);
            buf
        };
        let tests: [(&str, Vec<u8>); 8] = [
            ("empty", Vec::new()),
            ("too short", GROUP_WRITE_FRAME[..7].to_vec()),
            ("checksum", {
                let mut buf = GROUP_WRITE_FRAME.to_vec();
                buf[8] ^= 0x01;
                buf
            }),
            ("extended frame", frame(|b| b[CTRL] &= !0x80)),
            ("poll frame", frame(|b| b[CTRL] |= 0x40)),
            ("length too long", frame(|b| b[NPCI] = b[NPCI] & !0x0f | 2)),
            ("length too short", frame(|b| b[NPCI] &= !0x0f)),
            (
                "control transport",
                frame(|b| b[TPCI] = (Transport::UnnumberedControlData as u8) << TRANSPORT_OFFSET),
            ),
        ];
        for (name, buf) in tests {
            let mut tg = group_write();
            assert!(
                tg.unpack(&buf).is_err(),
                "{name}: unpack({buf:02x?}) succeeded: {tg:?}"
            );
            assert_eq!(
                tg,
                group_write(),
                "{name}: unpack({buf:02x?}) changed the telegram"
            );
        }
    }

    #[test]
    fn pack_errors() {
        let mut tg = group_write();
        tg.npci.length = 16;
        assert_eq!(pack(&tg), Err(Error::Payload));
        assert_eq!(tg.set_data(&[0; 16]), Err(Error::Payload));
        let tg = with(|tg| tg.tpci.transport = Transport::NumberedControlData);
        assert_eq!(pack(&tg), Err(Error::Transport));
        assert_eq!(
            Error::Transport.to_string(),
            "tp1: control data transports not yet supported"
        );
    }

    #[test]
    fn names() {
        for (got, want) in [
            (Priority::System.to_string(), "system"),
            (Priority::Low.to_string(), "low"),
            (Apci::GroupValueRead.to_string(), "GroupValueRead"),
            (Apci::GroupValueWrite.to_string(), "GroupValueWrite"),
            (Apci::Escape.to_string(), "Escape"),
        ] {
            assert_eq!(got, want);
        }
    }
}
