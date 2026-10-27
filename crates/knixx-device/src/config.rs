//! The device configuration as an INI file: parse, format, and get and set
//! its values by "section.key".

use core::fmt::{self, Write};

use knixx_knx::address::{GroupAddressError, IndividualAddressError};
use knixx_knx::{GroupAddress, IndividualAddress};

use crate::thunder::{self, Device, MAX_GROUPS, NUM_SWITCHES, Occupancy, Switch};

const DEVICE_SECTION: &str = "device";
const INDIVIDUAL_KEY: &str = "individual";
const GROUP_KEY: &str = "group";
const TEMPERATURE_SECTION: &str = "temperature";
const HUMIDITY_SECTION: &str = "humidity";
const ILLUMINANCE_SECTION: &str = "illuminance";
const OCCUPANCY_SECTION: &str = "occupancy";
const TIMEOUT_KEY: &str = "timeout";
const THRESHOLD_KEY: &str = "threshold";
const FREQUENCY_KEY: &str = "frequency";
const INTEGRATION_KEY: &str = "integration";
const MIN_DISTANCE_KEY: &str = "min_distance";
const MAX_DISTANCE_KEY: &str = "max_distance";
/// UNBOUND is the value of an optional group address that is not set.
const UNBOUND: &str = "-";

const SWITCH_NAMES: [&str; NUM_SWITCHES] = ["A", "B"];

/// MAX_FILE bounds the size of a configuration file; format of any device
/// fits it.
pub const MAX_FILE: usize = 1024;

/// MAX_VALUE bounds the length of a value get returns: a full list of
/// group addresses.
pub const MAX_VALUE: usize = MAX_GROUPS * "31/7/255".len() + (MAX_GROUPS - 1) * ", ".len();

/// DEFAULT is the configuration written to a freshly formatted device.
pub const DEFAULT: &[u8] = include_bytes!("../default.ini");

/// KEYS lists the parameters accessible through get and set, in file order.
pub const KEYS: [&str; 13] = [
    "device.individual",
    "A.group",
    "B.group",
    "temperature.group",
    "humidity.group",
    "illuminance.group",
    "occupancy.group",
    "occupancy.timeout",
    "occupancy.threshold",
    "occupancy.frequency",
    "occupancy.integration",
    "occupancy.min_distance",
    "occupancy.max_distance",
];

/// Store persists a configuration produced by format.
pub trait Store {
    type Error;

    fn save(&mut self, data: &[u8]) -> Result<(), Self::Error>;
}

/// Error is an error parsing a configuration, or getting or setting one of
/// its values. Its Display is the Go error text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error<'a> {
    /// The file is not UTF-8: "config: invalid UTF-8".
    Utf8,
    /// A line is neither blank, a comment, a section nor a key/value pair.
    Line(&'a str),
    /// A required key is missing: "config: [section] key: missing".
    Missing {
        section: &'static str,
        key: &'static str,
    },
    /// A value in the file is invalid: "config: [section] key: <err>".
    Field {
        section: &'static str,
        key: &'static str,
        err: ValueError<'a>,
    },
    /// A value given to set is invalid: "config: <key>: <err>".
    Value { key: &'a str, err: ValueError<'a> },
    /// "config: unknown key <quoted key>".
    UnknownKey(&'a str),
}

impl fmt::Display for Error<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Utf8 => f.write_str("config: invalid UTF-8"),
            Self::Line(line) => {
                write!(
                    f,
                    "config: Came accross an error : {line} is NOT a valid key/value pair"
                )
            }
            Self::Missing { section, key } => write!(f, "config: [{section}] {key}: missing"),
            Self::Field { section, key, err } => write!(f, "config: [{section}] {key}: {err}"),
            Self::Value { key, err } => write!(f, "config: {key}: {err}"),
            Self::UnknownKey(key) => {
                f.write_str("config: unknown key ")?;
                write_quoted(f, key)
            }
        }
    }
}

impl core::error::Error for Error<'_> {}

