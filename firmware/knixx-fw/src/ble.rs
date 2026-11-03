//! Bluetooth LE with Nordic's SoftDevice Controller (nrf-sdc), linked into
//! the application, under the trouble host. The S140 SoftDevice that the
//! bootloader keeps in flash is never enabled.
//!
//! The multiprotocol service layer (MPSL) owns the clocks, RTC0, TIMER0,
//! TEMP, RADIO and PPI channels 17 to 31; it runs from boot, also when BLE
//! is idle, because USB gets its crystal clock through it.
//!
//! The BLE task serves the console's NUS port (see nus): it advertises
//! while the console asks for it, accepts one central, hands its writes to
//! the console and notifies it of the console's output.
//!
//! Pairing: while the configuration's ble.pair is on, NUS serves only a
//! central on an encrypted link that is bonded (see bonds) or paired while
//! NUS is on, the pairing window. Its requests are refused with
//! "insufficient encryption" until then, on which the central's OS pairs
//! (LE Secure Connections, Just Works) or encrypts with its bond. The check
//! is made at run time, rather than with the GATT table's permissions, so
//! that ble.pair can change without a reboot.

use core::fmt::Write;
use core::sync::atomic::{AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{Either, Either3, select, select3};
use embassy_nrf::mode::Async;
use embassy_nrf::rng::Rng;
use embassy_sync::once_lock::OnceLock;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use knixx_console::nus as framing;
use nrf_mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use trouble_host::prelude::*;

use crate::{bonds, console, nus};

/// Outgoing and incoming L2CAP buffers per link.
const L2CAP_TXQ: u8 = 3;
const L2CAP_RXQ: u8 = 3;

/// The memory of the SoftDevice Controller for its configuration below.
const SDC_MEM: usize = 4720;

/// How long nus off waits for the queued output to reach the central.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(1);

/// The connection parameters requested from a central once it connected:
/// each key typed takes an event for the write and one for its echo, and 15
/// ms is the shortest interval that macOS also accepts.
const INTERVAL: Duration = Duration::from_millis(15);
const SUPERVISION: Duration = Duration::from_secs(4);

/// The NUS service UUID, 6e400001-b5a3-f393-e0a9-e50e24dcca9e, little
/// endian as in advertising data.
const NUS_UUID_LE: [u8; 16] = [
    0x9e, 0xca, 0xdc, 0x24, 0x0e, 0xe5, 0xa9, 0xe0, 0x93, 0xf3, 0xa3, 0xb5, 0x01, 0x00, 0x40, 0x6e,
];

/// The size of the attribute table: the services' attributes, and the GAP
/// and GATT services that ble_task adds, Service Changed included.
const ATTRIBUTES: usize = 40;

/// The GATT database as centrals see it: the Service Changed
/// characteristic, and the hash of the database, which bonds keep.
struct Db {
    service_changed: Characteristic<[u8; 4]>,
    hash: u128,
}

static DB: OnceLock<Db> = OnceLock::new();

#[gatt_server(attribute_table_size = ATTRIBUTES)]
struct Server {
    nus: NusService,
}

/// The Nordic UART Service: the central writes RX and is notified on TX.
#[gatt_service(uuid = "6e400001-b5a3-f393-e0a9-e50e24dcca9e")]
struct NusService {
    #[characteristic(uuid = "6e400002-b5a3-f393-e0a9-e50e24dcca9e", write, write_without_response)]
    rx: heapless::Vec<u8, 244>,
    #[characteristic(uuid = "6e400003-b5a3-f393-e0a9-e50e24dcca9e", notify, read)]
    tx: heapless::Vec<u8, 20>,
}

/// The clock configuration: the Feather's 32.768 kHz crystal, at 250 ppm
/// as the TinyGo firmware configured the SoftDevice.
pub fn lfclk() -> nrf_mpsl::raw::mpsl_clock_lfclk_cfg_t {
    nrf_mpsl::raw::mpsl_clock_lfclk_cfg_t {
        source: nrf_mpsl::raw::MPSL_CLOCK_LF_SRC_XTAL as u8,
        rc_ctiv: 0,
        rc_temp_ctiv: 0,
        accuracy_ppm: 250,
        skip_wait_lfclk_started: false,
    }
}

#[embassy_executor::task]
pub async fn mpsl_task(mpsl: &'static MultiprotocolServiceLayer<'static>) -> ! {
    mpsl.run().await
}

/// Builds the SoftDevice Controller and starts the BLE task.
pub fn start(
    spawner: &Spawner,
    p: sdc::Peripherals<'static>,
    rng: &'static mut Rng<'static, Async>,
    mpsl: &'static MultiprotocolServiceLayer<'static>,
) {
    static MEM: StaticCell<sdc::Mem<SDC_MEM>> = StaticCell::new();
    let sdc = sdc::Builder::new()
        .and_then(|b| b.support_adv().support_peripheral().peripheral_count(1))
        .and_then(|b| {
            b.buffer_cfg(
                DefaultPacketPool::MTU as u16,
                DefaultPacketPool::MTU as u16,
                L2CAP_TXQ,
                L2CAP_RXQ,
            )
        })
        .and_then(|b| b.build(p, rng, mpsl, MEM.init(sdc::Mem::new())))
        .expect("nrf-sdc");
    spawner.spawn(ble_task(sdc).unwrap());
}

type Stack = trouble_host::Stack<'static, SoftdeviceController<'static>, DefaultPacketPool>;
type Conn = GattConnection<'static, 'static, DefaultPacketPool>;

#[embassy_executor::task]
async fn ble_task(sdc: SoftdeviceController<'static>) -> ! {
    // A static random address, stable across resets: the device ID.
    let ficr = embassy_nrf::pac::FICR;
    let id = (u64::from(ficr.deviceid(1).read()) << 32) | u64::from(ficr.deviceid(0).read());
    let mut addr = id.to_le_bytes();
    addr[5] |= 0xC0; // static random
    let address = Address::random([addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]]);

    // One bond more than bonds::MAX: the host stores a new bond itself, so
    // it needs room for it before the oldest is dropped.
    static RESOURCES: StaticCell<HostResources<DefaultPacketPool, 1, 2, 1, { bonds::MAX + 1 }>> =
        StaticCell::new();
    static STACK: StaticCell<Stack> = StaticCell::new();
    let stack = STACK.init(
        trouble_host::new(sdc, RESOURCES.init(HostResources::new()))
            .set_random_address(address)
            .set_io_capabilities(IoCapabilities::NoInputNoOutput)
            .build(),
    );
    for b in bonds::list() {
        let _ = stack.add_bond_information(b.info());
    }
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();
    // The GAP service, and the GATT service with Service Changed, which
    // GapConfig leaves out.
    let mut table = AttributeTable::new();
    let mut gap = table.add_service(Service::new(service::GAP));
    gap.add_characteristic_ro(characteristic::DEVICE_NAME, nus::NAME);
    gap.add_characteristic_ro(characteristic::APPEARANCE, &appearance::UNKNOWN);
    gap.build();
    static SERVICE_CHANGED: StaticCell<[u8; 4]> = StaticCell::new();
    let mut gatt = table.add_service(Service::new(service::GATT));
    let service_changed = gatt
        .add_characteristic(
            characteristic::SERVICE_CHANGED,
            [CharacteristicProp::Indicate],
            [0u8; 4],
            SERVICE_CHANGED.init([0; 4]),
        )
        .build();
    gatt.build();
    static SERVER: StaticCell<Server> = StaticCell::new();
    let server = SERVER.init(Server::new(table));
    let _ = DB.init(Db {
        service_changed,
        hash: server.table().hash(),
    });

    let _ = embassy_futures::join::join(runner.run(), serve(stack, &mut peripheral, server)).await;
    panic!("ble: the host runner stopped");
}

