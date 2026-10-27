//! Updating the application of a Nordic SDK 11 bootloader, such as the
//! Adafruit nRF52 bootloader, over BLE with the legacy DFU protocol. The
//! transport is abstracted by [`Link`]. The packages are those of
//! adafruit-nrfutil dfu genpkg: [`Package::new`] builds one from an image,
//! which [`read_hex`] makes from a hex file, [`Package::write_zip`] writes it
//! and [`Package::open`] reads it.

mod genpkg;
mod ihex;

use std::fmt;
use std::io::{Read, Seek};
use std::path::Path;
use std::time::Duration;

pub use genpkg::{InitPacket, crc16, parse_ids, parse_uint};
pub use ihex::{HexError, read_hex};

/// The DFU service and its characteristics, from the bootloader's ble_dfu.
pub const SERVICE_UUID: &str = "00001530-1212-efde-1523-785feabcd123";
pub const CONTROL_POINT_UUID: &str = "00001531-1212-efde-1523-785feabcd123";
pub const PACKET_UUID: &str = "00001532-1212-efde-1523-785feabcd123";

// Control point op codes.
const OP_START: u8 = 0x01;
const OP_INIT: u8 = 0x02;
const OP_RECEIVE: u8 = 0x03;
const OP_VALIDATE: u8 = 0x04;
const OP_ACTIVATE: u8 = 0x05;
const OP_PRN_REQ: u8 = 0x08;
const OP_RESPONSE: u8 = 0x10;
const OP_PRN: u8 = 0x11;
const INIT_RX: u8 = 0x00;
const INIT_DONE: u8 = 0x01;
const MODE_APP: u8 = 0x04;
const STAT_SUCCESS: u8 = 0x01;

/// Packet writes carry at least MIN_CHUNK bytes, the payload with the
/// default ATT MTU, and at most MAX_CHUNK, the payload with the MTU of 247
/// that the Adafruit bootloader negotiates. The bootloader takes only
/// multiples of 4 bytes per write.
pub const MIN_CHUNK: usize = 20;
pub const MAX_CHUNK: usize = 244;

/// The number of firmware packets after which the bootloader reports the
/// bytes it received; it keeps the writes without response from overrunning
/// it.
pub const PRN: usize = 10;

/// Timeouts for the bootloader's answers. Start erases the application area
/// and validation reads it back, so they get longer.
pub const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
pub const SLOW_TIMEOUT: Duration = Duration::from_secs(30);

/// An error of a DFU package or an update, with the step it happened in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

/// The application image and its init packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub bin: Vec<u8>,
    pub dat: Vec<u8>,
}

/// The part of manifest.json in a genpkg package used here.
#[derive(serde::Deserialize)]
struct Manifest {
    manifest: ManifestBody,
}

#[derive(serde::Deserialize)]
struct ManifestBody {
    application: Option<Application>,
}

#[derive(serde::Deserialize)]
struct Application {
    bin_file: String,
    dat_file: String,
}

impl Package {
    /// Reads the application of the DFU package at path.
    pub fn open(path: &Path) -> Result<Package, Error> {
        let f = std::fs::File::open(path).map_err(|e| Error(format!("{}: {e}", path.display())))?;
        Package::from_zip(f).map_err(|e| Error(format!("{}: {e}", path.display())))
    }

    /// Reads the application of a DFU package.
    pub fn from_zip<R: Read + Seek>(r: R) -> Result<Package, Error> {
        let mut z = zip::ZipArchive::new(r).map_err(|e| Error(format!("dfu package: {e}")))?;
        let raw = read_file(&mut z, "manifest.json")?;
        let m: Manifest = serde_json::from_slice(&raw).map_err(|e| Error(format!("manifest.json: {e}")))?;
        let Some(app) = m.manifest.application else {
            return Err(Error("dfu package: no application".into()));
        };
        let bin = read_file(&mut z, &app.bin_file)?;
        let dat = read_file(&mut z, &app.dat_file)?;
        if bin.is_empty() || bin.len() % 4 != 0 {
            return Err(Error(format!(
                "dfu package: application of {} bytes, not a non-zero multiple of 4",
                bin.len()
            )));
        }
        Ok(Package { bin, dat })
    }
}

fn read_file<R: Read + Seek>(z: &mut zip::ZipArchive<R>, name: &str) -> Result<Vec<u8>, Error> {
    let mut f = z
        .by_name(name)
        .map_err(|e| Error(format!("dfu package: {name}: {e}")))?;
    let mut out = Vec::new();
    f.read_to_end(&mut out)
        .map_err(|e| Error(format!("dfu package: {name}: {e}")))?;
    Ok(out)
}

/// What [`Link::notification`] got.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Next {
    /// A notification of the control point.
    Got(Vec<u8>),
    /// No notification within the timeout.
    Timeout,
    /// The link dropped.
    Closed,
}

/// A connection to the DFU service.
#[allow(async_fn_in_trait)]
pub trait Link {
    /// Writes the control point, with response.
    async fn write_control(&mut self, p: &[u8]) -> Result<(), String>;
    /// Writes the packet characteristic, without response.
    async fn write_packet(&mut self, p: &[u8]) -> Result<(), String>;
    /// Waits up to timeout for the next notification of the control point.
    async fn notification(&mut self, timeout: Duration) -> Next;
    /// The largest payload of a packet write, the ATT MTU less 3; 0 when
    /// unknown.
    fn packet_size(&self) -> usize;
}

