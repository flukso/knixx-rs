//! Bluetooth LE with Nordic's SoftDevice Controller (nrf-sdc), linked into
//! the application, under the trouble host. The S140 SoftDevice that the
//! bootloader keeps in flash is never enabled.
//!
//! The multiprotocol service layer (MPSL) owns the clocks, RTC0, TIMER0,
//! TEMP, RADIO and PPI channels 17 to 31; it runs from boot, also when BLE
//! is idle, because USB gets its crystal clock through it.
//!
//! For now the board advertises as "knixx" and accepts a connection; the
//! NUS console follows.

use embassy_executor::Spawner;
use embassy_nrf::mode::Async;
use embassy_nrf::rng::Rng;
use nrf_mpsl::MultiprotocolServiceLayer;
use nrf_sdc::{self as sdc, SoftdeviceController};
use static_cell::StaticCell;
use trouble_host::prelude::*;

/// The advertised name, as of the TinyGo firmware's NUS console.
pub const NAME: &[u8] = b"knixx";

/// Outgoing and incoming L2CAP buffers per link.
const L2CAP_TXQ: u8 = 3;
const L2CAP_RXQ: u8 = 3;

/// The memory of the SoftDevice Controller for its configuration below.
const SDC_MEM: usize = 4720;

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

#[embassy_executor::task]
async fn ble_task(sdc: SoftdeviceController<'static>) -> ! {
    // A static random address, stable across resets: the device ID.
    let ficr = embassy_nrf::pac::FICR;
    let id = (u64::from(ficr.deviceid(1).read()) << 32) | u64::from(ficr.deviceid(0).read());
    let mut addr = id.to_le_bytes();
    addr[5] |= 0xC0; // static random
    let address = Address::random([addr[0], addr[1], addr[2], addr[3], addr[4], addr[5]]);

    static RESOURCES: StaticCell<HostResources<DefaultPacketPool, 1, 2>> = StaticCell::new();
    let stack = trouble_host::new(sdc, RESOURCES.init(HostResources::new()))
        .set_random_address(address)
        .build();
    let mut runner = stack.runner();
    let mut peripheral = stack.peripheral();

    let mut adv_data = [0u8; 31];
    let len = AdStructure::encode_slice(
        &[
            AdStructure::Flags(LE_GENERAL_DISCOVERABLE | BR_EDR_NOT_SUPPORTED),
            AdStructure::CompleteLocalName(NAME),
        ],
        &mut adv_data,
    )
    .unwrap();

    let advertise = async {
        loop {
            let params = AdvertisementParameters::default();
            let Ok(advertiser) = peripheral
                .advertise(
                    &params,
                    Advertisement::ConnectableScannableUndirected {
                        adv_data: &adv_data[..len],
                        scan_data: &[],
                    },
                )
                .await
            else {
                embassy_time::Timer::after_secs(1).await;
                continue;
            };
            let Ok(conn) = advertiser.accept().await else {
                continue;
            };
            // Hold the connection until the central drops it.
            while !matches!(conn.next().await, ConnectionEvent::Disconnected { .. }) {}
        }
    };
    let _ = embassy_futures::join::join(runner.run(), advertise).await;
    panic!("ble: the host runner stopped");
}
