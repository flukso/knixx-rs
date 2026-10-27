//! bolt ota: flashing a DFU package over BLE to a board whose Adafruit
//! bootloader runs its BLE DFU mode, which the ota console command starts.

use std::path::Path;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Characteristic, Peripheral as _, ValueNotification, WriteType};
use btleplug::platform::Peripheral;
use futures_util::{Stream, StreamExt};
use knixx_host::legacydfu::{self, CONTROL_POINT_UUID, Next, PACKET_UUID, Package, SERVICE_UUID};

use crate::ble;

/// The flags of bolt ota.
pub struct Options<'a> {
    pub zip: &'a Path,
    pub name: String,
    pub addr: String,
    pub wait: Duration,
    /// The payload of the packet writes; 0 for the negotiated ATT MTU's.
    pub chunk: usize,
}

pub fn run(o: Options) -> Result<()> {
    let pkg = Package::open(o.zip)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let (p, _gone) = ble::connect(&o.name, &o.addr, o.wait, "ota").await?;
        let r = update(&p, &pkg, o.chunk).await;
        let _ = p.disconnect().await;
        r
    })
}

async fn update(p: &Peripheral, pkg: &Package, chunk: usize) -> Result<()> {
    if !ble::has_service(p, SERVICE_UUID) {
        bail!("no DFU service");
    }
    let (Some(control), Some(packet)) = (
        ble::characteristic(p, SERVICE_UUID, CONTROL_POINT_UUID),
        ble::characteristic(p, SERVICE_UUID, PACKET_UUID),
    ) else {
        bail!("DFU service lacks its control point or packet characteristic");
    };
    let notes = p.notifications().await.context("control point notifications")?;
    p.subscribe(&control)
        .await
        .context("control point notifications")?;
    // The payload of an ATT write with the negotiated MTU, which the
    // Adafruit bootloader raises to 247, unless --chunk sets it.
    let size = if chunk != 0 {
        chunk
    } else {
        let mtu = p.mtu();
        eprintln!("ATT MTU {mtu}");
        usize::from(mtu).saturating_sub(3)
    };
    let mut link = Link {
        p: p.clone(),
        control,
        packet,
        notes,
        size,
    };
    let start = Instant::now();
    let mut shown = None;
    let r = legacydfu::update(&mut link, pkg, |sent, total| {
        let pct = sent * 100 / total;
        if shown != Some(pct) {
            shown = Some(pct);
            eprint!("\rsending {sent}/{total} bytes ({pct}%)");
        }
    })
    .await;
    eprintln!();
    r.map_err(|e| anyhow!(e))?;
    eprintln!(
        "done in {:.0} s, the board restarts with the new firmware",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

/// The characteristics of the DFU service as a legacydfu::Link.
struct Link {
    p: Peripheral,
    control: Characteristic,
    packet: Characteristic,
    notes: Pin<Box<dyn Stream<Item = ValueNotification> + Send>>,
    size: usize,
}

impl legacydfu::Link for Link {
    async fn write_control(&mut self, p: &[u8]) -> Result<(), String> {
        self.p
            .write(&self.control, p, WriteType::WithResponse)
            .await
            .map_err(|e| e.to_string())
    }

    async fn write_packet(&mut self, p: &[u8]) -> Result<(), String> {
        self.p
            .write(&self.packet, p, WriteType::WithoutResponse)
            .await
            .map_err(|e| e.to_string())
    }

    async fn notification(&mut self, timeout: Duration) -> Next {
        let uuid = self.control.uuid;
        let next = async {
            while let Some(n) = self.notes.next().await {
                if n.uuid == uuid {
                    return Next::Got(n.value);
                }
            }
            Next::Closed
        };
        tokio::time::timeout(timeout, next).await.unwrap_or(Next::Timeout)
    }

    fn packet_size(&self) -> usize {
        self.size
    }
}
