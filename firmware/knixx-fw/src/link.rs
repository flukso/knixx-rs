//! The parameters of the active BLE connection, which the console's ble
//! link shows. The BLE task fills them in as the connection comes up and
//! changes, and reads the RSSI and the TX power every READ_PERIOD; there is
//! no record without a connection.

use core::cell::RefCell;
use core::fmt;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_time::{Duration, Instant};
use trouble_host::prelude::PhyKind;

use crate::bonds::Addr;

/// How often the BLE task reads the RSSI and the TX power of the link.
pub const READ_PERIOD: Duration = Duration::from_secs(2);

/// The connection as the board sees it.
#[derive(Clone, Copy)]
pub struct Link {
    /// The central's address, its identity address once it is known.
    pub addr: [u8; 6],
    pub bonded: bool,
    pub encrypted: bool,
    pub since: Instant,
    pub interval: Duration,
    pub latency: u16,
    pub timeout: Duration,
    pub tx_phy: PhyKind,
    pub rx_phy: PhyKind,
    /// The largest link-layer payloads, in bytes, and their air times, in
    /// µs: 27 B and 328 µs until both sides agree on more.
    pub tx_octets: u16,
    pub tx_time: u16,
    pub rx_octets: u16,
    pub rx_time: u16,
    pub mtu: u16,
    pub tx_power: Option<i8>,
    pub rssi: Option<i8>,
}

impl Link {
    /// A link with addr, its parameters as the connection reported them.
    pub fn new(addr: [u8; 6], interval: Duration, latency: u16, timeout: Duration) -> Link {
        Link {
            addr,
            bonded: false,
            encrypted: false,
            since: Instant::now(),
            interval,
            latency,
            timeout,
            tx_phy: PhyKind::Le1M,
            rx_phy: PhyKind::Le1M,
            tx_octets: 27,
            tx_time: 328,
            rx_octets: 27,
            rx_time: 328,
            mtu: 23,
            tx_power: None,
            rssi: None,
        }
    }
}

static LINK: Mutex<CriticalSectionRawMutex, RefCell<Option<Link>>> = Mutex::new(RefCell::new(None));

/// Records a new connection, or none.
pub fn set(l: Option<Link>) {
    LINK.lock(|c| *c.borrow_mut() = l);
}

/// Changes the record of the connection, if there is one.
pub fn update(f: impl FnOnce(&mut Link)) {
    LINK.lock(|c| {
        if let Some(l) = c.borrow_mut().as_mut() {
            f(l)
        }
    });
}

/// The record of the connection, if there is one.
pub fn get() -> Option<Link> {
    LINK.lock(|c| *c.borrow())
}

/// A PHY as people name it.
pub fn phy(p: PhyKind) -> &'static str {
    match p {
        PhyKind::Le1M => "1M",
        PhyKind::Le2M => "2M",
        PhyKind::LeCoded => "coded S8",
        PhyKind::LeCodedS2 => "coded S2",
    }
}

/// A duration in seconds, as Go's time.Duration prints it: 45s, 1m0s,
/// 2h26m30s.
pub struct Uptime(pub u64);

impl fmt::Display for Uptime {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let (h, m, s) = (self.0 / 3600, self.0 / 60 % 60, self.0 % 60);
        if h > 0 {
            write!(f, "{h}h{m}m{s}s")
        } else if m > 0 {
            write!(f, "{m}m{s}s")
        } else {
            write!(f, "{s}s")
        }
    }
}

/// A time in µs, rounded to a step of µs as BLE counts it (1250 for the
/// interval, 10 000 for the timeout): 15 ms, 7.5 ms, 4 s.
struct Time(u64, u64);

impl fmt::Display for Time {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let us = (self.0 + self.1 / 2) / self.1 * self.1;
        if us >= 1_000_000 && us.is_multiple_of(1_000_000) {
            write!(f, "{} s", us / 1_000_000)
        } else if us.is_multiple_of(1000) {
            write!(f, "{} ms", us / 1000)
        } else {
            let frac = us % 1000;
            let frac = if frac.is_multiple_of(100) {
                [b'0' + (frac / 100) as u8, 0]
            } else {
                [b'0' + (frac / 100) as u8, b'0' + (frac / 10 % 10) as u8]
            };
            let n = if frac[1] == 0 { 1 } else { 2 };
            write!(
                f,
                "{}.{} ms",
                us / 1000,
                core::str::from_utf8(&frac[..n]).unwrap_or("0")
            )
        }
    }
}

impl fmt::Display for Link {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let security = match (self.bonded, self.encrypted) {
            (true, true) => "bonded, encrypted",
            (false, true) => "encrypted, not bonded",
            (true, false) => "bonded, not yet encrypted",
            (false, false) => "not encrypted",
        };
        writeln!(f, "central    {} ({security})", Addr(self.addr))?;
        writeln!(f, "connected  {}", Uptime(self.since.elapsed().as_secs()))?;
        writeln!(
            f,
            "interval   {}, latency {}, supervision timeout {}",
            Time(self.interval.as_micros(), 1250),
            self.latency,
            Time(self.timeout.as_micros(), 10_000)
        )?;
        writeln!(f, "phy        tx {}, rx {}", phy(self.tx_phy), phy(self.rx_phy))?;
        writeln!(
            f,
            "data       tx {} B / {} µs, rx {} B / {} µs",
            self.tx_octets, self.tx_time, self.rx_octets, self.rx_time
        )?;
        writeln!(f, "att mtu    {}", self.mtu)?;
        match self.tx_power {
            Some(p) => writeln!(f, "tx power   {p} dBm")?,
            None => writeln!(f, "tx power   -")?,
        }
        match self.rssi {
            Some(r) => writeln!(f, "rssi       {r} dBm"),
            None => writeln!(f, "rssi       -"),
        }
    }
}
