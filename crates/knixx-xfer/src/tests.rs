//! A sender and a receiver over a simulated link that loses, duplicates
//! and reorders messages, and breaks: a push of firmware with resume, a
//! script, a config, and a pull's direction.

use std::collections::VecDeque;
use std::vec::Vec;

use crate::msg::{Kind, Msg};
use crate::receiver::{Fail, Offer, Out, Receiver, Sink};
use crate::sender::{Event, SendError, Sender};
use crate::{ACK_EVERY, code, crc32};

/// A sink into a Vec, with a fault to inject.
#[derive(Default)]
struct VecSink {
    data: Vec<u8>,
    opened: u32,
    kept: Option<Vec<u8>>,
    aborted: u32,
    refuse_finish: bool,
}

impl Sink for VecSink {
    fn open(&mut self, _: Kind, _: &str, _: u32, _: u8) -> Result<(), Fail> {
        self.data.clear();
        self.opened += 1;
        Ok(())
    }
    fn write(&mut self, d: &[u8]) -> Result<(), Fail> {
        self.data.extend_from_slice(d);
        Ok(())
    }
    fn finish(&mut self) -> Result<u32, Fail> {
        if self.refuse_finish {
            return Err(Fail::new(5, "bad signature"));
        }
        self.kept = Some(core::mem::take(&mut self.data));
        Ok(self.kept.as_ref().unwrap().len() as u32)
    }
    fn abort(&mut self) {
        self.aborted += 1;
        self.data.clear();
    }
}

/// A deterministic random source.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
    fn chance(&mut self, per_mille: u32) -> bool {
        self.next() % 1000 < per_mille
    }
}

/// A lossy link: messages as owned bytes.
struct Link {
    rng: Rng,
    loss: u32,
    dup: u32,
    reorder: u32,
    q: VecDeque<Vec<u8>>,
}

impl Link {
    fn new(seed: u64, loss: u32, dup: u32, reorder: u32) -> Link {
        Link {
            rng: Rng(seed),
            loss,
            dup,
            reorder,
            q: VecDeque::new(),
        }
    }
    fn send(&mut self, m: &Msg) {
        let mut buf = [0u8; 300];
        let n = m.encode(&mut buf).unwrap();
        if self.rng.chance(self.loss) {
            return;
        }
        let b = buf[..n].to_vec();
        if self.rng.chance(self.dup) {
            self.q.push_back(b.clone());
        }
        if self.rng.chance(self.reorder) && !self.q.is_empty() {
            self.q.push_front(b);
        } else {
            self.q.push_back(b);
        }
    }
}

const WINDOW: u32 = 16 * 1024;
const PIECE: u32 = 240;
const STALL_MS: u64 = 1500;

/// Runs a push of file over links with the given faults, from a receiver
/// that may hold part of it (resume). Returns the outcome and the time.
fn push(
    rx: &mut Receiver,
    sink: &mut VecSink,
    kind: Kind,
    file: &[u8],
    seed: u64,
    faults: (u32, u32, u32),
    break_at: Option<u32>,
) -> (Result<u32, SendError>, u64) {
    let crc = crc32(file);
    let mut now = 0u64;
    let mut up = Link::new(seed, faults.0, faults.1, faults.2);
    let mut down = Link::new(seed ^ 0x55, faults.0, faults.1, faults.2);
    // The request and its answer go over a reliable control path, as a
    // GATT write with response does.
    let first = rx.put(
        sink,
        &Offer {
            kind,
            flags: 0,
            size: file.len() as u32,
            crc,
            name: name(kind),
        },
        now,
    );
    let from = match first {
        Out::Ack(o) => o,
        Out::Error { code, reason, .. } => {
            return (
                Err(SendError::Refused {
                    code,
                    reason: reason_of(reason),
                }),
                now,
            );
        }
        other => panic!("{other:?}"),
    };
    let mut tx = Sender::new(file.len() as u32, crc, from, WINDOW, PIECE, 10);
    let mut last_heard = now;
    for _ in 0..1_000_000 {
        now += 1;
        while let Some((at, len)) = tx.next_piece() {
            up.send(&Msg::Data {
                offset: at,
                bytes: &file[at as usize..(at + len) as usize],
            });
        }
        // The receiver takes what came.
        while let Some(b) = up.q.pop_front() {
            let Some(Msg::Data { offset, bytes }) = Msg::decode(&b) else {
                continue;
            };
            if break_at.is_some_and(|b| offset >= b) {
                // The link breaks: what is in flight is gone.
                up.q.clear();
                down.q.clear();
                return (Err(SendError::Stalled { at: tx.acked() }), now);
            }
            match rx.data(sink, offset, bytes, now) {
                Out::Complete(s) => {
                    down.send(&Msg::Ack(s));
                    let done = rx.finish(sink);
                    down.send(&done.msg().unwrap());
                }
                o => {
                    if let Some(m) = o.msg() {
                        down.send(&m);
                    }
                }
            }
        }
        // The sender takes the answers.
        let mut heard = false;
        while let Some(b) = down.q.pop_front() {
            heard = true;
            let m = Msg::decode(&b).unwrap();
            match tx.on(&m) {
                Ok(Event::Done(info)) => return (Ok(info), now),
                Ok(_) => {}
                Err(e) => return (Err(e), now),
            }
        }
        if heard {
            last_heard = now;
        } else if now - last_heard >= STALL_MS {
            last_heard = now;
            if let Err(e) = tx.silence() {
                return (Err(e), now);
            }
            if tx.all_acked() && now > 200_000 {
                // A done lost for good: the caller's own deadline.
                return (Err(SendError::Stalled { at: tx.acked() }), now);
            }
        }
    }
    panic!("no end");
}