/// ValueError is an invalid value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueError<'a> {
    Individual(IndividualAddressError<'a>),
    Group(GroupAddressError<'a>),
    /// "empty group address"
    EmptyGroup,
    /// "more than 8 group addresses"
    TooManyGroups,
    /// "duplicate group address <addr>"
    DuplicateGroup(&'a str),
    /// "0/0/0 is the broadcast address"
    Broadcast,
    /// "<quoted value> is not a number from <min> to <max>"
    Number {
        value: &'a str,
        min: u16,
        max: u16,
    },
}

impl fmt::Display for ValueError<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Individual(e) => e.fmt(f),
            Self::Group(e) => e.fmt(f),
            Self::EmptyGroup => f.write_str("empty group address"),
            Self::TooManyGroups => write!(f, "more than {MAX_GROUPS} group addresses"),
            Self::DuplicateGroup(addr) => write!(f, "duplicate group address {addr}"),
            Self::Broadcast => f.write_str("0/0/0 is the broadcast address"),
            Self::Number { value, min, max } => {
                write_quoted(f, value)?;
                write!(f, " is not a number from {min} to {max}")
            }
        }
    }
}

impl core::error::Error for ValueError<'_> {}

/// Ini reads values from INI file contents as goini does, with sections
/// parsed and comments skipped: lines are trimmed, blank lines and lines
/// starting with ';' or '#' skipped, "[name]" starts section name (afresh,
/// when it repeats), and every other line must be "key = value". The last of
/// duplicate keys wins; names are case-sensitive.
struct Ini<'a> {
    text: &'a str,
}

enum Line<'a> {
    Skip,
    Section(&'a str),
    Pair(&'a str, &'a str),
}

impl<'a> Ini<'a> {
    /// new checks every line of text.
    fn new(text: &'a str) -> Result<Self, Error<'a>> {
        for line in text.split('\n') {
            Self::line(line)?;
        }
        Ok(Ini { text })
    }

    fn line(line: &'a str) -> Result<Line<'a>, Error<'a>> {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            return Ok(Line::Skip);
        }
        if line.starts_with('[') && line.ends_with(']') {
            return Ok(Line::Section(&line[1..line.len() - 1]));
        }
        match line.split_once('=') {
            Some((k, v)) => Ok(Line::Pair(k.trim(), v.trim())),
            None => Err(Error::Line(line)),
        }
    }

    /// section_get looks up the value of key in section.
    fn section_get(&self, section: &str, key: &str) -> Option<&'a str> {
        let mut current = "";
        let mut value = None;
        for line in self.text.split('\n') {
            match Self::line(line) {
                Ok(Line::Section(s)) => {
                    current = s;
                    if s == section {
                        value = None;
                    }
                }
                Ok(Line::Pair(k, v)) if current == section && k == key => value = Some(v),
                _ => {}
            }
        }
        value
    }
}

/// parse reads the device's individual address, the group addresses of
/// each switch, the optional sensor group addresses and the optional
/// occupancy settings from ini file contents.
pub fn parse(data: &[u8]) -> Result<Device, Error<'_>> {
    let text = core::str::from_utf8(data).map_err(|_| Error::Utf8)?;
    let ini = Ini::new(text)?;

    let mut dev = Device {
        occupancy: Occupancy::DEFAULT,
        ..Default::default()
    };

    let v = get_required(&ini, DEVICE_SECTION, INDIVIDUAL_KEY)?;
    dev.individual = IndividualAddress::parse(v)
        .map_err(|e| Error::Field {
            section: DEVICE_SECTION,
            key: INDIVIDUAL_KEY,
            err: ValueError::Individual(e),
        })?
        .0;

    for (i, name) in SWITCH_NAMES.into_iter().enumerate() {
        let v = get_required(&ini, name, GROUP_KEY)?;
        dev.switches[i] = Switch {
            name,
            ..Default::default()
        };
        parse_groups(v, &mut dev.switches[i]).map_err(|err| Error::Field {
            section: name,
            key: GROUP_KEY,
            err,
        })?;
    }

    // The sensor value sections are optional; a missing one leaves its
    // value unbound.
    for (section, field) in [
        (TEMPERATURE_SECTION, Field::Temperature),
        (HUMIDITY_SECTION, Field::Humidity),
        (ILLUMINANCE_SECTION, Field::Illuminance),
    ] {
        if let Some(v) = ini.section_get(section, GROUP_KEY) {
            field.slot(&mut dev).set(v).map_err(|err| Error::Field {
                section,
                key: GROUP_KEY,
                err,
            })?;
        }
    }

    // The occupancy section is optional too; missing keys keep their
    // defaults.
    for &(section, key, field) in FIELDS.iter().filter(|f| f.0 == OCCUPANCY_SECTION) {
        if let Some(v) = ini.section_get(section, key) {
            field
                .slot(&mut dev)
                .set(v)
                .map_err(|err| Error::Field { section, key, err })?;
        }
    }

    Ok(dev)
}

