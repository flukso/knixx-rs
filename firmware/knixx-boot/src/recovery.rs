//! Recovery: a console on USB (CDC-ACM, 1209:A7EA, product "knixx
//! bootloader") that takes an image into internal flash, for a board whose
//! application is gone, broken or to be replaced by hand.
//!
//! Its text commands, each answered with lines and the prompt ">>> ":
//!
//! ```text
//! version   the bootloader's version
//! info      why it waits, the application, the last install
//! boot      reset and boot the application
//! ```
//!
//! An image comes in over the transfer protocol (knixx_xfer), in frames
//! among the text: a put of the firmware kind, then data, stop-and-wait in
//! frames of 4 KiB. The header comes first: its key and counter are checked
//! before anything is erased (a counter not above the installed one only
//! with put's force flag). The body goes straight to internal flash; at the
//! end the CRC of the whole file, the body's CRC and signature, and the CRC
//! of what was written are checked, and only then is the header written:
//! done, or error.

use core::fmt::Write as _;

use embassy_executor::Executor;
use embassy_nrf::config::HfclkSource;
use embassy_nrf::nvmc::Nvmc;
use embassy_nrf::pac;
use embassy_nrf::usb::Driver;
use embassy_nrf::usb::vbus_detect::HardwareVbusDetect;
use embassy_time::{Duration, Timer, with_timeout};
use embassy_usb::class::cdc_acm::{CdcAcmClass, State};
use embassy_usb::driver::EndpointError;
use embassy_usb::{Builder, UsbDevice};
use knixx_image::{self as image, HEADER_SIZE, Header, Refusal, Verifier, Why, result};
use knixx_xfer::frame::{self, Decoder, Fed};
use knixx_xfer::msg::{FORCE, Kind, Msg};
use knixx_xfer::receiver::{Fail, Offer, Out as Reply, Receiver, Sink};
use static_cell::StaticCell;

use crate::writer::Writer;
use crate::{BUILT, Irqs, VERSION, app, keys, led, reset};

type UsbDriver = Driver<'static, HardwareVbusDetect>;
type Class = CdcAcmClass<'static, UsbDriver>;

/// A frame that stalls this long halfway is dropped: its rest is not
/// coming, and the host sends it again.
const STALL: Duration = Duration::from_millis(500);
/// After this long without data, in ms, a new put replaces a transfer.
const IDLE_MS: u64 = 10_000;
/// The largest USB packet of the CDC data endpoints.
const PACKET: usize = 64;

const PROMPT: &str = ">>> ";

/// What the console knows.
struct Ctx {
    why: Why,
    last: u8,
    sink: BootSink,
}

/// The firmware coming in: its header first, checked before anything is
/// erased, then its body, straight into internal flash.
struct BootSink {
    nvmc: Nvmc<'static>,
    /// The installed application, while it is valid.
    app: Option<Header>,
    force: bool,
    raw: [u8; HEADER_SIZE],
    got: usize,
    size: u32,
    verifier: Option<Verifier>,
    writer: Option<Writer>,
}

/// A short reason for a refusal.
fn refusal(r: Refusal) -> Fail {
    let reason = match r {
        Refusal::Header(_) => "bad header",
        Refusal::Length => "length mismatch",
        Refusal::Crc => "body CRC mismatch",
        Refusal::UnknownKey(_) => "unknown key",
        Refusal::Signature => "bad signature",
        Refusal::Downgrade { .. } => "downgrade",
        Refusal::TooLarge(_) => "too large",
    };
    Fail::new(r.code(), reason)
}

impl Sink for BootSink {
    fn open(&mut self, _: Kind, _: &str, size: u32, flags: u8) -> Result<(), Fail> {
        image::check_size(size).map_err(refusal)?;
        self.force = flags & FORCE != 0;
        self.got = 0;
        self.size = size;
        self.verifier = None;
        self.writer = None;
        Ok(())
    }

    fn write(&mut self, mut data: &[u8]) -> Result<(), Fail> {
        if self.got < HEADER_SIZE {
            let n = (HEADER_SIZE - self.got).min(data.len());
            self.raw[self.got..self.got + n].copy_from_slice(&data[..n]);
            self.got += n;
            data = &data[n..];
            if self.got < HEADER_SIZE {
                return Ok(());
            }
            // The whole header: check it before anything is erased.
            let v = Verifier::new(&self.raw, keys()).map_err(refusal)?;
            if v.header().file_size() != self.size {
                return Err(refusal(Refusal::Length));
            }
            image::check_counter(v.header(), self.app.as_ref(), self.force).map_err(refusal)?;
            self.app = None;
            self.writer = Some(
                Writer::begin(&mut self.nvmc)
                    .map_err(|_| Fail::new(knixx_xfer::code::STORAGE, "erase failed"))?,
            );
            self.verifier = Some(v);
            led::set(led::State::Writing);
        }
        if data.is_empty() {
            return Ok(());
        }
        let (Some(v), Some(w)) = (self.verifier.as_mut(), self.writer.as_mut()) else {
            return Err(Fail::new(knixx_xfer::code::STATE, "no header"));
        };
        v.update(data);
        w.write(&mut self.nvmc, data)
            .map_err(|_| Fail::new(knixx_xfer::code::STORAGE, "write failed"))
    }

