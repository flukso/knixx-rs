//! bolt is the host tool for knixx boards.
//!
//!     bolt convert  turn the firmware ELF into UF2, Intel HEX or a binary
//!     bolt genpkg   build a DFU package from the application's hex file
//!     bolt ota      send a DFU package over BLE to the bootloader
//!     bolt term     a terminal on the console, over USB or BLE
//!     bolt uf2      flash a UF2 file over USB through the UF2 bootloader
//!     bolt scan     look for a board's BLE advertising
//!
//! Status messages go to stderr.

mod ble;
mod genpkg;
mod input;
mod ota;
mod term;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use knixx_host::image::Image;
use knixx_host::legacydfu::{InitPacket, parse_ids, parse_uint};
use knixx_host::{blescan, flash, uf2};

#[derive(Parser)]
#[command(version, about = "Host tool for knixx boards")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Convert the firmware ELF to the format of the output's extension:
    /// .uf2 (for the UF2 bootloader's drive), .hex or .bin.
    Convert {
        /// The firmware ELF.
        elf: PathBuf,
        /// The output file: .uf2, .hex or .bin.
        #[arg(short, long)]
        output: PathBuf,
        /// The UF2 family ID.
        #[arg(long, default_value_t = uf2::FAMILY_NRF52840, value_parser = parse_u32)]
        family: u32,
    },
    /// Build a DFU package from the application's hex file, as
    /// adafruit-nrfutil dfu genpkg does. The defaults are those of the
    /// Feather nRF52840's Adafruit bootloader: device type 0x0052 and the
    /// firmware id 0x00B6 of its S140 6.1.1.
    Genpkg {
        /// The application's hex file.
        hex: PathBuf,
        /// The package to write.
        zip: PathBuf,
        /// The device type of the init packet.
        #[arg(long, default_value = "0x0052", value_parser = parse_u16)]
        dev_type: u16,
        /// The device revision of the init packet.
        #[arg(long, default_value = "0xFFFF", value_parser = parse_u16)]
        dev_rev: u16,
        /// The application version of the init packet.
        #[arg(long, default_value = "0xFFFFFFFF", value_parser = parse_u32)]
        app_ver: u32,
        /// Comma separated SoftDevice firmware ids the application runs on.
        #[arg(long, default_value = "0x00B6")]
        sd_req: String,
    },
    /// Send a DFU package over BLE to the bootloader (run ota on the knixx
    /// console first).
    Ota {
        /// The DFU package, as cargo xtask build makes it.
        #[arg(long, default_value = "build/knixx-dfu.zip")]
        zip: PathBuf,
        /// The advertised name of the bootloader.
        #[arg(long, default_value = "AdaDFU")]
        name: String,
        /// The address of the bootloader instead of the name: the MAC
        /// address on Linux, the device UUID on macOS.
        #[arg(long, default_value = "")]
        addr: String,
        /// How long to wait for the bootloader, in seconds.
        #[arg(long, default_value_t = 60)]
        wait: u64,
        /// The bytes per packet write, a multiple of 4 from 20 to 244; the
        /// default is the payload with the negotiated ATT MTU.
        #[arg(long, default_value_t = 0)]
        chunk: usize,
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
    /// Flash a UF2 file over USB: reset the board into its UF2 bootloader
    /// (or find it there after a double tap on reset), copy the file to its
    /// drive and wait for the board to come back.
    Uf2 {
        /// The UF2 file.
        #[arg(default_value = "build/knixx.uf2")]
        file: PathBuf,
        /// The serial port of the running board, instead of finding it.
        #[arg(long)]
        port: Option<String>,
        /// The name of the UF2 bootloader's drive.
        #[arg(long, default_value = flash::VOLUME)]
        volume: String,
        /// The line the board's version command must print after the flash.
        #[arg(long)]
        expect: Option<String>,
        /// How long to wait, in seconds, for the drive and for the board to
        /// come back.
        #[arg(long, default_value_t = 20)]
        wait: u64,
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

fn parse_u32(s: &str) -> Result<u32, String> {
    parse_uint(s, 32).map(|v| v as u32)
}

fn parse_u16(s: &str) -> Result<u16, String> {
    parse_uint(s, 16).map(|v| v as u16)
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Convert { elf, output, family } => convert(&elf, &output, family),
        Cmd::Uf2 {
            file,
            port,
            volume,
            expect,
            wait,
        } => flash::uf2(flash::Options {
            file: &file,
            port,
            volume: &volume,
            expect,
            wait: Duration::from_secs(wait),
        })
        .map_err(anyhow::Error::msg),
        Cmd::Scan { name, addr, wait } => scan(&name, &addr, wait),
        Cmd::Genpkg {
            hex,
            zip,
            dev_type,
            dev_rev,
            app_ver,
            sd_req,
        } => {
            let sd_req = parse_ids(&sd_req).map_err(|e| anyhow::anyhow!("--sd-req: {e}"))?;
            let ip = InitPacket {
                dev_type,
                dev_rev,
                app_ver,
                sd_req,
            };
            genpkg::run(&hex, &zip, &ip)
        }
        Cmd::Ota {
            zip,
            name,
            addr,
            wait,
            chunk,
        } => {
            if chunk != 0 && (chunk % 4 != 0 || !(20..=244).contains(&chunk)) {
                bail!("--chunk must be a multiple of 4 from 20 to 244");
            }
            ota::run(ota::Options {
                zip: &zip,
                name,
                addr,
                wait: Duration::from_secs(wait),
                chunk,
            })
        }
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

fn convert(elf: &Path, output: &Path, family: u32) -> Result<()> {
    let data = fs::read(elf).with_context(|| format!("read {}", elf.display()))?;
    let img = Image::from_elf(&data).with_context(|| elf.display().to_string())?;
    let out = match output.extension().and_then(|e| e.to_str()) {
        Some("uf2") => img.to_uf2(family),
        Some("hex") => img.to_hex().into_bytes(),
        Some("bin") => img.to_bin(),
        _ => bail!("{}: the extension must be .uf2, .hex or .bin", output.display()),
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