/// Advertises while NUS is on and serves one central at a time.
async fn serve(
    stack: &'static Stack,
    peripheral: &mut Peripheral<'static, SoftdeviceController<'static>, DefaultPacketPool>,
    server: &'static Server<'static>,
) {
    let mut adv_data = [0u8; 31];
    let mut scan_data = [0u8; 31];
    let (len, scan_len) = advertising(&mut adv_data, &mut scan_data);
    let adv = Advertisement::ConnectableScannableUndirected {
        adv_data: &adv_data[..len],
        scan_data: &scan_data[..scan_len],
    };

    // Whether the console asked for NUS: the last CONTROL.
    let mut nus_on = false;
    loop {
        if !nus_on {
            match select(nus::CONTROL.wait(), bonds::CHANGED.wait()).await {
                Either::First(c) => {
                    nus_on = c;
                    if !nus_on {
                        nus::OFF_DONE.signal(());
                    }
                }
                Either::Second(()) => sync_bonds(stack),
            }
            continue;
        }
        let accept = async {
            let advertiser = peripheral
                .advertise(&AdvertisementParameters::default(), adv)
                .await?;
            Ok::<_, BleHostError<_>>(advertiser.accept().await?.with_attribute_server(server)?)
        };
        // The advertising stops as its future is dropped.
        let conn = match select3(accept, nus::CONTROL.wait(), bonds::CHANGED.wait()).await {
            Either3::First(Ok(conn)) => conn,
            Either3::First(Err(_)) => {
                Timer::after_secs(1).await;
                continue;
            }
            Either3::Second(c) => {
                nus_on = c;
                if !nus_on {
                    nus::OFF_DONE.signal(());
                }
                continue;
            }
            Either3::Third(()) => {
                sync_bonds(stack);
                continue;
            }
        };

        nus::OUT.lock(|q| q.borrow_mut().reset());
        // Bonding is offered only in the pairing window; a pairing outside
        // it gives no bond, and the link is dropped.
        let _ = conn.raw().set_bondable(console::pairing_window());
        if allowed(&conn) {
            nus::CONNECTED.store(true, Ordering::Relaxed);
        } else {
            // The central's OS pairs, or encrypts with its bond, at once.
            let _ = conn.raw().request_security();
        }
        let mut first = true;
        loop {
            let params = async {
                if !core::mem::take(&mut first) {
                    return;
                }
                let params = RequestedConnParams {
                    min_connection_interval: INTERVAL,
                    max_connection_interval: INTERVAL,
                    supervision_timeout: SUPERVISION,
                    ..Default::default()
                };
                if let Err(e) = conn.raw().update_connection_params(stack, &params).await {
                    let _ = writeln!(crate::console::Out, "nus: connection interval: {e:?}");
                }
            };
            // The sender watches for nus on and off between notifications, so
            // that none is cut off halfway.
            let ctl = match select(join(params, events(stack, server, &conn)), send(server, &conn)).await {
                Either::Second(Some(on)) => on,
                _ => break,
            };
            if ctl {
                // The console asks for nus on only while NUS is off.
                continue;
            }
            // nus off: let the queued output, such as the reply to nus off,
            // reach the central.
            nus_on = false;
            let deadline = Instant::now() + FLUSH_TIMEOUT;
            while nus::OUT.lock(|q| q.borrow().buffered() > 0) && Instant::now() < deadline {
                if send_pending(server, &conn).await.is_err() {
                    break;
                }
            }
            nus::OUT.lock(|q| q.borrow_mut().reset());
            conn.raw().disconnect();
            let _ = with_timeout(FLUSH_TIMEOUT, async {
                while !matches!(conn.next().await, GattConnectionEvent::Disconnected { .. }) {}
            })
            .await;
            nus::CONNECTED.store(false, Ordering::Relaxed);
            nus::OFF_DONE.signal(());
            break;
        }
        nus::CONNECTED.store(false, Ordering::Relaxed);
        nus::OUT.lock(|q| q.borrow_mut().reset());
    }
}

