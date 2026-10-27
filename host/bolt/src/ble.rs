//! Connecting to a BLE peripheral of a knixx board: the console's NUS
//! service or the bootloader's DFU service.

use std::time::Duration;

use anyhow::{Context, Result, anyhow};
use btleplug::api::{Central, CentralEvent, Characteristic, Peripheral as _};
use btleplug::platform::{Adapter, Peripheral};
use futures_util::StreamExt;
use knixx_host::blescan;
use tokio::sync::watch;
use uuid::Uuid;

/// Scans for up to wait for the advertiser named name, or with address addr
/// when that is not empty, connects to it and discovers its services. hint
/// is the console command that makes the board advertise. The returned
/// watch turns true once the peripheral disconnects.
pub async fn connect(
    name: &str,
    addr: &str,
    wait: Duration,
    hint: &str,
) -> Result<(Peripheral, watch::Receiver<bool>)> {
    let adapter = blescan::adapter().await.map_err(|e| anyhow!(e))?;
    let target = if addr.is_empty() { name } else { addr };
    eprintln!("waiting for {target} (run {hint} on the knixx console)...");
    let found = blescan::find(&adapter, name, addr, wait)
        .await
        .map_err(|e| anyhow!(e))?;
    let p = found.peripheral;
    let gone = watch_disconnect(&adapter, &p).await?;
    eprintln!("connecting to {}", found.id);
    p.connect().await.context("connect")?;
    p.discover_services().await.context("discover services")?;
    Ok((p, gone))
}

/// Returns a watch that turns true once p disconnects: the adapter reports
/// the disconnections of every peripheral.
async fn watch_disconnect(adapter: &Adapter, p: &Peripheral) -> Result<watch::Receiver<bool>> {
    let mut events = adapter.events().await.context("bluetooth events")?;
    let id = p.id();
    let (tx, rx) = watch::channel(false);
    tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            if matches!(&ev, CentralEvent::DeviceDisconnected(d) if *d == id) {
                break;
            }
        }
        let _ = tx.send(true);
    });
    Ok(rx)
}

/// Returns the characteristic ch of service svc of p.
pub fn characteristic(p: &Peripheral, svc: &str, ch: &str) -> Option<Characteristic> {
    let (svc, ch) = (Uuid::parse_str(svc).ok()?, Uuid::parse_str(ch).ok()?);
    p.characteristics()
        .into_iter()
        .find(|c| c.service_uuid == svc && c.uuid == ch)
}

/// Whether p offers service svc.
pub fn has_service(p: &Peripheral, svc: &str) -> bool {
    let Ok(svc) = Uuid::parse_str(svc) else {
        return false;
    };
    p.services().iter().any(|s| s.uuid == svc)
}
