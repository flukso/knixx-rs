//! bolt is the host tool for knixx boards.
//!
//!     bolt convert  turn the firmware ELF into UF2, Intel HEX or a binary

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use knixx_host::image::Image;
use knixx_host::uf2;

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
}

fn parse_u32(s: &str) -> Result<u32, String> {
    let r = match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        Some(h) => u32::from_str_radix(h, 16),
        None => s.parse(),
    };
    r.map_err(|e| e.to_string())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Convert { elf, output, family } => convert(&elf, &output, family),
    }
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
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let tmp = dir.join(format!(".{}.tmp", path.file_name().unwrap().to_string_lossy()));
    fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