    fn finish(&mut self) -> Result<u32, Fail> {
        let storage = |_| Fail::new(knixx_xfer::code::STORAGE, "write failed");
        let (Some(v), Some(mut w)) = (self.verifier.take(), self.writer.take()) else {
            return Err(Fail::new(knixx_xfer::code::STATE, "no header"));
        };
        w.flush(&mut self.nvmc).map_err(storage)?;
        let hdr = v.finish().map_err(refusal)?;
        if image::crc32(app::body(hdr.len)) != hdr.body_crc {
            return Err(Fail::new(knixx_xfer::code::STORAGE, "flash CRC mismatch"));
        }
        w.commit(&mut self.nvmc, &self.raw).map_err(storage)?;
        self.app = Some(hdr);
        Ok(hdr.counter)
    }

    fn abort(&mut self) {
        self.verifier = None;
        self.writer = None;
    }
}

pub fn run(why: Why, app: Option<Header>) -> ! {
    let mut config = embassy_nrf::config::Config::default();
    // USB runs on the crystal.
    config.hfclk_source = HfclkSource::ExternalXtal;
    let p = embassy_nrf::init(config);
    led::start(led::State::Started);
    let last = pac::POWER.gpregret2().read().gpregret();

    let driver = Driver::new(p.USBD, Irqs, HardwareVbusDetect::new(Irqs));
    let mut config = embassy_usb::Config::new(0x1209, 0xA7EA);
    config.manufacturer = Some("flukso");
    config.product = Some("knixx bootloader");
    config.serial_number = Some(serial_number());
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
    let ctx = Ctx {
        why,
        last,
        sink: BootSink {
            nvmc: Nvmc::new(p.NVMC),
            app,
            force: false,
            raw: [0; HEADER_SIZE],
            got: 0,
            size: 0,
            verifier: None,
            writer: None,
        },
    };

    static EXECUTOR: StaticCell<Executor> = StaticCell::new();
    EXECUTOR.init(Executor::new()).run(|spawner| {
        spawner.spawn(usb_task(device).unwrap());
        spawner.spawn(console(class, ctx).unwrap());
    })
}

/// The serial number: the 64-bit FICR device ID in hex, as the application
/// has it.
fn serial_number() -> &'static str {
    static SERIAL: StaticCell<[u8; 16]> = StaticCell::new();
    let ficr = pac::FICR;
    let id = (u64::from(ficr.deviceid(1).read()) << 32) | u64::from(ficr.deviceid(0).read());
    let buf = SERIAL.init([0; 16]);
    for (i, b) in buf.iter_mut().enumerate() {
        *b = b"0123456789ABCDEF"[((id >> (60 - 4 * i)) & 0xf) as usize];
    }
    core::str::from_utf8(buf).unwrap_or("0")
}

#[embassy_executor::task]
async fn usb_task(mut device: UsbDevice<'static, UsbDriver>) -> ! {
    device.run().await
}

#[embassy_executor::task]
async fn console(mut class: Class, mut ctx: Ctx) -> ! {
    loop {
        class.wait_connection().await;
        led::set(led::State::Usb);
        let _ = session(&mut class, &mut ctx).await;
        led::set(led::State::Started);
    }
}

/// A small text buffer for the answers.
struct Out {
    buf: [u8; 512],
    len: usize,
}

impl Out {
    fn new() -> Out {
        Out {
            buf: [0; 512],
            len: 0,
        }
    }
}

impl core::fmt::Write for Out {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        for &b in s.as_bytes() {
            if b == b'\n' {
                self.push(b'\r');
            }
            self.push(b);
        }
        Ok(())
    }
}

impl Out {
    fn push(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }
}

/// Sends out's text and empties it.
async fn send(class: &mut Class, out: &mut Out) -> Result<(), EndpointError> {
    let data = &out.buf[..out.len];
    for p in data.chunks(PACKET) {
        class.write_packet(p).await?;
    }
    if !data.is_empty() && data.len().is_multiple_of(PACKET) {
        class.write_packet(&[]).await?;
    }
    out.len = 0;
    Ok(())
}

/// Sends a reply of the transfer protocol, framed.
async fn reply(class: &mut Class, r: Reply) -> Result<(), EndpointError> {
    let Some(m) = r.msg() else {
        return Ok(());
    };
    let mut msg = [0u8; knixx_xfer::msg::MAX_CONTROL];
    let Ok(n) = m.encode(&mut msg) else {
        return Ok(());
    };
    let mut buf = [0u8; knixx_xfer::msg::MAX_CONTROL + frame::OVERHEAD];
    let Some(n) = frame::encode(&msg[..n], &mut buf) else {
        return Ok(());
    };
    for p in buf[..n].chunks(PACKET) {
        class.write_packet(p).await?;
    }
    if n.is_multiple_of(PACKET) {
        class.write_packet(&[]).await?;
    }
    Ok(())
}

