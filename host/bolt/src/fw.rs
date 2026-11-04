//! bolt fw: the firmware on a board, over its console on USB or BLE:
//! stage a .kfw file while the firmware runs, install it through the
//! bootloader, show what runs and what is staged.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use knixx_host::image::Image;
use knixx_image::{crc32, stage};

use crate::client::{Client, Target};
use crate::script::with_client;

/// Reads lines until one that starts with prefix, and returns its rest.
async fn reply_with(c: &mut Client, prefix: &str, wait: Duration) -> Result<String> {
    loop {
        let l = c.line(wait).await?;
        if let Some(rest) = l.strip_prefix(prefix) {
            return Ok(rest.to_string());
        }
        if let Some(e) = l.strip_prefix("error: ") {
            bail!("the board: {e}");
        }
        if !l.is_empty() {
            eprintln!("board: {l}");
        }
    }
}

/// What the board says to a frame.
enum Ack {
    /// `ok <offset>`: send from offset on.
    Ok(usize),
    /// `resend <offset>`: the frame was damaged, or for another offset.
    Resend(usize),
    /// Nothing within the wait: a packet of the frame went missing.
    Silent,
}

/// How long the host waits for the answer to a frame before it sends the
/// frame again.
const FRAME_WAIT: Duration = Duration::from_secs(2);

/// How often a frame is sent before the upload gives up.
const FRAME_TRIES: u32 = 8;

async fn ack(c: &mut Client) -> Result<Ack> {
    loop {
        let Some(l) = c.line_within(FRAME_WAIT).await? else {
            return Ok(Ack::Silent);
        };
        if let Some(at) = l.strip_prefix("ok ") {
            return Ok(Ack::Ok(at.trim().parse().context("ok offset")?));
        }
        if let Some(at) = l.strip_prefix("resend ") {
            return Ok(Ack::Resend(at.trim().parse().context("resend offset")?));
        }
        if let Some(e) = l.strip_prefix("error: ") {
            bail!("the board: {e}");
        }
        if !l.is_empty() {
            eprintln!("board: {l}");
        }
    }
}

/// Stages the .kfw file at path on the board at t; with install, installs
/// it too.
pub fn put(path: &Path, t: &Target, install: bool) -> Result<()> {
    let data = fs::read(path).with_context(|| path.display().to_string())?;
    let (_, hdr) = Image::from_kfw(&data).with_context(|| path.display().to_string())?;
    eprintln!(
        "staging {}: {} (counter {}, {} KiB)",
        path.display(),
        hdr.version_str(),
        hdr.counter,
        data.len() / 1024
    );
    with_client(t, async |c| {
        c.send_line(&format!("fw put {} {:08x}", data.len(), crc32(&data)))
            .await?;
        let chunk: usize = reply_with(c, "ready ", Duration::from_secs(10))
            .await?
            .trim()
            .parse()
            .context("the board's chunk size")?;
        let start = Instant::now();
        // Each chunk goes as a frame, [offset][CRC][data]; the board answers
        // ok with the offset to go on from, or resend. A frame without an
        // answer lost a packet on the way and is sent again.
        let mut off = 0;
        let mut repeats = 0;
        let mut tries = 0;
        let mut frame = vec![0u8; stage::FRAME_HEADER + chunk];
        while off < data.len() {
            let end = (off + chunk).min(data.len());
            let n = stage::frame(off as u32, &data[off..end], &mut frame);
            c.write(&frame[..n]).await?;
            match ack(c).await? {
                Ack::Ok(next) if next > off && next <= data.len() => {
                    off = next;
                    tries = 0;
                }
                Ack::Ok(next) | Ack::Resend(next) if next <= data.len() => {
                    if std::env::var_os("KNIXX_DEBUG").is_some() {
                        eprintln!("\nframe at {off}: board asks for {next}");
                    }
                    repeats += 1;
                    tries += 1;
                    off = next;
                }
                Ack::Silent => {
                    if std::env::var_os("KNIXX_DEBUG").is_some() {
                        eprintln!("\nframe at {off}: no answer");
                    }
                    repeats += 1;
                    tries += 1;
                }
                Ack::Ok(at) | Ack::Resend(at) => bail!("the board asks for {at}, past the end"),
            }
            if tries >= FRAME_TRIES {
                bail!("no answer to the frame at {off} after {FRAME_TRIES} tries");
            }
            let rate = off as f64 / start.elapsed().as_secs_f64() / 1024.0;
            eprint!(
                "\rsent {} of {} KiB, {rate:.1} KiB/s ",
                off / 1024,
                data.len() / 1024
            );
        }
        // The board reads the file back before it answers.
        reply_with(c, "done ", Duration::from_secs(60)).await?;
        if repeats > 0 {
            eprintln!("\r{repeats} frames sent again                    ");
        }
        eprintln!(
            "\rstaged in {:.1} s ({:.1} KiB/s)                ",
            start.elapsed().as_secs_f32(),
            data.len() as f64 / 1024.0 / start.elapsed().as_secs_f64()
        );
        c.prompt(Duration::from_secs(5)).await?;
        if install {
            // The board resets into its bootloader: the link goes away.
            c.send_line("fw install").await?;
            let l = reply_with(c, "fw: installing ", Duration::from_secs(5)).await?;
            eprintln!("installing {l}; the board resets into its bootloader");
        }
        Ok(())
    })
}

/// Installs the staged image on the board at t: it resets into its
/// bootloader, so the link goes away.
pub fn install(t: &Target) -> Result<()> {
    with_client(t, async |c| {
        c.send_line("fw install").await?;
        let l = reply_with(c, "fw: installing ", Duration::from_secs(5)).await?;
        eprintln!("installing {l}; the board resets into its bootloader");
        Ok(())
    })
}

/// Runs an fw command on the board at t and shows its output.
pub fn command(cmd: &str, t: &Target) -> Result<()> {
    let out = with_client(t, async |c| c.command(cmd, Duration::from_secs(10)).await)?;
    println!("{}", out.trim_end());
    Ok(())
}
