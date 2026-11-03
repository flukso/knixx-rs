//! The values of the knixx GATT service, by which a central (the web app)
//! sends group telegrams through the board and sees those on the bus. Each
//! fits the 20 bytes of a notification at the default ATT MTU.
//!
//! - send, written by the central: seq, op, group (big endian), priority,
//!   then the data from the APCI octet on, as [`Telegram::data`] holds it
//!   (a read has none, or one 0 octet).
//! - result, notified: seq and a [`Status`], once the board knows the
//!   outcome of the send with that seq.
//! - group, notified: flags (bit 7 the board's own telegram, bits 4-5 the op,
//!   bits 0-1 the priority), source and destination (big endian), then the
//!   data, for each group value write, read and response on the bus.

use crate::tp1::{AddrType, Apci, Ctrl, MAX_PAYLOAD, Npci, Priority, Telegram, Tpci, Transport};

/// The most bytes of a value.
pub const MAX_VALUE: usize = 20;

/// The bytes of a send before its data, and of a group notification.
const SEND_HEAD: usize = 5;
const GROUP_HEAD: usize = 5;

/// A group value service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Op {
    Write = 0,
    Read = 1,
    Response = 2,
}

impl Op {
    pub fn from_u8(b: u8) -> Option<Op> {
        match b {
            0 => Some(Op::Write),
            1 => Some(Op::Read),
            2 => Some(Op::Response),
            _ => None,
        }
    }

    pub fn apci(self) -> Apci {
        match self {
            Op::Write => Apci::GroupValueWrite,
            Op::Read => Apci::GroupValueRead,
            Op::Response => Apci::GroupValueResponse,
        }
    }

    pub fn from_apci(a: Apci) -> Option<Op> {
        match a {
            Apci::GroupValueWrite => Some(Op::Write),
            Apci::GroupValueRead => Some(Op::Read),
            Apci::GroupValueResponse => Some(Op::Response),
            _ => None,
        }
    }
}

/// The outcome of a send.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Status {
    /// The bus acknowledged it: a positive L_Data.con.
    Confirmed = 0,
    /// A negative L_Data.con: no device acknowledged it.
    Negative = 1,
    /// No L_Data.con in time: is the bus there?
    NoCon = 2,
    /// The board does not send it: not a group value service, or bad data.
    Refused = 3,
    /// The board's command queue is full: send again later.
    Busy = 4,
    /// The telegram could not be handed to the transceiver.
    Failed = 5,
}

impl Status {
    pub fn from_u8(b: u8) -> Option<Status> {
        Some(match b {
            0 => Status::Confirmed,
            1 => Status::Negative,
            2 => Status::NoCon,
            3 => Status::Refused,
            4 => Status::Busy,
            5 => Status::Failed,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Status::Confirmed => "confirmed",
            Status::Negative => "negative confirmation",
            Status::NoCon => "no confirmation",
            Status::Refused => "refused",
            Status::Busy => "busy",
            Status::Failed => "failed",
        }
    }
}

/// Why a value does not decode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Too short, or longer than MAX_VALUE.
    Length,
    /// An unknown op or status.
    Op,
    /// Data on a read, or none on a write or a response.
    Data,
}

/// A send: what the central asks the board to put on the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Send {
    pub seq: u8,
    pub op: Op,
    pub group: u16,
    pub priority: Priority,
    data: [u8; MAX_PAYLOAD],
    len: u8,
}

impl Send {
    /// A send of data, from the APCI octet on; a read takes no data.
    pub fn new(seq: u8, op: Op, group: u16, priority: Priority, data: &[u8]) -> Result<Send, Error> {
        let data = if op == Op::Read && data.is_empty() {
            &[0][..]
        } else {
            data
        };
        if data.is_empty() || data.len() > MAX_PAYLOAD || (op == Op::Read && data != [0]) {
            return Err(Error::Data);
        }
        let mut d = [0; MAX_PAYLOAD];
        d[..data.len()].copy_from_slice(data);
        Ok(Send {
            seq,
            op,
            group,
            priority,
            data: d,
            len: data.len() as u8,
        })
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }

