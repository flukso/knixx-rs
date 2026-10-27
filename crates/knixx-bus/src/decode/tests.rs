use std::collections::VecDeque;
use std::format;
use std::string::String;
use std::vec::Vec;

use core::sync::atomic::Ordering;

use knixx_device::thunder::{Device, Sensor};
use knixx_knx::dpt::{Dpt1001, Dpt1018};
use knixx_knx::tp1::{AddrType, Apci, Ctrl, MAX_FRAME, Npci, Priority, Telegram, Transport};

use super::*;
use crate::format::f16_write;
use crate::tests::{FakeDevice, run, test_device};
use crate::{Command, Flags};

/// Returns the group address main/middle/sub.
const fn ga(main: u16, middle: u16, sub: u16) -> u16 {
    main << 11 | middle << 8 | sub
}

/// Binds a group to every decoded datapoint type, on the main/middle groups
/// of the default configuration.
fn decode_device() -> Device {
    let mut dev = test_device();
    dev.sensor = Sensor {
        temperature: ga(2, 0, 1),
        humidity: ga(3, 0, 1),
        illuminance: ga(4, 0, 1),
    };
    dev.occupancy.group = ga(5, 0, 1);
    dev
}

/// Renders frame with d into a new string.
fn render(d: &mut Decoder, dev: &Device, frame: &[u8]) -> String {
    let mut line = Line::new();
    d.push_frame(&mut line, dev, frame);
    String::from_utf8(line.as_bytes().to_vec()).unwrap()
}

/// Renders frame with a decoder for dev.
fn describe(dev: &Device, frame: &[u8]) -> String {
    render(&mut Decoder::new(), dev, frame)
}

/// The packed DPT 9 value of h hundredths, as knx-go packs the values used
/// here.
fn dpt9(h: i32) -> [u8; 3] {
    f16_write(0, h).data
}

/// Packs a telegram from 1.1.5 to dest with service apci and data.
fn frame_of(dest: u16, apci: Apci, data: &[u8], edit: impl FnOnce(&mut Telegram)) -> Vec<u8> {
    let mut tg = Telegram {
        ctrl: Ctrl {
            repeat: false,
            priority: Priority::Low,
        },
        src: 0x1105,
        dest,
        npci: Npci {
            addr_type: AddrType::Group,
            routing: 6,
            length: 0,
        },
        apci,
        ..Telegram::default()
    };
    tg.set_data(data).unwrap();
    edit(&mut tg);
    let mut buf = [0; MAX_FRAME];
    tg.pack_to(&mut buf).unwrap().to_vec()
}

fn plain(dest: u16, apci: Apci, data: &[u8]) -> Vec<u8> {
    frame_of(dest, apci, data, |_| {})
}

