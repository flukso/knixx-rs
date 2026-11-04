//! bolt fw push: stages a .kfw on a board over its fw GATT service, in its
//! update window (`fw window <minutes>` on its console), without pairing.
//! The board takes data only at the offset it expects and acks every
//! 4 KiB; this keeps WINDOW bytes in flight, goes back on a rewind, and to
//! the last ack when nothing comes.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Peripheral as _, WriteType};
use futures_util::StreamExt;
use knixx_host::image::Image;
use knixx_image::crc32;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::ble;

pub const FW_SERVICE: &str = "6b6e7810-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const FW_CTRL: &str = "6b6e7811-1b5d-4c8e-a2f0-3e5b9d8c7a10";
const FW_DATA: &str = "6b6e7812-1b5d-4c8e-a2f0-3e5b9d8c7a10";

const BEGIN: u8 = 0x01;
const ACK: u8 = 0x10;
const REWIND: u8 = 0x11;
const DONE: u8 = 0x20;
const ERROR: u8 = 0x21;

/// The bytes in flight before an ack.
const WINDOW: usize = 16 * 1024;
/// How long without a notification before going back to the last ack.
const STALL: Duration = Duration::from_millis(1500);
/// How often the push goes back without progress before it gives up.
const MAX_STALLS: u32 = 10;

/// What the board says on ctrl.
#[derive(Debug)]
enum Note {
    Ack(usize),
    Rewind(usize),
    Done(u32),
    Error(u8, String),
}

fn parse(n: &[u8]) -> Option<Note> {
    let u32_at = |i: usize| {
        n.get(i..i + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
    match *n.first()? {
        ACK => Some(Note::Ack(u32_at(1)? as usize)),
        REWIND => Some(Note::Rewind(u32_at(1)? as usize)),
        DONE => Some(Note::Done(u32_at(1)?)),
        ERROR => Some(Note::Error(
            *n.get(1)?,
            String::from_utf8_lossy(n.get(2..).unwrap_or(&[])).into_owned(),
        )),
        _ => None,
    }
}

/// Stages the .kfw at path on the board name or addr, over BLE.
pub fn push(path: &Path, name: &str, addr: &str, wait: Duration) -> Result<()> {
    let data = fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    eprintln!(
        "pushing {}: {} (counter {}, {} KiB)",
        path.display(),
        hdr.version_str(),
        hdr.counter,
        data.len() / 1024
    );
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(push_async(&data, name, addr, wait))
}

async fn push_async(data: &[u8], name: &str, addr: &str, wait: Duration) -> Result<()> {
    let (p, _gone) = ble::connect(name, addr, wait, "fw window 10").await?;
    let (Some(ctrl), Some(dchar)) = (
        ble::characteristic(&p, FW_SERVICE, FW_CTRL),
        ble::characteristic(&p, FW_SERVICE, FW_DATA),
    ) else {
        bail!("the board has no fw service: its firmware is too old");
    };
    let mut notes_in = p.notifications().await.context("ctrl notifications")?;
    p.subscribe(&ctrl).await.context("ctrl notifications")?;
    let (tx, mut notes) = mpsc::unbounded_channel();
    let uuid = ctrl.uuid;
    tokio::spawn(async move {
        while let Some(n) = notes_in.next().await {
            if n.uuid == uuid && tx.send(n.value).is_err() {
                return;
            }
        }
    });
    let next_note =
        async |notes: &mut mpsc::UnboundedReceiver<Vec<u8>>, wait: Duration| -> Result<Option<Note>> {
            match timeout(wait, notes.recv()).await {
                Ok(Some(n)) => Ok(parse(&n)),
                Ok(None) => bail!("the board went away"),
                Err(_) => Ok(None),
            }
        };

    let mut begin = vec![BEGIN];
    begin.extend_from_slice(&(data.len() as u32).to_le_bytes());
    begin.extend_from_slice(&crc32(data).to_le_bytes());
    p.write(&ctrl, &begin, WriteType::WithResponse)
        .await
        .context("begin")?;
    // The board goes on with an image it has part of, after a lost link.
    let from = match next_note(&mut notes, Duration::from_secs(10)).await? {
        Some(Note::Ack(at)) if at <= data.len() => at,
        Some(Note::Error(c, r)) => bail!("the board: {r} ({c:#04x})"),
        other => bail!("the board did not take begin: {other:?}"),
    };
    if from > 0 {
        eprintln!(
            "the board has {} KiB of it already: going on from there",
            from / 1024
        );
    }

    let mtu = usize::from(p.mtu());
    let piece = mtu.saturating_sub(3 + 4).clamp(16, 240);
    let start = Instant::now();
    let (mut sent, mut acked) = (from, from);
    let mut stalls = 0;
    let mut buf = Vec::with_capacity(4 + piece);
    while acked < data.len() {
        while sent < data.len() && sent - acked < WINDOW {
            let end = (sent + piece).min(data.len());
            buf.clear();
            buf.extend_from_slice(&(sent as u32).to_le_bytes());
            buf.extend_from_slice(&data[sent..end]);
            p.write(&dchar, &buf, WriteType::WithoutResponse)
                .await
                .map_err(|e| anyhow!("data: {e}"))?;
            sent = end;
        }
        let note = if sent < data.len() && sent - acked < WINDOW {
            // Room to send: only take what has come.
            match notes.try_recv() {
                Ok(n) => parse(&n),
                Err(_) => continue,
            }
        } else {
            next_note(&mut notes, STALL).await?
        };
        match note {
            Some(Note::Ack(off)) if off >= acked => {
                if off > acked {
                    stalls = 0;
                }
                acked = off;
                sent = sent.max(acked);
            }
            Some(Note::Rewind(off)) => {
                acked = off;
                sent = off;
            }
            Some(Note::Error(c, r)) => bail!("the board: {r} ({c:#04x})"),
            Some(Note::Done(_)) => break,
            Some(Note::Ack(_)) => {}
            None => {
                stalls += 1;
                if stalls > MAX_STALLS {
                    bail!("no progress at {acked} after {MAX_STALLS} tries");
                }
                sent = acked;
            }
        }
        let rate = acked as f64 / start.elapsed().as_secs_f64() / 1024.0;
        eprint!(
            "\rsent {} of {} KiB, {rate:.1} KiB/s ",
            acked / 1024,
            data.len() / 1024
        );
    }
    // The board reads the file back before it says done.
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match next_note(&mut notes, left).await? {
            Some(Note::Done(counter)) => {
                eprintln!(
                    "\rstaged counter {counter} in {:.1} s ({:.1} KiB/s)           ",
                    start.elapsed().as_secs_f32(),
                    data.len() as f64 / 1024.0 / start.elapsed().as_secs_f64()
                );
                break;
            }
            Some(Note::Error(c, r)) => bail!("the board: {r} ({c:#04x})"),
            Some(_) => {}
            None if left.is_zero() => bail!("no done from the board"),
            None => {}
        }
    }
    let _ = p.disconnect().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notes() {
        assert!(matches!(parse(&[ACK, 0, 16, 0, 0]), Some(Note::Ack(4096))));
        assert!(matches!(parse(&[REWIND, 1, 0, 0, 0]), Some(Note::Rewind(1))));
        assert!(matches!(parse(&[DONE, 7, 0, 0, 0]), Some(Note::Done(7))));
        assert!(matches!(parse(&[ERROR, 5, b'b', b'a']), Some(Note::Error(5, ref s)) if s == "ba"));
        assert!(parse(&[ACK, 1]).is_none());
        assert!(parse(&[]).is_none());
    }
}
