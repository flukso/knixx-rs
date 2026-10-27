use super::*;
use crate::thunder::Sensor;
use std::string::ToString;
use std::vec::Vec;

fn fmt(dev: &Device) -> std::string::String {
    let mut buf = [0; MAX_FILE];
    std::str::from_utf8(format(dev, &mut buf)).unwrap().into()
}

fn round_trip(dev: &Device) {
    let text = fmt(dev);
    let again = parse(text.as_bytes()).unwrap();
    assert_eq!(again, *dev, "round trip");
}

// TestLoad, with parse in place of the host-only Load.
#[test]
fn parse_file() {
    let dev = parse(
        b"
[device]
individual = 7.8.1

[A]
group = 0/0/2

[B]
group = 0/0/3
",
    )
    .unwrap();
    assert_eq!(dev.individual, IndividualAddress::new3(7, 8, 1).0);
    for (i, (name, group)) in [
        ("A", GroupAddress::new3(0, 0, 2).0),
        ("B", GroupAddress::new3(0, 0, 3).0),
    ]
    .into_iter()
    .enumerate()
    {
        let sw = dev.switches[i];
        assert!(
            sw.name == name && sw.n_groups == 1 && sw.groups[0] == group,
            "switches[{i}] = {sw:?}"
        );
    }
}

// TestLoadErrors, without the unreadable file of the host-only Load.
#[test]
fn parse_errors() {
    for (name, content) in [
        (
            "missing device section",
            "[A]\ngroup = 0/0/2\n[B]\ngroup = 0/0/3\n",
        ),
        (
            "missing switch section",
            "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/2\n",
        ),
        (
            "missing group key",
            "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/2\n[B]\nfoo = bar\n",
        ),
        (
            "bad individual address",
            "[device]\nindividual = 7/8/1\n[A]\ngroup = 0/0/2\n[B]\ngroup = 0/0/3\n",
        ),
        (
            "bad group address",
            "[device]\nindividual = 7.8.1\n[A]\ngroup = nope\n[B]\ngroup = 0/0/3\n",
        ),
    ] {
        assert!(parse(content.as_bytes()).is_err(), "{name}: expected an error");
    }
}

#[test]
fn default() {
    let dev = parse(DEFAULT).unwrap();
    assert!(dev.switch("A").is_some() && dev.switch("B").is_some(), "{dev:?}");
}

#[test]
fn format_matches_default() {
    let dev = parse(DEFAULT).unwrap();
    assert_eq!(fmt(&dev).as_bytes(), DEFAULT);
}

#[test]
fn get_set() {
    let mut dev = parse(DEFAULT).unwrap();
    for key in KEYS {
        assert!(get(&dev, key).is_ok(), "get({key})");
    }

    set(&mut dev, "b.GROUP", "1/2/3").unwrap();
    set(&mut dev, "Device.Individual", "1.1.12").unwrap();
    assert_eq!(get(&dev, "B.group").unwrap(), "1/2/3");
    assert_eq!(get(&dev, "device.individual").unwrap(), "1.1.12");

    let before = dev;
    for (key, value) in [
        ("C.group", "0/0/1"),
        ("A.individual", "1.1.1"),
        ("device", "1.1.1"),
        ("A.group", "1.1.1"),
        ("device.individual", "0/0/1"),
        ("A.group", "nope"),
    ] {
        assert!(
            set(&mut dev, key, value).is_err(),
            "set({key}, {value}): expected an error"
        );
    }
    assert_eq!(dev, before, "failed set modified the device");

    round_trip(&dev);
}

// TestFileStore, with a store in memory in place of the host-only file.
#[test]
fn store() {
    struct Mem(Vec<u8>);
    impl Store for Mem {
        type Error = ();
        fn save(&mut self, data: &[u8]) -> Result<(), ()> {
            self.0 = data.to_vec();
            Ok(())
        }
    }
    let mut s = Mem(Vec::new());
    s.save(DEFAULT).unwrap();
    parse(&s.0).unwrap();
}