fn get_required<'a>(ini: &Ini<'a>, section: &'static str, key: &'static str) -> Result<&'a str, Error<'a>> {
    ini.section_get(section, key)
        .ok_or(Error::Missing { section, key })
}

/// parse_groups replaces the group addresses of sw with the comma separated
/// list in value. sw is left untouched when value is invalid.
fn parse_groups<'a>(value: &'a str, sw: &mut Switch) -> Result<(), ValueError<'a>> {
    let mut groups = [0u16; MAX_GROUPS];
    let mut n = 0;
    for entry in value.split(',') {
        let entry = entry.trim();
        if entry.is_empty() {
            return Err(ValueError::EmptyGroup);
        }
        if n == MAX_GROUPS {
            return Err(ValueError::TooManyGroups);
        }
        let a = GroupAddress::parse(entry).map_err(ValueError::Group)?.0;
        if groups[..n].contains(&a) {
            return Err(ValueError::DuplicateGroup(entry));
        }
        groups[n] = a;
        n += 1;
    }
    sw.groups = groups;
    sw.n_groups = n as u8;
    Ok(())
}

/// parse_optional_group sets *group to the single group address in value,
/// or to 0 when value is the unbound marker. *group is left untouched when
/// value is invalid.
fn parse_optional_group<'a>(value: &'a str, group: &mut u16) -> Result<(), ValueError<'a>> {
    let value = value.trim();
    if value == UNBOUND {
        *group = 0;
        return Ok(());
    }
    let a = GroupAddress::parse(value).map_err(ValueError::Group)?.0;
    if a == 0 {
        return Err(ValueError::Broadcast);
    }
    *group = a;
    Ok(())
}

/// parse_number is strconv.ParseUint(value, 10, 16) on the trimmed value.
fn parse_number(value: &str) -> Option<u16> {
    let digits = value.trim().as_bytes();
    if digits.is_empty() {
        return None;
    }
    let mut n: u16 = 0;
    for &c in digits {
        if !c.is_ascii_digit() {
            return None;
        }
        n = n.checked_mul(10)?.checked_add((c - b'0') as u16)?;
    }
    Some(n)
}

fn write_optional_group(w: &mut impl Write, group: u16) -> fmt::Result {
    if group == 0 {
        return w.write_str(UNBOUND);
    }
    write!(w, "{}", GroupAddress(group))
}

fn write_groups(w: &mut impl Write, sw: &Switch) -> fmt::Result {
    for (i, &g) in sw.group_list().iter().enumerate() {
        if i > 0 {
            w.write_str(", ")?;
        }
        write!(w, "{}", GroupAddress(g))?;
    }
    Ok(())
}

/// Field names a value of the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Field {
    Individual,
    Switch(u8),
    Temperature,
    Humidity,
    Illuminance,
    OccupancyGroup,
    Timeout,
    Threshold,
    Frequency,
    Integration,
    MinDistance,
    MaxDistance,
}

/// FIELDS maps the "section.key" names of all but the switches' values.
const FIELDS: [(&str, &str, Field); 11] = [
    (DEVICE_SECTION, INDIVIDUAL_KEY, Field::Individual),
    (TEMPERATURE_SECTION, GROUP_KEY, Field::Temperature),
    (HUMIDITY_SECTION, GROUP_KEY, Field::Humidity),
    (ILLUMINANCE_SECTION, GROUP_KEY, Field::Illuminance),
    (OCCUPANCY_SECTION, GROUP_KEY, Field::OccupancyGroup),
    (OCCUPANCY_SECTION, TIMEOUT_KEY, Field::Timeout),
    (OCCUPANCY_SECTION, THRESHOLD_KEY, Field::Threshold),
    (OCCUPANCY_SECTION, FREQUENCY_KEY, Field::Frequency),
    (OCCUPANCY_SECTION, INTEGRATION_KEY, Field::Integration),
    (OCCUPANCY_SECTION, MIN_DISTANCE_KEY, Field::MinDistance),
    (OCCUPANCY_SECTION, MAX_DISTANCE_KEY, Field::MaxDistance),
];

