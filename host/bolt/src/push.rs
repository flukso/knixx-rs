//! Pushes over a board's transfer service (knixx-xfer on BLE), in its
//! update window (`upload window <minutes>` on its console, or the KNX
//! control plane's), without pairing: a signed .kfw, a script, or the
//! whole knixx.ini.
//!
//! bolt is the sender: it keeps WINDOW bytes in flight, goes on from each
//! ack, back on a rewind, and to the last ack when nothing comes
//! ([`knixx_xfer::sender::Sender`]). CoreBluetooth holds writes without
//! response back until it can send them; BlueZ does not, which is why the
//! window and the rewind are there. No sleeps.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures_util::StreamExt;
use knixx_host::image::Image;
use knixx_xfer::msg::FORCE;
use knixx_xfer::msg::{DATA_HEADER, MAX_CONTROL};
use knixx_xfer::sender::{Event, Sender};
use knixx_xfer::{Kind, MAX_CONFIG, MAX_SCRIPT, Msg};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::ble;
use crate::client::Target;

pub const XFER_SERVICE: &str = "6b6e7810-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const XFER_CTRL: &str = "6b6e7811-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const XFER_DATA: &str = "6b6e7812-1b5d-4c8e-a2f0-3e5b9d8c7a10";

/// The bytes in flight before an ack.
const WINDOW: u32 = 16 * 1024;
/// How long without a message before going back to the last ack.
const STALL: Duration = Duration::from_millis(1500);
/// How often the push goes back without progress before it gives up.
const MAX_STALLS: u32 = 10;
/// How long the board may take to answer a put.
const PUT_WAIT: Duration = Duration::from_secs(10);
/// How long the board may take to check a file once it has all of it: a
/// firmware's signature and read-back.
const DONE_WAIT: Duration = Duration::from_secs(60);
/// The largest data value the board takes.
const MAX_DATA: usize = 244;

/// Where to push and how: the board on BLE, and its console for the
/// window and the install.
pub struct Opts {
    pub name: String,
    pub addr: String,
    pub wait: Duration,
    /// Open the window first, for this many minutes, over the console.
    pub open: Option<u8>,
    pub console: Target,
}

/// Opens the board's update window over its console.
pub fn open_window(t: &Target, minutes: u8) -> Result<()> {
    let out = crate::script::with_client(t, async |c| {
        c.command(&format!("upload window {minutes}"), Duration::from_secs(5))
            .await
    })?;
    let out = out.trim();
    if !out.starts_with("upload: window open") {
        bail!("the board did not open its window: {out}");
    }
    eprintln!("{out}");
    Ok(())
}

/// Stages the .kfw at path on the board, over BLE; then installs it over
/// the console when install.
pub fn fw(path: &Path, o: &Opts, install: bool) -> Result<()> {
    let data = fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    if let Some(m) = o.open {
        open_window(&o.console, m)?;
    }
    eprintln!(
        "pushing {}: {} (counter {}, {} KiB)",
        path.display(),
        hdr.version_str(),
        hdr.counter,
        data.len() / 1024
    );
    let counter = run(push_async(Kind::Firmware, 0, "", &data, o))?;
    eprintln!("staged counter {counter}");
    if install {
        crate::fw::install(&o.console)?;
    }
    Ok(())
}

/// Builds the script at path if it is source, and pushes it to the board,
/// which loads it, anew if it ran.
pub fn script(path: &Path, name: Option<&str>, o: &Opts) -> Result<()> {
    let (name, data) = crate::script::bytecode(path, name)?;
    if data.len() > MAX_SCRIPT as usize {
        bail!(
            "{name}: {} bytes, over the {MAX_SCRIPT} a script may have",
            data.len()
        );
    }
    if let Some(m) = o.open {
        open_window(&o.console, m)?;
    }
    eprintln!("pushing script {name}: {} bytes", data.len());
    run(push_async(Kind::Script, 0, &name, &data, o))?;
    eprintln!("the board took {name}");
    Ok(())
}

/// Pushes the knixx.ini at path to the board, which checks it, applies it
/// and saves it as it is; one for another individual address only with
/// force.
pub fn config(path: &Path, force: bool, o: &Opts) -> Result<()> {
    let data = fs::read(path).with_context(|| path.display().to_string())?;
    if data.len() > MAX_CONFIG as usize {
        bail!(
            "{}: {} bytes, over the {MAX_CONFIG} a config may have",
            path.display(),
            data.len()
        );
    }
    // The board says only that a file does not parse: say where here.
    let dev = knixx_device::config::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    if let Some(m) = o.open {
        open_window(&o.console, m)?;
    }
    eprintln!(
        "pushing {} for {}: {} bytes",
        path.display(),
        knixx_knx::address::IndividualAddress(dev.individual),
        data.len()
    );
    let flags = if force { FORCE } else { 0 };
    run(push_async(Kind::Config, flags, "", &data, o)).map_err(|e| {
        if format!("{e}").contains(&format!("{:#04x}", knixx_xfer::code::GUARD)) {
            anyhow!("{e}: the file is for another board; --force to push it anyway")
        } else {
            e
        }
    })?;
    eprintln!("the board took it: applied and saved");
    Ok(())
}

fn run<T>(f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(f)
}