#[test]
fn group_lists() {
    let mut dev =
        parse(b"[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1,0/0/5 , 1/2/3\n[B]\ngroup = 0/0/2\n")
            .unwrap();
    assert_eq!(get(&dev, "A.group").unwrap(), "0/0/1, 0/0/5, 1/2/3");
    assert_eq!(get(&dev, "B.group").unwrap(), "0/0/2");
    round_trip(&dev);

    set(&mut dev, "B.group", "0/0/3,0/0/4").unwrap();
    assert_eq!(get(&dev, "B.group").unwrap(), "0/0/3, 0/0/4");

    let before = dev;
    for value in [
        "",
        "0/0/1,",
        ",0/0/1",
        "0/0/1,,0/0/2",
        "0/0/1, 0/0/1",
        "0/0/1, nope",
        "0/0/1,0/0/2,0/0/3,0/0/4,0/0/5,0/0/6,0/0/7,0/0/8,0/0/9",
    ] {
        assert!(
            set(&mut dev, "A.group", value).is_err(),
            "set(A.group, {value:?}): expected an error"
        );
    }
    assert_eq!(dev, before, "failed set modified the device");
    set(
        &mut dev,
        "A.group",
        "0/0/1,0/0/2,0/0/3,0/0/4,0/0/5,0/0/6,0/0/7,0/0/8",
    )
    .unwrap();
    assert!(
        parse(b"[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1, 0/0/1\n[B]\ngroup = 0/0/2\n").is_err(),
        "parse with a duplicate group: expected an error"
    );
}

#[test]
fn sensor() {
    const NO_SENSOR: &str = "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1\n[B]\ngroup = 0/0/2\n";
    // NO_SENSOR2 is NO_SENSOR as format writes it.
    const NO_SENSOR2: &str = "[device]\nindividual = 7.8.1\n\n[A]\ngroup = 0/0/1\n\n[B]\ngroup = 0/0/2\n";
    let with = |extra: &str| std::format!("{NO_SENSOR}{extra}");

    let mut dev = parse(NO_SENSOR.as_bytes()).unwrap();
    assert_eq!(dev.sensor, Sensor::default(), "sensor without a section");
    for key in ["temperature.group", "humidity.group", "illuminance.group"] {
        assert_eq!(get(&dev, key).unwrap(), "-", "{key}");
    }
    assert_eq!(
        fmt(&dev),
        "[device]\nindividual = 7.8.1\n\n[A]\ngroup = 0/0/1\n\n[B]\ngroup = 0/0/2\n"
    );

    set(&mut dev, "illuminance.group", "0/1/3").unwrap();
    assert_eq!(dev.sensor.illuminance, GroupAddress::new3(0, 1, 3).0);
    assert_eq!(
        fmt(&dev),
        std::format!("{NO_SENSOR2}\n[illuminance]\ngroup = 0/1/3\n")
    );
    round_trip(&dev);
    for value in ["", "0/1/3, 0/1/4", "0/0/0", "lux"] {
        assert!(
            set(&mut dev, "illuminance.group", value).is_err(),
            "set(illuminance.group, {value:?}): expected an error"
        );
    }
    set(&mut dev, "illuminance.group", "-").unwrap();
    assert_eq!(dev.sensor.illuminance, 0);

    // The [sensor] section of earlier versions is ignored.
    let dev = parse(with("[sensor]\ntemperature = 0/1/1\nhumidity = 0/1/2\n").as_bytes()).unwrap();
    assert_eq!(dev.sensor, Sensor::default(), "legacy [sensor]");

    let text = with("[humidity]\ngroup = 1/2/3\n");
    let mut dev = parse(text.as_bytes()).unwrap();
    let want = Sensor {
        humidity: GroupAddress::new3(1, 2, 3).0,
        ..Default::default()
    };
    assert_eq!(dev.sensor, want);
    assert_eq!(
        fmt(&dev),
        std::format!("{NO_SENSOR2}\n[humidity]\ngroup = 1/2/3\n")
    );

    set(&mut dev, "Temperature.Group", "0/1/1").unwrap();
    assert_eq!(get(&dev, "temperature.group").unwrap(), "0/1/1");
    round_trip(&dev);
    set(&mut dev, "humidity.group", "-").unwrap();
    assert_eq!(dev.sensor.humidity, 0);
    round_trip(&dev);

    let before = dev;
    for value in ["", "0/1/1, 0/1/2", "nope", "1.1.1", "0/0/0"] {
        assert!(
            set(&mut dev, "temperature.group", value).is_err(),
            "set(temperature.group, {value:?}): expected an error"
        );
    }
    assert_eq!(dev, before, "failed set modified the device");
    assert!(
        parse(with("[temperature]\ngroup = nope\n").as_bytes()).is_err(),
        "parse with a bad sensor group: expected an error"
    );
}