fn now_ms() -> u64 {
    embassy_time::Instant::now().as_millis()
}

/// Takes a message of the transfer protocol.
async fn message(
    class: &mut Class,
    ctx: &mut Ctx,
    rx: &mut Receiver,
    m: Msg<'_>,
) -> Result<(), EndpointError> {
    match m {
        Msg::Put { .. } => {
            let Some(offer) = Offer::of(&m) else {
                return Ok(());
            };
            let r = rx.put(&mut ctx.sink, &offer, now_ms());
            reply(class, r).await
        }
        Msg::Data { offset, bytes } => match rx.data(&mut ctx.sink, offset, bytes, now_ms()) {
            Reply::Complete(size) => {
                reply(class, Reply::Ack(size)).await?;
                let r = rx.finish(&mut ctx.sink);
                led::set(led::State::Usb);
                if matches!(r, Reply::Done { .. }) {
                    ctx.last = result::INSTALLED;
                }
                reply(class, r).await
            }
            r @ Reply::Error { .. } => {
                led::set(led::State::Usb);
                reply(class, r).await
            }
            r => reply(class, r).await,
        },
        Msg::Abort => {
            rx.abort(&mut ctx.sink);
            led::set(led::State::Usb);
            Ok(())
        }
        Msg::Get { .. } => {
            let r = Reply::Error {
                code: knixx_xfer::code::KIND,
                crc: 0,
                reason: "the bootloader sends no files",
            };
            reply(class, r).await
        }
        _ => Ok(()),
    }
}

async fn session(class: &mut Class, ctx: &mut Ctx) -> Result<(), EndpointError> {
    let mut out = Out::new();
    let _ = writeln!(out, "=== knixx bootloader ===");
    info(&mut out, ctx);
    let _ = write!(out, "{PROMPT}");
    send(class, &mut out).await?;
    let mut line = [0u8; 96];
    let mut len = 0;
    let mut pkt = [0u8; PACKET];
    let mut dec = Decoder::new();
    let mut rx = Receiver::new(&[Kind::Firmware], IDLE_MS);
    loop {
        let n = match with_timeout(STALL, class.read_packet(&mut pkt)).await {
            Ok(r) => r?,
            Err(_) => {
                // A frame whose rest is not coming: the host sends it again.
                dec.reset();
                continue;
            }
        };
        for &b in &pkt[..n] {
            match dec.feed(b) {
                Fed::None => {}
                Fed::Frame => {
                    if let Some(m) = Msg::decode(dec.message()) {
                        message(class, ctx, &mut rx, m).await?;
                    }
                }
                Fed::Text(b'\r' | b'\n') => {
                    let cmd = core::str::from_utf8(&line[..len]).unwrap_or("");
                    let cmd = trim(cmd);
                    let _ = writeln!(out);
                    if !cmd.is_empty() {
                        command(class, ctx, &mut out, cmd).await?;
                    }
                    let _ = write!(out, "{PROMPT}");
                    send(class, &mut out).await?;
                    len = 0;
                }
                Fed::Text(0x08 | 0x7F) => len = len.saturating_sub(1),
                Fed::Text(b) if len < line.len() => {
                    line[len] = b;
                    len += 1;
                }
                Fed::Text(_) => {}
            }
        }
    }
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || c == '\t')
}

async fn command(class: &mut Class, ctx: &mut Ctx, out: &mut Out, cmd: &str) -> Result<(), EndpointError> {
    match cmd {
        "version" => {
            let _ = writeln!(out, "knixx bootloader {VERSION} built {BUILT}");
        }
        "info" => info(out, ctx),
        "boot" | "reset" => {
            let _ = writeln!(out, "booting");
            send(class, out).await?;
            Timer::after(Duration::from_millis(50)).await;
            reset(0, result::NONE);
        }
        _ => {
            let _ = writeln!(out, "unknown command; try version, info, boot");
        }
    }
    Ok(())
}

fn info(out: &mut Out, ctx: &Ctx) {
    let _ = writeln!(out, "bootloader {VERSION}, waiting over USB: {}", ctx.why);
    match &ctx.sink.app {
        Some(h) => {
            let _ = writeln!(
                out,
                "application {} (counter {}, {} bytes)",
                h.version_str(),
                h.counter,
                h.len
            );
        }
        None => {
            let _ = writeln!(out, "application: none valid");
        }
    }
    let last = match ctx.last {
        result::NONE => None,
        result::INSTALLED => Some("installed"),
        result::WRITE => Some("failed writing internal flash"),
        result::READ => Some("the staged image could not be read"),
        r if r & 0xF0 == result::REFUSED => Some("the staged image was refused"),
        _ => None,
    };
    if let Some(l) = last {
        let _ = writeln!(out, "last install: {l} ({:#04x})", ctx.last);
    }
}