#[test]
fn describe_frame() {
    let dev = decode_device();
    let tests: Vec<(&str, Vec<u8>, &str)> = Vec::from([
        (
            "switch on",
            plain(0x0002, Apci::GroupValueWrite, &Dpt1001(true).pack()),
            "L_Data.ind: 1.1.5 > 0/0/2 GroupValueWrite $01 (A, DPT 1.001: On), low priority, routing 6",
        ),
        (
            "second group of a switch, off",
            plain(0x0004, Apci::GroupValueWrite, &Dpt1001(false).pack()),
            "L_Data.ind: 1.1.5 > 0/0/4 GroupValueWrite $00 (B, DPT 1.001: Off), low priority, routing 6",
        ),
        (
            "temperature response",
            plain(ga(2, 0, 1), Apci::GroupValueResponse, &dpt9(2100)),
            "L_Data.ind: 1.1.5 > 2/0/1 GroupValueResponse 0c 1a (temperature, DPT 9.001: 21.00 °C), low priority, routing 6",
        ),
        (
            "humidity",
            plain(ga(3, 0, 1), Apci::GroupValueWrite, &dpt9(4000)),
            "(humidity, DPT 9.007: 40.00 %)",
        ),
        (
            "illuminance",
            plain(ga(4, 0, 1), Apci::GroupValueWrite, &dpt9(32000)),
            "(illuminance, DPT 9.004: 320.00 lux)",
        ),
        (
            "occupancy",
            plain(ga(5, 0, 1), Apci::GroupValueWrite, &Dpt1018(true).pack()),
            "GroupValueWrite $01 (occupancy, DPT 1.018: occupied)",
        ),
        (
            "group read",
            plain(0x0002, Apci::GroupValueRead, &[0x00]),
            "L_Data.ind: 1.1.5 > 0/0/2 GroupValueRead (A, DPT 1.001), low priority, routing 6",
        ),
        (
            "unbound group",
            plain(0x0909, Apci::GroupValueWrite, &[0x00, 0x12, 0x34]),
            "L_Data.ind: 1.1.5 > 1/1/9 GroupValueWrite 12 34, low priority, routing 6",
        ),
        (
            "individual, numbered, repeated, system priority",
            frame_of(0x7801, Apci::MemoryRead, &[0x03, 0x00, 0x60], |tg| {
                tg.npci.addr_type = AddrType::Individual;
                tg.tpci.transport = Transport::NumberedDataPacket;
                tg.tpci.sequence = 3;
                tg.ctrl.repeat = true;
                tg.ctrl.priority = Priority::System;
            }),
            "L_Data.ind: 1.1.5 > 7.8.1 MemoryRead 03 00 60, system priority, routing 6, sequence 3, repeated",
        ),
        (
            "undecodable value",
            plain(ga(2, 0, 1), Apci::GroupValueWrite, &[0x00, 0x0c]),
            "GroupValueWrite 0c (temperature, DPT 9.001), low priority",
        ),
        (
            "inferred switch line",
            plain(ga(0, 0, 7), Apci::GroupValueWrite, &Dpt1001(true).pack()),
            "L_Data.ind: 1.1.5 > 0/0/7 GroupValueWrite $01 (DPT 1.001: On), low priority",
        ),
        (
            "inferred temperature line",
            plain(ga(2, 0, 9), Apci::GroupValueWrite, &dpt9(2100)),
            "L_Data.ind: 1.1.5 > 2/0/9 GroupValueWrite 0c 1a (DPT 9.001: 21.00 °C), low priority",
        ),
        (
            "inferred illuminance line",
            plain(ga(4, 0, 2), Apci::GroupValueWrite, &dpt9(32000)),
            "(DPT 9.004: 320.00 lux)",
        ),
        (
            "inferred occupancy line",
            plain(ga(5, 0, 3), Apci::GroupValueWrite, &Dpt1018(false).pack()),
            "(DPT 1.018: not occupied)",
        ),
        (
            "inferred read",
            plain(ga(2, 0, 9), Apci::GroupValueRead, &[0x00]),
            "2/0/9 GroupValueRead (DPT 9.001), low priority",
        ),
        (
            "inferred, value of another type",
            plain(ga(2, 0, 9), Apci::GroupValueWrite, &[0x00, 0x2a]),
            "2/0/9 GroupValueWrite 2a, low priority",
        ),
        (
            "other middle group",
            plain(ga(2, 1, 1), Apci::GroupValueWrite, &dpt9(2100)),
            "2/1/1 GroupValueWrite 0c 1a, low priority",
        ),
        (
            "other main group",
            plain(ga(6, 0, 1), Apci::GroupValueWrite, &dpt9(2100)),
            "6/0/1 GroupValueWrite 0c 1a, low priority",
        ),
        (
            "broadcast",
            plain(0, Apci::GroupValueWrite, &[0x01]),
            "0/0/0 GroupValueWrite $01, low priority",
        ),
        (
            "bad checksum",
            [0xbc, 0x11, 0x05, 0x00, 0x02, 0xe1, 0x00, 0x81, 0x00].to_vec(),
            "L_Data.ind: tp1: checksum mismatch",
        ),
    ]);
    for (name, frame, want) in tests {
        let got = describe(&dev, &frame);
        assert!(
            got.contains(want),
            "{name}: describe({frame:02x?}) = {got:?}, want {want:?}"
        );
    }
}

/// Feeds a received frame to the engine, which reads it while it waits for
/// the stat replies: it is decoded only while sniffing.
#[test]
fn sniff_decode() {
    let flags = Flags::new();
    let frame = plain(0x0003, Apci::GroupValueWrite, &Dpt1001(true).pack());
    let want = "L_Data.ind: 1.1.5 > 0/0/3 GroupValueWrite $01 (B, DPT 1.001: On)";
    for sniff in [false, true] {
        flags.sniff.store(sniff, Ordering::Relaxed);
        let mut d = FakeDevice {
            rx: frame.clone(),
            ..Default::default()
        };
        let got = run(&mut d, &test_device(), &flags, &[Command::Stat]);
        assert_eq!(
            got.text.contains(want),
            sniff,
            "sniff={sniff}: output {:?}",
            got.text
        );
    }
}

#[test]
fn decode_conflicting_line() {
    let mut dev = decode_device();
    dev.sensor.humidity = ga(2, 0, 2);
    let frame = plain(ga(2, 0, 3), Apci::GroupValueWrite, &dpt9(2100));
    let (got, want) = (
        describe(&dev, &frame),
        "2/0/3 GroupValueWrite 0c 1a, low priority",
    );
    assert!(got.contains(want), "got {got:?}, want {want:?}");
    // The bound groups keep their own type.
    let frame = plain(ga(2, 0, 2), Apci::GroupValueWrite, &dpt9(4000));
    let (got, want) = (describe(&dev, &frame), "(humidity, DPT 9.007: 40.00 %)");
    assert!(got.contains(want), "got {got:?}, want {want:?}");
}

