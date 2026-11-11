//! bolt fleet: the boards of an installation, through one proxy board that
//! this computer is paired with, over the KNX control plane
//! (knixx_knx::fw): the proxy's knixx service sends the requests on the bus
//! and hands the answers back.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures_util::StreamExt;
use knixx_knx::IndividualAddress;
use knixx_knx::fw::{Request, Response, State};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::ble;

const KNIXX_SERVICE: &str = "6b6e7801-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const KNIXX_RESULT: &str = "6b6e7803-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const KNIXX_KNX: &str = "6b6e7806-1b5d-4c8e-a2f0-3e5b9d8c7a10";

/// How long a scan collects answers: the boards spread theirs over 2 s.
pub const SCAN_WAIT: Duration = Duration::from_secs(4);

/// The proxy: the knx characteristic and its notifications.
pub struct Proxy {
    pub p: Peripheral,
    knx: Characteristic,
    notes: mpsc::UnboundedReceiver<(String, Vec<u8>)>,
    seq: u8,
}

/// A board's answer.
#[derive(Debug, Clone)]
pub struct Answer {
    pub from: u16,
    pub response: Response,
}

impl Proxy {
    /// Connects to the proxy name or addr over BLE; it must be paired, or
    /// in its pairing window.
    pub async fn connect(name: &str, addr: &str, wait: Duration) -> Result<Proxy> {
        let (p, _gone) = ble::connect(name, addr, wait, "nus on, the first time, to pair").await?;
        let (Some(knx), Some(result)) = (
            ble::characteristic(&p, KNIXX_SERVICE, KNIXX_KNX),
            ble::characteristic(&p, KNIXX_SERVICE, KNIXX_RESULT),
        ) else {
            bail!("the proxy has no knx characteristic: its firmware is too old, or it lacks web-ble");
        };
        let mut notes_in = p.notifications().await.context("notifications")?;
        p.subscribe(&knx)
            .await
            .context("knx notifications")
            .map_err(ble::pairing_hint)?;
        p.subscribe(&result)
            .await
            .context("result notifications")
            .map_err(ble::pairing_hint)?;
        let (tx, notes) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(n) = notes_in.next().await {
                if tx.send((n.uuid.to_string(), n.value)).is_err() {
                    return;
                }
            }
        });
        Ok(Proxy {
            p,
            knx,
            notes,
            seq: 0,
        })
    }

    /// Sends req to dest, 0 for the broadcast.
    pub async fn send(&mut self, dest: u16, req: &Request) -> Result<()> {
        self.seq = self.seq.wrapping_add(1);
        let mut w = vec![self.seq];
        w.extend_from_slice(&dest.to_be_bytes());
        w.extend_from_slice(req.encode().as_slice());
        self.p
            .write(&self.knx, &w, WriteType::WithResponse)
            .await
            .context("knx")
            .map_err(ble::pairing_hint)
    }

    /// The answers that come within wait; with until, it stops at the first
    /// from that board.
    pub async fn answers(&mut self, wait: Duration, until: Option<u16>) -> Result<Vec<Answer>> {
        let end = Instant::now() + wait;
        let mut out = Vec::new();
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let Ok(n) = timeout(left, self.notes.recv()).await else {
                return Ok(out);
            };
            let Some((uuid, v)) = n else {
                bail!("the proxy went away");
            };
            if uuid != KNIXX_KNX || v.len() < 3 {
                continue;
            }
            let from = u16::from_be_bytes([v[0], v[1]]);
            if let Some(response) = Response::decode(&v[2..]) {
                out.push(Answer { from, response });
                if until == Some(from) {
                    return Ok(out);
                }
            }
        }
    }

    pub async fn close(self) {
        let _ = self.p.disconnect().await;
    }
}

fn state(s: State) -> &'static str {
    match s {
        State::Idle => "idle",
        State::Receiving => "receiving",
        State::Staged => "staged",
        State::Failed => "failed",
    }
}

