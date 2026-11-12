//! Pushes and pulls over a board's transfer service (knixx-xfer on BLE), in
//! its update window (`upload window <minutes>` on its console, or the KNX
//! control plane's), without pairing: a signed .kfw, a script, or the
//! whole knixx.ini to the board; the knixx.ini, the scripts and their list
//! from it.
//!
//! In a push bolt is the sender: it keeps WINDOW bytes in flight, goes on
//! from each ack, back on a rewind, and to the last ack when nothing comes
//! ([`knixx_xfer::sender::Sender`]). CoreBluetooth holds writes without
//! response back until it can send them; BlueZ does not, which is why the
//! window and the rewind are there. No sleeps. In a pull the board sends,
//! and bolt is the receiver ([`knixx_xfer::receiver::Receiver`]).
//!
//! Pulled files go into one folder per board, by its individual address,
//! as `bolt fleet push` reads them: `<dir>/1.1.5/knixx.ini` and
//! `<dir>/1.1.5/scripts/<name>.rhgn`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures_util::StreamExt;
use knixx_host::image::Image;
use knixx_knx::address::IndividualAddress;
use knixx_xfer::msg::{DATA_HEADER, FORCE, MAX_CONTROL};
use knixx_xfer::receiver::{Fail, Out, Receiver, Sink};
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
/// How long the board may take to answer a put or a get.
const ANSWER_WAIT: Duration = Duration::from_secs(10);
/// How long the board may take to check a file once it has all of it: a
/// firmware's signature and read-back.
const DONE_WAIT: Duration = Duration::from_secs(60);
/// How long a pull waits for the board's next data.
const PULL_WAIT: Duration = Duration::from_secs(5);
/// The largest data value the board takes.
const MAX_DATA: usize = 244;
/// The kinds bolt takes in a pull.
const PULLED: &[Kind] = &[Kind::Script, Kind::Config, Kind::ScriptList];

/// Where to push or pull and how: the board on BLE, and its console for
/// the window and the install.
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

fn run<T>(f: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(f)
}

