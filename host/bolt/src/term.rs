//! bolt term: a terminal on the console of a knixx board, on its USB serial
//! port or on the Nordic UART Service (NUS) that the nus on console command
//! starts. Keys go to the board as typed; Ctrl-] quits. Without a mode it
//! takes USB when the board answers there: after nus on the USB port stays
//! silent, and it then takes BLE.

use std::io::{ErrorKind, IsTerminal, Read, Write};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures_util::StreamExt;
use knixx_host::usbserial::{self, APP_PID, APP_VID};
use serialport::TTYPort;
use tokio::sync::{mpsc, watch};

use crate::ble;
use crate::input::{DEFAULT_CHUNK, chunks};

/// The NUS service and its characteristics: the central writes RX and is
/// notified on TX.
const NUS_SERVICE: &str = "6e400001-b5a3-f393-e0a9-e50e24dcca9e";
const NUS_RX: &str = "6e400002-b5a3-f393-e0a9-e50e24dcca9e";
const NUS_TX: &str = "6e400003-b5a3-f393-e0a9-e50e24dcca9e";

/// How long bolt term waits for the board to answer a CR on USB before it
/// takes the console to be on BLE.
const USB_PROBE: Duration = Duration::from_millis(500);

/// The largest write to the USB serial port.
const USB_CHUNK: usize = 256;

/// How long a session waits for the board's answer after the input ended,
/// as with piped input.
const EOF_WAIT: Duration = Duration::from_secs(1);

/// The console modes of bolt term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// USB when the board answers there, BLE otherwise.
    Auto,
    Usb,
    Ble,
}

/// Returns the mode that the flags select: --port implies USB and --addr
/// BLE.
pub fn mode(usb: bool, ble: bool, port: bool, addr: bool) -> Result<Mode> {
    match () {
        _ if usb && ble => bail!("--usb and --ble exclude each other"),
        _ if (usb || port) && (ble || addr) => {
            bail!("--usb and --port are for USB, --ble and --addr for BLE")
        }
        _ if usb || port => Ok(Mode::Usb),
        _ if ble || addr => Ok(Mode::Ble),
        _ => Ok(Mode::Auto),
    }
}

/// The flags of bolt term.
pub struct Options {
    pub mode: Mode,
    pub port: Option<String>,
    pub name: String,
    pub addr: String,
    pub wait: Duration,
}

pub fn run(o: Options) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(run_async(o))
}

async fn run_async(o: Options) -> Result<()> {
    match o.mode {
        Mode::Ble => return term_ble(&o).await,
        Mode::Usb => {
            let port = match &o.port {
                Some(p) => p.clone(),
                None => app_port("use --port")?,
            };
            let p = usbserial::open(&port, 115_200)?;
            eprintln!("connected to {port}, Ctrl-] to quit");
            return term_usb(p, true).await;
        }
        Mode::Auto => {}
    }
    let ports = usbserial::ports(APP_VID, APP_PID);
    let port = match ports.as_slice() {
        [] => {
            eprintln!("no knixx board on USB, trying BLE");
            return term_ble(&o).await;
        }
        [p] => p.clone(),
        _ => bail!(
            "several serial ports ({}); pick one with --port, or use --ble",
            ports.join(", ")
        ),
    };
    let mut p = usbserial::open(&port, 115_200)?;
    let got = probe(&mut p, USB_PROBE).with_context(|| port.clone())?;
    if got.is_empty() {
        drop(p);
        eprintln!("no answer on {port}, the console is on BLE: trying BLE");
        return term_ble(&o).await;
    }
    eprintln!("connected to {port}, Ctrl-] to quit");
    let mut out = std::io::stdout();
    let _ = out.write_all(&got);
    let _ = out.flush();
    term_usb(p, false).await
}

/// Returns the serial port of the one knixx application on USB.
fn app_port(hint: &str) -> Result<String> {
    let ports = usbserial::ports(APP_VID, APP_PID);
    match ports.as_slice() {
        [] => bail!("no knixx board on USB; {hint}"),
        [p] => Ok(p.clone()),
        _ => bail!(
            "several serial ports ({}); pick one with --port",
            ports.join(", ")
        ),
    }
}

