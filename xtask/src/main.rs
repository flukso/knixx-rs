//! cargo xtask <task>: the build and flash tasks of knixx-rs.
//!
//!     build   build the firmware and write build/knixx.{elf,uf2,hex} and
//!             the DFU package build/knixx-dfu.zip
//!     flash   build, flash over USB and check the board runs the build
//!     ota     build, then send the DFU package over BLE with bolt ota
//!     term    a terminal on the board's console with bolt term
//!     cycle   build, then flash n times, checking USB and BLE after each
//!
//! The firmware is built in firmware/, a workspace of its own for the
//! nRF52840, stamped with the git commit and the build time.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use knixx_host::image::{Image, Ram};
use knixx_host::legacydfu::{InitPacket, Package, read_hex};
use knixx_host::{flash, uf2};

/// Where the application may go: from the end of the S140 6.1.1 SoftDevice
/// to the start of the user data that precedes the bootloader.
const APP_START: u32 = 0x26000;
const APP_END: u32 = 0xED000;

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  build   build the firmware and write build/knixx.{elf,uf2,hex} and the
          DFU package build/knixx-dfu.zip
  flash   build, flash over USB and check the board runs the build
          [--port <serial port>]
  ota     build, then send build/knixx-dfu.zip over BLE to a board in BLE
          DFU mode (run ota on its console) [bolt ota flags, e.g. --addr]
  term    a terminal on the console, over USB or BLE [bolt term flags]
  cycle   build, then flash <n> times; after each flash the board must
          report its version over USB and advertise as knixx over BLE
";

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") => build().map(|_| ()),
        Some("flash") => flash(&args[1..]),
        Some("cycle") => cycle(&args[1..]),
        Some("ota") => {
            build()?;
            let mut a = vec!["ota".to_string(), "--zip".into(), "build/knixx-dfu.zip".into()];
            a.extend_from_slice(&args[1..]);
            bolt(&a)
        }
        Some("term") => {
            let mut a = vec!["term".to_string()];
            a.extend_from_slice(&args[1..]);
            bolt(&a)
        }
        Some("help" | "-h" | "--help") | None => {
            eprint!("{USAGE}");
            Ok(())
        }
        Some(t) => bail!("unknown task {t:?}\n\n{USAGE}"),
    }
}

/// Returns the root of the repository.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

/// The version stamped into a build.
struct Version {
    commit: String,
    date: String,
}

impl Version {
    /// Returns the short commit hash, marked -dirty when tracked files
    /// differ from it, and the time now in UTC.
    fn now() -> Version {
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(root())
                .output()
                .ok()
                .filter(|o| o.status.success())
        };
        let mut commit = git(&["rev-parse", "--short", "HEAD"])
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_else(|| "unknown".into());
        if git(&["diff", "--quiet", "HEAD"]).is_none() && commit != "unknown" {
            commit.push_str("-dirty");
        }
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        Version {
            commit,
            date: utc(secs),
        }
    }
}