    pub fn decode(v: &[u8]) -> Result<Send, Error> {
        if v.len() < SEND_HEAD - 1 || v.len() > MAX_VALUE {
            return Err(Error::Length);
        }
        let op = Op::from_u8(v[1]).ok_or(Error::Op)?;
        let group = u16::from_be_bytes([v[2], v[3]]);
        let priority = Priority::from_bits(v.get(4).copied().unwrap_or(Priority::Low as u8));
        Send::new(v[0], op, group, priority, v.get(SEND_HEAD..).unwrap_or(&[]))
    }

    pub fn encode(&self, out: &mut [u8; MAX_VALUE]) -> usize {
        let g = self.group.to_be_bytes();
        out[..SEND_HEAD].copy_from_slice(&[self.seq, self.op as u8, g[0], g[1], self.priority as u8]);
        let data = if self.op == Op::Read { &[][..] } else { self.data() };
        out[SEND_HEAD..SEND_HEAD + data.len()].copy_from_slice(data);
        SEND_HEAD + data.len()
    }

    /// The telegram to send, from no source address: the board sends it from
    /// its own.
    pub fn telegram(&self) -> Telegram {
        let mut tg = Telegram {
            ctrl: Ctrl {
                repeat: false,
                priority: self.priority,
            },
            src: 0,
            dest: self.group,
            npci: Npci {
                addr_type: AddrType::Group,
                routing: 6,
                length: 0,
            },
            tpci: Tpci {
                transport: Transport::UnnumberedDataPacket,
                sequence: 0,
            },
            apci: self.op.apci(),
            data: [0; MAX_PAYLOAD],
        };
        // new checked the length.
        let _ = tg.set_data(self.data());
        tg
    }
}

/// Encodes a result.
pub fn encode_result(seq: u8, status: Status) -> [u8; 2] {
    [seq, status as u8]
}

/// Decodes a result.
pub fn decode_result(v: &[u8]) -> Result<(u8, Status), Error> {
    match v {
        [seq, s] => Ok((*seq, Status::from_u8(*s).ok_or(Error::Op)?)),
        _ => Err(Error::Length),
    }
}

/// A group value telegram seen on the bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Group {
    pub own: bool,
    pub op: Op,
    pub priority: Priority,
    pub src: u16,
    pub dest: u16,
    data: [u8; MAX_PAYLOAD],
    len: u8,
}

impl Group {
    /// The group value telegram tg; None for any other.
    pub fn from_telegram(tg: &Telegram, own: bool) -> Option<Group> {
        if tg.npci.addr_type != AddrType::Group {
            return None;
        }
        let op = Op::from_apci(tg.apci)?;
        let d = tg.data();
        let mut data = [0; MAX_PAYLOAD];
        data[..d.len()].copy_from_slice(d);
        Some(Group {
            own,
            op,
            priority: tg.ctrl.priority,
            src: tg.src,
            dest: tg.dest,
            data,
            len: d.len() as u8,
        })
    }

    pub fn data(&self) -> &[u8] {
        &self.data[..self.len as usize]
    }

    pub fn encode(&self, out: &mut [u8; MAX_VALUE]) -> usize {
        let flags = (self.own as u8) << 7 | (self.op as u8) << 4 | self.priority as u8;
        let (s, d) = (self.src.to_be_bytes(), self.dest.to_be_bytes());
        out[..GROUP_HEAD].copy_from_slice(&[flags, s[0], s[1], d[0], d[1]]);
        let data = self.data();
        out[GROUP_HEAD..GROUP_HEAD + data.len()].copy_from_slice(data);
        GROUP_HEAD + data.len()
    }

