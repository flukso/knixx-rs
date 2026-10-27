//! The knixx thunder board: its individual address, its switches, its
//! sensors and the occupancy they detect, and the telegrams they send.

use knixx_knx::dpt::Dpt1001;
use knixx_knx::tp1::{AddrType, Apci, Ctrl, MAX_PAYLOAD, Npci, Priority, Telegram, Tpci, Transport};

pub const NUM_SWITCHES: usize = 2;
/// MAX_GROUPS bounds the group addresses a switch can be bound to.
pub const MAX_GROUPS: usize = 8;

/// Switch sends to each of its group addresses `groups[..n_groups]`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Switch {
    pub name: &'static str,
    pub groups: [u16; MAX_GROUPS],
    pub n_groups: u8,
}

impl Switch {
    /// new returns a switch bound to groups. Groups beyond MAX_GROUPS are
    /// dropped.
    pub fn new(name: &'static str, groups: &[u16]) -> Self {
        let mut sw = Switch {
            name,
            ..Default::default()
        };
        let n = groups.len().min(MAX_GROUPS);
        sw.groups[..n].copy_from_slice(&groups[..n]);
        sw.n_groups = n as u8;
        sw
    }

    /// group_list returns the group addresses the switch is bound to.
    pub fn group_list(&self) -> &[u16] {
        &self.groups[..(self.n_groups as usize).min(MAX_GROUPS)]
    }
}

/// Sensor holds the group addresses that temperature, humidity and
/// illuminance readings are published to. 0 leaves a value unbound.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sensor {
    pub temperature: u16,
    pub humidity: u16,
    pub illuminance: u16,
}

/// Occupancy configures the occupancy derived from the motion indicator of
/// the time-of-flight sensor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// group receives DPT 1.018 occupancy telegrams. 0 leaves it unbound.
    pub group: u16,
    /// timeout is the time in seconds without motion after which occupancy
    /// is turned off.
    pub timeout: u16,
    /// threshold is the motion intensity above which a zone detects motion.
    pub threshold: u16,
    /// frequency is the ranging frequency in Hz of the sensor's autonomous
    /// mode, at resolution 4x4.
    pub frequency: u16,
    /// integration is the integration time in ms of each frame.
    pub integration: u16,
    /// min_distance and max_distance bound in mm the distance window in
    /// which the sensor detects motion, see distance_valid.
    pub min_distance: u16,
    pub max_distance: u16,
}

// Occupancy defaults and limits.
pub const DEFAULT_OCCUPANCY_TIMEOUT: u16 = 60;
pub const MIN_OCCUPANCY_TIMEOUT: u16 = 1;
pub const MAX_OCCUPANCY_TIMEOUT: u16 = 65535;
pub const DEFAULT_MOTION_THRESHOLD: u16 = 44;
pub const MIN_MOTION_THRESHOLD: u16 = 1;
/// MAX_MOTION_THRESHOLD is the largest threshold the sensor can store.
pub const MAX_MOTION_THRESHOLD: u16 = 32767;
pub const DEFAULT_RANGING_FREQUENCY: u16 = 1;
pub const MIN_RANGING_FREQUENCY: u16 = 1;
/// MAX_RANGING_FREQUENCY is the limit of resolution 4x4.
pub const MAX_RANGING_FREQUENCY: u16 = 60;
pub const DEFAULT_INTEGRATION_TIME: u16 = 5;
pub const MIN_INTEGRATION_TIME: u16 = 2;
pub const MAX_INTEGRATION_TIME: u16 = 1000;
pub const DEFAULT_MIN_DISTANCE: u16 = 400;
pub const DEFAULT_MAX_DISTANCE: u16 = 1500;
/// MIN_MOTION_DISTANCE and MAX_MOTION_DISTANCE bound each distance;
/// MAX_MOTION_SPAN bounds the width of the window.
pub const MIN_MOTION_DISTANCE: u16 = 400;
pub const MAX_MOTION_DISTANCE: u16 = 4000;
pub const MAX_MOTION_SPAN: u16 = 1500;

