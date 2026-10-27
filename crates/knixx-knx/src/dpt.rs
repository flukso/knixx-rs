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

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

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
