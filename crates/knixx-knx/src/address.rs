//! KNX individual and group addresses, with the syntax and error messages
//! of knx-go's cemi package.

use core::fmt::{self, Write};

/// IndividualAddress is an individual address for a KNX device. Its format
/// is defined in 03_03_02 Data Link Layer General v01.02.02 AS.pdf, 1.4.2
/// Individual Address. Individual address zero (0.0.0) is not allowed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct IndividualAddress(pub u16);

impl IndividualAddress {
    /// new3 generates an individual address from an "a.b.c" representation,
    /// where a is the Area Address [0..15], b is the Line Address [0..15]
    /// and c is the Device Address [0..255].
    pub const fn new3(a: u8, b: u8, c: u8) -> Self {
        Self(((a & 0xf) as u16) << 12 | ((b & 0xf) as u16) << 8 | c as u16)
    }

    /// new2 generates an individual address from an "a.b" representation,
    /// where a is the Subnetwork Address [0..255] and b is the Device
    /// Address [0..255].
    pub const fn new2(a: u8, b: u8) -> Self {
        Self((a as u16) << 8 | b as u16)
    }

    /// parse parses the given string to an individual address. Supported
    /// formats are %d.%d.%d ([0..15], [0..15], [0..255]), %d.%d ([0..255],
    /// [0..255]) and %d ([0..65535]). Validity is checked.
    pub fn parse(addr: &str) -> Result<Self, IndividualAddressError<'_>> {
        let (nums, n) = split_atoi(addr, '.').map_err(IndividualAddressError::Atoi)?;
        match n {
            3 => {
                if !(0..=15).contains(&nums[0])
                    || !(0..=15).contains(&nums[1])
                    || !(0..=255).contains(&nums[2])
                {
                    return Err(IndividualAddressError::AreaLineDevice(addr));
                }
                if nums == [0, 0, 0] {
                    return Err(IndividualAddressError::Zero3);
                }
                Ok(Self::new3(nums[0] as u8, nums[1] as u8, nums[2] as u8))
            }
            2 => {
                if !(0..=255).contains(&nums[0]) || !(0..=255).contains(&nums[1]) {
                    return Err(IndividualAddressError::SubnetworkDevice(addr));
                }
                if nums[0] == 0 && nums[1] == 0 {
                    return Err(IndividualAddressError::Zero2);
                }
                Ok(Self::new2(nums[0] as u8, nums[1] as u8))
            }
            1 => {
                if !(1..=65535).contains(&nums[0]) {
                    return Err(IndividualAddressError::Raw(addr));
                }
                Ok(Self(nums[0] as u16))
            }
            _ => Err(IndividualAddressError::Format),
        }
    }
}

impl From<u16> for IndividualAddress {
    fn from(raw: u16) -> Self {
        Self(raw)
    }
}

impl From<IndividualAddress> for u16 {
    fn from(addr: IndividualAddress) -> u16 {
        addr.0
    }
}

/// Display writes "a.b.c" where a = Area Address = 4 bits, b = Line Address
/// = 4 bits, c = Device Address = 1 byte.
impl fmt::Display for IndividualAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = self.0;
        write!(f, "{}.{}.{}", (a >> 12) & 0xf, (a >> 8) & 0xf, a & 0xff)
    }
}

/// GroupAddress is an address for a KNX group object. Group address zero
/// (0/0/0) is not allowed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GroupAddress(pub u16);

impl GroupAddress {
    /// new3 generates a group address from an "a/b/c" representation, where
    /// a is the Main Group [0..31], b is the Middle Group [0..7], c is the
    /// Sub Group [0..255].
    pub const fn new3(a: u8, b: u8, c: u8) -> Self {
        Self(((a & 0x1f) as u16) << 11 | ((b & 0x7) as u16) << 8 | c as u16)
    }

    /// new2 generates a group address from an "a/b" representation, where a
    /// is the Main Group [0..31] and b is the Sub Group [0..2047].
    pub const fn new2(a: u8, b: u16) -> Self {
        Self(((a & 0x1f) as u16) << 11 | (b & 0x7ff))
    }

