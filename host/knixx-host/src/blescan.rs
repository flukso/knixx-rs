//! Finding a BLE peripheral by its advertised name or its address, with the
//! computer's own Bluetooth adapter (CoreBluetooth on macOS, BlueZ on Linux).

use std::time::Duration;

use btleplug::api::{Central, CentralEvent, Manager as _, Peripheral as _, ScanFilter};
use btleplug::platform::{Adapter, Manager, Peripheral};
use futures_util::StreamExt;

/// An advertiser found by [`find`].
#[derive(Debug, Clone)]
pub struct Found {
    pub peripheral: Peripheral,
    /// The MAC address on Linux, the device UUID on macOS.
    pub id: String,
    pub name: Option<String>,
    pub rssi: Option<i16>,
}

/// Returns the first adapter of the computer.
pub async fn adapter() -> Result<Adapter, String> {
    let manager = Manager::new().await.map_err(|e| format!("bluetooth: {e}"))?;
    manager
        .adapters()
        .await
        .map_err(|e| format!("bluetooth: {e}"))?
        .into_iter()
        .next()
        .ok_or_else(|| "bluetooth: no adapter".to_string())
}

/// Reports whether an advertiser matches: by address when addr is set, else
/// by name.
pub fn matches(name: &str, addr: &str, found_name: Option<&str>, found_id: &str) -> bool {
    if !addr.is_empty() {
        return found_id.eq_ignore_ascii_case(addr);
    }
    found_name == Some(name)
}

/// Scans for up to wait for the advertiser named name, or with address addr
/// when that is not empty.
pub async fn find(adapter: &Adapter, name: &str, addr: &str, wait: Duration) -> Result<Found, String> {
    let mut events = adapter.events().await.map_err(|e| format!("bluetooth: {e}"))?;
    adapter
        .start_scan(ScanFilter::default())
        .await
        .map_err(|e| format!("bluetooth: scan: {e}"))?;
    let target = if addr.is_empty() { name } else { addr };
    let found = tokio::time::timeout(wait, async {
        while let Some(ev) = events.next().await {
            let (CentralEvent::DeviceDiscovered(id) | CentralEvent::DeviceUpdated(id)) = ev else {
                continue;
            };
            let Ok(p) = adapter.peripheral(&id).await else {
                continue;
            };
            let Ok(Some(props)) = p.properties().await else {
                continue;
            };
            let pid = p.id().to_string();
            if matches(name, addr, props.local_name.as_deref(), &pid) {
                return Some(Found {
                    peripheral: p,
                    id: pid,
                    name: props.local_name,
                    rssi: props.rssi,
                });
            }
        }
        None
    })
    .await;
    let _ = adapter.stop_scan().await;
    match found {
        Ok(Some(f)) => Ok(f),
        _ => Err(format!("no BLE advertiser {target} after {wait:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn match_by_name_or_address() {
        assert!(matches("knixx", "", Some("knixx"), "AA:BB"));
        assert!(!matches("knixx", "", Some("AdaDFU"), "AA:BB"));
        assert!(!matches("knixx", "", None, "AA:BB"));
        assert!(matches("knixx", "aa:bb", Some("other"), "AA:BB"));
        assert!(!matches("knixx", "aa:bc", Some("knixx"), "AA:BB"));
    }
}
