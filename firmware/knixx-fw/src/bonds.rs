//! The BLE bonds: the centrals that paired while NUS was on, up to MAX,
//! oldest first. They are kept in /ble/bonds on the flash and handed to the
//! BLE host at boot, so that a bonded central gets in again without
//! pairing. A new bond beyond MAX replaces the oldest.
//!
//! Each bond also keeps the hash of the GATT database its central last
//! saw: a central caches the services of a bonded peripheral, so after a
//! firmware update that changed them the BLE task tells it with Service
//! Changed (see ble).

use core::cell::RefCell;
use core::fmt;

use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use trouble_host::prelude::*;

use crate::storage;

/// The most bonds kept.
pub const MAX: usize = 4;

const DIR: &str = "/ble";
const PATH: &str = "/ble/bonds";

/// The file: MAGIC, then a record of RECORD bytes per bond. A file of the
/// first version, MAGIC_V1, has records of RECORD_V1 bytes without the
/// hash: its centrals are told of a changed database.
const MAGIC: [u8; 4] = *b"KXB2";
const RECORD: usize = 57;
const MAGIC_V1: [u8; 4] = *b"KXB1";
const RECORD_V1: usize = 41;
const FILE: usize = MAGIC.len() + MAX * RECORD;

/// A bond as the file keeps it: the central's identity address, its
/// identity resolving key if it sent one, the long term key and whether the
/// pairing was authenticated, and the hash of the GATT database the central
/// last saw.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Bond {
    kind: u8,
    /// The address as on air: least significant byte first.
    addr: [u8; 6],
    irk: Option<[u8; 16]>,
    ltk: [u8; 16],
    authenticated: bool,
    db_hash: u128,
}

impl Bond {
    /// The bond of info, made by a central that saw the database of hash
    /// db_hash.
    pub fn from_info(b: &BondInformation, db_hash: u128) -> Bond {
        let mut addr = [0; 6];
        addr.copy_from_slice(b.identity.addr.addr.raw());
        Bond {
            kind: b.identity.addr.kind.as_raw(),
            addr,
            irk: b.identity.irk.map(|k| k.to_le_bytes()),
            ltk: b.ltk.to_le_bytes(),
            authenticated: b.security_level == SecurityLevel::EncryptedAuthenticated,
            db_hash,
        }
    }

    pub fn info(&self) -> BondInformation {
        let identity = Identity {
            addr: Address {
                kind: AddrKind::new(self.kind),
                addr: BdAddr::new(self.addr),
            },
            irk: self.irk.and_then(IdentityResolvingKey::from_le_bytes),
        };
        let level = if self.authenticated {
            SecurityLevel::EncryptedAuthenticated
        } else {
            SecurityLevel::Encrypted
        };
        BondInformation::new(identity, LongTermKey::from_le_bytes(self.ltk), level, true)
    }

    /// Whether b is the same central: the same identity address.
    fn same(&self, b: &Bond) -> bool {
        self.kind == b.kind && self.addr == b.addr
    }

    fn encode(&self, r: &mut [u8]) {
        r[0] = self.kind;
        r[1..7].copy_from_slice(&self.addr);
        r[7] = self.irk.is_some() as u8;
        r[8..24].copy_from_slice(&self.irk.unwrap_or_default());
        r[24..40].copy_from_slice(&self.ltk);
        r[40] = self.authenticated as u8;
        r[41..57].copy_from_slice(&self.db_hash.to_le_bytes());
    }

    /// Decodes a record, of either version: one without the hash gets 0.
    fn decode(r: &[u8]) -> Bond {
        let mut addr = [0; 6];
        addr.copy_from_slice(&r[1..7]);
        let mut irk = [0; 16];
        irk.copy_from_slice(&r[8..24]);
        let mut ltk = [0; 16];
        ltk.copy_from_slice(&r[24..40]);
        Bond {
            kind: r[0],
            addr,
            irk: (r[7] != 0).then_some(irk),
            ltk,
            authenticated: r[40] != 0,
            db_hash: r
                .get(41..57)
                .and_then(|h| h.try_into().ok())
                .map(u128::from_le_bytes)
                .unwrap_or(0),
        }
    }

    /// The address as people write it: most significant byte first.
    pub fn addr(&self) -> Addr {
        Addr(self.addr)
    }
}

impl fmt::Display for Bond {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let kind = if self.kind == AddrKind::PUBLIC.as_raw() {
            "public"
        } else {
            "random"
        };
        write!(f, "{} ({kind}", self.addr())?;
        if self.irk.is_some() {
            f.write_str(", private addresses")?;
        }
        f.write_str(")")
    }
}

/// A BLE address, written aa:bb:cc:dd:ee:ff.
pub struct Addr(pub [u8; 6]);

impl fmt::Display for Addr {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let a = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            a[5], a[4], a[3], a[2], a[1], a[0]
        )
    }
}