    /// parse parses the given string to a group address. Supported formats
    /// are %d/%d/%d ([0..31], [0..7], [0..255]), %d/%d ([0..31], [0..2047])
    /// and %d ([0..65535]). Validity is checked.
    pub fn parse(addr: &str) -> Result<Self, GroupAddressError<'_>> {
        let (nums, n) = split_atoi(addr, '/').map_err(GroupAddressError::Atoi)?;
        match n {
            3 => {
                if !(0..=31).contains(&nums[0])
                    || !(0..=7).contains(&nums[1])
                    || !(0..=255).contains(&nums[2])
                {
                    return Err(GroupAddressError::MainMiddleSub(addr));
                }
                if nums == [0, 0, 0] {
                    return Err(GroupAddressError::Zero3);
                }
                Ok(Self::new3(nums[0] as u8, nums[1] as u8, nums[2] as u8))
            }
            2 => {
                if !(0..=31).contains(&nums[0]) || !(0..=2047).contains(&nums[1]) {
                    return Err(GroupAddressError::MainSub(addr));
                }
                if nums[0] == 0 && nums[1] == 0 {
                    return Err(GroupAddressError::Zero2);
                }
                Ok(Self::new2(nums[0] as u8, nums[1] as u16))
            }
            1 => {
                if !(1..=65535).contains(&nums[0]) {
                    return Err(GroupAddressError::Raw(addr));
                }
                Ok(Self(nums[0] as u16))
            }
            _ => Err(GroupAddressError::Format),
        }
    }
}

impl From<u16> for GroupAddress {
    fn from(raw: u16) -> Self {
        Self(raw)
    }
}

impl From<GroupAddress> for u16 {
    fn from(addr: GroupAddress) -> u16 {
        addr.0
    }
}

/// Display writes "a/b/c" where a = Main Group = 5 bits, b = Middle Group =
/// 3 bits, c = Sub Group = 1 byte.
impl fmt::Display for GroupAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let a = self.0;
        write!(f, "{}/{}/{}", (a >> 11) & 0x1f, (a >> 8) & 0x7, a & 0xff)
    }
}

/// IndividualAddressError is an error parsing an individual address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndividualAddressError<'a> {
    /// A part is not a number.
    Atoi(AtoiError<'a>),
    /// "invalid area, line or device address in <addr>"
    AreaLineDevice(&'a str),
    /// "invalid individual address 0.0.0"
    Zero3,
    /// "invalid subnetwork or device address in <addr>"
    SubnetworkDevice(&'a str),
    /// "invalid individual address 0.0"
    Zero2,
    /// "invalid raw individual address in <addr>"
    Raw(&'a str),
    /// More than three parts.
    Format,
}

impl fmt::Display for IndividualAddressError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Atoi(e) => e.fmt(f),
            Self::AreaLineDevice(addr) => write!(f, "invalid area, line or device address in {addr}"),
            Self::Zero3 => f.write_str("invalid individual address 0.0.0"),
            Self::SubnetworkDevice(addr) => write!(f, "invalid subnetwork or device address in {addr}"),
            Self::Zero2 => f.write_str("invalid individual address 0.0"),
            Self::Raw(addr) => write!(f, "invalid raw individual address in {addr}"),
            Self::Format => f.write_str("string cannot be parsed to an individual address"),
        }
    }
}

impl core::error::Error for IndividualAddressError<'_> {}

/// GroupAddressError is an error parsing a group address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupAddressError<'a> {
    /// A part is not a number: "invalid parsing <AtoiError>".
    Atoi(AtoiError<'a>),
    /// "invalid main, middle or sub group address in <addr>"
    MainMiddleSub(&'a str),
    /// "invalid group address 0/0/0"
    Zero3,
    /// "invalid main or sub group address in <addr>"
    MainSub(&'a str),
    /// "invalid group address 0/0"
    Zero2,
    /// "invalid raw group address in <addr>"
    Raw(&'a str),
    /// More than three parts.
    Format,
}

impl fmt::Display for GroupAddressError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Atoi(e) => write!(f, "invalid parsing {e}"),
            Self::MainMiddleSub(addr) => write!(f, "invalid main, middle or sub group address in {addr}"),
            Self::Zero3 => f.write_str("invalid group address 0/0/0"),
            Self::MainSub(addr) => write!(f, "invalid main or sub group address in {addr}"),
            Self::Zero2 => f.write_str("invalid group address 0/0"),
            Self::Raw(addr) => write!(f, "invalid raw group address in {addr}"),
            Self::Format => f.write_str("string cannot be parsed to a group address"),
        }
    }
}

impl core::error::Error for GroupAddressError<'_> {}

/// AtoiError is Go's strconv.Atoi error for num, with a 32-bit int as on
/// the nRF52840.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtoiError<'a> {
    pub num: &'a str,
    pub kind: AtoiErrorKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtoiErrorKind {
    Syntax,
    Range,
}

impl fmt::Display for AtoiError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("strconv.Atoi: parsing ")?;
        write_quoted(f, self.num)?;
        f.write_str(match self.kind {
            AtoiErrorKind::Syntax => ": invalid syntax",
            AtoiErrorKind::Range => ": value out of range",
        })
    }
}