/// Sends a CR and returns what the board answers within d.
fn probe(p: &mut TTYPort, d: Duration) -> std::io::Result<Vec<u8>> {
    p.write_all(b"\r")?;
    let mut got = Vec::new();
    let mut buf = [0u8; 256];
    let end = Instant::now() + d;
    while Instant::now() < end {
        match p.read(&mut buf) {
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(got)
}

/// The link to the board's console. What the board sends goes to the
/// terminal from the link's own reader.
#[allow(async_fn_in_trait)]
pub trait Conn {
    /// Sends p to the board, at most chunk bytes.
    async fn write(&mut self, p: &[u8]) -> Result<()>;
    /// The largest write.
    fn chunk(&self) -> usize;
}

/// The console on the board's USB serial port.
struct UsbConn(TTYPort);

impl Conn for UsbConn {
    async fn write(&mut self, p: &[u8]) -> Result<()> {
        self.0.write_all(p)?;
        Ok(())
    }

    fn chunk(&self) -> usize {
        USB_CHUNK
    }
}

/// Runs the terminal on USB port p.
async fn term_usb(p: TTYPort, greet: bool) -> Result<()> {
    let mut reader = p.try_clone_native().context("serial port")?;
    let (gone_tx, gone) = watch::channel(false);
    // Copies what the board sends to the terminal until the port fails, as
    // when the board is unplugged or resets.
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let mut out = std::io::stdout();
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = out.write_all(&buf[..n]);
                    let _ = out.flush();
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
                Err(_) => break,
            }
        }
        let _ = gone_tx.send(true);
    });
    let _raw = RawMode::enable()?;
    session(&mut UsbConn(p), stdin(), gone, greet, EOF_WAIT).await
}

/// The console on the board's NUS service.
struct BleConn {
    p: Peripheral,
    rx: btleplug::api::Characteristic,
    /// The payload of an ATT write with the negotiated MTU.
    size: usize,
}

impl Conn for BleConn {
    async fn write(&mut self, p: &[u8]) -> Result<()> {
        // Writes with response pace the input to what the board takes, so
        // that a paste does not overflow its receive ring.
        self.p.write(&self.rx, p, WriteType::WithResponse).await?;
        Ok(())
    }

    fn chunk(&self) -> usize {
        self.size
    }
}

/// Runs the terminal on the BLE console of the board named name, or with
/// address addr.
async fn term_ble(o: &Options) -> Result<()> {
    let (p, gone) = ble::connect(&o.name, &o.addr, o.wait, "nus on").await?;
    let r = async {
        if !ble::has_service(&p, NUS_SERVICE) {
            bail!("no NUS service");
        }
        let (Some(rx), Some(tx)) = (
            ble::characteristic(&p, NUS_SERVICE, NUS_RX),
            ble::characteristic(&p, NUS_SERVICE, NUS_TX),
        ) else {
            bail!("NUS service lacks its RX or TX characteristic");
        };
        let mut notes = p.notifications().await.context("TX notifications")?;
        p.subscribe(&tx).await.context("TX notifications")?;
        let tx_uuid = tx.uuid;
        tokio::spawn(async move {
            let mut out = std::io::stdout();
            while let Some(n) = notes.next().await {
                if n.uuid == tx_uuid {
                    let _ = out.write_all(&n.value);
                    let _ = out.flush();
                }
            }
        });
        let mtu = usize::from(p.mtu());
        let size = if mtu > 3 { mtu - 3 } else { DEFAULT_CHUNK };
        eprintln!("connected, Ctrl-] to quit");
        let _raw = RawMode::enable()?;
        session(
            &mut BleConn {
                p: p.clone(),
                rx,
                size,
            },
            stdin(),
            gone,
            true,
            EOF_WAIT,
        )
        .await
    }
    .await;
    let _ = p.disconnect().await;
    r
}

/// Reads the terminal on a thread of its own; the channel closes at the end
/// of the input.
fn stdin() -> mpsc::UnboundedReceiver<Vec<u8>> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        let mut buf = [0u8; 256];
        let mut input = std::io::stdin().lock();
        loop {
            match input.read(&mut buf) {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    if tx.send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
            }
        }
    });
    rx
}

/// The terminal in raw mode while stdin is one, restored on drop.
struct RawMode(bool);

