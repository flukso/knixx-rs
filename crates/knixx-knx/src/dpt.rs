//! The datapoint types knixx uses, as knx-go's dpt package.

use core::fmt;

/// Error is an error unpacking a datapoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The application data has unexpected length.
    InvalidLength,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::InvalidLength => f.write_str("given application data has invalid length"),
        }
    }
}

impl core::error::Error for Error {}

fn pack_b1(b: bool) -> u8 {
    b as u8
}

fn unpack_b1(data: &[u8]) -> Result<bool, Error> {
    match data {
        [x] => Ok(x & 1 == 1),
        _ => Err(Error::InvalidLength),
    }
}

/// Dpt1001 represents DPT 1.001 (G) / DPT_Switch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dpt1001(pub bool);

impl Dpt1001 {
    pub fn pack(self) -> [u8; 1] {
        [pack_b1(self.0)]
    }

    pub fn unpack(data: &[u8]) -> Result<Self, Error> {
        unpack_b1(data).map(Self)
    }

    pub const fn as_str(self) -> &'static str {
        if self.0 { "On" } else { "Off" }
    }
}

impl fmt::Display for Dpt1001 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Dpt1018 represents DPT 1.018 (G) / DPT_Occupancy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dpt1018(pub bool);

impl Dpt1018 {
    pub fn pack(self) -> [u8; 1] {
        [pack_b1(self.0)]
    }

    pub fn unpack(data: &[u8]) -> Result<Self, Error> {
        unpack_b1(data).map(Self)
    }

    pub const fn as_str(self) -> &'static str {
        if self.0 { "occupied" } else { "not occupied" }
    }
}

impl fmt::Display for Dpt1018 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Dpt5001 represents DPT 5.001 / DPT_Scaling: a percentage from 0 to 100,
/// carried as a byte from 0 to 255.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Dpt5001(pub u8);

impl Dpt5001 {
    /// Packs the percentage, clamped to 100, as a telegram's data: the APCI
    /// octet and the value octet, rounded to the nearest step.
    pub fn pack(self) -> [u8; 2] {
        let p = self.0.min(100) as u32;
        [0, ((p * 255 + 50) / 100) as u8]
    }

    /// Unpacks the percentage, rounded to the nearest whole one.
    pub fn unpack(data: &[u8]) -> Result<Self, Error> {
        match data {
            [_, v] => Ok(Self(((*v as u32 * 100 + 127) / 255) as u8)),
            _ => Err(Error::InvalidLength),
        }
    }
}

impl fmt::Display for Dpt5001 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} %", self.0)
    }
}

/// The DPT 9 bounds, in hundredths.
const MIN_F16: i32 = -67_108_864;
const MAX_F16: i32 = 67_076_096;

/// Packs the DPT 9 (2-octet float) value h hundredths as a telegram's data,
/// the APCI octet and the two value octets: the value is m·2^e hundredths
/// with an 11-bit signed mantissa m, so m loses its low bits as e grows. A
/// value out of range is clamped.
pub fn f16_pack(h: i32) -> [u8; 3] {
    let mut m = h.clamp(MIN_F16, MAX_F16);
    let mut e = 0u8;
    while !(-2048..=2047).contains(&m) {
        m /= 2;
        e += 1;
    }
    let mut data = [0, e << 3, 0];
    if m < 0 {
        m += 2048;
        data[1] |= 0x80;
    }
    data[1] |= (m >> 8) as u8 & 0x07;
    data[2] = m as u8;
    data
}

/// Unpacks the DPT 9 value in data, the APCI octet and two value octets, in
/// hundredths: the value is 0.01·m·2^e, so this is exact. None for another
/// length.
pub fn f16_unpack(data: &[u8]) -> Option<i32> {
    let [_, hi, lo] = *data else {
        return None;
    };
    let mut m = ((hi & 0x07) as i32) << 8 | lo as i32;
    if hi & 0x80 != 0 {
        m -= 2048;
    }
    Some(m << (hi >> 3 & 0x0f))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn dpt5001() {
        for (p, v) in [(0, 0x00), (1, 0x03), (50, 0x80), (99, 0xfc), (100, 0xff)] {
            assert_eq!(Dpt5001(p).pack(), [0, v], "{p} %");
            assert_eq!(Dpt5001::unpack(&[0, v]), Ok(Dpt5001(p)), "{v:#x}");
        }
        assert_eq!(Dpt5001(150).pack(), [0, 0xff]);
        // Every byte unpacks to a percentage that packs back to it or a
        // neighbour.
        for v in 0..=255u8 {
            let p = Dpt5001::unpack(&[0, v]).unwrap();
            assert!(p.0 <= 100);
            assert!(Dpt5001(p.0).pack()[1].abs_diff(v) <= 2, "{v}");
        }
        assert_eq!(Dpt5001::unpack(&[0]), Err(Error::InvalidLength));
        assert_eq!(Dpt5001(42).to_string(), "42 %");
    }

    #[test]
    fn dpt_1001() {
        assert_eq!(Dpt1001(true).pack(), [0x01]);
        assert_eq!(Dpt1001(false).pack(), [0x00]);
        assert_eq!(Dpt1001(true).to_string(), "On");
        assert_eq!(Dpt1001(false).to_string(), "Off");
        assert_eq!(Dpt1001::unpack(&[0x01]), Ok(Dpt1001(true)));
        assert_eq!(Dpt1001::unpack(&[0xfe]), Ok(Dpt1001(false)));
        assert_eq!(Dpt1001::unpack(&[]), Err(Error::InvalidLength));
        assert_eq!(Dpt1001::unpack(&[1, 0]), Err(Error::InvalidLength));
    }

    #[test]
    fn dpt_1018() {
        assert_eq!(Dpt1018(true).pack(), [0x01]);
        assert_eq!(Dpt1018(false).pack(), [0x00]);
        assert_eq!(Dpt1018(true).to_string(), "occupied");
        assert_eq!(Dpt1018(false).to_string(), "not occupied");
        assert_eq!(Dpt1018::unpack(&[0x03]), Ok(Dpt1018(true)));
        assert_eq!(
            Error::InvalidLength.to_string(),
            "given application data has invalid length"
        );
    }
}