fn name(kind: Kind) -> &'static str {
    if kind == Kind::Script { "light" } else { "" }
}

fn reason_of(s: &str) -> crate::sender::Reason {
    // Reasons only come out of the sender; build one through it.
    let mut tx = Sender::new(1, 7, 0, 1, 1, 0);
    match tx.on(&Msg::Error {
        code: 0,
        crc: 7,
        reason: s,
    }) {
        Err(SendError::Refused { reason, .. }) => reason,
        _ => unreachable!(),
    }
}

fn file(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 + i / 7) as u8).collect()
}

const ALL: &[Kind] = &[Kind::Firmware, Kind::Script, Kind::Config];

#[test]
fn clean_push() {
    let f = file(100_000);
    let mut rx = Receiver::new(ALL, 10_000);
    let mut sink = VecSink::default();
    let (r, _) = push(&mut rx, &mut sink, Kind::Firmware, &f, 1, (0, 0, 0), None);
    assert_eq!(r, Ok(100_000));
    assert_eq!(sink.kept.as_deref(), Some(&f[..]));
    assert!(!rx.busy());
}

#[test]
fn pushes_through_loss_duplicates_and_reordering() {
    for seed in 1..20 {
        let f = file(60_000 + seed as usize * 37);
        let mut rx = Receiver::new(ALL, 10_000);
        let mut sink = VecSink::default();
        // Lost acks and dones are recovered by the stall: a lost done is
        // the one loss a sender cannot repair; it is what the caller's
        // done deadline is for, so the done path is kept reliable here.
        let (r, _) = push(&mut rx, &mut sink, Kind::Firmware, &f, seed, (20, 30, 50), None);
        match r {
            Ok(n) => {
                assert_eq!(n as usize, f.len(), "seed {seed}");
                assert_eq!(sink.kept.as_deref(), Some(&f[..]), "seed {seed}");
            }
            Err(SendError::Stalled { at }) => {
                assert_eq!(at as usize, f.len(), "only a lost done may end it, seed {seed}")
            }
            Err(e) => panic!("seed {seed}: {e}"),
        }
    }
}

#[test]
fn firmware_resumes_after_a_broken_link() {
    let f = file(50_000);
    let mut rx = Receiver::new(ALL, 10_000);
    let mut sink = VecSink::default();
    let (r, _) = push(&mut rx, &mut sink, Kind::Firmware, &f, 3, (0, 0, 0), Some(30_000));
    assert!(r.is_err());
    let (_, _, _, got) = rx.progress().unwrap();
    assert!(got > 0 && got <= 30_000);
    // The same put again: the receiver answers where it got to.
    let (r, _) = push(&mut rx, &mut sink, Kind::Firmware, &f, 4, (0, 0, 0), None);
    assert_eq!(r, Ok(50_000));
    assert_eq!(sink.kept.as_deref(), Some(&f[..]));
    assert_eq!(sink.opened, 1, "resumed, not restarted");
}

