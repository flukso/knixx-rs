//! The KNX bus engine: sending, confirmations, occupancy and the sniffer.
//!
//! It drives the NCN5121 transceiver: it transmits queued commands and
//! reports what the transceiver sends back. [`Engine`] holds the state and
//! does no I/O; [`run`] drives it over a UART.
#![no_std]

#[cfg(test)]
extern crate std;

mod decode;
mod format;
#[cfg(test)]
mod tests;

use core::fmt::{self, Write as _};
use core::future::pending;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_futures::select::{Either3, select3};
use embassy_sync::blocking_mutex::raw::RawMutex;
use embassy_sync::channel::Receiver as ChannelReceiver;
use embassy_time::{Duration, Instant, Timer};
use knixx_device::thunder::{Device, MAX_GROUPS};
use knixx_knx::GroupAddress;
use knixx_knx::tp1::{self, MAX_FRAME};
use knixx_ncn::{self as ncn, Parser, Service, TxBuf};

use decode::Decoder;
use format::{Line, Write, bool_write, f16_write};

/// The length of the command queue.
pub const QUEUE_LEN: usize = 16;

/// Bounds the wait for the L_Data.con of a sent telegram. It covers the
/// NCN5121's worst case of 3 NACK and 3 BUSY repetitions.
const CON_TIMEOUT: Duration = Duration::from_millis(500);

/// Bounds the wait for the replies to a stat request.
const STAT_TIMEOUT: Duration = Duration::from_millis(100);

/// The flags the engine shares with the rest of the firmware.
pub struct Flags {
    /// Enables the trace of the transceiver's indications and of the decoded
    /// telegrams on the bus.
    pub sniff: AtomicBool,
    /// The occupancy that the engine last published.
    pub occupied: AtomicBool,
}

impl Flags {
    pub const fn new() -> Self {
        Flags {
            sniff: AtomicBool::new(false),
            occupied: AtomicBool::new(false),
        }
    }
}

impl Default for Flags {
    fn default() -> Self {
        Self::new()
    }
}

/// A command for the engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Command {
    /// Sets a switch, an index into Device::switches.
    Switch { switch: u8, on: bool },
    /// Requests the state and the system status of the transceiver.
    Stat,
    /// Replaces the device configuration.
    Config(Device),
    /// Publishes the temperature in milli degrees Celsius and the humidity in
    /// hundredths of a percent to the sensor's groups.
    Sensor { temperature: i32, humidity: i32 },
    /// Reports motion seen by the time-of-flight sensor.
    Motion,
    /// Publishes the illuminance in tenths of a lux to the illuminance group.
    Light { illuminance: i32 },
}

/// Receives everything the engine prints.
pub trait Output {
    /// Writes a whole line, ending in "\n".
    fn write(&mut self, b: &[u8]);
    /// Is called for each error the engine reports.
    fn failed(&mut self);
}

/// Prints an error line on out and reports it to out.failed.
fn errorln(line: &mut Line, out: &mut impl Output, args: fmt::Arguments<'_>) {
    line.clear();
    let _ = line.write_fmt(args);
    line.push(b'\n');
    out.write(line.as_bytes());
    out.failed();
}

/// Turns motion into occupancy: it is on from the first motion until a
/// timeout passed without motion.
#[derive(Clone, Copy, Debug)]
struct Occupancy {
    on: bool,
    last: Instant,
}

impl Occupancy {
    /// Records motion at now and reports whether occupancy turns on.
    fn motion(&mut self, now: Instant) -> bool {
        self.last = now;
        if self.on {
            return false;
        }
        self.on = true;
        true
    }

    /// Reports whether occupancy turns off at now, timeout after the last
    /// motion.
    fn expire(&mut self, now: Instant, timeout: Duration) -> bool {
        if !self.on || now.saturating_duration_since(self.last) < timeout {
            return false;
        }
        self.on = false;
        true
    }
}

struct Receiver {
    parser: Parser,
    /// Decodes the frames while sniffing.
    dec: Decoder,
    /// Requests that a U_State.ind and U_SystemStat.ind be reported in full;
    /// the flags record which of the two have been seen.
    stat: bool,
    got_state: bool,
    got_system_stat: bool,
    /// Records an L_Data.con and con_positive its outcome.
    got_con: bool,
    con_positive: bool,
    /// Where a line of the trace is rendered: one write each.
    line: Line,
}

