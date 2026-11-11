//! bolt fw: the firmware on a board, over its console on USB or BLE:
//! install the staged image through the bootloader, show what runs and
//! what is staged. bolt fw push stages one (push.rs).

use std::time::Duration;

use anyhow::{Result, bail};

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
