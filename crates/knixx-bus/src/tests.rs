use std::collections::VecDeque;
use std::string::String;
use std::vec::Vec;

use embassy_time::{Duration, Instant};
use knixx_device::thunder::{self, Device, NUM_SWITCHES, Switch};
use knixx_ncn as ncn;

use super::*;
use crate::format::{Write, bool_write, f16_write};

/// Records what is written and hands out rx when the engine reads. With
/// confirm set it answers every frame with U_FrameState.ind and a positive
/// L_Data.con, and flags a frame written before the previous one's
/// confirmation was read.
///
/// Each U_State.req written hands out the next of on_stat.
#[derive(Default)]
pub(crate) struct FakeDevice {
    pub(crate) written: Vec<u8>,
    pub(crate) rx: Vec<u8>,
    pub(crate) confirm: bool,
    pub(crate) overlap: bool,
    pub(crate) on_stat: VecDeque<Vec<u8>>,
}

impl FakeDevice {
    fn write(&mut self, p: &[u8]) {
        if self.confirm && p.first() == Some(&ncn::U_L_DATA_START_REQ) {
            if !self.rx.is_empty() {
                self.overlap = true;
            }
            self.rx.extend_from_slice(&[0x13, 0x8b]);
        }
        if p.first() == Some(&ncn::U_STATE_REQ)
            && let Some(reply) = self.on_stat.pop_front()
        {
            self.rx.extend_from_slice(&reply);
        }
        self.written.extend_from_slice(p);
    }
}

/// Captures the output and counts the errors.
#[derive(Default)]
pub(crate) struct Capture {
    pub(crate) text: String,
    pub(crate) errors: u32,
}

impl Output for Capture {
    fn write(&mut self, b: &[u8]) {
        assert!(b.ends_with(b"\n"), "not a whole line: {b:?}");
        self.text.push_str(core::str::from_utf8(b).unwrap());
    }

    fn failed(&mut self) {
        self.errors += 1;
    }
}

/// The start of simulated time.
const T0: Instant = Instant::from_secs(1000);

/// Runs an engine on dev in simulated time, as Go's Run, over d: it sends
/// the queued commands, each once the engine takes it, transmits to d and
/// feeds the engine what d returns, and advances the time to the engine's
/// deadlines. It stops when nothing is left to do, or when the next deadline
/// is past `limit` after the start.
pub(crate) fn run_for(
    d: &mut FakeDevice,
    dev: &Device,
    flags: &Flags,
    queued: &[Command],
    limit: Option<Duration>,
) -> Capture {
    let mut out = Capture::default();
    let mut engine = Engine::new(*dev, flags);
    let mut now = T0;
    let mut cmds = queued.iter();
    let mut cmd = cmds.next();
    for _ in 0..10_000 {
        while let Some(b) = engine.poll(now, &mut out) {
            d.write(b);
        }
        // A command goes before what is received, as Go's select reads a
        // queued command before the first tick reads the transceiver.
        if engine.accepts_commands()
            && let Some(&c) = cmd
        {
            engine.command(c, now, &mut out);
            cmd = cmds.next();
            continue;
        }
        if !d.rx.is_empty() {
            let rx = core::mem::take(&mut d.rx);
            engine.feed(&rx, now, &mut out);
            continue;
        }
        match engine.deadline() {
            Some(t) if limit.is_none_or(|l| t <= T0 + l) => now = now.max(t),
            _ => return out,
        }
    }
    panic!("the engine did not finish");
}

/// Runs the queued commands until nothing is left to do.
pub(crate) fn run(d: &mut FakeDevice, dev: &Device, flags: &Flags, queued: &[Command]) -> Capture {
    run_for(d, dev, flags, queued, None)
}

pub(crate) fn test_device() -> Device {
    let mut switches = [Switch::default(); NUM_SWITCHES];
    switches[0] = Switch::new("A", &[0x0002]);
    switches[1] = Switch::new("B", &[0x0003, 0x0004, 0x0005]);
    Device {
        individual: 0x7801,
        switches,
        ..Device::default()
    }
}

/// Returns the request that sends tg.
fn request(tg: &knixx_knx::tp1::Telegram) -> Vec<u8> {
    let mut frame = [0; knixx_knx::tp1::MAX_FRAME];
    let buf = tg.pack_to(&mut frame).unwrap();
    let mut req = [0; 2 * ncn::MAX_FRAME_LEN];
    ncn::tx_data(buf, &mut req).unwrap().to_vec()
}