impl Addr {
    /// Parses aa:bb:cc:dd:ee:ff, in any case.
    pub fn parse(s: &str) -> Option<Addr> {
        let mut a = [0u8; 6];
        let mut parts = s.split(':');
        for i in (0..6).rev() {
            let p = parts.next()?;
            if p.len() != 2 {
                return None;
            }
            a[i] = u8::from_str_radix(p, 16).ok()?;
        }
        parts.next().is_none().then_some(Addr(a))
    }
}

type List = heapless::Vec<Bond, MAX>;

static BONDS: Mutex<CriticalSectionRawMutex, RefCell<List>> = Mutex::new(RefCell::new(heapless::Vec::new()));

/// Tells the BLE task that the bonds or the BLE settings changed: it drops
/// the bonds that are gone from the host and the central that may no longer
/// be connected.
pub static CHANGED: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Loads the bonds from the flash, writing what went wrong to log.
pub fn load(log: &mut dyn fmt::Write) {
    let mut buf = [0u8; FILE];
    let n = match storage::read_file(PATH, &mut buf) {
        Ok(n) => n,
        Err(storage::FileError::Fs(e)) if e.not_found() => return,
        Err(e) => {
            let _ = writeln!(log, "ble bonds: {e}");
            return;
        }
    };
    let data = &buf[..n];
    let record = match data.get(..MAGIC.len()) {
        Some(m) if m == MAGIC => RECORD,
        Some(m) if m == MAGIC_V1 => RECORD_V1,
        _ => 0,
    };
    if record == 0 || !(n - MAGIC.len()).is_multiple_of(record) {
        let _ = writeln!(log, "ble bonds: {PATH} is not a bonds file, ignored");
        return;
    }
    BONDS.lock(|b| {
        let mut b = b.borrow_mut();
        b.clear();
        for r in data[MAGIC.len()..].chunks_exact(record) {
            let _ = b.push(Bond::decode(r));
        }
    });
}

/// Writes the bonds to the flash.
fn save(list: &List) -> Result<(), storage::FileError> {
    let mut buf = [0u8; FILE];
    buf[..MAGIC.len()].copy_from_slice(&MAGIC);
    for (i, b) in list.iter().enumerate() {
        let at = MAGIC.len() + i * RECORD;
        b.encode(&mut buf[at..at + RECORD]);
    }
    storage::write_file(DIR, PATH, &buf[..MAGIC.len() + list.len() * RECORD])
}

/// The bonds, oldest first.
pub fn list() -> List {
    BONDS.lock(|b| b.borrow().clone())
}

/// Adds a bond, or renews the central's, and saves the bonds. It returns
/// the oldest bond when it made room for the new one.
pub fn add(bond: Bond) -> (Option<Bond>, Result<(), storage::FileError>) {
    let (evicted, list) = BONDS.lock(|b| {
        let mut b = b.borrow_mut();
        if let Some(i) = b.iter().position(|x| x.same(&bond)) {
            b.remove(i);
        }
        let evicted = if b.is_full() { Some(b.remove(0)) } else { None };
        let _ = b.push(bond);
        (evicted, b.clone())
    });
    (evicted, save(&list))
}

/// Removes the bonds for which drop returns true, saves the rest and tells
/// the BLE task. It returns how many it removed.
pub fn forget(mut drop: impl FnMut(&Bond) -> bool) -> (usize, Result<(), storage::FileError>) {
    let (n, list) = BONDS.lock(|b| {
        let mut b = b.borrow_mut();
        let before = b.len();
        b.retain(|x| !drop(x));
        (before - b.len(), b.clone())
    });
    if n == 0 {
        return (0, Ok(()));
    }
    let saved = save(&list);
    CHANGED.signal(());
    (n, saved)
}

/// Whether info is one of the bonds.
pub fn contains(info: &BondInformation) -> bool {
    let b = Bond::from_info(info, 0);
    BONDS.lock(|l| l.borrow().iter().any(|x| x.same(&b)))
}

/// Whether the bonded central of identity id last saw a GATT database
/// other than that of hash db_hash. A central without a bond saw none.
pub fn stale(id: &Identity, db_hash: u128) -> bool {
    BONDS.lock(|l| {
        l.borrow()
            .iter()
            .any(|b| b.info().identity.match_identity(id) && b.db_hash != db_hash)
    })
}

/// Records that the bonded central of identity id saw the GATT database of
/// hash db_hash, and saves the bonds.
pub fn seen(id: &Identity, db_hash: u128) -> Result<(), storage::FileError> {
    let list = BONDS.lock(|l| {
        let mut l = l.borrow_mut();
        for b in l.iter_mut().filter(|b| b.info().identity.match_identity(id)) {
            b.db_hash = db_hash;
        }
        l.clone()
    });
    save(&list)
}