/// The payload of the packet writes for a link with packet size size.
pub fn chunk(size: usize) -> usize {
    (size.min(MAX_CHUNK) & !3).max(MIN_CHUNK)
}

/// Sends p to the bootloader on l and activates it. progress is called with
/// the bytes sent of the image and its size.
pub async fn update<L: Link>(
    l: &mut L,
    p: &Package,
    mut progress: impl FnMut(usize, usize),
) -> Result<(), Error> {
    let chunk = chunk(l.packet_size());
    let mut u = Updater { l, chunk };

    let mut sizes = [0u8; 12];
    sizes[8..].copy_from_slice(&(p.bin.len() as u32).to_le_bytes());
    u.control("start", &[OP_START, MODE_APP]).await?;
    u.packets("image size", &sizes).await?;
    u.response("start", OP_START, SLOW_TIMEOUT).await?;

    u.control("init", &[OP_INIT, INIT_RX]).await?;
    u.packets("init packet", &p.dat).await?;
    u.control("init", &[OP_INIT, INIT_DONE]).await?;
    u.response("init", OP_INIT, REPLY_TIMEOUT).await?;

    u.control("receipt request", &[OP_PRN_REQ, PRN as u8, 0]).await?;
    u.control("receive", &[OP_RECEIVE]).await?;
    let mut sent = 0;
    let mut n = 0;
    while sent < p.bin.len() {
        let end = (sent + u.chunk).min(p.bin.len());
        u.packets("image", &p.bin[sent..end]).await?;
        sent = end;
        n += 1;
        if n % PRN == 0 && sent < p.bin.len() {
            u.receipt(sent).await?;
        }
        progress(sent, p.bin.len());
    }
    u.response("receive", OP_RECEIVE, REPLY_TIMEOUT).await?;

    u.control("validate", &[OP_VALIDATE]).await?;
    u.response("validate", OP_VALIDATE, SLOW_TIMEOUT).await?;

    // The bootloader resets at once; the link may drop before the write is
    // acknowledged.
    let _ = u.l.write_control(&[OP_ACTIVATE]).await;
    Ok(())
}

/// Runs the steps of an update.
struct Updater<'a, L> {
    l: &'a mut L,
    chunk: usize,
}

impl<L: Link> Updater<'_, L> {
    async fn control(&mut self, step: &str, p: &[u8]) -> Result<(), Error> {
        self.l
            .write_control(p)
            .await
            .map_err(|e| Error(format!("{step}: {e}")))
    }

    /// Writes p in chunks of at most self.chunk bytes.
    async fn packets(&mut self, step: &str, p: &[u8]) -> Result<(), Error> {
        for c in p.chunks(self.chunk) {
            self.l
                .write_packet(c)
                .await
                .map_err(|e| Error(format!("{step}: {e}")))?;
        }
        Ok(())
    }

    /// Waits for the next notification.
    async fn next(&mut self, step: &str, timeout: Duration) -> Result<Vec<u8>, Error> {
        match self.l.notification(timeout).await {
            Next::Got(n) => Ok(n),
            Next::Timeout => Err(Error(format!("{step}: no answer within {timeout:?}"))),
            Next::Closed => Err(Error(format!("{step}: link closed"))),
        }
    }

    /// Waits for the response to op, skipping receipt notifications.
    async fn response(&mut self, step: &str, op: u8, timeout: Duration) -> Result<(), Error> {
        loop {
            let n = self.next(step, timeout).await?;
            if n.first() == Some(&OP_PRN) {
                continue;
            }
            if n.len() < 3 || n[0] != OP_RESPONSE || n[1] != op {
                return Err(Error(format!("{step}: unexpected notification {}", hex(&n))));
            }
            if n[2] != STAT_SUCCESS {
                return Err(Error(format!("{step}: {}", status(n[2]))));
            }
            return Ok(());
        }
    }

    /// Waits for the receipt notification of sent bytes.
    async fn receipt(&mut self, sent: usize) -> Result<(), Error> {
        let n = self.next("receipt", REPLY_TIMEOUT).await?;
        if n.len() == 3 && n[0] == OP_RESPONSE {
            return Err(Error(format!("image: {}", status(n[2]))));
        }
        if n.len() != 5 || n[0] != OP_PRN {
            return Err(Error(format!("receipt: unexpected notification {}", hex(&n))));
        }
        let got = u32::from_le_bytes([n[1], n[2], n[3], n[4]]) as usize;
        if got != sent {
            return Err(Error(format!("receipt: bootloader has {got} bytes, sent {sent}")));
        }
        Ok(())
    }
}

/// Formats p as Go's % x: hex bytes separated by spaces.
fn hex(p: &[u8]) -> String {
    p.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

/// Names a response value.
fn status(v: u8) -> String {
    match v {
        2 => "invalid state".into(),
        3 => "not supported".into(),
        4 => "data size exceeds limit".into(),
        5 => "CRC error".into(),
        6 => "operation failed".into(),
        _ => format!("status {v}"),
    }
}

#[cfg(test)]
mod tests;
