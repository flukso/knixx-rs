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
    p.connect().await.context("connect").map_err(pairing_hint)?;
    p.discover_services()
        .await
        .context("discover services")
        .map_err(pairing_hint)?;
    Ok((p, gone))
}

/// Adds what to do to an error that comes of pairing: the board refuses a
/// central that is not paired while ble.pair is on, and drops one whose bond
/// it forgot.
pub fn pairing_hint(e: anyhow::Error) -> anyhow::Error {
    let text = format!("{e:#}").to_lowercase();
    if [
        "encrypt",
        "authenticat",
        "not paired",
        "notpermitted",
        "not permitted",
    ]
    .iter()
    .any(|w| text.contains(w))
    {
        let how = if cfg!(target_os = "linux") {
            "pair once with bluetoothctl pair <address> while nus is on"
        } else {
            "connect while nus is on and accept the pairing request"
        };
        return e.context(format!("the board wants a paired central (ble.pair on): {how}"));
    }
    if text.contains("disconnected") {
        let how = if cfg!(target_os = "linux") {
            "bluetoothctl remove <address>"
        } else {
            "remove knixx in the Bluetooth settings"
        };
        return e.context(format!(
            "if the board forgot this computer (ble forget), forget the board too: {how}, then connect while nus is on"
        ));
    }
    e
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