impl Receiver {
    /// Reports the services data contains. Frames are decoded only while
    /// sniffing.
    fn handle(&mut self, data: &[u8], dev: &Device, sniff: &AtomicBool, out: &mut impl Output) {
        for &b in data {
            let Some((s, v)) = self.parser.feed(b) else {
                continue;
            };
            if s == Service::LDataInd {
                if sniff.load(Ordering::Relaxed) {
                    self.line.clear();
                    self.line.push_str("  ");
                    self.dec.push_frame(&mut self.line, dev, self.parser.frame());
                    self.line.push(b'\n');
                    out.write(self.line.as_bytes());
                }
                continue;
            }
            if s == Service::LDataCon {
                self.got_con = true;
                self.con_positive = ncn::con_positive(v);
            }
            if self.stat && s == Service::UStateInd && !self.got_state {
                self.got_state = true;
            } else if self.stat && s == Service::USystemStatInd && !self.got_system_stat {
                self.got_system_stat = true;
            } else if !sniff.load(Ordering::Relaxed) {
                continue;
            }
            self.line.clear();
            self.line.push_str("  ");
            let _ = write!(self.line, "{}", ncn::describe(s, v));
            self.line.push_str(" (0x");
            self.line.push_hex2(v);
            self.line.push_str(")\n");
            out.write(self.line.as_bytes());
        }
    }
}

/// An error sending a telegram.
enum SendError {
    Tp1(tp1::Error),
    Ncn(ncn::Error),
}

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SendError::Tp1(e) => e.fmt(f),
            SendError::Ncn(e) => e.fmt(f),
        }
    }
}

/// Holds the buffers of a telegram being sent.
struct Sender {
    frame: [u8; MAX_FRAME],
    req: TxBuf,
}

impl Sender {
    /// Builds the request of the group value write of data to group in req
    /// and returns its length.
    fn send(&mut self, dev: &Device, group: u16, data: &[u8]) -> Result<usize, SendError> {
        let tg = dev.group_write(group, data);
        let buf = tg.pack_to(&mut self.frame).map_err(SendError::Tp1)?;
        let req = ncn::tx_data(buf, &mut self.req).map_err(SendError::Ncn)?;
        Ok(req.len())
    }
}

/// Requests the state and the system status of the transceiver.
const STAT_REQ: [u8; 2] = [ncn::U_STATE_REQ, ncn::U_SYSTEM_STAT_REQ];

/// The bus engine, without I/O: it executes the commands and reports
/// whatever the transceiver returns in between.
///
/// A switch command sends one telegram per bound group and a sensor command
/// one per bound value, each after the previous one was confirmed by
/// L_Data.con or timed out; further commands wait until all telegrams are
/// done. Motion turns occupancy on, and the occupancy is turned off again
/// once the configured timeout passed without motion; both are published as
/// DPT 1.018 to the occupancy group, if it is bound. The engine works on its
/// own copy of the device, which [`Command::Config`] replaces.
pub struct Engine<'a> {
    flags: &'a Flags,
    cur: Device,
    rx: Receiver,
    /// The writes of the command being sent, and the index of the next one.
    queue: [Write; MAX_GROUPS],
    nq: usize,
    next: usize,
    /// Whether the L_Data.con for group is awaited, until con_deadline.
    awaiting: bool,
    group: u16,
    con_deadline: Instant,
    /// A stat request waits for poll; the replies are awaited until
    /// stat_deadline while rx.stat is set.
    stat_req: bool,
    stat_deadline: Instant,
    occ: Occupancy,
    tx: Sender,
}