#[test]
fn scripts_do_not_resume_but_replace_once_idle() {
    let f = file(10_000);
    let mut rx = Receiver::new(ALL, 10_000);
    let mut sink = VecSink::default();
    let crc = crc32(&f);
    assert_eq!(
        rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Script,
                flags: 0,
                size: 10_000,
                crc,
                name: "light"
            },
            0
        ),
        Out::Ack(0)
    );
    rx.data(&mut sink, 0, &f[..240], 10);
    // The same script again soon after: busy; a script does not resume.
    assert!(matches!(
        rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Script,
                flags: 0,
                size: 10_000,
                crc,
                name: "light"
            },
            20
        ),
        Out::Error { code: code::BUSY, .. }
    ));
    // After the idle time it replaces the stale transfer.
    assert_eq!(
        rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Script,
                flags: 0,
                size: 10_000,
                crc,
                name: "light"
            },
            10_020
        ),
        Out::Ack(0)
    );
    assert_eq!(sink.aborted, 1);
}

#[test]
fn refusals() {
    let mut rx = Receiver::new(&[Kind::Firmware], 10_000);
    let mut sink = VecSink::default();
    let e = |o: Out| match o {
        Out::Error { code, .. } => code,
        other => panic!("{other:?}"),
    };
    // A kind this place does not take, a bad name, size 0, over the cap.
    assert_eq!(
        e(rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Script,
                flags: 0,
                size: 10,
                crc: 0,
                name: "x"
            },
            0
        )),
        code::KIND
    );
    let mut rx = Receiver::new(ALL, 10_000);
    assert_eq!(
        e(rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Script,
                flags: 0,
                size: 10,
                crc: 0,
                name: "a/b"
            },
            0
        )),
        code::NAME
    );
    assert_eq!(
        e(rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Config,
                flags: 0,
                size: 10,
                crc: 0,
                name: "x"
            },
            0
        )),
        code::NAME
    );
    assert_eq!(
        e(rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Config,
                flags: 0,
                size: 0,
                crc: 0,
                name: ""
            },
            0
        )),
        code::SIZE
    );
    assert_eq!(
        e(rx.put(
            &mut sink,
            &Offer {
                kind: Kind::Script,
                flags: 0,
                size: crate::MAX_SCRIPT + 1,
                crc: 0,
                name: "x"
            },
            0
        )),
        code::SIZE
    );
    assert_eq!(
        e(rx.put(
            &mut sink,
            &Offer {
                kind: Kind::ScriptList,
                flags: 0,
                size: 10,
                crc: 0,
                name: ""
            },
            0
        )),
        code::KIND
    );
    assert!(!rx.busy());
}

#[test]
fn crc_and_sink_failures() {
    let f = file(5000);
    // A wrong CRC in put: the file is refused at the end.
    let mut rx = Receiver::new(ALL, 10_000);
    let mut sink = VecSink::default();
    rx.put(
        &mut sink,
        &Offer {
            kind: Kind::Config,
            flags: 0,
            size: 1000,
            crc: 12345,
            name: "",
        },
        0,
    );
    assert_eq!(rx.data(&mut sink, 0, &f[..1000], 1), Out::Complete(1000));
    assert!(matches!(
        rx.finish(&mut sink),
        Out::Error {
            code: code::CRC,
            crc: 12345,
            ..
        }
    ));
    assert!(sink.kept.is_none() && sink.aborted == 1);
    // The sink refuses: its code goes out, with the file's CRC.
    let mut sink = VecSink {
        refuse_finish: true,
        ..Default::default()
    };
    let crc = crc32(&f[..1000]);
    rx.put(
        &mut sink,
        &Offer {
            kind: Kind::Config,
            flags: 0,
            size: 1000,
            crc,
            name: "",
        },
        0,
    );
    rx.data(&mut sink, 0, &f[..1000], 1);
    assert_eq!(
        rx.finish(&mut sink),
        Out::Error {
            code: 5,
            crc,
            reason: "bad signature"
        }
    );
}

#[test]
fn acks_rewinds_and_aborts() {
    let f = file(10_000);
    let crc = crc32(&f);
    let mut rx = Receiver::new(ALL, 10_000);
    let mut sink = VecSink::default();
    rx.put(
        &mut sink,
        &Offer {
            kind: Kind::Firmware,
            flags: 0,
            size: 10_000,
            crc,
            name: "",
        },
        0,
    );
    // An ack every ACK_EVERY bytes.
    let mut acks = Vec::new();
    for c in (0..8192).step_by(512) {
        if let Out::Ack(o) = rx.data(&mut sink, c, &f[c as usize..c as usize + 512], 1) {
            acks.push(o);
        }
    }
    assert_eq!(acks, [ACK_EVERY, 2 * ACK_EVERY]);
    // Data at another offset: a rewind, then none for REWIND_GAP_MS.
    assert_eq!(rx.data(&mut sink, 9000, &f[9000..9100], 10), Out::Rewind(8192));
    assert_eq!(rx.data(&mut sink, 9100, &f[9100..9200], 50), Out::None);
    assert_eq!(rx.data(&mut sink, 9100, &f[9100..9200], 300), Out::Rewind(8192));
    // An abort drops it.
    rx.abort(&mut sink);
    assert!(!rx.busy());
    assert_eq!(rx.data(&mut sink, 8192, &f[8192..], 400), Out::None);
}