/// Opens the window if asked, connects to the board and runs f on its
/// transfer service.
fn with_link<T>(o: &Opts, f: impl AsyncFnOnce(&mut Link<'_>) -> Result<T>) -> Result<T> {
    if let Some(m) = o.open {
        open_window(&o.console, m)?;
    }
    run(async {
        let (p, _gone) = ble::connect(&o.name, &o.addr, o.wait, "upload window 10").await?;
        let r = async {
            let mut link = Link::open(&p).await?;
            f(&mut link).await
        }
        .await;
        let _ = p.disconnect().await;
        r
    })
}

/// Stages the .kfw at path on the board, over BLE; then installs it over
/// the console when install.
pub fn fw(path: &Path, o: &Opts, install: bool) -> Result<()> {
    let data = fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    eprintln!(
        "pushing {}: {} (counter {}, {} KiB)",
        path.display(),
        hdr.version_str(),
        hdr.counter,
        data.len() / 1024
    );
    let counter = with_link(o, async |l| put_shown(l, Kind::Firmware, 0, "", &data).await)?;
    eprintln!("staged counter {counter}");
    if install {
        crate::fw::install(&o.console)?;
    }
    Ok(())
}

/// Pushes with a progress line.
async fn put_shown(l: &mut Link<'_>, kind: Kind, flags: u8, name: &str, data: &[u8]) -> Result<u32> {
    let start = Instant::now();
    // The first progress is where the board starts: 0, or where it got to.
    let mut from = None;
    let r = l
        .put(kind, flags, name, data, |acked| {
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
    r
}

/// A script on the board, as its list gives it.
#[derive(Debug, PartialEq, Eq)]
pub struct Listed {
    pub size: u32,
    /// The md5 of its source, None when the board does not know it.
    pub hash: Option<String>,
}

/// Parses the board's script list.
pub fn parse_list(data: &[u8]) -> BTreeMap<String, Listed> {
    String::from_utf8_lossy(data)
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let (name, size, hash) = (f.next()?, f.next()?, f.next()?);
            Some((
                name.to_string(),
                Listed {
                    size: size.parse().ok()?,
                    hash: (hash != "-").then(|| hash.to_string()),
                },
            ))
        })
        .collect()
}

/// Builds the script at path if it is source, and pushes it to the board,
/// which loads it, anew if it ran. It is skipped when the board has it
/// already, by the source's hash, or the bytes of a .rhgn, unless always.
pub fn script(path: &Path, name: Option<&str>, always: bool, o: &Opts) -> Result<()> {
    let (name, data) = crate::script::bytecode(path, name)?;
    if data.len() > MAX_SCRIPT as usize {
        bail!(
            "{name}: {} bytes, over the {MAX_SCRIPT} a script may have",
            data.len()
        );
    }
    let source = crate::script::source_hash(path)?;
    with_link(o, async |l| {
        let list = l.list().await?;
        if !always && l.has_script(&list, &name, source.as_deref(), &data).await? {
            eprintln!("{name}: the board has it already; --always to push it anyway");
            return Ok(());
        }
        eprintln!("pushing script {name}: {} bytes", data.len());
        put_shown(l, Kind::Script, 0, &name, &data).await?;
        eprintln!("the board took {name}");
        Ok(())
    })
}

/// Pushes the knixx.ini at path to the board, which checks it, applies it
/// and saves it as it is; one for another individual address only with
/// force.
pub fn config(path: &Path, force: bool, o: &Opts) -> Result<()> {
    let data = read_config(path)?;
    let flags = if force { FORCE } else { 0 };
    with_link(o, async |l| l.put(Kind::Config, flags, "", &data, |_| {}).await)?;
    eprintln!("the board took it: applied and saved");
    Ok(())
}

/// Reads a knixx.ini and checks it as the board will, to say where it is
/// wrong: the board says only that it does not parse.
pub fn read_config(path: &Path) -> Result<Vec<u8>> {
    let data = fs::read(path).with_context(|| path.display().to_string())?;
    if data.len() > MAX_CONFIG as usize {
        bail!(
            "{}: {} bytes, over the {MAX_CONFIG} a config may have",
            path.display(),
            data.len()
        );
    }
    let dev = knixx_device::config::parse(&data).map_err(|e| anyhow!("{}: {e}", path.display()))?;
    eprintln!(
        "{}: for {}, {} bytes",
        path.display(),
        IndividualAddress(dev.individual),
        data.len()
    );
    Ok(data)
}

/// The board's folder in dir: its individual address, from its config.
fn board_dir(dir: &Path, config: &[u8]) -> Result<PathBuf> {
    let dev = knixx_device::config::parse(config).map_err(|e| anyhow!("the board's knixx.ini: {e}"))?;
    Ok(dir.join(IndividualAddress(dev.individual).to_string()))
}

/// Writes data to path, making its folder; says, and returns, whether it
/// is new, changed or unchanged.
pub fn write_file(path: &Path, data: &[u8]) -> Result<&'static str> {
    if let Some(d) = path.parent() {
        fs::create_dir_all(d).with_context(|| d.display().to_string())?;
    }
    let what = match fs::read(path) {
        Ok(old) if old == data => "unchanged",
        Ok(_) => "changed",
        Err(_) => "new",
    };
    fs::write(path, data).with_context(|| path.display().to_string())?;
    eprintln!("{}: {} bytes, {what}", path.display(), data.len());
    Ok(what)
}

/// Pulls the board's knixx.ini into `<dir>/<board>/knixx.ini`.
pub fn config_pull(dir: &Path, o: &Opts) -> Result<()> {
    with_link(o, async |l| {
        let ini = l.get(Kind::Config, "").await?;
        write_file(&board_dir(dir, &ini)?.join("knixx.ini"), &ini).map(|_| ())
    })
}

/// Pulls the board's script name, or all of them, into
/// `<dir>/<board>/scripts/<name>.rhgn`, and says whether a `<name>.rhai`
/// next to it is the source the board's runs.
pub fn script_pull(name: Option<&str>, dir: &Path, o: &Opts) -> Result<()> {
    with_link(o, async |l| {
        let ini = l.get(Kind::Config, "").await?;
        let dir = board_dir(dir, &ini)?.join("scripts");
        let list = l.list().await?;
        let names: Vec<&String> = match name {
            Some(n) => vec![
                list.get_key_value(n)
                    .ok_or_else(|| anyhow!("the board has no script {n}"))?
                    .0,
            ],
            None => list.keys().collect(),
        };
        if names.is_empty() {
            eprintln!("the board has no scripts");
        }
        for n in names {
            let data = l.get(Kind::Script, n).await?;
            write_file(&dir.join(format!("{n}.rhgn")), &data)?;
            let src = dir.join(format!("{n}.rhai"));
            if let (Some(have), Ok(Some(h))) = (&list[n].hash, crate::script::source_hash(&src)) {
                let same = if *have == h { "is" } else { "is not" };
                eprintln!("  {} {same} the source the board runs", src.display());
            }
        }
        Ok(())
    })
}

