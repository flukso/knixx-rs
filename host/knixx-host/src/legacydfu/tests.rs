//! The tests of the update against a fake bootloader, as in the Go
//! legacydfu package.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{Cursor, Write};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use super::*;

/// Runs f, whose futures are all ready, to completion.
fn block_on<F: Future>(f: F) -> F::Output {
    let mut f = pin!(f);
    let mut cx = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
    }
}

/// Answers like the SDK 11 bootloader and logs the writes.
#[derive(Default)]
struct Fake {
    log: Vec<String>,
    notify: VecDeque<Vec<u8>>,
    receiving: bool,
    size: usize,
    got: Vec<u8>,
    packets: usize,
    prn: usize,
    /// Answers validation with a CRC error.
    fail_validate: bool,
    /// Drops the response to start.
    silent: bool,
    /// What packet_size reports.
    packet_size: usize,
    /// The sizes of the image packets.
    sizes: Vec<usize>,
    /// Reports a wrong byte count in the first receipt.
    bad_receipt: bool,
    /// Fails every control point write.
    gone: bool,
}

impl Fake {
    fn respond(&mut self, op: u8, status: u8) {
        self.notify.push_back(vec![OP_RESPONSE, op, status]);
    }
}

impl Link for Fake {
    async fn write_control(&mut self, p: &[u8]) -> Result<(), String> {
        if self.gone {
            return Err("gone".into());
        }
        self.log.push(format!("C {}", hex(p)));
        match p[0] {
            OP_INIT if p[1] == INIT_DONE => self.respond(OP_INIT, STAT_SUCCESS),
            OP_PRN_REQ => self.prn = usize::from(u16::from_le_bytes([p[1], p[2]])),
            OP_RECEIVE => self.receiving = true,
            OP_VALIDATE => {
                let s = if self.fail_validate { 5 } else { STAT_SUCCESS };
                self.respond(OP_VALIDATE, s);
            }
            _ => {}
        }
        Ok(())
    }

    async fn write_packet(&mut self, p: &[u8]) -> Result<(), String> {
        if p.len() > chunk(self.packet_size) || (!p.len().is_multiple_of(4) && self.receiving) {
            return Err(format!("bad packet of {} bytes", p.len()));
        }
        if !self.receiving {
            self.log.push(format!("P {}", hex(p)));
            if p.len() == 12 && self.size == 0 {
                self.size = u32::from_le_bytes([p[8], p[9], p[10], p[11]]) as usize;
                if !self.silent {
                    self.respond(OP_START, STAT_SUCCESS);
                }
            }
            return Ok(());
        }
        self.got.extend_from_slice(p);
        self.packets += 1;
        self.sizes.push(p.len());
        if self.got.len() == self.size {
            self.respond(OP_RECEIVE, STAT_SUCCESS);
        } else if self.packets.is_multiple_of(self.prn) {
            let n = if self.bad_receipt {
                1
            } else {
                self.got.len() as u32
            };
            let mut r = vec![OP_PRN];
            r.extend_from_slice(&n.to_le_bytes());
            self.notify.push_back(r);
        }
        Ok(())
    }

    async fn notification(&mut self, _: Duration) -> Next {
        match self.notify.pop_front() {
            Some(n) => Next::Got(n),
            None => Next::Timeout,
        }
    }

    fn packet_size(&self) -> usize {
        self.packet_size
    }
}

fn test_package(n: usize) -> Package {
    Package {
        bin: (0..n).map(|i| i as u8).collect(),
        dat: vec![
            0x52, 0, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1, 0, 0xb6, 0, 0xd9, 0x7f,
        ],
    }
}

#[test]
fn update_ok() {
    let mut f = Fake::default();
    let p = test_package(1000);
    let mut last = 0;
    block_on(update(&mut f, &p, |sent, _| last = sent)).unwrap();
    assert_eq!(f.got, p.bin);
    assert_eq!(last, p.bin.len());
    assert_eq!(
        f.log,
        [
            "C 01 04",
            "P 00 00 00 00 00 00 00 00 e8 03 00 00",
            "C 02 00",
            "P 52 00 ff ff ff ff ff ff 01 00 b6 00 d9 7f",
            "C 02 01",
            "C 08 0a 00",
            "C 03",
            "C 04",
            "C 05",
        ]
    );
}