/// Slot points at the value a field refers to.
enum Slot<'d> {
    Individual(&'d mut u16),
    Switch(&'d mut Switch),
    /// An optional single group address.
    Group(&'d mut u16),
    /// A decimal number between min and max.
    Number {
        n: &'d mut u16,
        min: u16,
        max: u16,
    },
}

impl Field {
    /// resolve resolves a "section.key" name to the value it refers to.
    fn resolve<'a>(dev: &Device, key: &'a str) -> Result<Field, Error<'a>> {
        let (section, name) = key.split_once('.').unwrap_or((key, ""));
        for &(s, k, field) in &FIELDS {
            if section.eq_ignore_ascii_case(s) && name.eq_ignore_ascii_case(k) {
                return Ok(field);
            }
        }
        match dev.switch_index(section) {
            Some(i) if name.eq_ignore_ascii_case(GROUP_KEY) => Ok(Field::Switch(i)),
            _ => Err(Error::UnknownKey(key)),
        }
    }

    fn slot(self, dev: &mut Device) -> Slot<'_> {
        let o = &mut dev.occupancy;
        match self {
            Field::Individual => Slot::Individual(&mut dev.individual),
            Field::Switch(i) => Slot::Switch(&mut dev.switches[i as usize]),
            Field::Temperature => Slot::Group(&mut dev.sensor.temperature),
            Field::Humidity => Slot::Group(&mut dev.sensor.humidity),
            Field::Illuminance => Slot::Group(&mut dev.sensor.illuminance),
            Field::OccupancyGroup => Slot::Group(&mut o.group),
            Field::Timeout => Slot::Number {
                n: &mut o.timeout,
                min: thunder::MIN_OCCUPANCY_TIMEOUT,
                max: thunder::MAX_OCCUPANCY_TIMEOUT,
            },
            Field::Threshold => Slot::Number {
                n: &mut o.threshold,
                min: thunder::MIN_MOTION_THRESHOLD,
                max: thunder::MAX_MOTION_THRESHOLD,
            },
            Field::Frequency => Slot::Number {
                n: &mut o.frequency,
                min: thunder::MIN_RANGING_FREQUENCY,
                max: thunder::MAX_RANGING_FREQUENCY,
            },
            Field::Integration => Slot::Number {
                n: &mut o.integration,
                min: thunder::MIN_INTEGRATION_TIME,
                max: thunder::MAX_INTEGRATION_TIME,
            },
            Field::MinDistance => Slot::Number {
                n: &mut o.min_distance,
                min: thunder::MIN_MOTION_DISTANCE,
                max: thunder::MAX_MOTION_DISTANCE,
            },
            Field::MaxDistance => Slot::Number {
                n: &mut o.max_distance,
                min: thunder::MIN_MOTION_DISTANCE,
                max: thunder::MAX_MOTION_DISTANCE,
            },
        }
    }
}

impl Slot<'_> {
    /// set parses value into the referenced field, which is left untouched
    /// when value is invalid.
    fn set<'a>(self, value: &'a str) -> Result<(), ValueError<'a>> {
        match self {
            Slot::Individual(n) => {
                *n = IndividualAddress::parse(value).map_err(ValueError::Individual)?.0;
                Ok(())
            }
            Slot::Switch(sw) => parse_groups(value, sw),
            Slot::Group(g) => parse_optional_group(value, g),
            Slot::Number { n, min, max } => match parse_number(value) {
                Some(v) if (min..=max).contains(&v) => {
                    *n = v;
                    Ok(())
                }
                _ => Err(ValueError::Number { value, min, max }),
            },
        }
    }

    /// write formats the referenced field.
    fn write(&self, w: &mut impl Write) -> fmt::Result {
        match self {
            Slot::Individual(n) => write!(w, "{}", IndividualAddress(**n)),
            Slot::Switch(sw) => write_groups(w, sw),
            Slot::Group(g) => write_optional_group(w, **g),
            Slot::Number { n, .. } => write!(w, "{}", **n),
        }
    }
}

