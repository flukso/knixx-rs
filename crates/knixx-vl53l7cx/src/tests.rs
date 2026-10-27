//! testdata/configure.golden is the log of the C driver, run by knixx-go's
//! src/internal/vl53l7cx/testdata/harness against the fake sensor that
//! FakeBus reimplements.

use core::cell::RefCell;
use core::future::Future;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::format;
use std::string::String;
use std::vec::Vec;

use embedded_hal_async::i2c::{ErrorKind, ErrorType, NoAcknowledgeSource, Operation};

use super::motion::motion_fields;
use super::*;

const NACK: ErrorKind = ErrorKind::NoAcknowledge(NoAcknowledgeSource::Unknown);
const FRAME_SIZE: usize = 196;

/// Polls fut to completion with a no-op waker.
fn block_on<F: Future>(fut: F) -> F::Output {
    let mut fut = pin!(fut);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// A logged transaction: a write, a read or a wait.
#[derive(Clone)]
struct Op {
    kind: u8, // b'W', b'R' or b'D'
    reg: u16,
    n: usize,
    data: Vec<u8>, // of a write
}

/// The log the fake bus and delay share.
#[derive(Default)]
struct Log {
    ops: RefCell<Vec<Op>>,
}

/// Answers like the fake sensor of the harness and logs the transactions.
struct FakeBus<'a> {
    log: &'a Log,
    fail: Option<ErrorKind>,
}

/// Logs the waits in ms.
struct FakeDelay<'a>(&'a Log);

fn put32be(p: &mut [u8], v: u32) {
    p[..4].copy_from_slice(&v.to_be_bytes());
}

/// Builds the ranging data frame the fake sensor returns.
fn frame(raw: &mut [u8]) {
    raw.fill(0);
    raw[0] = 7;
    raw[10] = 0x12;
    raw[11] = 0x34;
    put32be(&mut raw[16..], 0x54B400C0);
    put32be(&mut raw[32..], 0x54C00040);
    put32be(&mut raw[40..], 0xD85808C0);
    put32be(&mut raw[44..], 1111);
    put32be(&mut raw[48..], 2222);
    raw[52 + 3] = 1;
    raw[52 + 2] = 3;
    raw[52 + 1] = 16;
    for k in 0..32u32 {
        put32be(&mut raw[56 + 4 * k as usize..], (k + 1) * 10 * 65535);
    }
    raw[FRAME_SIZE - 2] = 0x12;
    raw[FRAME_SIZE - 1] = 0x34;
}

fn reply(reg: u16, p: &mut [u8]) {
    for (i, v) in p.iter_mut().enumerate() {
        *v = (i * 7 + 3) as u8;
    }
    match reg {
        0x0000 => {
            if p.len() == 1 {
                p[0] = 0xF0;
            } else if p.len() == FRAME_SIZE {
                frame(p);
            }
        }
        0x0001 => p[0] = 0x02,
        0x0006 => p[0] = 0x01,
        0x0007 => p[0] = 0x00,
        0x0021 => p[0] = 0x10,
        0x2FFC => p[..4].copy_from_slice(&[0xFF, 0x04, 0x00, 0x00]),
        0x2C00 => {
            let status = [0x02, 0x03, 0x00, 0x00];
            for (i, v) in p.iter_mut().enumerate() {
                *v = if i < 4 { status[i] } else { 0 };
            }
        }
        0x2C04 => {
            if p.len() == 24 {
                p[14] = (FRAME_SIZE >> 8) as u8;
                p[15] = (FRAME_SIZE & 0xff) as u8;
            }
        }
        _ => {
            if p.len() == 1 {
                p[0] = 0;
            }
        }
    }
}

impl ErrorType for FakeBus<'_> {
    type Error = ErrorKind;
}

