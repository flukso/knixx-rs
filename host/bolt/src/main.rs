//! bolt is the host tool for knixx boards.
//!
//!     bolt convert  turn the firmware ELF into a signed .kfw, Intel HEX or a binary
//!     bolt flash    flash a .kfw file over USB through the bootloader's recovery
//!     bolt fw       push a .kfw file over BLE, install it
//!     bolt term     a terminal on the console, over USB or BLE
//!     bolt script   make, build and push scripts; list and remove them
//!     bolt config   push a whole knixx.ini over BLE
//!     bolt scan     look for a board's BLE advertising
//!
//! Status messages go to stderr.

mod ble;
mod client;
mod fleet;
mod fw;
mod input;
mod push;
mod script;
mod term;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use knixx_host::image::{Image, Stamp};
use knixx_host::{blescan, flash, sign};

#[derive(Parser)]
#[command(version, about = "Host tool for knixx boards")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert the firmware ELF to the format of the output's extension:
    /// .kfw (signed, for bolt flash), .hex or .bin. A .kfw is signed with
    /// $KNIXX_SIGNING_KEY, else the developer's key.
    Convert {
        /// The firmware ELF.
        elf: PathBuf,
        /// The output file: .kfw, .hex or .bin.
        #[arg(short, long)]
        output: PathBuf,
        /// The version string of a .kfw; the ELF's name by default.
        #[arg(long)]
        version: Option<String>,
    },
    /// A terminal on the console, over USB or BLE (run nus on on the knixx
    /// console for BLE). Without --usb or --ble it takes USB if the board
    /// answers there, else BLE. Ctrl-] quits.
    Term {
        /// The console on the USB serial port.
        #[arg(long)]
        usb: bool,
        /// The console on BLE.
        #[arg(long)]
        ble: bool,
        /// The USB serial port of the board, instead of finding it; implies
        /// --usb.
        #[arg(long)]
        port: Option<String>,
        /// The advertised name of the board on BLE.
        #[arg(long, default_value = "knixx")]
        name: String,
        /// The BLE address of the board instead of the name: the MAC address
        /// on Linux, the device UUID on macOS; implies --ble.
        #[arg(long, default_value = "")]
        addr: String,
        /// How long to wait for the board on BLE, in seconds.
        #[arg(long, default_value_t = 30)]
        wait: u64,
    },
    /// Flash a .kfw file over USB: reset the board into the bootloader's
    /// recovery (or find it there after a double tap of reset), send the
    /// file, and wait for the board to come back.
    Flash {
        /// The .kfw file.
        #[arg(default_value = "build/thunder.kfw")]
        file: PathBuf,
        /// The serial port of the running board, instead of finding it.
        #[arg(long)]
        port: Option<String>,
        /// Take an image whose version counter is not above the installed
        /// one.
        #[arg(long)]
        force: bool,
        /// The line the board's version command must print after the flash.
        #[arg(long)]
        expect: Option<String>,
        /// How long to wait, in seconds, for the bootloader and for the board
        /// to come back.
        #[arg(long, default_value_t = 20)]
        wait: u64,
    },
    /// The firmware on the board, over its console on USB or BLE: stage a
    /// .kfw file while it runs, install it through the bootloader, or show
    /// what runs and what is staged.
    Fw {
        #[command(subcommand)]
        cmd: FwCmd,
    },
    /// The boards of an installation, through one proxy board that this
    /// computer is paired with, over the KNX control plane.
    Fleet {
        #[command(subcommand)]
        cmd: FleetCmd,
        /// The advertised name of the proxy.
        #[arg(long, default_value = "knixx", global = true)]
        via: String,
        /// The BLE address of the proxy instead of its name.
        #[arg(long, default_value = "", global = true)]
        via_addr: String,
        /// How long to wait for the proxy, in seconds.
        #[arg(long, default_value_t = 30, global = true)]
        wait: u64,
    },
    /// The scripts of the board: make one from the template, build it to
    /// bytecode, push it, list and remove the ones on the board.
    Script {
        #[command(subcommand)]
        cmd: ScriptCmd,
    },
    /// The board's knixx.ini, whole, over its BLE transfer service.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Look for a board's BLE advertising, by name or address, and show it.
    Scan {
        /// The advertised name.
        #[arg(long, default_value = "knixx")]
        name: String,
        /// The address instead of the name: the MAC address on Linux, the
        /// device UUID on macOS.
        #[arg(long, default_value = "")]
        addr: String,
        /// How long to scan, in seconds.
        #[arg(long, default_value_t = 10)]
        wait: u64,
    },
}