/// The transfer service of a connected board: its characteristics and
/// their notifications.
pub struct Link<'p> {
    p: &'p Peripheral,
    ctrl: Characteristic,
    data: Characteristic,
    notes: mpsc::UnboundedReceiver<Vec<u8>>,
    pulled: mpsc::UnboundedReceiver<Vec<u8>>,
}

/// What a pull receives into.
#[derive(Default)]
struct VecSink(Vec<u8>);

impl Sink for VecSink {
    fn open(&mut self, _: Kind, _: &str, size: u32, _: u8) -> Result<(), Fail> {
        self.0 = Vec::with_capacity(size as usize);
        Ok(())
    }

    fn write(&mut self, data: &[u8]) -> Result<(), Fail> {
        self.0.extend_from_slice(data);
        Ok(())
    }

    fn finish(&mut self) -> Result<u32, Fail> {
        Ok(self.0.len() as u32)
    }

    fn abort(&mut self) {
        self.0.clear();
    }
}

fn refused(code: u8, reason: &str) -> anyhow::Error {
    let hint = match code {
        knixx_xfer::code::WINDOW => ": open it with --open <minutes>, or upload window on the console",
        knixx_xfer::code::GUARD => ": the file is for another board; --force to push it anyway",
        _ => "",
    };
    anyhow!("the board: {reason} ({code:#04x}){hint}")
}