impl I2c for FakeBus<'_> {
    async fn transaction(&mut self, addr: u8, ops: &mut [Operation<'_>]) -> Result<(), ErrorKind> {
        assert_eq!(addr, ADDRESS, "address {addr:#x}");
        if let Some(e) = self.fail {
            return Err(e);
        }
        let mut w: &[u8] = &[];
        let mut r: Option<&mut [u8]> = None;
        for op in ops.iter_mut() {
            match op {
                Operation::Write(b) => w = b,
                Operation::Read(b) => r = Some(b),
            }
        }
        let reg = u16::from_be_bytes([w[0], w[1]]);
        let mut log = self.log.ops.borrow_mut();
        match r {
            None => log.push(Op {
                kind: b'W',
                reg,
                n: w.len() - 2,
                data: w[2..].to_vec(),
            }),
            Some(r) => {
                log.push(Op {
                    kind: b'R',
                    reg,
                    n: r.len(),
                    data: Vec::new(),
                });
                reply(reg, r);
            }
        }
        Ok(())
    }
}

impl DelayNs for FakeDelay<'_> {
    async fn delay_ns(&mut self, ns: u32) {
        self.0.ops.borrow_mut().push(Op {
            kind: b'D',
            reg: 0,
            n: (ns / 1_000_000) as usize,
            data: Vec::new(),
        });
    }
}

fn fnv32a(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c9dc5;
    for &b in data {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x01000193);
    }
    h
}

impl Log {
    fn len(&self) -> usize {
        self.ops.borrow().len()
    }

    /// Renders the transactions like the harness, merging the chunks of a
    /// large write.
    fn lines(&self) -> Vec<String> {
        let mut merged: Vec<Op> = Vec::new();
        for o in self.ops.borrow().iter() {
            if let Some(m) = merged.last_mut()
                && o.kind == b'W'
                && m.kind == b'W'
                && m.n > 1
                && o.n > 1
                && usize::from(m.reg) + m.n == usize::from(o.reg)
            {
                m.data.extend_from_slice(&o.data);
                m.n += o.n;
                continue;
            }
            merged.push(o.clone());
        }
        merged
            .iter()
            .map(|o| match o.kind {
                b'W' => format!("W {:04x} {} {:08x}", o.reg, o.n, fnv32a(&o.data)),
                b'R' => format!("R {:04x} {}", o.reg, o.n),
                _ => format!("D {}", o.n),
            })
            .collect()
    }
}

type Dev<'a> = Device<FakeBus<'a>, FakeDelay<'a>>;

fn new_device(log: &Log) -> std::boxed::Box<Dev<'_>> {
    std::boxed::Box::new(Device::new(FakeBus { log, fail: None }, FakeDelay(log)))
}

/// Runs the sequence of the harness.
async fn configure(d: &mut Dev<'_>) -> Result<Motion, Error<ErrorKind>> {
    if !d.is_alive().await? {
        return Err(Error::NotAlive);
    }
    d.init().await?;
    d.set_resolution(RESOLUTION_4X4).await?;
    d.set_ranging_frequency(1).await?;
    d.set_ranging_mode(RANGING_AUTONOMOUS).await?;
    d.set_integration_time(5).await?;
    d.motion_indicator_init(RESOLUTION_4X4).await?;
    d.set_motion_distance(1000, 2400).await?;
    d.set_motion_thresholds(44).await?;
    d.enable_detection_thresholds(true).await?;
    d.start_ranging().await?;
    let m = d.motion().await?;
    d.stop_ranging().await?;
    Ok(m)
}

#[test]
fn matches_uld() {
    let golden = include_str!("../testdata/configure.golden");
    let mut want = Vec::new();
    let mut motion = "";
    for line in golden.lines() {
        if line.starts_with("M ") {
            motion = line;
        } else {
            want.push(line);
        }
    }

    let log = Log::default();
    let mut d = new_device(&log);
    let m = block_on(configure(&mut d)).unwrap();
    let got = log.lines();

    for (i, (g, w)) in got.iter().zip(&want).enumerate() {
        assert_eq!(g, w, "transaction {i}: got {g:?}, want {w:?} (C driver)");
    }
    assert_eq!(
        got.len(),
        want.len(),
        "{} transactions, want {}",
        got.len(),
        want.len()
    );

    let mut got_motion = String::from("M");
    for v in m.values {
        got_motion += &format!(" {v}");
    }
    assert_eq!(got_motion, motion);
    assert!(
        m.global1 == 1111
            && m.global2 == 2222
            && m.status == 1
            && m.nb_detected == 3
            && m.nb_aggregates == 16,
        "motion {m:?}"
    );
}