/// Where the board's console is, for bolt script.
#[derive(clap::Args)]
struct Board {
    /// The USB serial port of the board, instead of finding it.
    #[arg(long)]
    port: Option<String>,
    /// The console on BLE (run nus on on the knixx console first). Without
    /// --ble or --port, it takes USB if the board answers there, else BLE.
    #[arg(long)]
    ble: bool,
    /// The advertised name of the board on BLE.
    #[arg(long, default_value = "knixx")]
    ble_name: String,
    /// The BLE address of the board instead of its name: the MAC address on
    /// Linux, the device UUID on macOS; implies --ble.
    #[arg(long, default_value = "")]
    addr: String,
    /// How long to wait for the board on BLE, in seconds.
    #[arg(long, default_value_t = 30)]
    wait: u64,
}

impl Board {
    /// The board on BLE for a push, and its console.
    fn push_opts(&self, open: Option<u8>) -> push::Opts {
        push::Opts {
            name: self.ble_name.clone(),
            addr: self.addr.clone(),
            wait: Duration::from_secs(self.wait),
            open,
            console: self.target(),
        }
    }

    fn target(&self) -> client::Target {
        client::Target {
            port: self.port.clone(),
            ble: self.ble,
            name: self.ble_name.clone(),
            addr: self.addr.clone(),
            wait: Duration::from_secs(self.wait),
        }
    }
}

#[derive(Subcommand)]
enum FleetCmd {
    /// List the boards that answer a scan on the bus.
    Scan,
    /// Show what a board runs and has staged.
    Status {
        /// The board's individual address, such as 1.1.5.
        board: String,
    },
    /// Open a board's update window for an image.
    Open {
        board: String,
        #[arg(default_value = "build/thunder.kfw")]
        file: PathBuf,
        #[arg(long, default_value_t = 10)]
        minutes: u8,
    },
    /// Have a board install its staged image, if it is this one.
    Install {
        board: String,
        #[arg(default_value = "build/thunder.kfw")]
        file: PathBuf,
    },
    /// Shut a board's update window.
    Close { board: String },
    /// Update every board to an image: scan, open, push in each window,
    /// install one by one, and check.
    Update {
        #[arg(default_value = "build/thunder.kfw")]
        file: PathBuf,
        /// Boards pushed to at a time.
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// Seconds between two boards' installs.
        #[arg(long, default_value_t = 2)]
        stagger: u64,
        /// How long each board's update window stays open, in minutes.
        #[arg(long, default_value_t = 15)]
        minutes: u8,
    },
}