#[test]
fn update_packet_size() {
    for (size, want) in [
        (0, MIN_CHUNK),   // unknown MTU
        (10, MIN_CHUNK),  // never below the default MTU
        (150, 148),       // a multiple of 4
        (244, MAX_CHUNK), // MTU 247
        (514, MAX_CHUNK), // MTU 517
    ] {
        let mut f = Fake {
            packet_size: size,
            ..Default::default()
        };
        let p = test_package(3000);
        block_on(update(&mut f, &p, |_, _| {})).unwrap_or_else(|e| panic!("size {size}: {e}"));
        assert_eq!(f.got, p.bin, "size {size}");
        assert_eq!(f.sizes[0], want, "size {size}");
        assert_eq!(*f.sizes.iter().max().unwrap(), want, "size {size}");
    }
}

#[test]
fn update_validate_fails() {
    let mut f = Fake {
        fail_validate: true,
        ..Default::default()
    };
    let e = block_on(update(&mut f, &test_package(40), |_, _| {})).unwrap_err();
    assert!(e.0.contains("validate: CRC error"), "{e}");
    assert_ne!(
        f.log.last().unwrap(),
        "C 05",
        "activated after a failed validation"
    );
}

#[test]
fn update_timeout() {
    let mut f = Fake {
        silent: true,
        ..Default::default()
    };
    let e = block_on(update(&mut f, &test_package(40), |_, _| {})).unwrap_err();
    assert!(e.0.contains("start: no answer"), "{e}");
    assert_eq!(f.log.len(), 2, "went on after the timeout: {:?}", f.log);
}

#[test]
fn update_bad_receipt() {
    let mut f = Fake {
        bad_receipt: true,
        ..Default::default()
    };
    let e = block_on(update(&mut f, &test_package(400), |_, _| {})).unwrap_err();
    assert!(e.0.contains("bootloader has 1 bytes, sent 200"), "{e}");
}

#[test]
fn update_write_error() {
    let mut f = Fake {
        gone: true,
        ..Default::default()
    };
    let e = block_on(update(&mut f, &test_package(40), |_, _| {})).unwrap_err();
    assert_eq!(e.0, "start: gone");
}

/// Builds a zip with the given files.
fn zip_of(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, data) in files {
        w.start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        w.write_all(data).unwrap();
    }
    w.finish().unwrap().into_inner()
}

const MANIFEST: &[u8] = br#"{"manifest": {"application": {"bin_file": "knixx.bin", "dat_file": "knixx.dat",
	"init_packet_data": {"device_type": 82}}, "dfu_version": 0.5}}"#;

#[test]
fn read() {
    let z = zip_of(&[
        ("manifest.json", MANIFEST),
        ("knixx.bin", &[0; 8]),
        ("knixx.dat", &[1, 2, 3]),
    ]);
    let p = Package::from_zip(Cursor::new(z)).unwrap();
    assert_eq!((p.bin.len(), p.dat.len()), (8, 3));
}

#[test]
fn read_errors() {
    for (name, files) in [
        ("no manifest", vec![("knixx.bin", &[0u8; 8][..])]),
        (
            "no application",
            vec![("manifest.json", &br#"{"manifest": {"bootloader": {}}}"#[..])],
        ),
        ("no bin", vec![("manifest.json", MANIFEST), ("knixx.dat", &[1])]),
        (
            "odd size",
            vec![
                ("manifest.json", MANIFEST),
                ("knixx.bin", &[0; 6]),
                ("knixx.dat", &[1]),
            ],
        ),
    ] {
        assert!(
            Package::from_zip(Cursor::new(zip_of(&files))).is_err(),
            "{name}: no error"
        );
    }
}
