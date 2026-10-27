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

use core::fmt::Write;
use core::sync::atomic::Ordering;

use embassy_executor::Spawner;
use embassy_futures::join::join;
use embassy_futures::select::{Either, select};
use embassy_nrf::mode::Async;
use embassy_nrf::rng::Rng;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use knixx_console::nus as framing;
use nrf_mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use trouble_host::prelude::*;

use crate::nus;

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

#[gatt_server]
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

    static RESOURCES: StaticCell<HostResources<DefaultPacketPool, 1, 2>> = StaticCell::new();
    static STACK: StaticCell<Stack> = StaticCell::new();
    let stack = STACK.init(
        trouble_host::new(sdc, RESOURCES.init(HostResources::new()))
            .set_random_address(address)
            .build(),
    );
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();
    static SERVER: StaticCell<Server> = StaticCell::new();
    let server = SERVER.init(
        Server::new_with_config(GapConfig::Peripheral(PeripheralConfig {
            name: nus::NAME,
            appearance: &appearance::UNKNOWN,
        }))
        .expect("gatt server"),
    );

    let _ = embassy_futures::join::join(runner.run(), serve(stack, &mut peripheral, server)).await;
    panic!("ble: the host runner stopped");
}

/// Advertises while the console asks for it and serves one central at a
/// time.
async fn serve(
    stack: &'static Stack,
    peripheral: &mut Peripheral<'static, SoftdeviceController<'static>, DefaultPacketPool>,
    server: &'static Server<'static>,
) {
    let mut adv_data = [0u8; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteServiceUuids128(&[NUS_UUID_LE]),
            AdStructure::CompleteLocalName(nus::NAME.as_bytes()),
        ],
        &mut adv_data,
    )
    .expect("advertising data");
    let adv = Advertisement::ConnectableScannableUndirected {
        adv_data: &adv_data[..len],
        scan_data: &[],
    };

    let mut on = false;
    loop {
        if !on {
            on = nus::CONTROL.wait().await;
            if !on {
                nus::OFF_DONE.signal(());
            }
            continue;
        }
        let accept = async {
            let advertiser = peripheral
                .advertise(&AdvertisementParameters::default(), adv)
                .await?;
            Ok::<_, BleHostError<_>>(advertiser.accept().await?.with_attribute_server(server)?)
        };
        let conn = match select(accept, nus::CONTROL.wait()).await {
            Either::First(Ok(conn)) => conn,
            Either::First(Err(_)) => {
                Timer::after_secs(1).await;
                continue;
            }
            Either::Second(c) => {
                // The advertising stops as its future is dropped.
                on = c;
                if !on {
                    nus::OFF_DONE.signal(());
                }
                continue;
            }
        };

        nus::OUT.lock(|q| q.borrow_mut().reset());
        nus::CONNECTED.store(true, Ordering::Relaxed);
        let params = async {
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

        // The sender watches for nus off between notifications, so that none
        // is cut off halfway.
        let off = matches!(
            select(join(params, events(server, &conn)), send(server, &conn)).await,
            Either::Second(true)
        );
        if off {
            // Let the queued output, such as the reply to nus off, reach the
            // central before the link drops.
            let deadline = Instant::now() + FLUSH_TIMEOUT;
            while nus::OUT.lock(|q| q.borrow().buffered() > 0) && Instant::now() < deadline {
                if send_pending(server, &conn).await.is_err() {
                    break;
                }
            }
            conn.raw().disconnect();
            let _ = with_timeout(FLUSH_TIMEOUT, async {
                while !matches!(conn.next().await, GattConnectionEvent::Disconnected { .. }) {}
            })
            .await;
        }
        nus::CONNECTED.store(false, Ordering::Relaxed);
        nus::OUT.lock(|q| q.borrow_mut().reset());
        if off {
            on = false;
            nus::OFF_DONE.signal(());
        }
    }
}

/// Handles the central's GATT requests until it disconnects: writes of RX
/// go to the console.
async fn events(server: &'static Server<'static>, conn: &Conn) {
    let rx = server.nus.rx.handle;
    loop {
        match conn.next().await {
            GattConnectionEvent::Disconnected { .. } => return,
            GattConnectionEvent::Gatt { event } => {
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
/// central is gone (false) or the console asks for nus off (true). The
/// console asks for nus on only while NUS is off, so that the control
/// signal means off here.
async fn send(server: &'static Server<'static>, conn: &Conn) -> bool {
    loop {
        match select(nus::WAKE.wait(), nus::CONTROL.wait()).await {
            Either::First(()) => {
                while nus::OUT.lock(|q| q.borrow().buffered() > 0) {
                    if send_pending(server, conn).await.is_err() {
                        return false;
                    }
                }
            }
            Either::Second(on) => {
                if !on {
                    return true;
                }
            }
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