async fn push_async(kind: Kind, flags: u8, name: &str, data: &[u8], o: &Opts) -> Result<u32> {
    let (p, _gone) = ble::connect(&o.name, &o.addr, o.wait, "upload window 10").await?;
    let start = Instant::now();
    // The first progress is where the board starts: 0, or where it got to.
    let mut from = None;
    let r = put(&p, kind, flags, name, data, |acked| {
        let from = *from.get_or_insert(acked);
        let rate = (acked - from) as f64 / start.elapsed().as_secs_f64() / 1024.0;
        eprint!(
            "\rsent {} of {} KiB, {rate:.1} KiB/s ",
            acked / 1024,
            data.len() / 1024
        );
    })
    .await;
    if r.is_ok() {
        let sent = data.len() - from.unwrap_or(0);
        eprintln!(
            "\rsent {} KiB in {:.1} s ({:.1} KiB/s)           ",
            sent / 1024,
            start.elapsed().as_secs_f32(),
            sent as f64 / 1024.0 / start.elapsed().as_secs_f64()
        );
    } else {
        eprintln!();
    }
    let _ = p.disconnect().await;
    r
}

/// The transfer service of a connected board: its characteristics and the
/// messages on ctrl.
struct Link<'p> {
    p: &'p Peripheral,
    ctrl: Characteristic,
    data: Characteristic,
    notes: mpsc::UnboundedReceiver<Vec<u8>>,
}

impl<'p> Link<'p> {
    async fn open(p: &'p Peripheral) -> Result<Link<'p>> {
        let (Some(ctrl), Some(data)) = (
            ble::characteristic(p, XFER_SERVICE, XFER_CTRL),
            ble::characteristic(p, XFER_SERVICE, XFER_DATA),
        ) else {
            bail!("the board has no transfer service: its firmware is too old");
        };
        let mut notes_in = p.notifications().await.context("ctrl notifications")?;
        p.subscribe(&ctrl).await.context("ctrl notifications")?;
        let (tx, notes) = mpsc::unbounded_channel();
        let uuid = ctrl.uuid;
        tokio::spawn(async move {
            while let Some(n) = notes_in.next().await {
                if n.uuid == uuid && tx.send(n.value).is_err() {
                    return;
                }
            }
        });
        Ok(Link { p, ctrl, data, notes })
    }

    async fn control(&self, m: &Msg<'_>) -> Result<()> {
        let mut buf = [0u8; MAX_CONTROL];
        let n = m.encode(&mut buf).map_err(|_| anyhow!("{m:?} too long"))?;
        self.p
            .write(&self.ctrl, &buf[..n], WriteType::WithResponse)
            .await
            .context("ctrl")
    }

    /// The next message on ctrl within wait; None when none came, or one
    /// that does not decode.
    async fn note(&mut self, wait: Duration) -> Result<Option<Vec<u8>>> {
        match timeout(wait, self.notes.recv()).await {
            Ok(Some(n)) => Ok(Some(n)),
            Ok(None) => bail!("the board went away"),
            Err(_) => Ok(None),
        }
    }
}

/// Pushes data, a file of kind named name, to the connected board p, and
/// returns done's info (firmware: the counter staged; the others: the
/// size); progress gets the bytes acked.
pub async fn put(
    p: &Peripheral,
    kind: Kind,
    flags: u8,
    name: &str,
    data: &[u8],
    mut progress: impl FnMut(usize),
) -> Result<u32> {
    let mut link = Link::open(p).await?;
    let size = u32::try_from(data.len()).context("too large")?;
    let crc = knixx_xfer::crc32(data);
    link.control(&Msg::Put {
        kind,
        flags,
        size,
        crc,
        name,
    })
    .await?;
    // The board answers ack 0, or where it got to with this very file
    // after a lost link; a done or error of an earlier transfer is skipped.
    let deadline = Instant::now() + PUT_WAIT;
    let from = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let Some(n) = link.note(left).await? else {
            if left.is_zero() {
                bail!("the board did not answer the put");
            }
            continue;
        };
        match Msg::decode(&n) {
            Some(Msg::Ack(at)) if at <= size => break at,
            Some(Msg::Error { code, crc: c, reason }) if c == crc || c == 0 => {
                bail!("the board: {reason} ({code:#04x})")
            }
            _ => {}
        }
    };
    if from > 0 {
        eprintln!(
            "the board has {} KiB of it already: going on from there",
            from / 1024
        );
    }
    progress(from as usize);

    let piece = (usize::from(p.mtu()).saturating_sub(3) - DATA_HEADER).clamp(16, MAX_DATA - DATA_HEADER);
    let mut s = Sender::new(size, crc, from, WINDOW, piece as u32, MAX_STALLS);
    let mut buf = Vec::with_capacity(MAX_DATA);
    let mut done_by = None;
    loop {
        while let Some((at, len)) = s.next_piece() {
            let bytes = &data[at as usize..(at + len) as usize];
            buf.resize(DATA_HEADER + bytes.len(), 0);
            let n = Msg::Data { offset: at, bytes }
                .encode(&mut buf)
                .map_err(|_| anyhow!("data too long"))?;
            p.write(&link.data, &buf[..n], WriteType::WithoutResponse)
                .await
                .map_err(|e| anyhow!("data: {e}"))?;
        }
        // Once all is acked, the board checks the file: wait for done.
        if s.all_acked() && done_by.is_none() {
            done_by = Some(Instant::now() + DONE_WAIT);
        }
        match link.note(STALL).await? {
            Some(n) => {
                let Some(m) = Msg::decode(&n) else { continue };
                match s.on(&m).map_err(|e| anyhow!("the board: {e}"))? {
                    Event::Done(info) => return Ok(info),
                    Event::Progress(acked) => progress(acked as usize),
                    Event::None => {}
                }
            }
            None => {
                if done_by.is_some_and(|t| Instant::now() >= t) {
                    bail!("no done from the board");
                }
                s.silence().map_err(|e| anyhow!("{e}"))?;
            }
        }
    }
}