fn ble_hex(b: &[u8; 6]) -> String {
    b.iter()
        .rev()
        .map(|x| format!("{x:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

pub fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// Lists the boards that answer a scan.
pub fn scan(name: &str, addr: &str, wait: Duration) -> Result<()> {
    runtime()?.block_on(async {
        let mut proxy = Proxy::connect(name, addr, wait).await?;
        proxy.send(0, &Request::Scan).await?;
        let mut answers = proxy.answers(SCAN_WAIT, None).await?;
        proxy.close().await;
        answers.sort_by_key(|a| a.from);
        answers.dedup_by_key(|a| a.from);
        println!("{:<10} {:<12} {:<10} ble", "board", "counter", "state");
        for a in &answers {
            if let Response::Info {
                counter,
                state: s,
                ble,
            } = a.response
            {
                println!(
                    "{:<10} {:<12} {:<10} {}",
                    IndividualAddress(a.from).to_string(),
                    counter,
                    state(s),
                    ble_hex(&ble)
                );
            }
        }
        eprintln!("{} board(s)", answers.len());
        Ok(())
    })
}

/// Shows the status of the board at individual address board.
pub fn status(board: &str, name: &str, addr: &str, wait: Duration) -> Result<()> {
    let dest = IndividualAddress::parse(board)
        .map_err(|e| anyhow::anyhow!("{board}: {e}"))?
        .0;
    runtime()?.block_on(async {
        let mut proxy = Proxy::connect(name, addr, wait).await?;
        proxy.send(dest, &Request::Status).await?;
        let answers = proxy.answers(Duration::from_secs(3), Some(dest)).await?;
        proxy.close().await;
        match answers.iter().find(|a| a.from == dest).map(|a| a.response) {
            Some(Response::Status {
                running,
                staged,
                state: s,
                error,
                progress,
            }) => {
                println!(
                    "{board}: running counter {running}, staged {}, {} ({progress}%), last error {error:#04x}",
                    if staged == 0 { "none".to_string() } else { staged.to_string() },
                    state(s)
                );
                Ok(())
            }
            _ => bail!("no answer from {board}"),
        }
    })
}

/// Sends one request to the board at individual address board and shows
/// its answer.
pub fn request(board: &str, req: Request, name: &str, addr: &str, wait: Duration) -> Result<()> {
    let dest = IndividualAddress::parse(board)
        .map_err(|e| anyhow::anyhow!("{board}: {e}"))?
        .0;
    runtime()?.block_on(async {
        let mut proxy = Proxy::connect(name, addr, wait).await?;
        proxy.send(dest, &req).await?;
        let answers = proxy.answers(Duration::from_secs(3), Some(dest)).await?;
        proxy.close().await;
        match answers.iter().find(|a| a.from == dest) {
            Some(a) => {
                println!("{board}: {:?}", a.response);
                Ok(())
            }
            None => bail!("no answer from {board}"),
        }
    })
}

/// The open request for the .kfw at path: its counter and body CRC.
pub fn open_request(path: &std::path::Path, minutes: u8) -> Result<Request> {
    let data = std::fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = knixx_host::image::Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    Ok(Request::Open {
        counter: hdr.counter,
        crc: hdr.body_crc,
        minutes,
    })
}

/// The install request for the .kfw at path.
pub fn install_request(path: &std::path::Path) -> Result<Request> {
    let data = std::fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = knixx_host::image::Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    Ok(Request::Install { counter: hdr.counter })
}

/// A board of the installation, as a scan found it.
#[derive(Debug, Clone)]
struct Seen {
    from: u16,
    counter: u32,
    ble: [u8; 6],
}

pub fn name(a: u16) -> String {
    IndividualAddress(a).to_string()
}

/// How an update went for a board.
#[derive(Debug, Clone)]
enum Outcome {
    Current,
    Refused(u8),
    Unreachable(String),
    Staged,
    Installing,
}

/// Updates every board of the installation to the .kfw at path: a scan,
/// open on each board that runs an older image, a push to each in its
/// window, up to parallel at a time (the proxy last, over its own link),
/// install one by one stagger apart (the proxy last), and a scan once
/// they are back.
pub fn update(
    path: &std::path::Path,
    name_: &str,
    addr: &str,
    wait: Duration,
    parallel: usize,
    stagger: Duration,
    minutes: u8,
) -> Result<()> {
    let data = std::fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = knixx_host::image::Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    eprintln!(
        "updating to {} (counter {}, {} KiB)",
        hdr.version_str(),
        hdr.counter,
        data.len() / 1024
    );
    runtime()?.block_on(async {
        // The boards that advertise now, the proxy among them, by the
        // address in their manufacturer data: it comes in the scan
        // response, after the first sight of a board, so a short scan
        // first rather than the proxy's properties.
        let adapter = knixx_host::blescan::adapter().await.map_err(anyhow::Error::msg)?;
        let advertising = knixx_host::blescan::find_boards(&adapter, &[], Duration::from_secs(3))
            .await
            .map_err(anyhow::Error::msg)?;
        let mut proxy = Proxy::connect(name_, addr, wait).await?;
        let proxy_id = proxy.p.id();
        let proxy_ble = advertising
            .iter()
            .find(|(_, p)| p.id() == proxy_id)
            .map(|(a, _)| *a);
        if proxy_ble.is_none() {
            eprintln!("the proxy's address was not seen in its advertising: it is updated like the others");
        }

        // 1. Who is there.
        proxy.send(0, &Request::Scan).await?;
        let mut seen: Vec<Seen> = proxy
            .answers(SCAN_WAIT, None)
            .await?
            .into_iter()
            .filter_map(|a| match a.response {
                Response::Info { counter, ble, .. } => Some(Seen {
                    from: a.from,
                    counter,
                    ble,
                }),
                _ => None,
            })
            .collect();
        seen.sort_by_key(|s| s.from);
        // Two boards on one individual address answer with different BLE
        // addresses: neither can be told apart on the bus.
        for (i, s) in seen.iter().enumerate() {
            if seen[..i].iter().any(|t| t.from == s.from && t.ble != s.ble) {
                eprintln!(
                    "{}: two boards on this individual address; give each its own (device.individual)",
                    name(s.from)
                );
            }
        }
        seen.dedup_by_key(|s| s.from);
        eprintln!("{} board(s) answered", seen.len());
        let mut outcome: Vec<(u16, Outcome)> = Vec::new();

        // 2. Open each board that runs an older image.
        let mut ready: Vec<Seen> = Vec::new();
        for s in &seen {
            if s.counter >= hdr.counter {
                outcome.push((s.from, Outcome::Current));
                continue;
            }
            proxy
                .send(
                    s.from,
                    &Request::Open {
                        counter: hdr.counter,
                        crc: hdr.body_crc,
                        minutes,
                    },
                )
                .await?;
            let a = proxy.answers(Duration::from_secs(3), Some(s.from)).await?;
            match a.iter().find(|a| a.from == s.from).map(|a| a.response) {
                Some(Response::Ready { .. }) => ready.push(s.clone()),
                Some(Response::Current) => outcome.push((s.from, Outcome::Current)),
                Some(Response::Refused { reason }) => outcome.push((s.from, Outcome::Refused(reason))),
                _ => outcome.push((s.from, Outcome::Unreachable("no answer to open".into()))),
            }
        }
        let is_proxy = |s: &Seen| Some(s.ble) == proxy_ble;
        let (proxies, others): (Vec<Seen>, Vec<Seen>) = ready.into_iter().partition(|s| is_proxy(s));
        eprintln!(
            "{} board(s) to push to{}",
            others.len() + proxies.len(),
            if proxies.is_empty() {
                ""
            } else {
                ", the proxy last"
            }
        );

        // 3. Find the others over BLE, by the address they advertise.
        let want: Vec<[u8; 6]> = others.iter().map(|s| s.ble).collect();
        let found = if want.is_empty() {
            Vec::new()
        } else {
            knixx_host::blescan::find_boards(&adapter, &want, Duration::from_secs(20))
                .await
                .map_err(anyhow::Error::msg)?
        };

        // 4. Push, parallel at a time.
        let mut staged: Vec<u16> = Vec::new();
        for group in others.chunks(parallel.max(1)) {
            let pushes = group.iter().map(|s| {
                let p = found.iter().find(|(a, _)| *a == s.ble).map(|(_, p)| p.clone());
                let data = &data;
                async move {
                    let Some(p) = p else {
                        return (s.from, Err(anyhow::anyhow!("not seen over BLE")));
                    };
                    let mut last = Err(anyhow::anyhow!("not tried"));
                    for _ in 0..3 {
                        last = push_to(&p, data, s.from).await;
                        if last.is_ok() {
                            break;
                        }
                    }
                    let _ = p.disconnect().await;
                    (s.from, last)
                }
            });
            for (from, r) in futures_util::future::join_all(pushes).await {
                match r {
                    Ok(_) => {
                        eprintln!("{}: staged", name(from));
                        staged.push(from);
                    }
                    Err(e) => {
                        eprintln!("{}: {e:#}", name(from));
                        outcome.push((from, Outcome::Unreachable(format!("{e:#}"))));
                    }
                }
            }
        }
        for s in &proxies {
            match push_to(&proxy.p, &data, s.from).await {
                Ok(_) => {
                    eprintln!("{} (the proxy): staged", name(s.from));
                    staged.push(s.from);
                }
                Err(e) => outcome.push((s.from, Outcome::Unreachable(format!("{e:#}")))),
            }
        }

        // 5. Install one by one, the proxy last.
        staged.sort_by_key(|f| proxies.iter().any(|p| p.from == *f));
        for (i, from) in staged.iter().enumerate() {
            if i > 0 {
                tokio::time::sleep(stagger).await;
            }
            proxy
                .send(*from, &Request::Install { counter: hdr.counter })
                .await?;
            let a = proxy.answers(Duration::from_secs(3), Some(*from)).await?;
            match a.iter().find(|a| a.from == *from).map(|a| a.response) {
                Some(Response::Installing) => outcome.push((*from, Outcome::Installing)),
                Some(Response::Refused { reason }) => outcome.push((*from, Outcome::Refused(reason))),
                _ => outcome.push((*from, Outcome::Staged)),
            }
        }
        proxy.close().await;

        // 6. Once they are back: what runs.
        let installing = outcome
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Installing))
            .count();
        if installing > 0 {
            eprintln!("{installing} board(s) installing; checking back in 70 s");
            tokio::time::sleep(Duration::from_secs(70)).await;
            let mut proxy = Proxy::connect(name_, addr, Duration::from_secs(60)).await?;
            proxy.send(0, &Request::Scan).await?;
            let back = proxy.answers(SCAN_WAIT, None).await?;
            proxy.close().await;
            for (from, o) in outcome.iter_mut() {
                if !matches!(o, Outcome::Installing) {
                    continue;
                }
                let now = back
                    .iter()
                    .find(|a| a.from == *from)
                    .and_then(|a| match a.response {
                        Response::Info { counter, .. } => Some(counter),
                        _ => None,
                    });
                *o = match now {
                    Some(c) if c == hdr.counter => Outcome::Current,
                    Some(c) => Outcome::Unreachable(format!("runs counter {c} after the install")),
                    None => Outcome::Unreachable("no answer after the install".into()),
                };
            }
        }
        outcome.sort_by_key(|(f, _)| *f);
        println!("{:<10} outcome", "board");
        let mut behind = 0;
        for (from, o) in &outcome {
            let text = match o {
                Outcome::Current => "current".to_string(),
                Outcome::Refused(r) => format!("refused ({r})"),
                Outcome::Unreachable(e) => {
                    behind += 1;
                    format!("not updated: {e}")
                }
                Outcome::Staged => {
                    behind += 1;
                    "staged, not installed".to_string()
                }
                Outcome::Installing => "installing".to_string(),
            };
            println!("{:<10} {text}", name(*from));
        }
        if behind > 0 {
            bail!("{behind} board(s) behind: run bolt fleet update again");
        }
        Ok(())
    })
}

/// Pushes data to the board p, connecting first if need be.
async fn push_to(p: &Peripheral, data: &[u8], from: u16) -> Result<u32> {
    if !p.is_connected().await.unwrap_or(false) {
        p.connect().await.context("connect")?;
        p.discover_services().await.context("services")?;
    }
    let label = name(from);
    let mut last_pct = usize::MAX;
    let mut link = crate::xfer::Link::open(p).await?;
    link.put(knixx_xfer::Kind::Firmware, 0, "", data, |acked| {
        let pct = acked * 100 / data.len().max(1);
        if pct / 10 != last_pct / 10 {
            last_pct = pct;
            eprintln!("{label}: {pct}%");
        }
    })
    .await
}
