//! The USB CDC serial port of the console, with the USB IDs and strings of
//! the TinyGo firmware, so that host tools find the board as before
//! (239A:802A, /dev/cu.usbmodem* on macOS: no serial number).
//!
//! Output goes to OUT, a queue that writers fill without waiting: a write
//! that does not fit is dropped whole, and the sender notes the loss once it
//! catches up, so a host that does not read never stalls the console.
//! tx_task drains it to the host while a terminal holds DTR, and resets the
//! board into its UF2 bootloader on the 1200-baud touch.

use core::cell::RefCell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_futures::select::{Either, select};
use embassy_nrf::interrupt::typelevel::Binding;
use embassy_nrf::usb::vbus_detect::{self, HardwareVbusDetect};
use embassy_nrf::usb::{self, Driver};
use embassy_nrf::{Peri, interrupt, peripherals};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_usb::class::cdc_acm::{CdcAcmClass, ControlChanged, Receiver, Sender, State};
use embassy_usb::{Builder, UsbDevice};
use knixx_console::outq::Queue;
use static_cell::StaticCell;

use crate::boot;

pub type UsbDriver = Driver<'static, HardwareVbusDetect>;

/// The console output to the host.
pub static OUT: Mutex<CriticalSectionRawMutex, RefCell<Queue>> = Mutex::new(RefCell::new(Queue::new()));

/// Wakes tx_task after a write to OUT.
static WAKE: Signal<CriticalSectionRawMutex, ()> = Signal::new();

/// Whether the host holds DTR: a terminal has the port open.
pub static DTR: AtomicBool = AtomicBool::new(false);

/// Counts the terminal sessions: it goes up each time the host raises DTR,
/// so that the console greets each new terminal with the banner.
pub static SESSIONS: AtomicU32 = AtomicU32::new(0);

/// The largest USB packet of the CDC data endpoints.
const PACKET: usize = 64;

/// Queues raw bytes for the host, whole or not at all.
pub fn write(b: &[u8]) {
    OUT.lock(|q| q.borrow_mut().write(b));
    WAKE.signal(());
}

/// Queues bytes for the host with "\n" turned into "\r\n", whole or not
/// at all.
pub fn write_crlf(b: &[u8]) {
    OUT.lock(|q| q.borrow_mut().write_crlf(b));
    WAKE.signal(());
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

/// Copies the next part of OUT into buf, the dropped note included, and
/// returns its length. The part stays queued until discarded.
fn next_part(buf: &mut [u8; PACKET]) -> usize {
    OUT.lock(|q| {
        let mut q = q.borrow_mut();
        let p = q.pending();
        let n = p.len().min(PACKET);
        buf[..n].copy_from_slice(&p[..n]);
        n
    })
}

/// Drops all of OUT: without a terminal the output is lost, as on a
/// serial line.
fn drain() {
    OUT.lock(|q| {
        let mut q = q.borrow_mut();
        loop {
            let n = q.pending().len();
            if n == 0 {
                break;
            }
            q.discard(n);
        }
    });
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
        'connected: loop {
            if let Either::First(()) = select(control.control_changed(), WAKE.wait()).await {
                let dtr = control.dtr();
                if dtr && !DTR.swap(dtr, Ordering::Relaxed) {
                    SESSIONS.fetch_add(1, Ordering::Relaxed);
                }
                DTR.store(dtr, Ordering::Relaxed);
                if tx.line_coding().data_rate() == 1200 && !dtr {
                    boot::reset_into(boot::UF2);
                }
            }
            if !DTR.load(Ordering::Relaxed) {
                drain();
                continue;
            }
            loop {
                let n = next_part(&mut buf);
                if n == 0 {
                    break;
                }
                if tx.write_packet(&buf[..n]).await.is_err() {
                    break 'connected;
                }
                OUT.lock(|q| q.borrow_mut().discard(n));
                // A full packet does not end a transfer: end it with an
                // empty one, so that the host passes the data on.
                if n == PACKET
                    && OUT.lock(|q| q.borrow().buffered() == 0)
                    && tx.write_packet(&[]).await.is_err()
                {
                    break 'connected;
                }
            }
        }
        DTR.store(false, Ordering::Relaxed);
    }
}