#[test]
fn firmware_chunks() {
    let log = Log::default();
    let mut d = new_device(&log);
    block_on(d.init()).unwrap();
    for o in log.ops.borrow().iter() {
        assert!(
            !(o.kind == b'W' && o.n > BUF_SIZE),
            "write of {} bytes exceeds the {BUF_SIZE} byte buffer",
            o.n
        );
    }
    assert_eq!(
        (
            FIRMWARE.len(),
            DEFAULT_CONFIGURATION.len(),
            DEFAULT_XTALK.len(),
            GET_NVM_CMD.len()
        ),
        (0x15000, 972, XTALK_SIZE, 40),
        "embedded sizes"
    );
}

#[test]
fn invalid() {
    let log = Log::default();
    let mut d = new_device(&log);
    let errs = block_on(async {
        [
            d.set_motion_thresholds(0).await,
            d.set_motion_thresholds(MAX_MOTION_THRESHOLD + 1).await,
            d.set_ranging_frequency(0).await,
            d.set_ranging_frequency(61).await,
            d.set_integration_time(1).await,
            d.set_integration_time(1001).await,
            d.set_resolution(32).await,
            d.set_ranging_mode(2).await,
            d.motion_indicator_init(32).await,
            d.set_motion_distance(400, 1500).await, // before motion_indicator_init
        ]
    });
    for err in errs {
        assert_eq!(err, Err(Error::Invalid));
    }
    assert_eq!(
        log.len(),
        0,
        "invalid parameters caused {} transactions",
        log.len()
    );
}

#[test]
fn bus_error() {
    let log = Log::default();
    let mut d = new_device(&log);
    d.bus.fail = Some(NACK);
    assert_eq!(block_on(d.init()), Err(Error::Bus(NACK)));
    assert!(
        block_on(d.is_alive()).is_err(),
        "is_alive succeeded on a failing bus"
    );
}

#[test]
fn errors_display() {
    assert_eq!(
        format!("{}", Error::<ErrorKind>::NotAlive),
        "vl53l7cx: no sensor found"
    );
    assert_eq!(
        format!("{}", Error::<ErrorKind>::Mcu(None)),
        "vl53l7cx: MCU error"
    );
    assert_eq!(
        format!("{}", Error::<ErrorKind>::Mcu(Some(0x86))),
        "vl53l7cx: MCU error 0x86"
    );
    assert_eq!(format!("{}", Error::Bus(NACK)), format!("vl53l7cx: {NACK:?}"));
}

/// Windows with their validity; knixx-go's thunder_test.go holds the same
/// table for thunder.Occupancy.DistanceValid.
const DISTANCE_PAIRS: [(u16, u16, bool); 8] = [
    (400, 1500, true),
    (400, 1900, true),
    (2500, 4000, true),
    (399, 1000, false),
    (1000, 4001, false),
    (1000, 2501, false),
    (1500, 1500, false),
    (1500, 1000, false),
];

#[test]
fn motion_distance() {
    // Computed by the ULD's formula in single precision.
    for (min, max, ref_bin_offset, feature_length) in [
        (400, 1500, 13636, 9),
        (400, 1900, 13636, 12),
        (1000, 2500, 46382, 12),
        (2500, 4000, 128246, 12),
    ] {
        assert_eq!(
            motion_fields(min, max),
            (ref_bin_offset, feature_length),
            "motion_fields({min}, {max})"
        );
    }

    let log = Log::default();
    let mut d = new_device(&log);
    block_on(d.motion_indicator_init(RESOLUTION_4X4)).unwrap();
    let mut n = log.len();
    for (min, max, valid) in DISTANCE_PAIRS {
        assert_eq!(
            valid_motion_distance(min, max),
            valid,
            "valid_motion_distance({min}, {max})"
        );
        let res = block_on(d.set_motion_distance(min, max));
        assert_eq!(res.is_ok(), valid, "set_motion_distance({min}, {max}): {res:?}");
        if !valid {
            assert_eq!(
                log.len(),
                n,
                "set_motion_distance({min}, {max}) caused transactions"
            );
        }
        n = log.len();
    }
}