#[derive(Subcommand)]
enum FwCmd {
    /// Stage a .kfw file over the board's BLE transfer service, in its
    /// update window (upload window <minutes> on its console, or --open),
    /// without pairing; --install installs it after.
    Push {
        #[arg(default_value = "build/thunder.kfw")]
        file: PathBuf,
        /// Open the window first, for this many minutes, over the console.
        #[arg(long)]
        open: Option<u8>,
        /// Install the image once it is staged, over the console.
        #[arg(long)]
        install: bool,
        #[command(flatten)]
        board: Board,
    },
    /// Install the staged image: the board resets into its bootloader.
    Install {
        #[command(flatten)]
        board: Board,
    },
    /// Show the running and the staged image.
    Status {
        #[command(flatten)]
        board: Board,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Push a knixx.ini, in the board's update window (or --open): the
    /// board checks it, applies it and saves it as it is, or refuses it
    /// whole. One for another individual address only with --force.
    Push {
        file: PathBuf,
        #[arg(long)]
        force: bool,
        /// Open the window first, for this many minutes, over the console.
        #[arg(long)]
        open: Option<u8>,
        #[command(flatten)]
        board: Board,
    },
}

#[derive(Subcommand)]
enum ScriptCmd {
    /// Write <name>.rhai from the template, to fill in.
    New { name: String },
    /// Build a script to bytecode, <script>.rhgn unless --output says.
    Build {
        script: PathBuf,
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
    /// Push a script, built first when it is source, over the board's BLE
    /// transfer service, in its update window (or --open); the board loads
    /// it, anew if it ran.
    Push {
        /// The script: source (.rhai) or bytecode (.rhgn); without an
        /// extension, <script>.rhai, else <script>.rhgn.
        script: PathBuf,
        /// The script's name on the board; the file's name by default.
        #[arg(long)]
        name: Option<String>,
        /// Open the window first, for this many minutes, over the console.
        #[arg(long)]
        open: Option<u8>,
        #[command(flatten)]
        board: Board,
    },
    /// Show what the board knows of a script: its on_info, calls, failures
    /// and last error; with --source, the lines where the last error was.
    Info {
        name: String,
        /// The script's source, rebuilt as the board's was to find the lines
        /// of the fault addresses.
        #[arg(long)]
        source: Option<PathBuf>,
        #[command(flatten)]
        board: Board,
    },
    /// List the scripts on the board.
    Ls {
        #[command(flatten)]
        board: Board,
    },
    /// Remove a script from the board.
    Rm {
        name: String,
        #[command(flatten)]
        board: Board,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Convert { elf, output, version } => convert(&elf, &output, version.as_deref()),
        Cmd::Flash {
            file,
            port,
            force,
            expect,
            wait,
        } => flash::kfw(flash::Options {
            file: &file,
            port,
            force,
            expect,
            wait: Duration::from_secs(wait),
        })
        .map_err(anyhow::Error::msg),
        Cmd::Scan { name, addr, wait } => scan(&name, &addr, wait),
        Cmd::Fleet {
            cmd,
            via,
            via_addr,
            wait,
        } => {
            let wait = Duration::from_secs(wait);
            match cmd {
                FleetCmd::Scan => fleet::scan(&via, &via_addr, wait),
                FleetCmd::Status { board } => fleet::status(&board, &via, &via_addr, wait),
                FleetCmd::Open { board, file, minutes } => fleet::request(
                    &board,
                    fleet::open_request(&file, minutes)?,
                    &via,
                    &via_addr,
                    wait,
                ),
                FleetCmd::Install { board, file } => {
                    fleet::request(&board, fleet::install_request(&file)?, &via, &via_addr, wait)
                }
                FleetCmd::Update {
                    file,
                    parallel,
                    stagger,
                    minutes,
                } => fleet::update(
                    &file,
                    &via,
                    &via_addr,
                    wait,
                    parallel,
                    Duration::from_secs(stagger),
                    minutes,
                ),
                FleetCmd::Close { board } => {
                    fleet::request(&board, knixx_knx::fw::Request::Close, &via, &via_addr, wait)
                }
            }
        }
        Cmd::Fw { cmd } => match cmd {
            FwCmd::Install { board } => fw::install(&board.target()),
            FwCmd::Push {
                file,
                open,
                install,
                board,
            } => push::fw(&file, &board.push_opts(open), install),
            FwCmd::Status { board } => fw::command("fw status", &board.target()),
        },
        Cmd::Config { cmd } => match cmd {
            ConfigCmd::Push {
                file,
                force,
                open,
                board,
            } => push::config(&file, force, &board.push_opts(open)),
        },
        Cmd::Script { cmd } => match cmd {
            ScriptCmd::New { name } => script::new(&name),
            ScriptCmd::Build { script, output } => script::build(&script, output.as_deref()),
            ScriptCmd::Push {
                script,
                name,
                open,
                board,
            } => push::script(&script, name.as_deref(), &board.push_opts(open)),
            ScriptCmd::Info { name, source, board } => {
                script::info(&name, source.as_deref(), &board.target())
            }
            ScriptCmd::Ls { board } => script::command("script ls", &board.target()),
            ScriptCmd::Rm { name, board } => script::command(&format!("script rm {name}"), &board.target()),
        },
        Cmd::Term {
            usb,
            ble,
            port,
            name,
            addr,
            wait,
        } => term::run(term::Options {
            mode: term::mode(usb, ble, port.is_some(), !addr.is_empty())?,
            port,
            name,
            addr,
            wait: Duration::from_secs(wait),
        }),
    }
}

fn scan(name: &str, addr: &str, wait: u64) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let found = rt.block_on(async {
        let adapter = blescan::adapter().await?;
        blescan::find(&adapter, name, addr, Duration::from_secs(wait)).await
    });
    let f = found.map_err(anyhow::Error::msg)?;
    println!(
        "{} {} rssi {}",
        f.id,
        f.name.as_deref().unwrap_or("-"),
        f.rssi.map_or("-".to_string(), |r| r.to_string())
    );
    Ok(())
}

fn convert(elf: &Path, output: &Path, version: Option<&str>) -> Result<()> {
    let data = fs::read(elf).with_context(|| format!("read {}", elf.display()))?;
    let img = Image::from_elf(&data).with_context(|| elf.display().to_string())?;
    let out = match output.extension().and_then(|e| e.to_str()) {
        Some("kfw") => {
            let key = sign::signing_key().map_err(anyhow::Error::msg)?;
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            let name = elf.file_stem().unwrap_or_default().to_string_lossy();
            let stamp = Stamp {
                counter: now as u32,
                version: version.unwrap_or(&name),
                built: now,
            };
            img.to_kfw(&stamp, &key)
                .with_context(|| elf.display().to_string())?
        }
        Some("hex") => img.to_hex().into_bytes(),
        Some("bin") => img.to_bin(),
        _ => bail!("{}: the extension must be .kfw, .hex or .bin", output.display()),
    };
    write_atomic(output, &out)?;
    eprintln!(
        "{}: {:#x}..{:#x} ({} bytes of image)",
        output.display(),
        img.start(),
        img.end(),
        img.runs().map(|(_, d)| d.len()).sum::<usize>()
    );
    Ok(())
}

/// Writes data to path through a temporary file in the same directory, so
/// that a failed write leaves no partial file.
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.tmp", path.file_name().unwrap().to_string_lossy()));
    fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