#[test]
fn occupancy() {
    const BASE: &str = "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1\n[B]\ngroup = 0/0/2\n";
    let mut dev = parse(BASE.as_bytes()).unwrap();
    let defaults = Occupancy {
        group: 0,
        timeout: thunder::DEFAULT_OCCUPANCY_TIMEOUT,
        threshold: thunder::DEFAULT_MOTION_THRESHOLD,
        frequency: thunder::DEFAULT_RANGING_FREQUENCY,
        integration: thunder::DEFAULT_INTEGRATION_TIME,
        min_distance: thunder::DEFAULT_MIN_DISTANCE,
        max_distance: thunder::DEFAULT_MAX_DISTANCE,
    };
    assert_eq!(dev.occupancy, defaults, "occupancy without a section");
    assert_eq!(
        fmt(&dev),
        "[device]\nindividual = 7.8.1\n\n[A]\ngroup = 0/0/1\n\n[B]\ngroup = 0/0/2\n"
    );

    let def = parse(DEFAULT).unwrap();
    let want = Occupancy {
        group: GroupAddress::new3(5, 0, 1).0,
        timeout: 60,
        threshold: 44,
        frequency: 1,
        integration: 5,
        min_distance: 400,
        max_distance: 1500,
    };
    assert_eq!(def.occupancy, want, "default occupancy");

    for (key, value) in [
        ("occupancy.group", "1/2/3"),
        ("Occupancy.Timeout", "65535"),
        ("occupancy.threshold", "32767"),
        ("occupancy.frequency", "60"),
        ("occupancy.integration", "1000"),
        ("occupancy.min_distance", "4000"),
        ("occupancy.max_distance", "400"),
    ] {
        set(&mut dev, key, value).unwrap();
        assert_eq!(get(&dev, key).unwrap(), value, "get({key})");
    }
    let again = parse(fmt(&dev).as_bytes()).unwrap();
    assert_eq!(again, dev, "round trip");
    set(&mut dev, "occupancy.group", "-").unwrap();
    assert_eq!(dev.occupancy.group, 0);
    let again = parse(fmt(&dev).as_bytes()).unwrap();
    assert_eq!(again, dev, "round trip after unbind");

    let before = dev;
    for (key, values) in [
        ("occupancy.timeout", &["0", "65536", "-1", "1.5", "", "sixty"][..]),
        ("occupancy.threshold", &["0", "32768", "-44", "nope"]),
        ("occupancy.group", &["0/0/0", "0/2/1, 0/2/2", "nope"]),
        ("occupancy.frequency", &["0", "61", "-1", "ten"]),
        ("occupancy.integration", &["1", "1001", "5ms"]),
        ("occupancy.min_distance", &["399", "4001", "far"]),
        ("occupancy.max_distance", &["0", "4001", "1.5m"]),
    ] {
        for value in values {
            assert!(
                set(&mut dev, key, value).is_err(),
                "set({key}, {value:?}): expected an error"
            );
        }
    }
    assert_eq!(dev, before, "failed set modified the device");
    for section in [
        "timeout = 0\n",
        "threshold = 40000\n",
        "group = x\n",
        "frequency = 61\n",
        "integration = 1\n",
        "min_distance = 399\n",
        "max_distance = 4001\n",
    ] {
        let text = std::format!("{BASE}[occupancy]\n{section}");
        assert!(
            parse(text.as_bytes()).is_err(),
            "parse with {section:?}: expected an error"
        );
    }
    for value in ["1", " 60 "] {
        set(&mut dev, "occupancy.timeout", value).unwrap();
    }

    // The window is not checked as a pair: an inverted one, as set above,
    // survives a round trip.
    let o = again.occupancy;
    assert!(
        o.min_distance == 4000 && o.max_distance == 400,
        "inverted window after round trip: {o:?}"
    );

    // A section written before the ranging keys existed keeps their defaults.
    let text = std::format!("{BASE}[occupancy]\ngroup = 5/0/1\ntimeout = 60\nthreshold = 44\n");
    let old = parse(text.as_bytes()).unwrap();
    assert_eq!(old.occupancy, want, "occupancy without ranging keys");
}