#[test]
fn sender_rules() {
    let mut tx = Sender::new(10_000, 7, 0, 4096, 1000, 2);
    let sent: Vec<_> = core::iter::from_fn(|| tx.next_piece()).collect();
    assert_eq!(sent.len(), 5, "the window of 4096 holds 5 pieces of 1000");
    // An old ack, a stale done of another file: nothing.
    assert_eq!(tx.on(&Msg::Ack(0)), Ok(Event::None));
    assert_eq!(
        tx.on(&Msg::Done {
            kind: Kind::Firmware,
            crc: 8,
            info: 1
        }),
        Ok(Event::None)
    );
    assert_eq!(tx.on(&Msg::Ack(4096)), Ok(Event::Progress(4096)));
    // An ack past the end is out of step.
    assert_eq!(tx.on(&Msg::Ack(10_001)), Err(SendError::Protocol));
    // A rewind goes back.
    assert_eq!(tx.on(&Msg::Rewind(4096)), Ok(Event::Progress(4096)));
    assert_eq!(tx.next_piece(), Some((4096, 1000)));
    // Silences go back to the last ack, then give up.
    assert_eq!(tx.silence(), Ok(()));
    assert_eq!(tx.next_piece(), Some((4096, 1000)));
    tx.silence().unwrap();
    assert_eq!(tx.silence(), Err(SendError::Stalled { at: 4096 }));
    // Once all is acked, silences only wait for done.
    let mut tx = Sender::new(100, 7, 100, 4096, 1000, 0);
    assert!(tx.all_acked());
    assert_eq!(tx.silence(), Ok(()));
    assert!(matches!(
        tx.on(&Msg::Error {
            code: 1,
            crc: 7,
            reason: "no"
        }),
        Err(SendError::Refused { code: 1, .. })
    ));
}

#[test]
fn a_pull_takes_an_empty_file() {
    // The script list of a board without scripts: file 0, done at once.
    let mut rx = Receiver::new(&[], 10_000);
    let mut sink = VecSink::default();
    assert_eq!(
        rx.take(&mut sink, Kind::ScriptList, 0, crc32(&[]), "", 0),
        Out::Complete(0)
    );
    assert!(matches!(rx.finish(&mut sink), Out::Done { info: 0, .. }));
}

#[test]
fn a_pull_through_a_lossy_link() {
    // The board sends, bolt receives: the same machines, the other way.
    let f = file(16 * 1024);
    for seed in 1..10 {
        let mut rx = Receiver::new(&[], 10_000);
        let mut sink = VecSink::default();
        let crc = crc32(&f);
        assert_eq!(
            rx.take(&mut sink, Kind::Script, f.len() as u32, crc, "light", 0),
            Out::Ack(0)
        );
        let mut tx = Sender::new(f.len() as u32, crc, 0, WINDOW, 200, 10);
        let mut up = Link::new(seed, 30, 20, 40);
        let mut now = 0;
        let mut quiet = 0;
        let done = loop {
            now += 1;
            while let Some((at, len)) = tx.next_piece() {
                up.send(&Msg::Data {
                    offset: at,
                    bytes: &f[at as usize..(at + len) as usize],
                });
            }
            let mut answers = Vec::new();
            while let Some(b) = up.q.pop_front() {
                let Some(Msg::Data { offset, bytes }) = Msg::decode(&b) else {
                    continue;
                };
                match rx.data(&mut sink, offset, bytes, now) {
                    Out::Complete(s) => {
                        answers.push(Msg::Ack(s));
                        answers.push(rx.finish(&mut sink).msg().unwrap());
                    }
                    o => answers.extend(o.msg()),
                }
            }
            if answers.is_empty() {
                quiet += 1;
                if quiet >= STALL_MS {
                    quiet = 0;
                    tx.silence().unwrap();
                }
            }
            let mut finished = None;
            for a in &answers {
                if let Event::Done(i) = tx.on(a).unwrap() {
                    finished = Some(i);
                }
            }
            if let Some(i) = finished {
                break i;
            }
            assert!(now < 1_000_000);
        };
        assert_eq!(done as usize, f.len());
        assert_eq!(sink.kept.as_deref(), Some(&f[..]), "seed {seed}");
    }
}