/// Formats seconds since the epoch as 2006-01-02T15:04:05Z.
fn utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    // Days to civil date, after Howard Hinnant's algorithm.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Builds the firmware and writes build/knixx.elf, .uf2 and .hex. It fails
/// when the image does not fit the application's flash.
fn build() -> Result<(PathBuf, Version)> {
    let v = Version::now();
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--release"])
        .current_dir(root().join("firmware"))
        .env("KNIXX_COMMIT", &v.commit)
        .env("KNIXX_BUILD_DATE", &v.date)
        .status()
        .context("run cargo build in firmware/")?;
    if !status.success() {
        bail!("firmware build failed");
    }
    let elf = root().join("firmware/target/thumbv7em-none-eabihf/release/knixx-fw");
    let data = fs::read(&elf).with_context(|| elf.display().to_string())?;
    let img = Image::from_elf(&data)?;
    let ram = Ram::from_elf(&data)?;
    if ram.allocator {
        bail!("the firmware links a global allocator: something allocates on the heap");
    }
    if img.start() < APP_START || img.end() > APP_END {
        bail!(
            "image {:#x}..{:#x} is outside the application's flash {APP_START:#x}..{APP_END:#x}",
            img.start(),
            img.end()
        );
    }
    let out = root().join("build");
    fs::create_dir_all(&out)?;
    fs::write(out.join("knixx.elf"), &data)?;
    fs::write(out.join("knixx.uf2"), img.to_uf2(uf2::FAMILY_NRF52840))?;
    let hex = img.to_hex();
    fs::write(out.join("knixx.hex"), &hex)?;
    // The package of bolt genpkg build/knixx.hex: from the hex file, whose
    // image genpkg rounds up to a multiple of 4.
    let ip = InitPacket::feather();
    let (_, bin) = read_hex(&hex)?;
    let mut zip = std::io::Cursor::new(Vec::new());
    Package::new(bin, &ip).write_zip(&mut zip, "knixx", &ip)?;
    fs::write(out.join("knixx-dfu.zip"), zip.into_inner())?;
    let used = img.end() - APP_START;
    eprintln!(
        "build/knixx.uf2: version {} built {}, {used} of {} bytes of flash ({}%)",
        v.commit,
        v.date,
        APP_END - APP_START,
        used * 100 / (APP_END - APP_START)
    );
    eprintln!(
        "RAM: {} bytes of statics, {} bytes of stack, no allocator",
        ram.statics_size(),
        ram.stack_size()
    );
    Ok((out.join("knixx.uf2"), v))
}

/// Runs bolt with args in the repository root. bolt is built on its own,
/// with BLE, which xtask does without.
fn bolt(args: &[String]) -> Result<()> {
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["run", "--quiet", "--package", "bolt", "--"])
        .args(args)
        .current_dir(root())
        .status()
        .context("run bolt")?;
    if !status.success() {
        bail!("bolt {} failed", args[0]);
    }
    Ok(())
}

/// Builds the firmware and flashes it over USB, then checks that the board
/// reports the new version.
fn flash(args: &[String]) -> Result<()> {
    let port = match args {
        [] => None,
        [flag, p] if flag == "--port" => Some(p.clone()),
        _ => bail!("usage: cargo xtask flash [--port <serial port>]"),
    };
    let (uf2, v) = build()?;
    flash::uf2(flash::Options {
        file: &uf2,
        port,
        volume: flash::VOLUME,
        expect: Some(format!("version {} built {}", v.commit, v.date)),
        wait: Duration::from_secs(20),
    })
    .map_err(anyhow::Error::msg)
}

/// Builds once, then flashes n times. After each flash the board must
/// report the version over USB and advertise over BLE: every flash ends in
/// the bootloader jumping to the application without a reset, the path on
/// which USB and the BLE controller can hang (nrf-sdc issue #103).
fn cycle(args: &[String]) -> Result<()> {
    let n: u32 = match args {
        [n] => n.parse().context("cycle count")?,
        _ => bail!("usage: cargo xtask cycle <n>"),
    };
    let (uf2, v) = build()?;
    let expect = format!("version {} built {}", v.commit, v.date);
    for i in 1..=n {
        eprintln!("--- cycle {i} of {n}");
        flash::uf2(flash::Options {
            file: &uf2,
            port: None,
            volume: flash::VOLUME,
            expect: Some(expect.clone()),
            wait: Duration::from_secs(20),
        })
        .map_err(|e| anyhow::anyhow!("cycle {i}: usb: {e} ({} passed)", i - 1))?;
        let scan = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
            .args([
                "run",
                "--quiet",
                "--package",
                "bolt",
                "--",
                "scan",
                "--wait",
                "10",
            ])
            .current_dir(root())
            .status()
            .context("run bolt scan")?;
        if !scan.success() {
            bail!("cycle {i}: ble: no advertising ({} passed)", i - 1);
        }
    }
    eprintln!("all {n} cycles passed");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::utc;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(utc(1_793_050_000), "2026-10-26T21:26:40Z");
    }
}