impl Occupancy {
    /// DEFAULT is the occupancy of a configuration without the section: no
    /// group and the default of every setting.
    pub const DEFAULT: Occupancy = Occupancy {
        group: 0,
        timeout: DEFAULT_OCCUPANCY_TIMEOUT,
        threshold: DEFAULT_MOTION_THRESHOLD,
        frequency: DEFAULT_RANGING_FREQUENCY,
        integration: DEFAULT_INTEGRATION_TIME,
        min_distance: DEFAULT_MIN_DISTANCE,
        max_distance: DEFAULT_MAX_DISTANCE,
    };

    /// distance_valid reports whether the sensor accepts the distance
    /// window: on top of the limits of each distance, max_distance must
    /// exceed min_distance by at most MAX_MOTION_SPAN. It mirrors
    /// vl53l7cx.ValidMotionDistance.
    pub fn distance_valid(&self) -> bool {
        self.min_distance >= MIN_MOTION_DISTANCE
            && self.max_distance <= MAX_MOTION_DISTANCE
            && self.max_distance > self.min_distance
            && self.max_distance - self.min_distance <= MAX_MOTION_SPAN
    }

    /// period_stretched reports whether the integration time plus the
    /// sensor's 1 ms overhead does not fit the ranging period, in which case
    /// the sensor lengthens the period to fit and ranges below frequency.
    pub fn period_stretched(&self) -> bool {
        // (integration + 1) ms >= 1000/frequency ms, without the rounding
        // of the division.
        (self.integration as u32 + 1) * self.frequency as u32 >= 1000
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Device {
    pub individual: u16,
    pub switches: [Switch; NUM_SWITCHES],
    pub sensor: Sensor,
    pub occupancy: Occupancy,
}

impl Device {
    /// switch looks up a switch by its case-insensitive name. It returns
    /// None when no switch matches.
    pub fn switch(&self, name: &str) -> Option<&Switch> {
        self.switch_index(name).map(|i| &self.switches[i as usize])
    }

    /// switch_mut is switch for a mutable switch.
    pub fn switch_mut(&mut self, name: &str) -> Option<&mut Switch> {
        self.switch_index(name).map(|i| &mut self.switches[i as usize])
    }

    /// switch_index returns the index of the switch with the
    /// case-insensitive name.
    pub fn switch_index(&self, name: &str) -> Option<u8> {
        self.switches
            .iter()
            .position(|sw| sw.name.eq_ignore_ascii_case(name))
            .map(|i| i as u8)
    }

    /// telegram builds the group value write that sets group to the
    /// requested state.
    pub fn telegram(&self, group: u16, on: bool) -> Telegram {
        self.group_write(group, &Dpt1001(on).pack())
    }

    /// group_write builds the group value write of data, a packed datapoint
    /// value, to group. Data over MAX_PAYLOAD octets gives a telegram whose
    /// pack_to fails, as in Go.
    pub fn group_write(&self, group: u16, data: &[u8]) -> Telegram {
        let n = data.len().min(MAX_PAYLOAD);
        let mut tg = Telegram {
            ctrl: Ctrl {
                repeat: false,
                priority: Priority::Low,
            },
            src: self.individual,
            dest: group,
            npci: Npci {
                addr_type: AddrType::Group,
                routing: 0,
                length: data.len().min(u8::MAX as usize) as u8,
            },
            tpci: Tpci {
                transport: Transport::UnnumberedDataPacket,
                sequence: 0,
            },
            apci: Apci::GroupValueWrite,
            data: [0; MAX_PAYLOAD],
        };
        tg.data[..n].copy_from_slice(&data[..n]);
        tg
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use knixx_knx::tp1::MAX_FRAME;
    use knixx_knx::{GroupAddress, IndividualAddress};

    fn test_device() -> Device {
        Device {
            individual: IndividualAddress::new3(7, 8, 1).0,
            switches: [
                Switch::new("A", &[GroupAddress::new3(0, 0, 2).0]),
                Switch::new("B", &[GroupAddress::new3(0, 0, 3).0]),
            ],
            ..Default::default()
        }
    }

    #[test]
    fn switch_lookup() {
        let d = test_device();
        assert!(core::ptr::eq(d.switch("A").unwrap(), &d.switches[0]));
        assert!(core::ptr::eq(d.switch("b").unwrap(), &d.switches[1]));
        assert!(d.switch("C").is_none());
    }

    #[test]
    fn telegram() {
        let d = test_device();
        let tests: &[(&str, bool, &[u8])] = &[
            (
                "A",
                false,
                &[0xbc, 0x78, 0x01, 0x00, 0x02, 0x81, 0x00, 0x80, 0x39],
            ),
            ("A", true, &[0xbc, 0x78, 0x01, 0x00, 0x02, 0x81, 0x00, 0x81, 0x38]),
            (
                "B",
                false,
                &[0xbc, 0x78, 0x01, 0x00, 0x03, 0x81, 0x00, 0x80, 0x38],
            ),
            ("B", true, &[0xbc, 0x78, 0x01, 0x00, 0x03, 0x81, 0x00, 0x81, 0x39]),
        ];
        for &(name, on, want) in tests {
            let tg = d.telegram(d.switch(name).unwrap().groups[0], on);
            let mut buf = [0; MAX_FRAME];
            let got = tg.pack_to(&mut buf).unwrap();
            assert_eq!(got, want, "{name} on={on}");
        }
    }

    // knixx-knx has no DPT 9.xxx, so the data is knx-go's
    // dpt.DPT_9001(21.5).Pack() and dpt.DPT_9007(48.2).Pack().
    #[test]
    fn group_write_dpt9() {
        let d = test_device();
        let tests: &[(&str, u16, &[u8], &[u8])] = &[
            (
                "21.5 °C to 1/0/1",
                GroupAddress::new3(1, 0, 1).0,
                &[0x00, 0x0c, 0x33],
                &[0xbc, 0x78, 0x01, 0x08, 0x01, 0x83, 0x00, 0x80, 0x0c, 0x33, 0x0f],
            ),
            (
                "48.2 % to 1/0/2",
                GroupAddress::new3(1, 0, 2).0,
                &[0x00, 0x14, 0xb5],
                &[0xbc, 0x78, 0x01, 0x08, 0x02, 0x83, 0x00, 0x80, 0x14, 0xb5, 0x92],
            ),
        ];
        for &(name, group, data, want) in tests {
            let tg = d.group_write(group, data);
            let mut buf = [0; MAX_FRAME];
            let got = tg.pack_to(&mut buf).unwrap();
            assert_eq!(got, want, "{name}");
        }
    }

    #[test]
    fn group_write_too_long() {
        let d = test_device();
        let tg = d.group_write(1, &[0; MAX_PAYLOAD + 1]);
        assert!(tg.pack_to(&mut [0; MAX_FRAME]).is_err());
    }

    #[test]
    fn new_switch() {
        let sw = Switch::new("A", &[1, 2, 3]);
        assert_eq!(sw.group_list(), &[1, 2, 3]);
        let many = [0u16; MAX_GROUPS + 2];
        let sw = Switch::new("B", &many);
        assert_eq!(sw.n_groups as usize, MAX_GROUPS);
    }

    #[test]
    fn period_stretched() {
        for (hz, ms, want) in [
            (1, 5, false),
            (60, 15, false),
            (60, 16, true),
            (10, 99, true),
            (10, 98, false),
            (2, 400, false),
            (1, 1000, true),
        ] {
            let o = Occupancy {
                frequency: hz,
                integration: ms,
                ..Default::default()
            };
            assert_eq!(o.period_stretched(), want, "{hz} Hz, {ms} ms");
        }
    }

    // The same table as distancePairs in internal/vl53l7cx.
    #[test]
    fn distance_valid() {
        for (min, max, valid) in [
            (400, 1500, true),
            (400, 1900, true),
            (2500, 4000, true),
            (399, 1000, false),
            (1000, 4001, false),
            (1000, 2501, false),
            (1500, 1500, false),
            (1500, 1000, false),
        ] {
            let o = Occupancy {
                min_distance: min,
                max_distance: max,
                ..Default::default()
            };
            assert_eq!(o.distance_valid(), valid, "{min}-{max} mm");
        }
    }
}
