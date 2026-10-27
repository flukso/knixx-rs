//! The sniffer's rendering of the frames on the bus.

use knixx_device::thunder::{Device, MAX_GROUPS, NUM_SWITCHES};
use knixx_knx::dpt::{Dpt1001, Dpt1018};
use knixx_knx::tp1::{AddrType, Apci, Telegram, Transport};

use crate::format::{Line, f16};

#[cfg(test)]
mod tests;

/// The datapoint type of a bound group.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DptKind {
    #[default]
    None,
    Dpt1001,
    Dpt1018,
    Dpt9001,
    Dpt9004,
    Dpt9007,
    /// A main/middle group with bindings of different types.
    Conflict,
}

/// The upper bound of the DPT 9 types, in hundredths.
const MAX_F16: i32 = 67_076_000;

impl DptKind {
    /// Returns the DPT ID of k. A DPT 9 type also gets the unit of its values
    /// and their lower bound in hundredths.
    fn id(self) -> Option<(&'static str, &'static str, i32)> {
        match self {
            DptKind::Dpt1001 => Some(("1.001", "", 0)),
            DptKind::Dpt1018 => Some(("1.018", "", 0)),
            DptKind::Dpt9001 => Some(("9.001", " °C", -27_300)),
            DptKind::Dpt9004 => Some(("9.004", " lux", 0)),
            DptKind::Dpt9007 => Some(("9.007", " %", 0)),
            DptKind::None | DptKind::Conflict => None,
        }
    }
}

/// A group bound in the configuration.
#[derive(Clone, Copy, Debug, Default)]
struct Bound {
    group: u16,
    name: &'static str,
    kind: DptKind,
}

const MAX_BOUND: usize = NUM_SWITCHES * MAX_GROUPS + 4;

/// Renders frames for the sniffer. It decodes the value of a group with the
/// datapoint type of its binding in the device: DPT 1.001 for a switch group,
/// DPT 9 for a sensor group and DPT 1.018 for the occupancy group. A group
/// that is not bound gets the type of the bindings on its main/middle group,
/// if they agree.
///
/// The bindings are looked up in tables that are built on the first frame
/// and dropped by reset when the configuration changes.
pub(crate) struct Decoder {
    valid: bool,
    bound: [Bound; MAX_BOUND],
    n: usize,
    /// The type of each main/middle group, indexed by the high octet of the
    /// group address.
    lines: [DptKind; 256],
}

impl Decoder {
    pub(crate) const fn new() -> Self {
        Decoder {
            valid: false,
            bound: [Bound {
                group: 0,
                name: "",
                kind: DptKind::None,
            }; MAX_BOUND],
            n: 0,
            lines: [DptKind::None; 256],
        }
    }

    /// Drops the tables, for a change of the device.
    pub(crate) fn reset(&mut self) {
        self.valid = false;
    }

    fn add(&mut self, group: u16, name: &'static str, kind: DptKind) {
        if group == 0 {
            return;
        }
        self.bound[self.n] = Bound { group, name, kind };
        self.n += 1;
        let line = &mut self.lines[(group >> 8) as usize];
        match *line {
            DptKind::None => *line = kind,
            k if k == kind => {}
            _ => *line = DptKind::Conflict,
        }
    }

    /// Fills the tables from dev.
    fn build(&mut self, dev: &Device) {
        self.n = 0;
        self.lines = [DptKind::None; 256];
        for sw in &dev.switches {
            for &g in sw.group_list() {
                self.add(g, sw.name, DptKind::Dpt1001);
            }
        }
        self.add(dev.sensor.temperature, "temperature", DptKind::Dpt9001);
        self.add(dev.sensor.humidity, "humidity", DptKind::Dpt9007);
        self.add(dev.sensor.illuminance, "illuminance", DptKind::Dpt9004);
        self.add(dev.occupancy.group, "occupancy", DptKind::Dpt1018);
        self.valid = true;
    }