/// Fills the advertising data, and the scan response, which stays empty,
/// and returns their lengths.
fn advertising(adv: &mut [u8; 31], _scan: &mut [u8; 31]) -> (usize, usize) {
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteServiceUuids128(&[NUS_UUID_LE]),
            AdStructure::CompleteLocalName(nus::NAME.as_bytes()),
        ],
        adv,
    )
    .expect("advertising data");
    (len, 0)
}

/// Whether ble.pair is on.
fn pair() -> bool {
    console::device().ble.pair
}

/// Whether the central may use the services: always without ble.pair, else
/// on an encrypted link with a bonded central, or with any central in the
/// pairing window.
fn allowed(conn: &Conn) -> bool {
    if !pair() {
        return true;
    }
    let raw = conn.raw();
    let encrypted = raw.security_level().is_ok_and(|l| l.encrypted());
    encrypted && (console::pairing_window() || raw.is_bonded_peer())
}

/// Lets the central in once allowed, or drops it when it no longer is: after
/// its pairing or encryption, a change of ble.pair or a forgotten bond. It
/// reports whether the central may stay.
fn admit(conn: &Conn) -> bool {
    if allowed(conn) {
        if !nus::CONNECTED.swap(true, Ordering::Relaxed) {
            nus::WAKE.signal(());
        }
        return true;
    }
    if conn.raw().security_level().is_ok_and(|l| l.encrypted()) || nus::connected() {
        nus::CONNECTED.store(false, Ordering::Relaxed);
        conn.raw().disconnect();
        return false;
    }
    true
}