/// Returns the requests for setting each group to on, in order.
fn frames(dev: &Device, on: bool, groups: &[u16]) -> Vec<u8> {
    groups
        .iter()
        .flat_map(|&g| request(&dev.telegram(g, on)))
        .collect()
}

/// Returns a write of data to group.
fn new_write(group: u16, data: &[u8]) -> Write {
    let mut w = Write {
        group,
        n: data.len() as u8,
        ..Write::default()
    };
    w.data[..data.len()].copy_from_slice(data);
    w
}

/// Returns the requests for the given group value writes, in order.
fn writes(dev: &Device, ws: &[Write]) -> Vec<u8> {
    ws.iter()
        .flat_map(|w| request(&dev.group_write(w.group, w.data())))
        .collect()
}

fn cat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

const fn switch(switch: u8, on: bool) -> Command {
    Command::Switch { switch, on }
}

#[test]
fn transmit() {
    let dev = test_device();
    let queued = [switch(0, true), switch(1, true), switch(0, false)];
    let want = cat(&[
        &frames(&dev, true, &[0x0002, 0x0003, 0x0004, 0x0005]),
        &frames(&dev, false, &[0x0002]),
    ]);
    let flags = Flags::new();
    for on in [true, false] {
        flags.sniff.store(on, Ordering::Relaxed);
        let mut d = FakeDevice {
            confirm: true,
            ..Default::default()
        };
        let got = run(&mut d, &dev, &flags, &queued);
        assert_eq!(d.written, want, "sniff={on}");
        assert!(
            !d.overlap,
            "sniff={on}: a frame was sent before the previous L_Data.con"
        );
        assert!(
            !got.text.contains("no L_Data.con"),
            "sniff={on}: unexpected timeout: {:?}",
            got.text
        );
        assert_eq!(got.errors, 0, "sniff={on}: failed called {} times", got.errors);
    }
}

#[test]
fn no_confirmation() {
    let flags = Flags::new();
    let dev = test_device();
    let mut d = FakeDevice::default();
    let got = run(&mut d, &dev, &flags, &[switch(1, true)]);
    assert_eq!(d.written, frames(&dev, true, &[0x0003, 0x0004, 0x0005]));
    for want in [
        "no L_Data.con for 0/0/3",
        "no L_Data.con for 0/0/4",
        "no L_Data.con for 0/0/5",
    ] {
        assert!(got.text.contains(want), "output {:?} lacks {want:?}", got.text);
    }
    assert_eq!(got.errors, 3);
}

#[test]
fn negative_confirmation() {
    let flags = Flags::new();
    let dev = test_device();
    for sniff in [false, true] {
        flags.sniff.store(sniff, Ordering::Relaxed);
        let mut d = FakeDevice {
            rx: [0x13, 0x0b].to_vec(),
            ..Default::default()
        };
        let got = run(&mut d, &dev, &flags, &[switch(0, true)]);
        assert!(
            got.text.contains("negative"),
            "sniff={sniff}: output {:?}",
            got.text
        );
        assert_eq!(got.errors, 1, "sniff={sniff}");
    }
}

#[test]
fn stat_after_switch() {
    let flags = Flags::new();
    let dev = test_device();
    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(&mut d, &dev, &flags, &[switch(1, true), Command::Stat]);
    let want = cat(&[
        &frames(&dev, true, &[0x0003, 0x0004, 0x0005]),
        &[ncn::U_STATE_REQ, ncn::U_SYSTEM_STAT_REQ],
    ]);
    assert_eq!(d.written, want);
}

#[test]
fn stat() {
    let flags = Flags::new();
    let mut d = FakeDevice {
        rx: [0x07, 0x4b, 0xfb].to_vec(),
        ..Default::default()
    };
    let got = run(&mut d, &test_device(), &flags, &[Command::Stat]);
    assert_eq!(d.written, [ncn::U_STATE_REQ, ncn::U_SYSTEM_STAT_REQ]);
    for want in [
        "U_State.ind: no errors (0x07)",
        "U_SystemStat.ind: V20V ok, VDD2 ok, VBUS ok, VFILT ok, XTAL ok, no thermal warning, mode Normal (0xfb)",
    ] {
        assert!(got.text.contains(want), "output {:?} lacks {want:?}", got.text);
    }
}