#[test]
fn decoder_reset() {
    let mut dev = decode_device();
    let mut d = Decoder::new();
    let frame = plain(ga(2, 0, 9), Apci::GroupValueWrite, &dpt9(2100));
    let got = render(&mut d, &dev, &frame);
    assert!(got.contains("DPT 9.001"), "before the change: {got:?}");
    dev.sensor.temperature = ga(6, 0, 1);
    let got = render(&mut d, &dev, &frame);
    assert!(
        got.contains("DPT 9.001"),
        "the tables follow dev before reset: {got:?}"
    );
    d.reset();
    let got = render(&mut d, &dev, &frame);
    assert!(!got.contains("DPT"), "after reset: {got:?}");
}

/// Checks that the engine decodes with the configuration of the last
/// Config. The frames arrive with the stat replies.
#[test]
fn sniff_decode_config() {
    let flags = Flags::new();
    flags.sniff.store(true, Ordering::Relaxed);
    let mut moved = decode_device();
    moved.sensor.temperature = ga(6, 0, 1);
    let replies = [0x07, 0x4b, 0xfb];
    let mut d = FakeDevice {
        on_stat: VecDeque::from([
            [
                plain(ga(2, 0, 9), Apci::GroupValueWrite, &dpt9(2100)),
                replies.to_vec(),
            ]
            .concat(),
            [
                plain(ga(2, 0, 9), Apci::GroupValueWrite, &dpt9(2100)),
                plain(ga(6, 0, 9), Apci::GroupValueWrite, &dpt9(2200)),
                replies.to_vec(),
            ]
            .concat(),
        ]),
        ..Default::default()
    };
    let got = run(
        &mut d,
        &decode_device(),
        &flags,
        &[Command::Stat, Command::Config(moved), Command::Stat],
    );
    for want in [
        "2/0/9 GroupValueWrite 0c 1a (DPT 9.001: 21.00 °C)",
        "2/0/9 GroupValueWrite 0c 1a, low priority",
        "6/0/9 GroupValueWrite 0c 4c (DPT 9.001: 22.00 °C)",
    ] {
        assert!(got.text.contains(want), "output {:?} lacks {want:?}", got.text);
    }
}

/// Renders the DPT 9 value in data.
fn hundredths(data: &[u8]) -> String {
    let Some(h) = f16(data) else {
        return "bad length".into();
    };
    let mut line = Line::new();
    line.push_hundredths(h);
    String::from_utf8(line.as_bytes().to_vec()).unwrap()
}

#[test]
fn append_f16() {
    let tests: [(&[u8], &str); 5] = [
        (&[0, 0x0c, 0x1a], "21.00"),
        (&[0, 0x00, 0x00], "0.00"),
        (&[0, 0x87, 0xce], "-0.50"),
        (&[0, 0x7f, 0xff], "670760.96"),  // the largest value
        (&[0, 0xf8, 0x00], "-671088.64"), // the smallest value
    ];
    for (data, want) in tests {
        assert_eq!(hundredths(data), want, "f16({data:02x?})");
    }
}

/// knx-go's DPT_9002 unpack: the float32 0.01·m·2^e, evaluated left to
/// right, or None out of its range.
fn knx_go_dpt9002(data: &[u8; 3]) -> Option<f32> {
    let mut m = ((data[1] & 0x07) as i32) << 8 | data[2] as i32;
    if data[1] & 0x80 != 0 {
        m -= 2048;
    }
    let e = (data[1] >> 3) & 0x0f;
    let v = 0.01f32 * (m as f32) * ((1u32 << e) as f32);
    if !(-670760.0..=670760.0).contains(&v) {
        return None;
    }
    Some(v)
}

/// Checks f16 and push_hundredths against the %.2f of the float32 that
/// knx-go unpacks, for every value small enough for float32 to hold its
/// hundredths.
#[test]
fn append_f16_fmt() {
    for i in 0..1u32 << 16 {
        let data = [0, (i >> 8) as u8, i as u8];
        let Some(v) = knx_go_dpt9002(&data) else {
            continue;
        };
        if v >= 10000.0 || v <= -10000.0 {
            continue;
        }
        let (got, want) = (hundredths(&data), format!("{v:.2}"));
        assert_eq!(got, want, "f16({data:02x?}), fmt");
    }
}

/// Shows a value outside the range of its type as for a group of another
/// type: the binding without a value, or nothing.
#[test]
fn decode_out_of_range() {
    let dev = decode_device();
    let neg = [0, 0x87, 0xce]; // -0.50
    let got = describe(&dev, &plain(ga(3, 0, 1), Apci::GroupValueWrite, &neg));
    assert!(got.contains("(humidity, DPT 9.007), low"), "bound: {got:?}");
    let got = describe(&dev, &plain(ga(3, 0, 9), Apci::GroupValueWrite, &neg));
    assert!(!got.contains("DPT"), "unbound: {got:?}");
    let got = describe(&dev, &plain(ga(2, 0, 9), Apci::GroupValueWrite, &neg));
    assert!(got.contains("(DPT 9.001: -0.50 °C)"), "temperature: {got:?}");
}