/// Drops the bonds from the host that are no longer in bonds, after a
/// bond was forgotten.
fn sync_bonds(stack: &Stack) {
    let mut gone: heapless::Vec<Identity, { bonds::MAX + 1 }> = heapless::Vec::new();
    stack.with_bond_information(|list| {
        for b in list.iter().filter(|b| !bonds::contains(b)) {
            let _ = gone.push(b.identity);
        }
    });
    for id in gone {
        let _ = stack.remove_bond_information(id);
    }
}

/// Keeps a new bond, dropping the oldest when there are bonds::MAX already.
fn keep_bond(stack: &Stack, info: &BondInformation) {
    // The central discovers the services as they are now.
    let hash = DB.try_get().map_or(0, |db| db.hash);
    let b = bonds::Bond::from_info(info, hash);
    let (evicted, saved) = bonds::add(b);
    if let Some(old) = evicted {
        let _ = stack.remove_bond_information(old.info().identity);
    }
    match saved {
        Ok(()) => {
            let _ = writeln!(console::Out, "ble: bonded with {b}");
        }
        Err(e) => {
            let _ = writeln!(console::Out, "ble: bonded with {b} (not saved: {e})");
        }
    }
}

/// Tells a bonded central whose cached GATT database is not the current
/// one, after a firmware update, to discover the services again: the
/// Service Changed indication over all handles. The central subscribed to
/// it once, which a bond keeps; the board does not store subscriptions, so
/// it sets this one again. Once confirmed, the bond records the database.
async fn services_changed(server: &'static Server<'static>, conn: &Conn) {
    let Some(db) = DB.try_get() else { return };
    let id = conn.raw().peer_identity();
    if !bonds::stale(&id, db.hash) {
        SERVICE_CHANGED.store(SC_CURRENT, Ordering::Relaxed);
        return;
    }
    if let (Some(cccd), Some(mut t)) = (
        db.service_changed.cccd_handle,
        server.get_client_att_table(conn.raw()),
    ) && t.write(cccd, 0, &[0x02, 0x00]).is_ok()
    {
        server.set_client_att_table(conn.raw(), &t.view());
    }
    let all = [0x01, 0x00, 0xff, 0xff];
    let sent = with_timeout(FLUSH_TIMEOUT * 5, db.service_changed.indicate(conn, &all, false)).await;
    let outcome = match sent {
        Ok(Ok(())) => {
            let _ = bonds::seen(&id, db.hash);
            SC_CONFIRMED
        }
        Ok(Err(_)) => SC_FAILED,
        Err(_) => SC_TIMEOUT,
    };
    SERVICE_CHANGED.store(outcome, Ordering::Relaxed);
}

/// What the last encrypted link of a bonded central did about Service
/// Changed, for the ble command.
pub static SERVICE_CHANGED: AtomicU8 = AtomicU8::new(SC_NONE);
const SC_NONE: u8 = 0;
const SC_CURRENT: u8 = 1;
const SC_CONFIRMED: u8 = 2;
const SC_FAILED: u8 = 3;
const SC_TIMEOUT: u8 = 4;

/// SERVICE_CHANGED as text.
pub fn service_changed() -> &'static str {
    match SERVICE_CHANGED.load(Ordering::Relaxed) {
        SC_CURRENT => "the central had the current services",
        SC_CONFIRMED => "service changed indicated and confirmed",
        SC_FAILED => "service changed not sent",
        SC_TIMEOUT => "service changed not confirmed",
        _ => "no bonded central encrypted yet",
    }
}