// The error texts of the Go firmware, and goini's handling of comments,
// whitespace, repeated sections and keys, beyond the Go tests.
#[test]
fn error_texts() {
    const BASE: &str = "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1\n[B]\ngroup = 0/0/2\n";
    let b = |s: &str| std::format!("{BASE}{s}");
    for (content, want) in [
        (
            "[A]\ngroup = 0/0/2\n[B]\ngroup = 0/0/3\n".to_string(),
            "config: [device] individual: missing",
        ),
        (
            "[device]\nindividual = 7/8/1\n[A]\ngroup = 0/0/2\n[B]\ngroup = 0/0/3\n".to_string(),
            "config: [device] individual: strconv.Atoi: parsing \"7/8/1\": invalid syntax",
        ),
        (
            "[device]\nindividual = 7.8.1\n[A]\ngroup = nope\n[B]\ngroup = 0/0/3\n".to_string(),
            "config: [A] group: invalid parsing strconv.Atoi: parsing \"nope\": invalid syntax",
        ),
        (
            "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1, 0/0/1\n[B]\ngroup = 0/0/2\n".to_string(),
            "config: [A] group: duplicate group address 0/0/1",
        ),
        (
            "[device]\nindividual = 7.8.1\n[A]\ngroup = 0/0/1,\n[B]\ngroup = 0/0/2\n".to_string(),
            "config: [A] group: empty group address",
        ),
        (
            "[device]\nindividual = 7.8.1\nfoo\n".to_string(),
            "config: Came accross an error : foo is NOT a valid key/value pair",
        ),
        (
            b("[occupancy]\ntimeout = 0\n"),
            "config: [occupancy] timeout: \"0\" is not a number from 1 to 65535",
        ),
        (
            b("[temperature]\ngroup = 0/0/0\n"),
            "config: [temperature] group: invalid group address 0/0/0",
        ),
        (
            b("[temperature]\ngroup = 0\n"),
            "config: [temperature] group: invalid raw group address in 0",
        ),
        (
            "  ; c\n# c\n[device]\n individual=7.8.1 \n[A]\ngroup=1\n[A]\nfoo=1\n[B]\ngroup = 0/0/2\n"
                .to_string(),
            "config: [A] group: missing",
        ),
        (
            "[device]\nindividual = 7.8.1\nindividual = 1.1.1\n[A]\ngroup = 0/0/1\n[ B ]\ngroup = 0/0/2\n"
                .to_string(),
            "config: [B] group: missing",
        ),
        (
            std::string::String::from_utf8_lossy(b"\xff").into_owned() + "x",
            "config: Came accross an error : \u{fffd}x is NOT a valid key/value pair",
        ),
    ] {
        assert_eq!(
            parse(content.as_bytes()).unwrap_err().to_string(),
            want,
            "{content:?}"
        );
    }
    assert_eq!(parse(b"\xff").unwrap_err().to_string(), "config: invalid UTF-8");

    let ok = parse(b"; c\r\n# c\r\n[device]\r\n\t individual=1.1.1\r\nindividual = 7.8.1\r\n[A]\r\ngroup=1\r\n[B]\r\ngroup = 0/0/2 \r\n")
        .unwrap();
    assert_eq!(ok.individual, IndividualAddress::new3(7, 8, 1).0);
    assert_eq!(ok.switches[0].group_list(), &[1]);

    let mut dev = parse(DEFAULT).unwrap();
    for (key, value, want) in [
        ("C.group", "1", "config: unknown key \"C.group\""),
        ("device", "x", "config: unknown key \"device\""),
        (
            "occupancy.timeout",
            " sixty\t",
            "config: occupancy.timeout: \" sixty\\t\" is not a number from 1 to 65535",
        ),
        (
            "A.group",
            "0/0/1,0/0/2,0/0/3,0/0/4,0/0/5,0/0/6,0/0/7,0/0/8,0/0/9",
            "config: A.group: more than 8 group addresses",
        ),
        (
            "device.individual",
            " 1.1.1",
            "config: device.individual: strconv.Atoi: parsing \" 1\": invalid syntax",
        ),
        (
            "occupancy.threshold",
            "+5",
            "config: occupancy.threshold: \"+5\" is not a number from 1 to 32767",
        ),
        (
            "A.group",
            "x",
            "config: A.group: invalid parsing strconv.Atoi: parsing \"x\": invalid syntax",
        ),
    ] {
        assert_eq!(
            set(&mut dev, key, value).unwrap_err().to_string(),
            want,
            "set({key}, {value:?})"
        );
    }
}

#[test]
fn format_fits() {
    let sw = |name| {
        Switch::new(
            name,
            &[0xffff, 0xfffe, 0xfffd, 0xfffc, 0xfffb, 0xfffa, 0xfff9, 0xfff8],
        )
    };
    let dev = Device {
        individual: 0xffff,
        switches: [sw("A"), sw("B")],
        sensor: Sensor {
            temperature: 0xffff,
            humidity: 0xffff,
            illuminance: 0xffff,
        },
        occupancy: Occupancy {
            group: 0xffff,
            timeout: 65535,
            threshold: 65535,
            frequency: 65535,
            integration: 65535,
            min_distance: 65535,
            max_distance: 65535,
        },
    };
    let text = fmt(&dev);
    assert!(text.ends_with("max_distance = 65535\n"), "{text}");
    assert_eq!(get(&dev, "A.group").unwrap().len(), MAX_VALUE);
}
