//! knixx-shell is a KNX shell on the host: the firmware's bus engine
//! (knixx-bus) and its shared console commands (knixx-cli: switch, sniff,
//! stat, config) over an NCN5121 transceiver attached through an FTDI
//! USB serial adapter (0403:6001) at 38400 baud, 8N1. It replaces the Go
//! firmware's ftdi tool; the FTDI adapter is used as a serial port, without
//! libftdi.
//!
//! The configuration is read from --config, knixx.ini by default (the
//! built-in default when the file does not exist), and config set saves it
//! there.

use std::fmt;
use std::io::{BufRead, ErrorKind, Read, Write};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::Parser;
use knixx_bus::{Command, Engine, Flags, Output, QUEUE_LEN};
use knixx_cli::Action;
use knixx_device::config;
use knixx_device::thunder::Device;
use knixx_host::usbserial;

/// The USB ID of the FTDI FT232 adapter.
const FTDI_VID: u16 = 0x0403;
const FTDI_PID: u16 = 0x6001;
const BAUD: u32 = 38_400;

const PROMPT: &str = ">>> ";

#[derive(Parser)]
#[command(version, about = "A KNX shell over an FTDI-attached NCN5121")]
struct Args {
    /// The serial port of the FTDI adapter, instead of finding it by USB ID.
    #[arg(long)]
    port: Option<String>,
    /// The configuration file.
    #[arg(long, default_value = "knixx.ini")]
    config: PathBuf,
}

/// The context of the commands: the device configuration, the engine's
/// command queue and flags, and the configuration file.
struct Ctx {
    dev: Arc<Mutex<Device>>,
    cmds: SyncSender<Command>,
    flags: &'static Flags,
    path: PathBuf,
}

/// Saving the configuration file failed.
struct SaveError(std::io::Error);

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "config: {}", self.0)
    }
}

impl knixx_cli::Ctx for Ctx {
    type SaveError = SaveError;

    fn device(&self) -> Device {
        *self.dev.lock().unwrap()
    }

    fn set_device(&mut self, dev: Device) {
        *self.dev.lock().unwrap() = dev;
    }

    fn send(&mut self, cmd: Command) -> bool {
        !matches!(self.cmds.try_send(cmd), Err(TrySendError::Full(_)))
    }

    fn flags(&self) -> &Flags {
        self.flags
    }

    fn save(&mut self, data: &[u8]) -> Result<(), SaveError> {
        std::fs::write(&self.path, data).map_err(SaveError)
    }
}

/// The engine's output: to stdout, errors counted only as such.
struct Stdout;

impl Output for Stdout {
    fn write(&mut self, b: &[u8]) {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b);
        let _ = out.flush();
    }

    fn failed(&mut self) {}
}

/// Returns the time since start as an embassy Instant, the engine's clock.
fn now(start: Instant) -> embassy_time::Instant {
    embassy_time::Instant::from_micros(start.elapsed().as_micros() as u64)
}

/// Runs the engine on the port: it feeds the engine what the transceiver
/// sends, the commands queued by the shell, and the time, and writes what
/// the engine transmits. The port's read timeout paces it.
fn run_engine(
    mut port: Box<dyn serialport::SerialPort>,
    dev: Device,
    flags: &'static Flags,
    cmds: Receiver<Command>,
) {
    let start = Instant::now();
    let mut engine = Engine::new(dev, flags);
    let mut out = Stdout;
    let mut buf = [0u8; 128];
    loop {
        while let Some(b) = engine.poll(now(start), &mut out) {
            if let Err(e) = port.write_all(b) {
                out.write(format!("bus: uart write: {e}\n").as_bytes());
                engine.transmit_failed();
            }
        }
        if engine.accepts_commands() {
            match cmds.try_recv() {
                Ok(cmd) => {
                    engine.command(cmd, now(start), &mut out);
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => return,
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        match port.read(&mut buf) {
            Ok(n) if n > 0 => engine.feed(&buf[..n], now(start), &mut out),
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::TimedOut => {}
            Err(e) => {
                out.write(format!("bus: uart read: {e}\n").as_bytes());
                return;
            }
        }
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    let data = match std::fs::read(&args.config) {
        Ok(d) => d,
        Err(e) if e.kind() == ErrorKind::NotFound => config::DEFAULT.to_vec(),
        Err(e) => return Err(e).with_context(|| format!("config: {}", args.config.display())),
    };
    let dev = config::parse(&data).map_err(|e| anyhow::anyhow!("{}: {e}", args.config.display()))?;

    let name = match args.port {
        Some(p) => p,
        None => {
            let ports = usbserial::ports(FTDI_VID, FTDI_PID);
            match ports.as_slice() {
                [p] => p.clone(),
                [] => bail!("no FTDI adapter (0403:6001) on USB; use --port"),
                _ => bail!(
                    "several FTDI adapters ({}); pick one with --port",
                    ports.join(", ")
                ),
            }
        }
    };
    let port = serialport::new(&name, BAUD)
        .timeout(Duration::from_millis(10))
        .open()
        .with_context(|| format!("open {name}"))?;
    // Drop what the adapter received before.
    let _ = port.clear(serialport::ClearBuffer::Input);
    eprintln!("knixx-shell on {name}, Ctrl-D quits");

    let flags: &'static Flags = Box::leak(Box::new(Flags::new()));
    let (tx, rx) = mpsc::sync_channel(QUEUE_LEN);
    let engine = thread::spawn(move || run_engine(port, dev, flags, rx));

    let commands = knixx_cli::commands::<Ctx>();
    let mut ctx = Ctx {
        dev: Arc::new(Mutex::new(dev)),
        cmds: tx,
        flags,
        path: args.config,
    };
    let stdin = std::io::stdin();
    let mut line = String::new();
    loop {
        print!("{PROMPT}");
        std::io::stdout().flush()?;
        line.clear();
        if stdin.lock().read_line(&mut line)? == 0 {
            println!();
            break;
        }
        let mut reply = String::new();
        let action = knixx_cli::exec(&commands, &mut ctx, line.trim_end(), &mut reply);
        print!("{reply}");
        if action == Action::Exit {
            break;
        }
    }
    drop(ctx);
    let _ = engine.join();
    Ok(())
}
