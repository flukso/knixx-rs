//! The firmware image of knixx thunder, as the bootloader, the firmware and
//! the host tools share it.
//!
//! A `.kfw` file is a 256-byte [`Header`] and the body: the application's
//! bytes from [`APP_START`] on. In internal flash the header sits right
//! below the application, at [`HEADER_ADDR`], and is written last, so that
//! it marks the application as whole.
//!
//! The header carries the body's length and CRC-32, a version counter that
//! only goes up, and an ed25519 signature over the header and the body. The
//! bootloader checks the CRC on every boot and the signature before it
//! installs an image; the firmware checks both before it keeps a staged one.
//!
//! The bootloader's decisions, which image to boot or install and when to
//! wait for one over USB, are pure functions here, so that the host tests
//! them.

#![no_std]

#[cfg(any(test, feature = "sign"))]
extern crate std;

use core::fmt;

pub use ed25519_compact::{PublicKey, Signature};

/// The bootloader, from the start of flash.
pub const BOOT_START: u32 = 0x0000_0000;
pub const BOOT_SIZE: u32 = 0x0001_0000;
/// The header of the installed application.
pub const HEADER_ADDR: u32 = 0x0001_0000;
pub const HEADER_SIZE: usize = 256;
/// The application, from its vector table to the end of flash.
pub const APP_START: u32 = HEADER_ADDR + HEADER_SIZE as u32;
pub const APP_END: u32 = 0x0010_0000;
/// The largest body: the application region.
pub const MAX_BODY: u32 = APP_END - APP_START;
/// The largest .kfw file that the board stages, header and body, so that a
/// sender cannot fill the littlefs.
pub const MAX_IMAGE: u32 = 1024 * 1024;
/// The internal flash's erase unit.
pub const FLASH_PAGE: u32 = 4096;

/// The staged image on the littlefs, and the file it is written to first.
pub const STAGED_DIR: &str = "/fw";
pub const STAGED_PATH: &str = "/fw/update.kfw";
pub const STAGED_TMP: &str = "/fw/update.tmp";

/// GPREGRET: what the next boot does. The bootloader clears it.
pub const GPREGRET_RECOVERY: u8 = 0x57;
pub const GPREGRET_INSTALL: u8 = 0xB1;
/// The bootloader's own: no application and nothing to install.
pub const GPREGRET_NO_APP: u8 = 0xB2;

/// The word at the start of RAM that the bootloader sets during the double
/// tap's wait: a second reset within it enters recovery.
pub const DOUBLE_TAP_ADDR: u32 = 0x2000_0000;
pub const DOUBLE_TAP_MAGIC: u32 = 0x4B44_5450; // "KDTP"

/// The header's magic.
pub const MAGIC: [u8; 4] = *b"KNXX";
/// The header's layout version.
pub const HEADER_VERSION: u16 = 1;

/// The length of the version string.
pub const VERSION_LEN: usize = 32;

// The header's layout, little-endian.
const OFF_MAGIC: usize = 0;
const OFF_HVERSION: usize = 4;
const OFF_FLAGS: usize = 6;
const OFF_KEY: usize = 7;
const OFF_LOAD: usize = 8;
const OFF_LEN: usize = 12;
const OFF_BODY_CRC: usize = 16;
const OFF_COUNTER: usize = 20;
const OFF_VERSION: usize = 24;
const OFF_BUILT: usize = OFF_VERSION + VERSION_LEN; // 56
const OFF_SIG: usize = 64;
const SIG_LEN: usize = 64;
const OFF_RESERVED: usize = OFF_SIG + SIG_LEN; // 128
const OFF_HCRC: usize = HEADER_SIZE - 4; // 252

/// The CRC-32 of zlib and the shells' `crc32` (IEEE 802.3, reflected,
/// polynomial 0xEDB88320), from a table.
pub fn crc32(data: &[u8]) -> u32 {
    let mut c = Crc32::new();
    c.update(data);
    c.finish()
}

const CRC_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xEDB8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// A CRC-32 over data fed in pieces.
#[derive(Clone, Copy, Debug)]
pub struct Crc32(u32);

impl Crc32 {
    pub const fn new() -> Crc32 {
        Crc32(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        let mut c = self.0;
        for &b in data {
            c = CRC_TABLE[((c ^ b as u32) & 0xFF) as usize] ^ (c >> 8);
        }
        self.0 = c;
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Crc32::new()
    }
}

/// What a header holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub flags: u8,
    /// Which of the board's public keys signed the image.
    pub key: u8,
    /// Where the body goes: APP_START.
    pub load: u32,
    /// The body's length in bytes.
    pub len: u32,
    pub body_crc: u32,
    /// The version counter: the firmware's build, in seconds since 1970,
    /// which only goes up from one image to the next.
    pub counter: u32,
    /// The version string, such as the git commit: NUL-padded.
    pub version: [u8; VERSION_LEN],
    /// The build time, in seconds since 1970.
    pub built: u64,
    pub signature: [u8; SIG_LEN],
}

