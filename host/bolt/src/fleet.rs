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
const SCAN_WAIT: Duration = Duration::from_secs(4);

/// The proxy: the knx characteristic and its notifications.
pub struct Proxy {
    p: Peripheral,
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

fn runtime() -> Result<tokio::runtime::Runtime> {
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