impl core::error::Error for AtoiError<'_> {}

/// split_atoi splits s at sep and parses each part with atoi, as
/// strings.Split and strconv.Atoi. It returns the first three numbers and
/// the number of parts.
fn split_atoi(s: &str, sep: char) -> Result<([i32; 3], usize), AtoiError<'_>> {
    let mut nums = [0; 3];
    let mut n = 0;
    for part in s.split(sep) {
        let v = atoi(part)?;
        if n < nums.len() {
            nums[n] = v;
        }
        n += 1;
    }
    Ok((nums, n))
}

/// atoi is strconv.Atoi for a 32-bit int: an optional sign and decimal
/// digits. Like Go, it reports an overflow at the digit that overflows,
/// before any later syntax error.
fn atoi(s: &str) -> Result<i32, AtoiError<'_>> {
    let syntax = AtoiError {
        num: s,
        kind: AtoiErrorKind::Syntax,
    };
    let range = AtoiError {
        num: s,
        kind: AtoiErrorKind::Range,
    };
    let (neg, digits) = match s.as_bytes() {
        [b'+', rest @ ..] => (false, rest),
        [b'-', rest @ ..] => (true, rest),
        b => (false, b),
    };
    if digits.is_empty() {
        return Err(syntax);
    }
    let mut n: u32 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return Err(syntax);
        }
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add((c - b'0') as u32))
            .ok_or(range)?;
    }
    let cutoff = 1u32 << 31;
    if (!neg && n >= cutoff) || (neg && n > cutoff) {
        return Err(range);
    }
    Ok(if neg { (n as i32).wrapping_neg() } else { n as i32 })
}

/// write_quoted writes s as Go's strconv.Quote.
fn write_quoted(f: &mut fmt::Formatter<'_>, s: &str) -> fmt::Result {
    f.write_char('"')?;
    for c in s.chars() {
        match c {
            '"' => f.write_str("\\\"")?,
            '\\' => f.write_str("\\\\")?,
            '\x07' => f.write_str("\\a")?,
            '\x08' => f.write_str("\\b")?,
            '\x0c' => f.write_str("\\f")?,
            '\n' => f.write_str("\\n")?,
            '\r' => f.write_str("\\r")?,
            '\t' => f.write_str("\\t")?,
            '\x0b' => f.write_str("\\v")?,
            ' '..='~' => f.write_char(c)?,
            c if c < ' ' || c == '\x7f' => write!(f, "\\x{:02x}", c as u32)?,
            c if is_print(c) => f.write_char(c)?,
            c if (c as u32) < 0x10000 => write!(f, "\\u{:04x}", c as u32)?,
            c => write!(f, "\\U{:08x}", c as u32)?,
        }
    }
    f.write_char('"')
}