/// The handles that need a paired central: those of NUS.
fn guarded(server: &Server, handle: u16) -> bool {
    let nus = &server.nus;
    handle == nus.rx.handle || handle == nus.tx.handle || Some(handle) == nus.tx.cccd_handle
}

/// The handle a GATT request is for.
fn handle_of(event: &GattEvent<'_, '_, DefaultPacketPool>) -> Option<u16> {
    match event {
        GattEvent::Read(r) => Some(r.handle()),
        GattEvent::Write(w) => Some(w.handle()),
        _ => None,
    }
}

/// Handles the central's GATT requests and security events until it
/// disconnects: writes of RX go to the console.
async fn events(stack: &'static Stack, server: &'static Server<'static>, conn: &Conn) {
    let rx = server.nus.rx.handle;
    loop {
        let event = match select(conn.next(), bonds::CHANGED.wait()).await {
            Either::First(e) => e,
            Either::Second(()) => {
                sync_bonds(stack);
                admit(conn);
                continue;
            }
        };
        match event {
            GattConnectionEvent::Disconnected { .. } => return,
            GattConnectionEvent::PairingComplete { bond, .. } => {
                if let Some(info) = bond {
                    if console::pairing_window() {
                        keep_bond(stack, &info);
                    } else {
                        let _ = stack.remove_bond_information(info.identity);
                    }
                }
                admit(conn);
            }
            GattConnectionEvent::Encrypted { .. } => {
                if admit(conn) {
                    services_changed(server, conn).await;
                }
            }
            GattConnectionEvent::PairingFailed(e) => {
                let _ = writeln!(console::Out, "ble: pairing failed: {e:?}");
            }
            GattConnectionEvent::Gatt { event } => {
                if pair() && handle_of(&event).is_some_and(|h| guarded(server, h)) && !allowed(conn) {
                    if let Ok(reply) = event.reject(AttErrorCode::INSUFFICIENT_ENCRYPTION) {
                        reply.send().await;
                    }
                    continue;
                }
                if let GattEvent::Write(w) = &event
                    && w.handle() == rx
                {
                    w.with_data(|_, data| nus::RX.lock(|r| r.borrow_mut().put(data)));
                    nus::RX_READY.signal(());
                }
                if let Ok(reply) = event.accept() {
                    reply.send().await;
                }
            }
            _ => {}
        }
    }
}

/// Notifies the central of the console output as it is queued, until the
/// central is gone (None) or the console asks for nus on or off (Some).
async fn send(server: &'static Server<'static>, conn: &Conn) -> Option<bool> {
    loop {
        match select(nus::WAKE.wait(), nus::CONTROL.wait()).await {
            Either::First(()) => {
                while nus::OUT.lock(|q| q.borrow().buffered() > 0) {
                    if send_pending(server, conn).await.is_err() {
                        return None;
                    }
                }
            }
            Either::Second(on) => return Some(on),
        }
    }
}

/// The TX characteristic as a framing::Link: notifications of up to
/// framing::CHUNK bytes.
struct Link<'c> {
    server: &'static Server<'static>,
    conn: &'c Conn,
}

impl framing::Link for Link<'_> {
    type Error = trouble_host::Error;

    async fn send(&mut self, p: &[u8]) -> Result<(), Self::Error> {
        self.server.nus.tx.notify_raw(self.conn, p, false).await
    }

    fn connected(&self) -> bool {
        nus::connected()
    }

    async fn sleep(&mut self, d: core::time::Duration) {
        Timer::after_micros(d.as_micros() as u64).await;
    }
}

/// Sends the next part of the output queue, the dropped note included.
async fn send_pending(server: &'static Server<'static>, conn: &Conn) -> Result<(), ()> {
    let mut buf = [0u8; 64];
    let n = nus::OUT.lock(|q| {
        let mut q = q.borrow_mut();
        let p = q.pending();
        let n = p.len().min(buf.len());
        buf[..n].copy_from_slice(&p[..n]);
        n
    });
    if n == 0 {
        return Ok(());
    }
    let mut link = Link { server, conn };
    framing::write(&mut link, &buf[..n]).await;
    nus::OUT.lock(|q| q.borrow_mut().discard(n));
    if nus::connected() { Ok(()) } else { Err(()) }
}