impl RawMode {
    fn enable() -> Result<RawMode> {
        if !std::io::stdin().is_terminal() {
            return Ok(RawMode(false));
        }
        crossterm::terminal::enable_raw_mode().map_err(|e| anyhow!("terminal: {e}"))?;
        Ok(RawMode(true))
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if self.0 {
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
}

/// Runs the terminal on c with the keys from input until Ctrl-], the end of
/// the input or gone turns true. With greet it first sends a CR: the board
/// shows its banner and prompt on the first key.
pub async fn session<C: Conn>(
    c: &mut C,
    mut input: mpsc::UnboundedReceiver<Vec<u8>>,
    mut gone: watch::Receiver<bool>,
    greet: bool,
    eof_wait: Duration,
) -> Result<()> {
    if greet {
        c.write(b"\r").await.context("write")?;
    }
    loop {
        tokio::select! {
            _ = gone.wait_for(|g| *g) => {
                eprint!("\r\n[disconnected]\r\n");
                return Ok(());
            }
            first = input.recv() => {
                // What was typed while the last write was in flight goes
                // out together.
                let (mut p, mut eof) = match first {
                    Some(p) => (p, false),
                    None => (Vec::new(), true),
                };
                while !eof {
                    match input.try_recv() {
                        Ok(more) => p.extend_from_slice(&more),
                        Err(mpsc::error::TryRecvError::Empty) => break,
                        Err(mpsc::error::TryRecvError::Disconnected) => eof = true,
                    }
                }
                let (out, quit) = chunks(&p, c.chunk());
                for b in out {
                    c.write(b).await.context("write")?;
                }
                if quit {
                    eprint!("\r\n");
                    return Ok(());
                }
                if eof {
                    // Leave the board time to answer.
                    tokio::time::sleep(eof_wait).await;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records the writes of a session.
    struct Fake {
        writes: Vec<Vec<u8>>,
        size: usize,
    }

    impl Conn for Fake {
        async fn write(&mut self, p: &[u8]) -> Result<()> {
            self.writes.push(p.to_vec());
            Ok(())
        }

        fn chunk(&self) -> usize {
            self.size
        }
    }

    impl Fake {
        fn sent(&self) -> Vec<u8> {
            self.writes.concat()
        }
    }

    /// Runs a session with input given at once, ending the input when eof.
    fn run_session(size: usize, input: &[&[u8]], eof: bool, greet: bool, gone: bool) -> Fake {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut c = Fake {
                writes: Vec::new(),
                size,
            };
            let (tx, rx) = mpsc::unbounded_channel();
            for p in input {
                tx.send(p.to_vec()).unwrap();
            }
            let _keep = if eof {
                drop(tx);
                None
            } else {
                Some(tx)
            };
            let (gone_tx, gone_rx) = watch::channel(gone);
            let s = session(&mut c, rx, gone_rx, greet, Duration::from_millis(10));
            tokio::time::timeout(Duration::from_secs(2), s)
                .await
                .expect("the session did not end")
                .unwrap();
            drop(gone_tx);
            c
        })
    }

    #[test]
    fn eof() {
        let c = run_session(4, &[b"version\r"], true, true, false);
        assert_eq!(c.sent(), b"\rversion\r", "the greeting CR and the line");
        assert!(c.writes.iter().all(|w| w.len() <= 4));
    }

    #[test]
    fn no_greet() {
        let c = run_session(USB_CHUNK, &[b"sys\r"], true, false, false);
        assert_eq!(c.sent(), b"sys\r");
    }

    #[test]
    fn escape() {
        // The input stays open, so only Ctrl-] ends the session.
        let c = run_session(USB_CHUNK, &[b"ab\x1dcd"], false, false, false);
        assert_eq!(c.sent(), b"ab");
    }

    #[test]
    fn coalesced() {
        // Keys typed while a write is in flight go out together.
        let c = run_session(USB_CHUNK, &[b"a", b"bc", b"\x1d"], false, false, false);
        assert_eq!(c.writes, [b"abc".to_vec()]);
    }

    #[test]
    fn gone() {
        let c = run_session(USB_CHUNK, &[], false, false, true);
        assert!(c.writes.is_empty());
    }

    #[test]
    fn modes() {
        for (usb, ble, port, addr, want) in [
            (false, false, false, false, Some(Mode::Auto)),
            (true, false, false, false, Some(Mode::Usb)),
            (false, true, false, false, Some(Mode::Ble)),
            (false, false, true, false, Some(Mode::Usb)),
            (false, false, false, true, Some(Mode::Ble)),
            (true, false, true, false, Some(Mode::Usb)),
            (false, true, false, true, Some(Mode::Ble)),
            (true, true, false, false, None),
            (false, true, true, false, None),
            (false, false, true, true, None),
            (true, false, false, true, None),
        ] {
            assert_eq!(mode(usb, ble, port, addr).ok(), want, "{usb} {ble} {port} {addr}");
        }
    }
}