/// Value is a value in ini notation, as get returns it.
#[derive(Clone, Copy)]
pub struct Value {
    buf: [u8; MAX_VALUE],
    len: usize,
}

impl Value {
    pub fn as_str(&self) -> &str {
        // Only whole str writes fill buf.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or_default()
    }
}

impl Write for Value {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

impl core::ops::Deref for Value {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(self.as_str(), f)
    }
}

impl PartialEq<&str> for Value {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// get returns the value of key in ini notation.
pub fn get<'a>(dev: &Device, key: &'a str) -> Result<Value, Error<'a>> {
    let field = Field::resolve(dev, key)?;
    let mut copy = *dev;
    let mut v = Value {
        buf: [0; MAX_VALUE],
        len: 0,
    };
    // MAX_VALUE fits every value.
    let _ = field.slot(&mut copy).write(&mut v);
    Ok(v)
}

/// set parses value and assigns it to key. A switch's group key takes a
/// comma separated list; temperature.group, humidity.group,
/// illuminance.group and occupancy.group take a single group address, or
/// "-" to unbind it; the other occupancy keys take a number within their
/// range; the distance window is not checked as a pair, see
/// Occupancy::distance_valid. dev is left untouched when value is invalid.
pub fn set<'a>(dev: &mut Device, key: &'a str, value: &'a str) -> Result<(), Error<'a>> {
    let field = Field::resolve(dev, key)?;
    field
        .slot(dev)
        .set(value)
        .map_err(|err| Error::Value { key, err })
}

/// Cursor writes into a byte buffer.
struct Cursor<'b> {
    buf: &'b mut [u8],
    len: usize,
}

impl Write for Cursor<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len + s.len();
        if end > self.buf.len() {
            return Err(fmt::Error);
        }
        self.buf[self.len..end].copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// format renders dev into buf as ini file contents that parse reads back,
/// and returns the part of buf it fills.
pub fn format<'b>(dev: &Device, buf: &'b mut [u8; MAX_FILE]) -> &'b [u8] {
    let mut c = Cursor { buf, len: 0 };
    // MAX_FILE fits any device.
    let _ = write_ini(&mut c, dev);
    let len = c.len;
    &c.buf[..len]
}

fn write_ini(w: &mut impl Write, dev: &Device) -> fmt::Result {
    write!(
        w,
        "[{DEVICE_SECTION}]\n{INDIVIDUAL_KEY} = {}\n",
        IndividualAddress(dev.individual)
    )?;
    for sw in &dev.switches {
        write!(w, "\n[{}]\n{GROUP_KEY} = ", sw.name)?;
        write_groups(w, sw)?;
        w.write_char('\n')?;
    }
    for (section, g) in [
        (TEMPERATURE_SECTION, dev.sensor.temperature),
        (HUMIDITY_SECTION, dev.sensor.humidity),
        (ILLUMINANCE_SECTION, dev.sensor.illuminance),
    ] {
        if g != 0 {
            write!(w, "\n[{section}]\n{GROUP_KEY} = {}\n", GroupAddress(g))?;
        }
    }
    // The occupancy section is left out while it holds nothing but the
    // defaults of a file without it.
    let o = &dev.occupancy;
    if *o != Occupancy::DEFAULT {
        write!(w, "\n[{OCCUPANCY_SECTION}]\n")?;
        if o.group != 0 {
            writeln!(w, "{GROUP_KEY} = {}", GroupAddress(o.group))?;
        }
        write!(
            w,
            "{TIMEOUT_KEY} = {}\n{THRESHOLD_KEY} = {}\n{FREQUENCY_KEY} = {}\n{INTEGRATION_KEY} = {}\n",
            o.timeout, o.threshold, o.frequency, o.integration
        )?;
        write!(
            w,
            "{MIN_DISTANCE_KEY} = {}\n{MAX_DISTANCE_KEY} = {}\n",
            o.min_distance, o.max_distance
        )?;
    }
    Ok(())
}

/// write_quoted writes s as Go's strconv.Quote, as knixx-knx does.
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
mod tests;
