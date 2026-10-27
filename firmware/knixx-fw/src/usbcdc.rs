//! The USB CDC serial port of the console, with the USB IDs and strings of
//! the TinyGo firmware, so that host tools find the board as before
//! (239A:802A, /dev/cu.usbmodem* on macOS: no serial number).
//!
//! Output goes through OUT, a pipe that writers fill without waiting: a
//! write that does not fit is dropped, so a host that does not read never
//! stalls the console. tx_task drains it to the host and also resets the
//! board into its UF2 bootloader on the 1200-baud touch.

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::usb::vbus_detect::{self, HardwareVbusDetect};
use embassy_nrf::usb::{self, Driver};
use embassy_nrf::{Peri, interrupt, peripherals};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::pipe::Pipe;
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender, State};
use embassy_usb::{Builder, UsbDevice};
use static_cell::StaticCell;

use crate::boot;

type UsbDriver = Driver<'static, HardwareVbusDetect>;

/// The output to the host.
pub static OUT: Pipe<CriticalSectionRawMutex, 2048> = Pipe::new();

/// Whether the host holds DTR: a terminal has the port open.
pub static DTR: AtomicBool = AtomicBool::new(false);

/// The largest USB packet of the CDC data endpoints.
const PACKET: usize = 64;

/// Queues bytes for the host, all of them or, when they do not fit, none.
pub fn write(b: &[u8]) -> bool {
    if OUT.free_capacity() < b.len() {
        return false;
    }
    let _ = OUT.try_write(b);
    true
}

/// Starts the USB device and returns the receiving and sending ends of its
/// CDC port.
pub fn start(
    spawner: &Spawner,
    usbd: Peri<'static, peripherals::USBD>,
    irqs: impl Binding<interrupt::typelevel::USBD, usb::InterruptHandler<peripherals::USBD>>
    + Binding<interrupt::typelevel::CLOCK_POWER, vbus_detect::InterruptHandler>
    + 'static,
) -> (
    Receiver<'static, UsbDriver>,
    (Sender<'static, UsbDriver>, ControlChanged<'static>),
) {
    let driver = Driver::new(usbd, irqs, HardwareVbusDetect::new(irqs));

    let mut config = embassy_usb::Config::new(0x239A, 0x802A);
    config.manufacturer = Some("Adafruit Industries LLC");
    config.product = Some("Feather nRF52840 Express");
    config.serial_number = None;
    config.max_power = 100;
    config.max_packet_size_0 = 64;

    static CONFIG_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static BOS_DESC: StaticCell<[u8; 256]> = StaticCell::new();
    static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();
    static STATE: StaticCell<State> = StaticCell::new();

    let mut builder = Builder::new(
        driver,
        config,
        CONFIG_DESC.init([0; 256]),
        BOS_DESC.init([0; 256]),
        &mut [],
        CONTROL_BUF.init([0; 64]),
    );
    let class = CdcAcmClass::new(&mut builder, STATE.init(State::new()), PACKET as u16);
    let device = builder.build();
    spawner.spawn(device_task(device).unwrap());

    let (tx, rx, control) = class.split_with_control();
    (rx, (tx, control))
}

#[embassy_executor::task]
async fn device_task(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

/// Drains OUT to the host while a terminal holds DTR, and resets into the
/// UF2 bootloader when the host sets 1200 baud and drops DTR, as the
/// Arduino tools and the TinyGo firmware do.
#[embassy_executor::task]
pub async fn tx_task(port: (Sender<'static, UsbDriver>, ControlChanged<'static>)) -> ! {
    let (mut tx, control) = port;
    let mut buf = [0u8; PACKET];
    loop {
        tx.wait_connection().await;
        loop {
            match select(control.control_changed(), OUT.read(&mut buf)).await {
                Either::First(()) => {
                    let dtr = control.dtr();
                    DTR.store(dtr, Ordering::Relaxed);
                    if tx.line_coding().data_rate() == 1200 && !dtr {
                        boot::reset_into(boot::UF2);
                    }
                }
                Either::Second(n) => {
                    if !DTR.load(Ordering::Relaxed) {
                        // No terminal: the output is lost, as on a
                        // serial line.
                        continue;
                    }
                    if tx.write_packet(&buf[..n]).await.is_err() {
                        break;
                    }
                    // A full packet does not end a transfer: end it with
                    // an empty one, so that the host passes the data on.
                    if n == PACKET && OUT.is_empty() && tx.write_packet(&[]).await.is_err() {
                        break;
                    }
                }
            }
        }
        DTR.store(false, Ordering::Relaxed);
    }
}
