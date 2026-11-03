//! The knixx GATT service (the web-ble feature): a central, such as the web
//! app, sends group value writes, reads and responses through the board and
//! is notified of their outcome and of the group value telegrams on the bus.
//! The values are those of knixx_knx::gatt. The rssi characteristic
//! carries the signal strength of the link as the board receives it, in dBm,
//! every RSSI_PERIOD: a browser cannot read it of a connected device. While
//! ble.pair is on, only a paired central gets in, as for NUS (see ble).

use embassy_futures::select::{Either, select};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use knixx_bus::{Command, Status, Tag};
use knixx_knx::gatt::{self, Group, MAX_VALUE, Send};
use knixx_knx::tp1::Telegram;
use trouble_host::prelude::*;

use crate::{console, nus};

/// The service UUID, 6b6e7801-1b5d-4c8e-a2f0-3e5b9d8c7a10, little endian as
/// in advertising data.
pub const SERVICE_UUID_LE: [u8; 16] = [
    0x10, 0x7a, 0x8c, 0x9d, 0x5b, 0x3e, 0xf0, 0xa2, 0x8e, 0x4c, 0x5d, 0x1b, 0x01, 0x78, 0x6e, 0x6b,
];

/// How often the central is told the link's RSSI.
pub const RSSI_PERIOD: embassy_time::Duration = embassy_time::Duration::from_secs(2);

/// The knixx service: the central writes send and is notified on result,
/// group and rssi.
#[gatt_service(uuid = "6b6e7801-1b5d-4c8e-a2f0-3e5b9d8c7a10")]
pub struct KnixxService {
    #[characteristic(uuid = "6b6e7802-1b5d-4c8e-a2f0-3e5b9d8c7a10", write)]
    pub send: heapless::Vec<u8, MAX_VALUE>,
    #[characteristic(uuid = "6b6e7803-1b5d-4c8e-a2f0-3e5b9d8c7a10", notify)]
    pub result: heapless::Vec<u8, 2>,
    #[characteristic(uuid = "6b6e7804-1b5d-4c8e-a2f0-3e5b9d8c7a10", notify)]
    pub group: heapless::Vec<u8, MAX_VALUE>,
    #[characteristic(uuid = "6b6e7805-1b5d-4c8e-a2f0-3e5b9d8c7a10", read, notify)]
    pub rssi: i8,
}

/// The outcomes of the central's sends, by seq.
static RESULTS: Channel<CriticalSectionRawMutex, (u8, Status), 8> = Channel::new();

/// The group value telegrams on the bus, the board's own included.
static GROUP: Channel<CriticalSectionRawMutex, Group, 8> = Channel::new();

/// Drops what was queued for an earlier central.
pub fn reset() {
    RESULTS.clear();
    GROUP.clear();
}

/// Queues a telegram of the bus for the central, if one is connected and
/// let in (see ble). A full queue drops it: the central reads the group
/// again if it must know.
pub fn telegram(tg: &Telegram, own: bool) {
    if !nus::connected() {
        return;
    }
    if let Some(g) = Group::from_telegram(tg, own) {
        let _ = GROUP.try_send(g);
    }
}

/// Queues the outcome of a send of the central.
pub fn sent(tag: Tag, status: Status) {
    if let Tag::Gatt(seq) = tag {
        let _ = RESULTS.try_send((seq, status));
    }
}

/// Handles a write of the send characteristic: queues the telegram for the
/// bus engine, or reports at once why not.
pub fn write(data: &[u8]) {
    let seq = data.first().copied().unwrap_or(0);
    let status = match Send::decode(data) {
        Err(_) => Status::Refused,
        Ok(s) => {
            let cmd = Command::Send {
                tg: s.telegram(),
                tag: Tag::Gatt(s.seq),
            };
            match console::CMDS.try_send(cmd) {
                Ok(()) => return,
                Err(_) => Status::Busy,
            }
        }
    };
    let _ = RESULTS.try_send((seq, status));
}

/// Notifies the central, once it is let in, of the results and the
/// telegrams it subscribed to. It returns once a notification fails: the
/// central is gone.
pub async fn serve<P: PacketPool>(svc: &KnixxService, conn: &GattConnection<'_, '_, P>) {
    let mut buf = [0u8; MAX_VALUE];
    loop {
        let r = match select(RESULTS.receive(), GROUP.receive()).await {
            _ if !nus::connected() => continue,
            Either::First((seq, status)) => {
                svc.result
                    .notify_raw(conn, &gatt::encode_result(seq, status), false)
                    .await
            }
            Either::Second(g) => {
                let n = g.encode(&mut buf);
                svc.group.notify_raw(conn, &buf[..n], false).await
            }
        };
        if r.is_err() {
            return;
        }
    }
}