impl<'a> Engine<'a> {
    pub fn new(dev: Device, flags: &'a Flags) -> Self {
        Engine {
            flags,
            cur: dev,
            rx: Receiver {
                parser: Parser::new(),
                dec: Decoder::new(),
                stat: false,
                got_state: false,
                got_system_stat: false,
                got_con: false,
                con_positive: false,
                line: Line::new(),
            },
            queue: [Write::default(); MAX_GROUPS],
            nq: 0,
            next: 0,
            awaiting: false,
            group: 0,
            con_deadline: Instant::MIN,
            stat_req: false,
            stat_deadline: Instant::MIN,
            occ: Occupancy {
                on: false,
                last: Instant::MIN,
            },
            tx: Sender {
                frame: [0; MAX_FRAME],
                req: [0; 2 * ncn::MAX_FRAME_LEN],
            },
        }
    }

    /// Reports whether the engine takes the next command: no telegram is
    /// pending or awaited and no stat is in progress.
    pub fn accepts_commands(&self) -> bool {
        self.next >= self.nq && !self.awaiting && !self.stat_req && !self.rx.stat
    }

    /// Executes cmd. The caller passes commands only while
    /// [`accepts_commands`](Self::accepts_commands), then calls
    /// [`poll`](Self::poll) for what to transmit.
    pub fn command(&mut self, cmd: Command, now: Instant, out: &mut impl Output) {
        match cmd {
            Command::Switch { switch, on } => {
                let Some(sw) = self.cur.switches.get(switch as usize) else {
                    errorln(&mut self.rx.line, out, format_args!("bus: no switch {switch}"));
                    return;
                };
                self.nq = 0;
                self.next = 0;
                for &g in sw.group_list() {
                    self.queue[self.nq] = bool_write(g, on);
                    self.nq += 1;
                }
            }
            Command::Sensor {
                temperature,
                humidity,
            } => {
                self.nq = 0;
                self.next = 0;
                let s = self.cur.sensor;
                if s.temperature != 0 {
                    self.queue[self.nq] = f16_write(s.temperature, temperature / 10);
                    self.nq += 1;
                }
                if s.humidity != 0 {
                    self.queue[self.nq] = f16_write(s.humidity, humidity);
                    self.nq += 1;
                }
            }
            Command::Light { illuminance } => {
                let g = self.cur.sensor.illuminance;
                if g == 0 {
                    return;
                }
                self.queue[0] = f16_write(g, illuminance.wrapping_mul(10));
                self.nq = 1;
                self.next = 0;
            }
            Command::Motion => {
                let g = self.cur.occupancy.group;
                if g == 0 || !self.occ.motion(now) {
                    return;
                }
                self.flags.occupied.store(true, Ordering::Relaxed);
                self.queue[0] = bool_write(g, true);
                self.nq = 1;
                self.next = 0;
            }
            Command::Stat => self.stat_req = true,
            Command::Config(dev) => {
                self.cur = dev;
                self.rx.dec.reset();
            }
        }
    }

    /// Handles bytes from the transceiver: it reports the services they
    /// contain and acts on an L_Data.con and the stat replies right away.
    pub fn feed(&mut self, rx: &[u8], _now: Instant, out: &mut impl Output) {
        self.rx.handle(rx, &self.cur, &self.flags.sniff, out);
        if self.rx.stat && self.rx.got_state && self.rx.got_system_stat {
            self.end_stat(out);
        }
        if self.awaiting && self.rx.got_con {
            self.awaiting = false;
            if self.rx.con_positive {
            } else if self.flags.sniff.load(Ordering::Relaxed) {
                // The sniffer has shown it already.
                out.failed();
            } else {
                let g = GroupAddress(self.group);
                errorln(
                    &mut self.rx.line,
                    out,
                    format_args!("  L_Data.con: negative for {g}"),
                );
            }
        }
    }

    /// Ends a stat, reporting the replies that did not arrive.
    fn end_stat(&mut self, out: &mut impl Output) {
        self.rx.stat = false;
        if !self.rx.got_state {
            errorln(&mut self.rx.line, out, format_args!("  no U_State.ind reply"));
        }
        if !self.rx.got_system_stat {
            errorln(
                &mut self.rx.line,
                out,
                format_args!("  no U_SystemStat.ind reply"),
            );
        }
    }