/// Why a header is not one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderError {
    /// Erased flash, or not a header at all.
    Magic,
    /// A layout this code does not know.
    Version(u16),
    /// The header's own CRC is wrong.
    Crc,
    /// The body does not go to APP_START.
    Load(u32),
    /// The body is empty or larger than the application region.
    Len(u32),
}

impl fmt::Display for HeaderError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            HeaderError::Magic => f.write_str("no image header"),
            HeaderError::Version(v) => write!(f, "header version {v} unknown"),
            HeaderError::Crc => f.write_str("header CRC mismatch"),
            HeaderError::Load(a) => write!(f, "load address {a:#x}, not {APP_START:#x}"),
            HeaderError::Len(n) => write!(f, "body of {n} bytes, not 1 to {MAX_BODY}"),
        }
    }
}

fn get_u16(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

impl Header {
    /// A header for a body, unsigned: sign it before use.
    pub fn new(body: &[u8], counter: u32, version: &str, built: u64) -> Header {
        let mut v = [0u8; VERSION_LEN];
        let n = version.len().min(VERSION_LEN);
        v[..n].copy_from_slice(&version.as_bytes()[..n]);
        Header {
            flags: 0,
            key: 0,
            load: APP_START,
            len: body.len() as u32,
            body_crc: crc32(body),
            counter,
            version: v,
            built,
            signature: [0; SIG_LEN],
        }
    }

    /// Reads a header: magic, layout version, its CRC, the load address and
    /// the length are checked; the body and the signature are not.
    pub fn parse(b: &[u8; HEADER_SIZE]) -> Result<Header, HeaderError> {
        if b[OFF_MAGIC..OFF_MAGIC + 4] != MAGIC {
            return Err(HeaderError::Magic);
        }
        let hv = get_u16(b, OFF_HVERSION);
        if hv != HEADER_VERSION {
            return Err(HeaderError::Version(hv));
        }
        if crc32(&b[..OFF_HCRC]) != get_u32(b, OFF_HCRC) {
            return Err(HeaderError::Crc);
        }
        let load = get_u32(b, OFF_LOAD);
        if load != APP_START {
            return Err(HeaderError::Load(load));
        }
        let len = get_u32(b, OFF_LEN);
        if len == 0 || len > MAX_BODY {
            return Err(HeaderError::Len(len));
        }
        let mut version = [0u8; VERSION_LEN];
        version.copy_from_slice(&b[OFF_VERSION..OFF_VERSION + VERSION_LEN]);
        let mut signature = [0u8; SIG_LEN];
        signature.copy_from_slice(&b[OFF_SIG..OFF_SIG + SIG_LEN]);
        let mut built = [0u8; 8];
        built.copy_from_slice(&b[OFF_BUILT..OFF_BUILT + 8]);
        Ok(Header {
            flags: b[OFF_FLAGS],
            key: b[OFF_KEY],
            load,
            len,
            body_crc: get_u32(b, OFF_BODY_CRC),
            counter: get_u32(b, OFF_COUNTER),
            version,
            built: u64::from_le_bytes(built),
            signature,
        })
    }

    /// The header's 256 bytes, with its CRC.
    pub fn to_bytes(&self) -> [u8; HEADER_SIZE] {
        let mut b = [0u8; HEADER_SIZE];
        b[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&MAGIC);
        b[OFF_HVERSION..OFF_HVERSION + 2].copy_from_slice(&HEADER_VERSION.to_le_bytes());
        b[OFF_FLAGS] = self.flags;
        b[OFF_KEY] = self.key;
        b[OFF_LOAD..OFF_LOAD + 4].copy_from_slice(&self.load.to_le_bytes());
        b[OFF_LEN..OFF_LEN + 4].copy_from_slice(&self.len.to_le_bytes());
        b[OFF_BODY_CRC..OFF_BODY_CRC + 4].copy_from_slice(&self.body_crc.to_le_bytes());
        b[OFF_COUNTER..OFF_COUNTER + 4].copy_from_slice(&self.counter.to_le_bytes());
        b[OFF_VERSION..OFF_VERSION + VERSION_LEN].copy_from_slice(&self.version);
        b[OFF_BUILT..OFF_BUILT + 8].copy_from_slice(&self.built.to_le_bytes());
        b[OFF_SIG..OFF_SIG + SIG_LEN].copy_from_slice(&self.signature);
        let crc = crc32(&b[..OFF_HCRC]);
        b[OFF_HCRC..].copy_from_slice(&crc.to_le_bytes());
        b
    }

    /// The version string, without its padding.
    pub fn version_str(&self) -> &str {
        let n = self.version.iter().position(|&c| c == 0).unwrap_or(VERSION_LEN);
        core::str::from_utf8(&self.version[..n]).unwrap_or("?")
    }

    /// The .kfw file's size: header and body.
    pub fn file_size(&self) -> u32 {
        HEADER_SIZE as u32 + self.len
    }

    /// The header's part that the signature covers: all but the signature
    /// and the header's CRC.
    fn signed_parts(b: &[u8; HEADER_SIZE]) -> [&[u8]; 2] {
        [&b[..OFF_SIG], &b[OFF_RESERVED..OFF_HCRC]]
    }
}

/// Why an image is not taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    Header(HeaderError),
    /// The body is not as long as the header says.
    Length,
    /// The body's CRC does not match the header's.
    Crc,
    /// No key of the board has the header's key number.
    UnknownKey(u8),
    /// The signature does not verify.
    Signature,
    /// The counter is not above the running firmware's.
    Downgrade {
        running: u32,
        image: u32,
    },
    /// The .kfw file is larger than MAX_IMAGE.
    TooLarge(u32),
}