impl<'p> Link<'p> {
    /// Subscribes to ctrl and data, whose notifications the link then
    /// holds.
    pub async fn open(p: &'p Peripheral) -> Result<Link<'p>> {
        let (Some(ctrl), Some(data)) = (
            ble::characteristic(p, XFER_SERVICE, XFER_CTRL),
            ble::characteristic(p, XFER_SERVICE, XFER_DATA),
        ) else {
            bail!("the board has no transfer service: its firmware is too old");
        };
        let mut notes_in = p.notifications().await.context("notifications")?;
        p.subscribe(&ctrl).await.context("ctrl notifications")?;
        p.subscribe(&data).await.context("data notifications")?;
        let (tx, notes) = mpsc::unbounded_channel();
        let (dtx, pulled) = mpsc::unbounded_channel();
        let (cu, du) = (ctrl.uuid, data.uuid);
        tokio::spawn(async move {
            while let Some(n) = notes_in.next().await {
                let to = if n.uuid == cu {
                    &tx
                } else if n.uuid == du {
                    &dtx
                } else {
                    continue;
                };
                if to.send(n.value).is_err() {
                    return;
                }
            }
        });
        Ok(Link {
            p,
            ctrl,
            data,
            notes,
            pulled,
        })
    }

    async fn control(&self, m: &Msg<'_>) -> Result<()> {
        let mut buf = [0u8; MAX_CONTROL];
        let n = m.encode(&mut buf).map_err(|_| anyhow!("{m:?} too long"))?;
        self.p
            .write(&self.ctrl, &buf[..n], WriteType::WithResponse)
            .await
            .context("ctrl")
    }

    /// The next message on ctrl within wait; None when none came.
    async fn note(&mut self, wait: Duration) -> Result<Option<Vec<u8>>> {
        match timeout(wait, self.notes.recv()).await {
            Ok(Some(n)) => Ok(Some(n)),
            Ok(None) => bail!("the board went away"),
            Err(_) => Ok(None),
        }
    }

    /// Pushes data, a file of kind named name, and returns done's info
    /// (firmware: the counter staged; the others: the size); progress gets
    /// the bytes acked.
    pub async fn put(
        &mut self,
        kind: Kind,
        flags: u8,
        name: &str,
        data: &[u8],
        mut progress: impl FnMut(usize),
    ) -> Result<u32> {
        let size = u32::try_from(data.len()).context("too large")?;
        let crc = knixx_xfer::crc32(data);
        self.control(&Msg::Put {
            kind,
            flags,
            size,
            crc,
            name,
        })
        .await?;
        // The board answers ack 0, or where it got to with this very file
        // after a lost link; a done or error of an earlier transfer is
        // skipped.
        let deadline = Instant::now() + ANSWER_WAIT;
        let from = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(n) = self.note(left).await? else {
                if left.is_zero() {
                    bail!("the board did not answer the put");
                }
                continue;
            };
            match Msg::decode(&n) {
                Some(Msg::Ack(at)) if at <= size => break at,
                Some(Msg::Error { code, crc: c, reason }) if c == crc || c == 0 => {
                    return Err(refused(code, reason));
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

        let piece =
            (usize::from(self.p.mtu()).saturating_sub(3) - DATA_HEADER).clamp(16, MAX_DATA - DATA_HEADER);
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
                self.p
                    .write(&self.data, &buf[..n], WriteType::WithoutResponse)
                    .await
                    .map_err(|e| anyhow!("data: {e}"))?;
            }
            // Once all is acked, the board checks the file: wait for done.
            if s.all_acked() && done_by.is_none() {
                done_by = Some(Instant::now() + DONE_WAIT);
            }
            match self.note(STALL).await? {
                Some(n) => {
                    let Some(m) = Msg::decode(&n) else { continue };
                    if let Msg::Error { code, crc: c, reason } = m
                        && c == crc
                    {
                        return Err(refused(code, reason));
                    }
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

    /// Pulls the file of kind named name. A pull does not resume: a lost
    /// one is asked for again.
    pub async fn get(&mut self, kind: Kind, name: &str) -> Result<Vec<u8>> {
        while self.pulled.try_recv().is_ok() {}
        self.control(&Msg::Get { kind, name }).await?;
        // The board announces the file, or refuses.
        let deadline = Instant::now() + ANSWER_WAIT;
        let (size, crc) = loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let Some(n) = self.note(left).await? else {
                if left.is_zero() {
                    bail!("the board did not answer the get");
                }
                continue;
            };
            match Msg::decode(&n) {
                Some(Msg::File {
                    kind: k,
                    size,
                    crc,
                    name: f,
                }) if k == kind && f == name => break (size, crc),
                Some(Msg::Error { code, crc: 0, reason }) => return Err(refused(code, reason)),
                _ => {}
            }
        };
        let mut rx = Receiver::new(PULLED, 10_000);
        let mut sink = VecSink::default();
        let start = Instant::now();
        let now = || start.elapsed().as_millis() as u64;
        let mut out = rx.take(&mut sink, kind, size, crc, name, now());
        loop {
            if let Some(m) = out.msg() {
                self.control(&m).await?;
            }
            match out {
                Out::Complete(_) => {
                    let end = rx.finish(&mut sink);
                    if let Some(m) = end.msg() {
                        self.control(&m).await?;
                    }
                    return match end {
                        Out::Done { .. } => Ok(sink.0),
                        Out::Error { reason, .. } => bail!("{}: {reason}", kind.as_str()),
                        _ => bail!("{}: out of step", kind.as_str()),
                    };
                }
                Out::Error { reason, .. } => bail!("{}: {reason}", kind.as_str()),
                _ => {}
            }
            out = tokio::select! {
                d = self.pulled.recv() => match d.as_deref().and_then(Msg::decode) {
                    Some(Msg::Data { offset, bytes }) => rx.data(&mut sink, offset, bytes, now()),
                    Some(_) => Out::None,
                    None if d.is_none() => bail!("the board went away"),
                    None => Out::None,
                },
                n = self.notes.recv() => match n.as_deref().and_then(Msg::decode) {
                    Some(Msg::Error { code, crc: c, reason }) if c == crc => return Err(refused(code, reason)),
                    Some(Msg::Abort) => bail!("the board gave up"),
                    _ if n.is_none() => bail!("the board went away"),
                    _ => Out::None,
                },
                _ = tokio::time::sleep(PULL_WAIT) => bail!("the board stopped sending"),
            };
        }
    }

    /// The board's scripts.
    pub async fn list(&mut self) -> Result<BTreeMap<String, Listed>> {
        Ok(parse_list(&self.get(Kind::ScriptList, "").await?))
    }

    /// Whether the board, whose scripts are list, has the script name
    /// already: its source, by the hash the list gives, or the very bytes
    /// of data.
    pub async fn has_script(
        &mut self,
        list: &BTreeMap<String, Listed>,
        name: &str,
        source: Option<&str>,
        data: &[u8],
    ) -> Result<bool> {
        let Some(on) = list.get(name) else {
            return Ok(false);
        };
        if let (Some(have), Some(src)) = (&on.hash, source) {
            return Ok(have == src);
        }
        if on.size as usize != data.len() {
            return Ok(false);
        }
        Ok(self.get(Kind::Script, name).await? == data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists() {
        let l = parse_list(b"light 1466 0123abcd\nold 20 -\nbad line\n");
        assert_eq!(l.len(), 2);
        assert_eq!(
            l["light"],
            Listed {
                size: 1466,
                hash: Some("0123abcd".into())
            }
        );
        assert_eq!(l["old"].hash, None);
    }
}
