//! Recovery: a console on USB (CDC-ACM, 1209:A7EA, product "knixx
//! bootloader") that takes an image into internal flash, for a board whose
//! application is gone, broken or to be replaced by hand.
//!
//! Its commands, each answered with lines and the prompt ">>> ":
//!
//! ```text
//! version                      the bootloader's version
//! info                         why it waits, the application, the last install
//! fw put <size> <crc32> [force] take a .kfw file of size bytes
//! boot                         reset and boot the application
//! ```
//!
//! `fw put` answers `ready 4096`; the host then sends the file in chunks of
//! 4096 bytes and waits for `ok <offset>` after each. The header comes
//! first: its key and counter are checked before anything is erased (a
//! counter not above the installed one only with `force`). The body goes
//! straight to internal flash; at the end the CRC of the whole file, the
//! body's CRC and signature, and the CRC of what was written are checked,
//! and only then is the header written: `done <size>`, or `error: <reason>`.

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
use knixx_image::{self as image, HEADER_SIZE, Header, Verifier, Why, result};
use static_cell::StaticCell;

use crate::writer::Writer;
use crate::{BUILT, Irqs, VERSION, app, keys, led, reset};

type UsbDriver = Driver<'static, HardwareVbusDetect>;
type Class = CdcAcmClass<'static, UsbDriver>;

/// The chunk the host sends before it waits for an ok.
const CHUNK: usize = 4096;
/// How long an upload waits for the next packet.
const TIMEOUT: Duration = Duration::from_secs(5);
/// The largest USB packet of the CDC data endpoints.
const PACKET: usize = 64;

const PROMPT: &str = ">>> ";