    pub fn decode(v: &[u8]) -> Result<Group, Error> {
        if v.len() < GROUP_HEAD || v.len() > MAX_VALUE {
            return Err(Error::Length);
        }
        let op = Op::from_u8(v[0] >> 4 & 0x03).ok_or(Error::Op)?;
        let d = &v[GROUP_HEAD..];
        let mut data = [0; MAX_PAYLOAD];
        data[..d.len()].copy_from_slice(d);
        Ok(Group {
            own: v[0] & 0x80 != 0,
            op,
            priority: Priority::from_bits(v[0]),
            src: u16::from_be_bytes([v[1], v[2]]),
            dest: u16::from_be_bytes([v[3], v[4]]),
            data,
            len: d.len() as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dpt::Dpt1001;
    use crate::tp1::MAX_FRAME;
    use crate::{GroupAddress, IndividualAddress};

    #[test]
    fn send_round_trip() {
        let g = GroupAddress::new3(0, 1, 7).0;
        let s = Send::new(9, Op::Write, g, Priority::Low, &Dpt1001(true).pack()).unwrap();
        let mut v = [0; MAX_VALUE];
        let n = s.encode(&mut v);
        assert_eq!(&v[..n], &[9, 0, 0x01, 0x07, 3, 0x01]);
        assert_eq!(Send::decode(&v[..n]).unwrap(), s);

        // A read takes no data on the wire.
        let r = Send::new(1, Op::Read, g, Priority::Low, &[]).unwrap();
        let n = r.encode(&mut v);
        assert_eq!(&v[..n], &[1, 1, 0x01, 0x07, 3]);
        assert_eq!(Send::decode(&v[..n]).unwrap(), r);
        assert_eq!(r.data(), &[0]);
        // Nor does a priority have to be given: low.
        assert_eq!(Send::decode(&[1, 1, 0x01, 0x07]).unwrap(), r);

        // The most data: 15 octets.
        let big = [0x80; MAX_PAYLOAD];
        let s = Send::new(2, Op::Response, g, Priority::High, &big).unwrap();
        let n = s.encode(&mut v);
        assert_eq!(n, MAX_VALUE);
        assert_eq!(Send::decode(&v[..n]).unwrap(), s);
    }

    #[test]
    fn send_errors() {
        assert_eq!(Send::decode(&[1, 0, 0]), Err(Error::Length));
        assert_eq!(Send::decode(&[0; MAX_VALUE + 1]), Err(Error::Length));
        assert_eq!(Send::decode(&[1, 3, 0, 1, 3, 0]), Err(Error::Op));
        assert_eq!(Send::decode(&[1, 0, 0, 1, 3]), Err(Error::Data));
        assert_eq!(Send::decode(&[1, 1, 0, 1, 3, 1]), Err(Error::Data));
    }

    #[test]
    fn send_telegram() {
        let s = Send::new(
            0,
            Op::Write,
            GroupAddress::new3(0, 0, 1).0,
            Priority::Low,
            &[0x01],
        )
        .unwrap();
        let mut tg = s.telegram();
        tg.src = IndividualAddress::new3(7, 8, 1).0;
        let mut buf = [0; MAX_FRAME];
        // The frame of Device::telegram(0/0/1, on), but for routing 6.
        assert_eq!(
            tg.pack_to(&mut buf).unwrap(),
            &[0xbc, 0x78, 0x01, 0x00, 0x01, 0xe1, 0x00, 0x81, 0x5b]
        );
        let mut back = Telegram::default();
        back.unpack(&buf[..9]).unwrap();
        assert_eq!(back, tg);
    }

    #[test]
    fn group_round_trip() {
        let s = Send::new(
            0,
            Op::Response,
            GroupAddress::new3(0, 1, 2).0,
            Priority::Alarm,
            &[0x00, 0x0c, 0x33],
        )
        .unwrap();
        let mut tg = s.telegram();
        tg.src = IndividualAddress::new3(1, 1, 5).0;
        let g = Group::from_telegram(&tg, true).unwrap();
        let mut v = [0; MAX_VALUE];
        let n = g.encode(&mut v);
        assert_eq!(
            &v[..n],
            &[0x80 | 0x20 | 2, 0x11, 0x05, 0x01, 0x02, 0x00, 0x0c, 0x33]
        );
        let back = Group::decode(&v[..n]).unwrap();
        assert_eq!(back, g);
        assert!(back.own);
        assert_eq!(back.op, Op::Response);
        assert_eq!(back.priority, Priority::Alarm);
        assert_eq!(back.data(), &[0x00, 0x0c, 0x33]);

        // Not a group value telegram.
        tg.apci = Apci::MemoryRead;
        assert!(Group::from_telegram(&tg, false).is_none());
        tg.apci = Apci::GroupValueWrite;
        tg.npci.addr_type = AddrType::Individual;
        assert!(Group::from_telegram(&tg, false).is_none());
    }

    #[test]
    fn results() {
        for b in 0..=5 {
            let s = Status::from_u8(b).unwrap();
            assert_eq!(decode_result(&encode_result(7, s)), Ok((7, s)));
        }
        assert_eq!(decode_result(&[7, 6]), Err(Error::Op));
        assert_eq!(decode_result(&[7]), Err(Error::Length));
    }
}