    /// Pushes the rendering of a frame from the bus to dst: the source and
    /// destination, the service with its data, the datapoint type and value
    /// of a group, and the priority and routing counter.
    pub(crate) fn push_frame(&mut self, dst: &mut Line, dev: &Device, frame: &[u8]) {
        dst.push_str("L_Data.ind: ");
        let mut tg = Telegram::default();
        if let Err(e) = tg.unpack(frame) {
            dst.push_str(e.as_str());
            return;
        }

        dst.push_individual_addr(tg.src);
        dst.push_str(" > ");
        if tg.npci.addr_type == AddrType::Group {
            dst.push_group_addr(tg.dest);
        } else {
            dst.push_individual_addr(tg.dest);
        }
        dst.push(b' ');
        dst.push_str(tg.apci.as_str());
        let group_value = tg.apci == Apci::GroupValueWrite || tg.apci == Apci::GroupValueResponse;
        let data = tg.data();
        if tg.apci == Apci::GroupValueRead || data.is_empty() {
        } else if group_value && data.len() == 1 {
            // A value of up to 6 bits, in the APCI octet.
            dst.push_str(" $");
            dst.push_hex2(data[0]);
        } else if group_value {
            // A longer value follows the APCI octet.
            dst.push(b' ');
            dst.push_hex_bytes(&data[1..]);
        } else {
            dst.push(b' ');
            dst.push_hex_bytes(data);
        }
        self.push_value(dst, dev, &tg);
        dst.push_str(", ");
        dst.push_str(tg.ctrl.priority.as_str());
        dst.push_str(" priority, routing ");
        dst.push_dec(tg.npci.routing as u64);
        if tg.tpci.transport == Transport::NumberedDataPacket {
            dst.push_str(", sequence ");
            dst.push_dec(tg.tpci.sequence as u64);
        }
        if tg.ctrl.repeat {
            dst.push_str(", repeated");
        }
    }

    /// Pushes, in parentheses, the datapoint type of the group a telegram is
    /// sent to, followed by the value of a group value write or response:
    ///   - a bound group with its binding, e.g. "A, DPT 1.001: On";
    ///   - a group on the main/middle group of bindings of one type with that
    ///     type, e.g. "DPT 9.001: 21.00 °C". A value that does not unpack
    ///     shows that the group is of another type; nothing is pushed then.
    ///
    /// It pushes nothing for any other group, for 0/0/0 and for an
    /// individual address.
    fn push_value(&mut self, dst: &mut Line, dev: &Device, tg: &Telegram) {
        if tg.npci.addr_type != AddrType::Group || tg.dest == 0 {
            return;
        }
        if !self.valid {
            self.build(dev);
        }
        let (name, mut kind) = match self.bound[..self.n].iter().find(|b| b.group == tg.dest) {
            Some(b) => (b.name, b.kind),
            None => ("", DptKind::None),
        };
        if kind == DptKind::None {
            kind = self.lines[(tg.dest >> 8) as usize];
        }
        let Some((id, unit, min)) = kind.id() else {
            return;
        };
        let start = dst.len();
        dst.push_str(" (");
        if !name.is_empty() {
            dst.push_str(name);
            dst.push_str(", ");
        }
        dst.push_str("DPT ");
        dst.push_str(id);
        if tg.apci != Apci::GroupValueWrite && tg.apci != Apci::GroupValueResponse {
            dst.push(b')');
            return;
        }
        // The value, if it has the length and range of the type.
        let data = tg.data();
        let value = match kind {
            DptKind::Dpt1001 | DptKind::Dpt1018 => match data {
                [b] => {
                    let on = b & 1 == 1;
                    Some(Value::Text(if kind == DptKind::Dpt1001 {
                        Dpt1001(on).as_str()
                    } else {
                        Dpt1018(on).as_str()
                    }))
                }
                _ => None,
            },
            _ => f16(data)
                .filter(|&h| h >= min && h <= MAX_F16)
                .map(Value::Hundredths),
        };
        let Some(value) = value else {
            if name.is_empty() {
                dst.truncate(start);
            } else {
                dst.push(b')');
            }
            return;
        };
        dst.push_str(": ");
        match value {
            Value::Text(text) => dst.push_str(text),
            Value::Hundredths(h) => {
                dst.push_hundredths(h);
                dst.push_str(unit);
            }
        }
        dst.push(b')');
    }
}

/// A decoded value: the text of a DPT 1 value, or DPT 9 hundredths.
enum Value {
    Text(&'static str),
    Hundredths(i32),
}