impl From<HeaderError> for Refusal {
    fn from(e: HeaderError) -> Self {
        Refusal::Header(e)
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Refusal::Header(e) => e.fmt(f),
            Refusal::Length => f.write_str("body length mismatch"),
            Refusal::Crc => f.write_str("body CRC mismatch"),
            Refusal::UnknownKey(k) => write!(f, "signed with key {k}, which this board does not know"),
            Refusal::Signature => f.write_str("bad signature"),
            Refusal::Downgrade { running, image } => {
                write!(f, "downgrade: image counter {image}, running {running}")
            }
            Refusal::TooLarge(n) => write!(f, "image of {n} bytes, larger than {MAX_IMAGE}"),
        }
    }
}

/// A short code for each refusal, for GPREGRET2 and the fw service.
impl Refusal {
    pub fn code(&self) -> u8 {
        match self {
            Refusal::Header(_) => 1,
            Refusal::Length => 2,
            Refusal::Crc => 3,
            Refusal::UnknownKey(_) => 4,
            Refusal::Signature => 5,
            Refusal::Downgrade { .. } => 6,
            Refusal::TooLarge(_) => 7,
        }
    }
}

/// The board's public keys, by key number: the header's `key` picks one.
#[derive(Clone, Copy)]
pub struct Keys<'a>(pub &'a [(u8, [u8; 32])]);

impl Keys<'_> {
    fn get(&self, id: u8) -> Option<PublicKey> {
        self.0
            .iter()
            .find(|(k, _)| *k == id)
            .map(|(_, pk)| PublicKey::new(*pk))
    }
}

/// Checks an image as its body comes in, piece by piece: the length, the
/// CRC and the signature.
pub struct Verifier {
    hdr: Header,
    crc: Crc32,
    got: u32,
    state: ed25519_compact::VerifyingState,
}

impl Verifier {
    /// Starts the check of the image of raw header b, signed with one of keys.
    pub fn new(b: &[u8; HEADER_SIZE], keys: Keys) -> Result<Verifier, Refusal> {
        let hdr = Header::parse(b)?;
        let pk = keys.get(hdr.key).ok_or(Refusal::UnknownKey(hdr.key))?;
        let sig = Signature::new(hdr.signature);
        let mut state = pk.verify_incremental(&sig).map_err(|_| Refusal::Signature)?;
        for part in Header::signed_parts(b) {
            state.absorb(part);
        }
        Ok(Verifier {
            hdr,
            crc: Crc32::new(),
            got: 0,
            state,
        })
    }

    pub fn header(&self) -> &Header {
        &self.hdr
    }

    /// Takes the next piece of the body; bytes past its length are an error
    /// at the end.
    pub fn update(&mut self, data: &[u8]) {
        self.crc.update(data);
        self.state.absorb(data);
        self.got = self.got.saturating_add(data.len() as u32);
    }

    /// Checks the whole body.
    pub fn finish(self) -> Result<Header, Refusal> {
        if self.got != self.hdr.len {
            return Err(Refusal::Length);
        }
        if self.crc.finish() != self.hdr.body_crc {
            return Err(Refusal::Crc);
        }
        self.state.verify().map_err(|_| Refusal::Signature)?;
        Ok(self.hdr)
    }
}