/// is_print approximates Go's strconv.IsPrint with Rust's escape_debug.
fn is_print(c: char) -> bool {
    let mut e = c.escape_debug();
    e.next() == Some(c) && e.next().is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    // Test Individual Addresses
    #[test]
    fn individual_addresses() {
        let addrs: &[(&str, bool, &str)] = &[
            ("1.2.3", true, "1.2.3"),
            ("1.3.255", true, "1.3.255"),
            ("1.3.0", true, "1.3.0"),
            ("75.235", true, "4.11.235"),
            ("65535", true, "15.15.255"),
            ("15.15.255", true, "15.15.255"),
            ("15.15.0", true, "15.15.0"),
            ("13057", true, "3.3.1"),
            ("16.17.255", false, ""),
            ("1..0", false, ""),
            ("15.15.", false, ""),
            (" . .15", false, ""),
            ("18.15.240", false, ""),
            ("1.3.450", false, ""),
            ("1.450", false, ""),
            ("-2", false, ""),
            (".400", false, ""),
            ("-11.0.0", false, ""),
            ("0.0.0", false, ""),
            ("0.0", false, ""),
            ("0", false, ""),
        ];
        for &(src, valid, printed) in addrs {
            match IndividualAddress::parse(src) {
                Ok(ia) if valid => assert_eq!(ia.to_string(), printed, "{src:?} wrongly parsed."),
                Err(err) if valid => panic!("{src:?} has error {err}."),
                Ok(_) => panic!("{src:?} invalid parsed."),
                Err(_) => {}
            }
        }
    }

    // Test Group Addresses
    #[test]
    fn group_addresses() {
        let addrs: &[(&str, bool, &str)] = &[
            ("1/2/3", true, "1/2/3"),
            ("31/7/255", true, "31/7/255"),
            ("31/2040", true, "31/7/248"),
            ("31/2047", true, "31/7/255"),
            ("65535", true, "31/7/255"),
            ("82/8/260", false, ""),
            ("84/230", false, ""),
            ("31/2060", false, ""),
            ("0/0/0", false, ""),
            ("0/0", false, ""),
            ("0", false, ""),
            ("123/foobar", false, ""),
            ("1000/2000/3000", false, ""),
        ];
        for &(src, valid, printed) in addrs {
            match GroupAddress::parse(src) {
                Ok(ga) if valid => assert_eq!(ga.to_string(), printed, "{src:?} wrongly parsed."),
                Err(err) if valid => panic!("{src:?} has error {err}."),
                Ok(_) => panic!("{src:?} invalid parsed."),
                Err(_) => {}
            }
        }
    }

    #[test]
    fn individual_address_errors() {
        for (src, want) in [
            ("16.17.255", "invalid area, line or device address in 16.17.255"),
            ("0.0.0", "invalid individual address 0.0.0"),
            ("1.450", "invalid subnetwork or device address in 1.450"),
            ("0.0", "invalid individual address 0.0"),
            ("0", "invalid raw individual address in 0"),
            ("65536", "invalid raw individual address in 65536"),
            ("1.2.3.4", "string cannot be parsed to an individual address"),
            ("1..0", "strconv.Atoi: parsing \"\": invalid syntax"),
            (" . .15", "strconv.Atoi: parsing \" \": invalid syntax"),
            ("1.2.x", "strconv.Atoi: parsing \"x\": invalid syntax"),
            ("1.2.3.x", "strconv.Atoi: parsing \"x\": invalid syntax"),
            ("1/2/3", "strconv.Atoi: parsing \"1/2/3\": invalid syntax"),
            (
                "2147483648",
                "strconv.Atoi: parsing \"2147483648\": value out of range",
            ),
            (
                "99999999999x",
                "strconv.Atoi: parsing \"99999999999x\": value out of range",
            ),
        ] {
            assert_eq!(
                IndividualAddress::parse(src).unwrap_err().to_string(),
                want,
                "{src:?}"
            );
        }
    }

    #[test]
    fn group_address_errors() {
        for (src, want) in [
            (
                "82/8/260",
                "invalid main, middle or sub group address in 82/8/260",
            ),
            ("0/0/0", "invalid group address 0/0/0"),
            ("31/2060", "invalid main or sub group address in 31/2060"),
            ("0/0", "invalid group address 0/0"),
            ("-1", "invalid raw group address in -1"),
            ("1/2/3/4", "string cannot be parsed to a group address"),
            (
                "123/foobar",
                "invalid parsing strconv.Atoi: parsing \"foobar\": invalid syntax",
            ),
            (
                "1/2/\"\t\x01é\u{a0}",
                "invalid parsing strconv.Atoi: parsing \"\\\"\\t\\x01é\\u00a0\": invalid syntax",
            ),
            ("", "invalid parsing strconv.Atoi: parsing \"\": invalid syntax"),
        ] {
            assert_eq!(GroupAddress::parse(src).unwrap_err().to_string(), want, "{src:?}");
        }
    }

    #[test]
    fn atoi_forms() {
        for (s, want) in [
            ("0", Ok(0)),
            ("+7", Ok(7)),
            ("-0", Ok(0)),
            ("007", Ok(7)),
            ("-2147483648", Ok(i32::MIN)),
            ("2147483647", Ok(i32::MAX)),
            ("+", Err(AtoiErrorKind::Syntax)),
            ("-", Err(AtoiErrorKind::Syntax)),
            ("1_000", Err(AtoiErrorKind::Syntax)),
            (" 1", Err(AtoiErrorKind::Syntax)),
            ("1 ", Err(AtoiErrorKind::Syntax)),
            ("0x10", Err(AtoiErrorKind::Syntax)),
            ("-2147483649", Err(AtoiErrorKind::Range)),
        ] {
            assert_eq!(atoi(s).map_err(|e| e.kind), want, "{s:?}");
        }
    }

    #[test]
    fn constructors() {
        assert_eq!(IndividualAddress::new3(7, 8, 1), IndividualAddress(0x7801));
        assert_eq!(IndividualAddress::new2(0x78, 1), IndividualAddress(0x7801));
        assert_eq!(GroupAddress::new3(0, 0, 1), GroupAddress(1));
        assert_eq!(GroupAddress::new2(31, 2047), GroupAddress(0xffff));
        assert_eq!(GroupAddress::parse("+1/+2/3"), Ok(GroupAddress::new3(1, 2, 3)));
        assert_eq!(IndividualAddress(0).to_string(), "0.0.0");
        assert_eq!(GroupAddress(0).to_string(), "0/0/0");
    }
}