#[test]
fn stat_no_reply() {
    let flags = Flags::new();
    let mut d = FakeDevice::default();
    let got = run(&mut d, &test_device(), &flags, &[Command::Stat]);
    for want in ["no U_State.ind reply", "no U_SystemStat.ind reply"] {
        assert!(got.text.contains(want), "output {:?} lacks {want:?}", got.text);
    }
}

#[test]
fn config() {
    let flags = Flags::new();
    let dev = test_device();
    let mut moved = dev;
    moved.switches[0] = Switch::new("A", &[0x0909]);

    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(
        &mut d,
        &dev,
        &flags,
        &[switch(0, true), Command::Config(moved), switch(0, true)],
    );
    let want = cat(&[&frames(&dev, true, &[0x0002]), &frames(&moved, true, &[0x0909])]);
    assert_eq!(d.written, want);
    assert_eq!(
        dev.switches[0].groups[0], 0x0002,
        "the engine modified the caller's device"
    );
}

/// A request that cannot be written ends the wait for its answer at once:
/// the next telegram goes out without a timeout, and a stat prints no
/// missing-reply errors.
#[test]
fn transmit_failed() {
    let flags = Flags::new();
    let mut out = Capture::default();
    let mut engine = Engine::new(test_device(), &flags);
    engine.command(switch(1, true), T0, &mut out);
    assert!(engine.poll(T0, &mut out).is_some());
    engine.transmit_failed();
    assert!(engine.poll(T0, &mut out).is_some(), "the next write waits");
    engine.transmit_failed();
    assert!(engine.poll(T0, &mut out).is_some());
    engine.transmit_failed();
    assert!(engine.poll(T0, &mut out).is_none());
    assert!(engine.accepts_commands());
    assert_eq!(engine.deadline(), None);

    engine.command(Command::Stat, T0, &mut out);
    assert!(engine.poll(T0, &mut out).is_some());
    engine.transmit_failed();
    assert!(engine.accepts_commands());
    assert_eq!(engine.poll(T0 + Duration::from_secs(1), &mut out), None);
    assert_eq!(out.text, "");
    assert_eq!(out.errors, 0);
}

#[test]
fn no_switch() {
    let flags = Flags::new();
    let mut d = FakeDevice::default();
    let got = run(
        &mut d,
        &test_device(),
        &flags,
        &[switch(NUM_SWITCHES as u8, true)],
    );
    assert_eq!(got.text, "bus: no switch 2\n");
    assert_eq!(got.errors, 1);
    assert!(d.written.is_empty());
}

#[test]
fn sensor() {
    let flags = Flags::new();
    let mut dev = test_device();
    dev.sensor = thunder::Sensor {
        temperature: 0x0101,
        humidity: 0x0102,
        illuminance: 0,
    };
    let reading = Command::Sensor {
        temperature: 21500,
        humidity: 4820,
    };

    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(&mut d, &dev, &flags, &[reading, switch(0, true)]);
    let want = cat(&[
        &writes(
            &dev,
            &[
                new_write(0x0101, &[0x00, 0x0c, 0x33]), // 21.5 °C
                new_write(0x0102, &[0x00, 0x14, 0xb5]), // 48.2 %
            ],
        ),
        &frames(&dev, true, &[0x0002]),
    ]);
    assert_eq!(d.written, want);
    assert!(!d.overlap, "a frame was sent before the previous L_Data.con");

    // An unbound value is not sent.
    dev.sensor.temperature = 0;
    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(&mut d, &dev, &flags, &[reading]);
    assert_eq!(
        d.written,
        writes(&dev, &[new_write(0x0102, &[0x00, 0x14, 0xb5])]),
        "humidity only"
    );

    dev.sensor.humidity = 0;
    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(&mut d, &dev, &flags, &[reading]);
    assert!(d.written.is_empty(), "unbound sensor sent {:02x?}", d.written);
}

#[test]
fn occupancy_state() {
    let mut o = Occupancy {
        on: false,
        last: Instant::MIN,
    };
    let timeout = Duration::from_secs(60);
    // motion, at in seconds, want: turns on for motion, off otherwise.
    let steps = [
        (false, 0, false),  // nothing to expire
        (true, 0, true),    // first motion turns on
        (true, 10, false),  // already on, restarts the timer
        (false, 69, false), // 59 s after the last motion
        (false, 70, true),  // 60 s after the last motion
        (false, 80, false), // already off
        (true, 90, true),   // on again
    ];
    for (i, (motion, at, want)) in steps.into_iter().enumerate() {
        let now = T0 + Duration::from_secs(at);
        let got = if motion {
            o.motion(now)
        } else {
            o.expire(now, timeout)
        };
        assert_eq!(got, want, "step {i}");
    }
}

