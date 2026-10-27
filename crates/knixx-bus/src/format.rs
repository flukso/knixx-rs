//! The push methods render parts of a sniffer line into a fixed buffer.

use core::fmt;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// A line of output, rendered into a fixed buffer. 256 bytes hold the
/// longest frame with room to spare; what does not fit is dropped.
pub(crate) struct Line {
    buf: [u8; Line::CAP],
    n: usize,
}

impl Line {
    pub(crate) const CAP: usize = 256;

    pub(crate) const fn new() -> Self {
        Line {
            buf: [0; Line::CAP],
            n: 0,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.n = 0;
    }

    pub(crate) fn len(&self) -> usize {
        self.n
    }

    pub(crate) fn truncate(&mut self, n: usize) {
        self.n = self.n.min(n);
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.n]
    }

    pub(crate) fn push(&mut self, b: u8) {
        if self.n < Line::CAP {
            self.buf[self.n] = b;
            self.n += 1;
        }
    }

    pub(crate) fn push_bytes(&mut self, p: &[u8]) {
        let k = p.len().min(Line::CAP - self.n);
        self.buf[self.n..self.n + k].copy_from_slice(&p[..k]);
        self.n += k;
    }

    pub(crate) fn push_str(&mut self, s: &str) {
        self.push_bytes(s.as_bytes());
    }

    /// Pushes v in decimal.
    pub(crate) fn push_dec(&mut self, mut v: u64) {
        let mut digits = [0u8; 20];
        let mut i = digits.len();
        loop {
            i -= 1;
            digits[i] = b'0' + (v % 10) as u8;
            v /= 10;
            if v == 0 {
                break;
            }
        }
        self.push_bytes(&digits[i..]);
    }

    /// Pushes b as two hex digits.
    pub(crate) fn push_hex2(&mut self, b: u8) {
        self.push(HEX_DIGITS[(b >> 4) as usize]);
        self.push(HEX_DIGITS[(b & 0x0f) as usize]);
    }

    /// Pushes p as hex octets separated by spaces, like Go's % x verb.
    pub(crate) fn push_hex_bytes(&mut self, p: &[u8]) {
        for (i, &b) in p.iter().enumerate() {
            if i > 0 {
                self.push(b' ');
            }
            self.push_hex2(b);
        }
    }

    /// Pushes an individual address as area.line.device.
    pub(crate) fn push_individual_addr(&mut self, a: u16) {
        self.push_dec((a >> 12 & 0x0f) as u64);
        self.push(b'.');
        self.push_dec((a >> 8 & 0x0f) as u64);
        self.push(b'.');
        self.push_dec((a & 0xff) as u64);
    }

    /// Pushes a group address as main/middle/sub.
    pub(crate) fn push_group_addr(&mut self, a: u16) {
        self.push_dec((a >> 11 & 0x1f) as u64);
        self.push(b'/');
        self.push_dec((a >> 8 & 0x07) as u64);
        self.push(b'/');
        self.push_dec((a & 0xff) as u64);
    }

    /// Pushes h/100 with two decimals.
    pub(crate) fn push_hundredths(&mut self, h: i32) {
        if h < 0 {
            self.push(b'-');
        }
        let h = h.unsigned_abs();
        self.push_dec((h / 100) as u64);
        self.push(b'.');
        self.push(b'0' + (h / 10 % 10) as u8);
        self.push(b'0' + (h % 10) as u8);
    }
}

impl fmt::Write for Line {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

/// Decodes the DPT 9 value in data, the APCI octet and two value octets, in
/// hundredths: the value is 0.01·m·2^e, so this is exact. None for another
/// length.
pub(crate) fn f16(data: &[u8]) -> Option<i32> {
    let [_, hi, lo] = *data else {
        return None;
    };
    let mut m = ((hi & 0x07) as i32) << 8 | lo as i32;
    if hi & 0x80 != 0 {
        m -= 2048;
    }
    Some(m << (hi >> 3 & 0x0f))
}

/// The DPT 9 bounds, in hundredths.
const MIN_F16_PACK: i32 = -67_108_864;
const MAX_F16_PACK: i32 = 67_076_096;

/// The packed values sent fit: DPT 1 and DPT 9.
pub(crate) const MAX_DATA: usize = 3;

/// A group value write waiting to be sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Write {
    pub(crate) group: u16,
    pub(crate) data: [u8; MAX_DATA],
    pub(crate) n: u8,
}

impl Write {
    pub(crate) fn data(&self) -> &[u8] {
        &self.data[..self.n as usize]
    }
}

/// Returns a write of a DPT 1 value to group.
pub(crate) fn bool_write(group: u16, on: bool) -> Write {
    Write {
        group,
        data: [on as u8, 0, 0],
        n: 1,
    }
}

/// Returns a write of the DPT 9 value h hundredths to group, after the APCI
/// octet: the value is m·2^e hundredths with an 11-bit signed mantissa m, so
/// m loses its low bits as e grows. A value out of range is clamped.
pub(crate) fn f16_write(group: u16, h: i32) -> Write {
    let mut m = h.clamp(MIN_F16_PACK, MAX_F16_PACK);
    let mut e = 0u8;
    while !(-2048..=2047).contains(&m) {
        m /= 2;
        e += 1;
    }
    let mut w = Write {
        group,
        data: [0, e << 3, 0],
        n: 3,
    };
    if m < 0 {
        m += 2048;
        w.data[1] |= 0x80;
    }
    w.data[1] |= (m >> 8) as u8 & 0x07;
    w.data[2] = m as u8;
    w
}