    /// Handles the deadlines that expired at now, then returns the next bytes
    /// to transmit, if any: a stat request or the request of the next group
    /// write. The caller writes them to the transceiver and calls poll again
    /// until it returns None.
    pub fn poll(&mut self, now: Instant, out: &mut impl Output) -> Option<&[u8]> {
        if self.rx.stat {
            if now < self.stat_deadline {
                return None;
            }
            self.end_stat(out);
        }
        if self.stat_req {
            self.stat_req = false;
            self.rx.stat = true;
            self.rx.got_state = false;
            self.rx.got_system_stat = false;
            self.stat_deadline = now + STAT_TIMEOUT;
            return Some(&STAT_REQ);
        }
        if self.awaiting {
            if now < self.con_deadline {
                return None;
            }
            self.awaiting = false;
            let g = GroupAddress(self.group);
            errorln(&mut self.rx.line, out, format_args!("  no L_Data.con for {g}"));
        }
        if self.next >= self.nq && self.occ.expire(now, self.occupancy_timeout()) {
            self.flags.occupied.store(false, Ordering::Relaxed);
            let g = self.cur.occupancy.group;
            if g != 0 {
                self.queue[0] = bool_write(g, false);
                self.nq = 1;
                self.next = 0;
            }
        }
        while self.next < self.nq {
            let w = self.queue[self.next];
            self.next += 1;
            self.group = w.group;
            match self.tx.send(&self.cur, w.group, w.data()) {
                Ok(n) => {
                    self.awaiting = true;
                    self.rx.got_con = false;
                    self.con_deadline = now + CON_TIMEOUT;
                    return Some(&self.tx.req[..n]);
                }
                Err(e) => errorln(&mut self.rx.line, out, format_args!("{e}")),
            }
        }
        None
    }

    /// Reports that the bytes poll returned last could not be written to the
    /// transceiver: nothing will answer them, so the engine stops waiting for
    /// the L_Data.con or the stat replies, as Go did on a write error.
    pub fn transmit_failed(&mut self) {
        if self.awaiting {
            self.awaiting = false;
        } else if self.rx.stat {
            self.rx.stat = false;
        }
    }

    /// Returns the earliest pending deadline: of the awaited L_Data.con, of
    /// the stat replies or of the occupancy. The caller calls poll then.
    pub fn deadline(&self) -> Option<Instant> {
        if self.awaiting {
            Some(self.con_deadline)
        } else if self.rx.stat {
            Some(self.stat_deadline)
        } else if self.occ.on && self.next >= self.nq && !self.stat_req {
            Some(self.occ.last.saturating_add(self.occupancy_timeout()))
        } else {
            None
        }
    }

    fn occupancy_timeout(&self) -> Duration {
        Duration::from_secs(self.cur.occupancy.timeout as u64)
    }
}

/// Runs engine over uart: it transmits what the engine has to send and
/// passes it the bytes received, the commands from cmds while it takes them,
/// and its deadlines. The UART's read must be cancel-safe: it is dropped when
/// a command or a deadline comes first.
pub async fn run<U, M, const N: usize>(
    uart: &mut U,
    engine: &mut Engine<'_>,
    cmds: &ChannelReceiver<'_, M, Command, N>,
    out: &mut impl Output,
) -> !
where
    U: embedded_io_async::Read + embedded_io_async::Write,
    M: RawMutex,
{
    use embedded_io_async::Error as _;

    let mut input = [0u8; 128];
    loop {
        while let Some(b) = engine.poll(Instant::now(), out) {
            if let Err(e) = uart.write_all(b).await {
                let kind = e.kind();
                errorln(
                    &mut engine.rx.line,
                    out,
                    format_args!("bus: uart write: {kind:?}"),
                );
                engine.transmit_failed();
            }
        }

        let accept = engine.accepts_commands();
        let deadline = engine.deadline();
        let cmd = async {
            if accept {
                cmds.receive().await
            } else {
                pending().await
            }
        };
        let timer = async {
            match deadline {
                Some(t) => Timer::at(t).await,
                None => pending().await,
            }
        };
        let r = select3(uart.read(&mut input), cmd, timer).await;
        match r {
            Either3::First(Ok(n)) => engine.feed(&input[..n], Instant::now(), out),
            // Read errors are dropped, as in Go.
            Either3::First(Err(_)) => {}
            Either3::Second(cmd) => engine.command(cmd, Instant::now(), out),
            Either3::Third(()) => {}
        }
    }
}