#[test]
fn occupancy() {
    let flags = Flags::new();
    let mut dev = test_device();
    dev.occupancy = thunder::Occupancy {
        group: 0x0201,
        timeout: 10,
        threshold: 44,
        ..thunder::Occupancy::DEFAULT
    };
    let on = writes(&dev, &[bool_write(0x0201, true)]);
    let off = writes(&dev, &[bool_write(0x0201, false)]);
    let motion = Command::Motion;

    // Motion turns occupancy on once; it is turned off after 10 s without
    // motion.
    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run_for(
        &mut d,
        &dev,
        &flags,
        &[motion, motion, motion],
        Some(Duration::from_secs(5)),
    );
    assert_eq!(d.written, on, "before the timeout");
    assert!(
        flags.occupied.load(Ordering::Relaxed),
        "not occupied after motion"
    );

    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run_for(
        &mut d,
        &dev,
        &flags,
        &[motion, switch(0, true)],
        Some(Duration::from_secs(30)),
    );
    let want = cat(&[&on, &frames(&dev, true, &[0x0002]), &off]);
    assert_eq!(d.written, want, "after the timeout");
    assert!(!d.overlap, "a frame was sent before the previous L_Data.con");
    assert!(
        !flags.occupied.load(Ordering::Relaxed),
        "still occupied after the timeout"
    );

    // Without a group, motion is ignored.
    dev.occupancy.group = 0;
    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run_for(&mut d, &dev, &flags, &[motion], Some(Duration::from_secs(5)));
    assert!(d.written.is_empty(), "unbound occupancy sent {:02x?}", d.written);
}

#[test]
fn light() {
    let flags = Flags::new();
    let mut dev = test_device();
    dev.sensor = thunder::Sensor {
        illuminance: 0x0103,
        ..Default::default()
    };
    let reading = Command::Light { illuminance: 4321 };

    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(&mut d, &dev, &flags, &[reading]);
    // 432.1 lx
    assert_eq!(d.written, writes(&dev, &[new_write(0x0103, &[0x00, 0x2d, 0x46])]));

    // The temperature and humidity groups do not receive it.
    dev.sensor = thunder::Sensor {
        temperature: 0x0101,
        humidity: 0x0102,
        illuminance: 0,
    };
    let mut d = FakeDevice {
        confirm: true,
        ..Default::default()
    };
    run(&mut d, &dev, &flags, &[reading]);
    assert!(
        d.written.is_empty(),
        "unbound illuminance sent {:02x?}",
        d.written
    );
}

/// Checks the DPT 9 packing against knx-go, which packs from a float32, for
/// values it holds exactly, and the clamping of values out of range.
#[test]
fn f16_write_values() {
    // knx-go's dpt.DPT_9002(float32(h) / 100).Pack().
    let knx_go: [(i32, [u8; 3]); 10] = [
        (0, [0, 0, 0]),
        (1, [0, 0, 1]),
        (-1, [0, 0x87, 0xff]),
        (2048, [0, 0x0c, 0x00]),
        (-2048, [0, 0x80, 0x00]),
        (-2049, [0, 0x8c, 0x00]),
        (2150, [0, 0x0c, 0x33]),
        (4820, [0, 0x14, 0xb5]),
        (-27300, [0, 0xa1, 0x56]),
        (100000, [0, 0x36, 0x1a]),
    ];
    for (h, want) in knx_go {
        let w = f16_write(1, h);
        assert_eq!(w.data(), want, "f16_write({h}), knx-go");
    }
    let tests: [(i32, [u8; 3]); 6] = [
        // knx-go's float32 truncates these one hundredth low.
        (2031, [0x00, 0x07, 0xef]),
        (2047, [0x00, 0x07, 0xff]),
        (67076096, [0x00, 0x7f, 0xff]),
        (-67108864, [0x00, 0xf8, 0x00]),
        (99_999_999, [0x00, 0x7f, 0xff]),
        (-99999999, [0x00, 0xf8, 0x00]),
    ];
    for (h, want) in tests {
        let w = f16_write(1, h);
        assert_eq!(w.data(), want, "f16_write({h})");
    }
    let w = bool_write(1, true);
    assert_eq!(w.data(), [1], "bool_write(true)");
}
