//! A client of the board's console, for tools rather than people: it sends
//! a line, then reads what the board answers, line by line or up to the
//! prompt, over USB or BLE (NUS).

use std::io::{ErrorKind, Read, Write};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::{Characteristic, Peripheral as _, WriteType};
use btleplug::platform::Peripheral;
use futures_util::StreamExt;
use knixx_host::usbserial::{self, APP_IDS};
use serialport::TTYPort;
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::ble;
use crate::input::DEFAULT_CHUNK;
use crate::term::{NUS_RX, NUS_SERVICE, NUS_TX};

/// The console's prompt.
pub const PROMPT: &str = ">>> ";

/// How long a board found on USB has to answer before the console is taken
/// to be on BLE, as bolt term does: after nus on, USB stays silent.
const USB_PROBE: Duration = Duration::from_millis(500);

/// Where the console is: USB, on the given port or the one board found, or
/// BLE.
pub struct Target {
    pub port: Option<String>,
    pub ble: bool,
    pub name: String,
    pub addr: String,
    pub wait: Duration,
}

enum Link {
    Usb(TTYPort),
    Ble {
        p: Peripheral,
        rx: Characteristic,
        chunk: usize,
    },
}

/// A connection to the console.
pub struct Client {
    link: Link,
    incoming: mpsc::UnboundedReceiver<Vec<u8>>,
    /// What came in and was not read yet.
    text: String,
    /// Whether the console answered with its prompt already.
    awake: bool,
}

impl Client {
    /// Connects to the console at t, as bolt term does: on the USB port
    /// given, or on BLE when asked for; else on the one knixx board on USB
    /// if it answers there, else on BLE.
    pub async fn connect(t: &Target) -> Result<Client> {
        if t.ble || !t.addr.is_empty() {
            return Client::ble(t).await;
        }
        if let Some(port) = &t.port {
            return Client::usb(port);
        }
        let port = match usbserial::ports(&APP_IDS).as_slice() {
            [p] => p.clone(),
            [] => {
                eprintln!("no knixx board on USB, trying BLE");
                return Client::ble(t).await;
            }
            ports => bail!(
                "several boards on USB ({}); pick one with --port, or use --ble",
                ports.join(", ")
            ),
        };
        let mut c = Client::usb(&port)?;
        c.write(b"\r").await?;
        if c.read_until(PROMPT, USB_PROBE).await.is_ok() {
            c.awake = true;
            return Ok(c);
        }
        drop(c);
        eprintln!("no answer on {port}, the console is on BLE: trying BLE");
        Client::ble(t).await
    }