/// What the console knows.
struct Ctx {
    why: Why,
    app: Option<Header>,
    last: u8,
    nvmc: Nvmc<'static>,
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
        app,
        last,
        nvmc: Nvmc::new(p.NVMC),
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

async fn session(class: &mut Class, ctx: &mut Ctx) -> Result<(), EndpointError> {
    let mut out = Out::new();
    let _ = writeln!(out, "=== knixx bootloader ===");
    info(&mut out, ctx);
    let _ = write!(out, "{PROMPT}");
    send(class, &mut out).await?;
    let mut line = [0u8; 96];
    let mut len = 0;
    let mut pkt = [0u8; PACKET];
    loop {
        let n = class.read_packet(&mut pkt).await?;
        for &b in &pkt[..n] {
            match b {
                b'\r' | b'\n' => {
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
                0x08 | 0x7F => len = len.saturating_sub(1),
                _ if len < line.len() => {
                    line[len] = b;
                    len += 1;
                }
                _ => {}
            }
        }
    }
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || c == '\t')
}

async fn command(class: &mut Class, ctx: &mut Ctx, out: &mut Out, cmd: &str) -> Result<(), EndpointError> {
    let mut words = cmd.split(' ').filter(|w| !w.is_empty());
    match (words.next(), words.next()) {
        (Some("version"), None) => {
            let _ = writeln!(out, "knixx bootloader {VERSION} built {BUILT}");
        }
        (Some("info"), None) => info(out, ctx),
        (Some("boot" | "reset"), None) => {
            let _ = writeln!(out, "booting");
            send(class, out).await?;
            Timer::after(Duration::from_millis(50)).await;
            reset(0, result::NONE);
        }
        (Some("fw"), Some("put")) => {
            let size = words.next().and_then(|s| s.parse::<u32>().ok());
            let crc = words
                .next()
                .and_then(|s| u32::from_str_radix(s.trim_start_matches("0x"), 16).ok());
            let force = match words.next() {
                None => Some(false),
                Some("force") => Some(true),
                Some(_) => None,
            };
            let (Some(size), Some(crc), Some(force)) = (size, crc, force) else {
                let _ = writeln!(out, "usage: fw put <size> <crc32> [force]");
                return Ok(());
            };
            let r = upload(class, ctx, out, size, crc, force).await;
            led::set(led::State::Usb);
            match r {
                Ok(()) => {
                    let _ = writeln!(out, "done {size}");
                }
                Err(Fail::Link(e)) => return Err(e),
                Err(Fail::Msg(m)) => {
                    let _ = writeln!(out, "error: {m}");
                    drain(class).await?;
                }
                Err(Fail::Refused(r)) => {
                    let _ = writeln!(out, "error: {r}");
                    drain(class).await?;
                }
            }
        }
        _ => {
            let _ = writeln!(
                out,
                "unknown command; try version, info, fw put <size> <crc32> [force], boot"
            );
        }
    }
    Ok(())
}

fn info(out: &mut Out, ctx: &Ctx) {
    let _ = writeln!(out, "bootloader {VERSION}, waiting over USB: {}", ctx.why);
    match &ctx.app {
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

/// Reads and drops what the host still sends after a failed upload, until
/// the line has been quiet for 200 ms.
async fn drain(class: &mut Class) -> Result<(), EndpointError> {
    let mut pkt = [0u8; PACKET];
    while let Ok(r) = with_timeout(Duration::from_millis(200), class.read_packet(&mut pkt)).await {
        r?;
    }
    Ok(())
}

enum Fail {
    Link(EndpointError),
    Msg(&'static str),
    Refused(image::Refusal),
}

impl From<EndpointError> for Fail {
    fn from(e: EndpointError) -> Self {
        Fail::Link(e)
    }
}

async fn upload(
    class: &mut Class,
    ctx: &mut Ctx,
    out: &mut Out,
    size: u32,
    crc: u32,
    force: bool,
) -> Result<(), Fail> {
    image::check_size(size).map_err(Fail::Refused)?;
    let _ = writeln!(out, "ready {CHUNK}");
    send(class, out).await?;

    let mut file_crc = image::Crc32::new();
    let mut raw = [0u8; HEADER_SIZE];
    let mut got = 0u32;
    let mut pkt = [0u8; PACKET];
    let mut verifier: Option<Verifier> = None;
    let mut writer: Option<Writer> = None;
    while got < size {
        let n = match with_timeout(TIMEOUT, class.read_packet(&mut pkt)).await {
            Ok(r) => r?,
            Err(_) => return Err(Fail::Msg("timed out")),
        };
        let data = &pkt[..n.min((size - got) as usize)];
        file_crc.update(data);
        let mut body = data;
        if (got as usize) < HEADER_SIZE {
            let k = (HEADER_SIZE - got as usize).min(data.len());
            raw[got as usize..got as usize + k].copy_from_slice(&data[..k]);
            body = &data[k..];
            if got as usize + k == HEADER_SIZE {
                // The whole header: check it before anything is erased.
                let v = Verifier::new(&raw, keys()).map_err(Fail::Refused)?;
                if v.header().file_size() != size {
                    return Err(Fail::Refused(image::Refusal::Length));
                }
                image::check_counter(v.header(), ctx.app.as_ref(), force).map_err(Fail::Refused)?;
                verifier = Some(v);
                ctx.app = None;
                led::set(led::State::Writing);
                writer = Some(Writer::begin(&mut ctx.nvmc).map_err(|_| Fail::Msg("erasing flash failed"))?);
            }
        }
        if !body.is_empty()
            && let (Some(v), Some(w)) = (verifier.as_mut(), writer.as_mut())
        {
            v.update(body);
            w.write(body).map_err(|_| Fail::Msg("writing flash failed"))?;
        }
        let before = got as usize / CHUNK;
        got += data.len() as u32;
        if (got as usize / CHUNK) > before && got < size {
            let _ = writeln!(out, "ok {}", got as usize / CHUNK * CHUNK);
            send(class, out).await?;
        }
    }
    if file_crc.finish() != crc {
        return Err(Fail::Msg("file CRC mismatch"));
    }
    let (Some(v), Some(mut w)) = (verifier, writer) else {
        return Err(Fail::Msg("no header"));
    };
    w.flush().map_err(|_| Fail::Msg("writing flash failed"))?;
    let hdr = v.finish().map_err(Fail::Refused)?;
    if image::crc32(app::body(hdr.len)) != hdr.body_crc {
        return Err(Fail::Msg("flash CRC mismatch after writing"));
    }
    w.commit(&raw)
        .map_err(|_| Fail::Msg("writing the header failed"))?;
    ctx.app = Some(hdr);
    ctx.last = result::INSTALLED;
    Ok(())
}