/// Checks that an image may replace the running firmware: its counter is
/// above the running one, unless forced (over USB only). With no valid
/// running firmware any counter is taken.
pub fn check_counter(image: &Header, running: Option<&Header>, force: bool) -> Result<(), Refusal> {
    match running {
        Some(r) if !force && image.counter <= r.counter => Err(Refusal::Downgrade {
            running: r.counter,
            image: image.counter,
        }),
        _ => Ok(()),
    }
}

/// Checks the size of a .kfw file before any of it is written.
pub fn check_size(file_size: u32) -> Result<(), Refusal> {
    if file_size > MAX_IMAGE {
        return Err(Refusal::TooLarge(file_size));
    }
    if file_size <= HEADER_SIZE as u32 {
        return Err(Refusal::Length);
    }
    Ok(())
}

/// What the bootloader does after a reset, before it looks at the staged
/// image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Boot the application.
    Boot,
    /// Install the staged image, then reset.
    Install,
    /// Wait for an image over USB.
    Recovery(Why),
}

/// Why the bootloader waits over USB.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    /// GPREGRET asked for it: the 1200-baud touch, or `fw recovery`.
    Asked,
    /// A double tap of reset.
    DoubleTap,
    /// No valid application, and no staged image to install.
    NoApp,
}

impl fmt::Display for Why {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(match self {
            Why::Asked => "asked for",
            Why::DoubleTap => "double tap of reset",
            Why::NoApp => "no valid application",
        })
    }
}

/// The first decision, from GPREGRET, the double tap and whether the
/// application's header and CRC are good.
pub fn first_step(gpregret: u8, double_tap: bool, app_valid: bool) -> Step {
    if gpregret == GPREGRET_RECOVERY {
        Step::Recovery(Why::Asked)
    } else if gpregret == GPREGRET_NO_APP && !app_valid {
        Step::Recovery(Why::NoApp)
    } else if double_tap {
        Step::Recovery(Why::DoubleTap)
    } else if gpregret == GPREGRET_INSTALL || !app_valid {
        Step::Install
    } else {
        Step::Boot
    }
}

/// What the bootloader does with the staged image, once it has checked it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallStep {
    /// Copy it into internal flash.
    Copy,
    /// The application is the staged image already: boot it.
    Boot,
    /// Nothing to install and no application: wait over USB.
    Recovery,
}

/// Decides on the staged image: staged is its check (None without one),
/// running the installed header if it is valid.
pub fn install_step(staged: Option<Result<Header, Refusal>>, running: Option<&Header>) -> InstallStep {
    match (staged, running) {
        (Some(Ok(s)), Some(r)) if s == *r => InstallStep::Boot,
        (Some(Ok(_)), _) => InstallStep::Copy,
        (_, Some(_)) => InstallStep::Boot,
        (_, None) => InstallStep::Recovery,
    }
}

/// GPREGRET2 after an install: what the firmware reports on its next boot.
pub mod result {
    pub const NONE: u8 = 0;
    pub const INSTALLED: u8 = 0x10;
    /// The staged image was refused: 0x20 | Refusal::code().
    pub const REFUSED: u8 = 0x20;
    /// Writing internal flash failed, or its CRC did not match after.
    pub const WRITE: u8 = 0x30;
    /// The staged file could not be read.
    pub const READ: u8 = 0x31;
}

pub mod stage;

/// Signing, on the host: the release workflow and cargo xtask build.
#[cfg(any(test, feature = "sign"))]
pub mod sign {
    use super::*;
    use ed25519_compact::{KeyPair, Seed};
    use std::vec::Vec;

    /// The public key of a 32-byte seed.
    pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
        *KeyPair::from_seed(Seed::new(*seed)).pk
    }

    /// Signs hdr for body with the key of seed, numbered key, and returns
    /// the .kfw file.
    pub fn kfw(mut hdr: Header, body: &[u8], seed: &[u8; 32], key: u8) -> Vec<u8> {
        hdr.key = key;
        hdr.signature = [0; SIG_LEN];
        let kp = KeyPair::from_seed(Seed::new(*seed));
        let b = hdr.to_bytes();
        let mut msg = Vec::with_capacity(HEADER_SIZE + body.len());
        for part in Header::signed_parts(&b) {
            msg.extend_from_slice(part);
        }
        msg.extend_from_slice(body);
        hdr.signature = *kp.sk.sign(&msg, None);
        let mut out = hdr.to_bytes().to_vec();
        out.extend_from_slice(body);
        out
    }
}

#[cfg(test)]
mod stage_tests;
#[cfg(test)]
mod tests;