    fn usb(port: &str) -> Result<Client> {
        let p = usbserial::open(port, 115_200)?;
        let mut reader = p.try_clone_native().context("serial port")?;
        let (tx, rx) = mpsc::unbounded_channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => return,
                    Ok(n) => {
                        if tx.send(buf[..n].to_vec()).is_err() {
                            return;
                        }
                    }
                    Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
                    Err(_) => return,
                }
            }
        });
        eprintln!("connected to {port}");
        Ok(Client {
            link: Link::Usb(p),
            incoming: rx,
            text: String::new(),
            awake: false,
        })
    }

    async fn ble(t: &Target) -> Result<Client> {
        let (p, _gone) = ble::connect(&t.name, &t.addr, t.wait, "nus on").await?;
        let (Some(rx), Some(txc)) = (
            ble::characteristic(&p, NUS_SERVICE, NUS_RX),
            ble::characteristic(&p, NUS_SERVICE, NUS_TX),
        ) else {
            bail!("no NUS service: run nus on on the knixx console");
        };
        let mut notes = p.notifications().await.context("TX notifications")?;
        p.subscribe(&txc)
            .await
            .context("TX notifications")
            .map_err(ble::pairing_hint)?;
        let (tx, incoming) = mpsc::unbounded_channel();
        let uuid = txc.uuid;
        tokio::spawn(async move {
            while let Some(n) = notes.next().await {
                if n.uuid == uuid && tx.send(n.value).is_err() {
                    return;
                }
            }
        });
        let mtu = usize::from(p.mtu());
        let chunk = if mtu > 3 { mtu - 3 } else { DEFAULT_CHUNK };
        Ok(Client {
            link: Link::Ble { p, rx, chunk },
            incoming,
            text: String::new(),
            awake: false,
        })
    }

    /// Sends p to the board.
    pub async fn write(&mut self, p: &[u8]) -> Result<()> {
        match &mut self.link {
            Link::Usb(port) => port.write_all(p).context("write")?,
            Link::Ble { p: per, rx, chunk } => {
                for c in p.chunks(*chunk) {
                    per.write(rx, c, WriteType::WithResponse)
                        .await
                        .context("write")
                        .map_err(ble::pairing_hint)?;
                }
            }
        }
        Ok(())
    }

    /// Reads until the text holds pat, and returns the text up to it, pat
    /// included; what follows stays for the next read.
    async fn read_until(&mut self, pat: &str, wait: Duration) -> Result<String> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(i) = self.text.find(pat) {
                let rest = self.text.split_off(i + pat.len());
                return Ok(std::mem::replace(&mut self.text, rest));
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match timeout(left, self.incoming.recv()).await {
                Ok(Some(b)) => self.text.push_str(&String::from_utf8_lossy(&b)),
                Ok(None) => bail!("the board went away"),
                Err(_) => bail!("no answer within {wait:?}"),
            }
        }
    }

    /// Reads the next line, as line does, or None when none comes within
    /// wait.
    pub async fn line_within(&mut self, wait: Duration) -> Result<Option<String>> {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            if let Some(i) = self.text.find('\n') {
                let rest = self.text.split_off(i + 1);
                let l = std::mem::replace(&mut self.text, rest);
                return Ok(Some(usbserial::strip(&l).trim_end().to_string()));
            }
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            match timeout(left, self.incoming.recv()).await {
                Ok(Some(b)) => self.text.push_str(&String::from_utf8_lossy(&b)),
                Ok(None) => bail!("the board went away"),
                Err(_) => return Ok(None),
            }
        }
    }

    /// Reads the next line, without its line end and escape sequences.
    pub async fn line(&mut self, wait: Duration) -> Result<String> {
        let l = self.read_until("\n", wait).await?;
        Ok(usbserial::strip(&l).trim_end().to_string())
    }

    /// Wakes the console: sends a line end and reads up to the prompt, past
    /// the banner if it comes.
    pub async fn wake(&mut self) -> Result<()> {
        if self.awake {
            return Ok(());
        }
        // A line end now and then: on USB a packet can get stuck in the
        // board until the next one arrives.
        let mut last = None;
        for _ in 0..4 {
            self.write(b"\r").await?;
            match self.read_until(PROMPT, Duration::from_millis(1500)).await {
                Ok(_) => {
                    self.awake = true;
                    return Ok(());
                }
                Err(e) => last = Some(e),
            }
        }
        let e = last.map(|e| e.to_string()).unwrap_or_default();
        Err(anyhow!(
            "{e}: is the console on this link? (nus on moves it to BLE)"
        ))
    }

    /// Runs a command and returns its output, up to the next prompt, without
    /// the echo of the line, which send_line reads.
    pub async fn command(&mut self, line: &str, wait: Duration) -> Result<String> {
        self.send_line(line).await?;
        let out = self.read_until(PROMPT, wait).await?;
        let out = usbserial::strip(&out);
        Ok(out.strip_suffix(PROMPT).unwrap_or(&out).trim_end().to_string())
    }

    /// Sends line and reads past its echo.
    pub async fn send_line(&mut self, line: &str) -> Result<()> {
        self.write(format!("{line}\r").as_bytes()).await?;
        self.read_until("\n", Duration::from_secs(5)).await?;
        Ok(())
    }

    /// Reads up to the next prompt.
    pub async fn prompt(&mut self, wait: Duration) -> Result<()> {
        self.read_until(PROMPT, wait).await.map(|_| ())
    }

    /// Disconnects from BLE; USB closes when dropped.
    pub async fn close(self) {
        if let Link::Ble { p, .. } = self.link {
            let _ = p.disconnect().await;
        }
    }
}
