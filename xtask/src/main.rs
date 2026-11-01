//! cargo xtask <task>: the build and flash tasks of knixx-rs.
//!
//!     build   build the firmware and write build/knixx.{elf,uf2,hex} and
//!             the DFU package build/knixx-dfu.zip
//!     flash   build, flash over USB and check the board runs the build
//!     ota     build, then send the DFU package over BLE with bolt ota
//!     term    a terminal on the board's console with bolt term
//!     cycle   build, then flash n times, checking USB and BLE after each
//!     golden  record the VL53L7CX golden log with ST's C driver
//!     genfw   extract the VL53L7CX firmware from ST's vl53l7cx_buffers.h
//!     cad     the renders and the front plate, with the Python tools in cad/
//!
//! The firmware is built in firmware/, a workspace of its own for the
//! nRF52840, stamped with the git commit and the build time.

mod cad;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use knixx_host::image::{Image, Ram};
use knixx_host::legacydfu::{InitPacket, Package, read_hex};
use knixx_host::time::utc;
use knixx_host::{flash, uf2};

/// Where the application may go: from the end of the S140 6.1.1 SoftDevice
/// to the start of the user data that precedes the bootloader.
const APP_START: u32 = 0x26000;
const APP_END: u32 = 0xED000;

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  build   build the firmware and write build/knixx.{elf,uf2,hex} and the
          DFU package build/knixx-dfu.zip [--no-scripts: without Rhai
          Grain and its heap, so that no allocator is linked]
  flash   build, flash over USB and check the board runs the build
          [--port <serial port>]
  ota     build, then send build/knixx-dfu.zip over BLE to a board in BLE
          DFU mode (run ota on its console) [bolt ota flags, e.g. --addr]
  term    a terminal on the console, over USB or BLE [bolt term flags]
  golden  record crates/knixx-vl53l7cx/testdata/configure.golden with ST's
          C driver and the harness; --check only compares [$CC, default cc]
  genfw   extract crates/knixx-vl53l7cx/firmware/*.bin from ST's
          vl53l7cx_buffers.h; --check only compares
  cad     render|frontplate|readme|clean: the renders and the front plate
          into build/cad/ [--step <model.step>, $PY]
  cycle   build, then flash <n> times; after each flash the board must
          report its version over USB and advertise as knixx over BLE
";

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("build") => {
            let scripts = match &args[1..] {
                [] => true,
                [flag] if flag == "--no-scripts" => false,
                _ => bail!("usage: cargo xtask build [--no-scripts]"),
            };
            build_with(scripts).map(|_| ())
        }
        Some("flash") => flash(&args[1..]),
        Some("cycle") => cycle(&args[1..]),
        Some("golden") => golden(&args[1..]),
        Some("genfw") => genfw(&args[1..]),
        Some("cad") => cad::run(&args[1..]),
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

/// Builds the firmware, with scripts.
fn build() -> Result<(PathBuf, Version)> {
    build_with(true)
}

/// Builds the firmware and writes build/knixx.elf, .uf2 and .hex. It fails
/// when the image does not fit the application's flash, and on the heap
/// rule: with scripts the allocator is the heap of Rhai Grain; without
/// them no allocator may be linked.
fn build_with(scripts: bool) -> Result<(PathBuf, Version)> {
    let v = Version::now();
    let mut cargo = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cargo.args(["build", "--release"]);
    if !scripts {
        cargo.arg("--no-default-features");
    }
    let status = cargo
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
    if !scripts && ram.allocator {
        bail!("the firmware links a global allocator without scripts: something allocates on the heap");
    }
    if scripts && ram.heap == 0 {
        bail!("the firmware with scripts lacks its heap, KNIXX_HEAP");
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
        "RAM: {} bytes of statics, {} bytes of stack, {}",
        ram.statics_size(),
        ram.stack_size(),
        if scripts {
            format!("a heap of {} bytes for Rhai Grain in the statics", ram.heap)
        } else {
            "no allocator".to_string()
        }
    );
    Ok((out.join("knixx.uf2"), v))
}

/// Builds ST's VL53L7CX driver with the harness for the host, runs it
/// against its fake sensor and writes its log of I2C transactions to
/// testdata/configure.golden, which the Rust port's golden test replays.
/// With --check it only compares the log with the file.
fn golden(args: &[String]) -> Result<()> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: cargo xtask golden [--check]"),
    };
    let dir = root().join("crates/knixx-vl53l7cx");
    let uld = "VL53L7CX_ULD_driver_2.0.1/VL53L7CX_ULD_API";
    let out = root().join("target/vl53l7cx-harness");
    fs::create_dir_all(root().join("target"))?;
    let cc = env::var("CC").unwrap_or_else(|_| "cc".into());
    let status = Command::new(&cc)
        .args(["-std=c99", "-O1", "-Itestdata/harness"])
        .arg(format!("-I{uld}/inc"))
        .arg("-o")
        .arg(&out)
        .arg("testdata/harness/harness.c")
        .args(
            [
                "vl53l7cx_api.c",
                "vl53l7cx_plugin_motion_indicator.c",
                "vl53l7cx_plugin_detection_thresholds.c",
            ]
            .map(|f| format!("{uld}/src/{f}")),
        )
        .arg("-lm")
        .current_dir(&dir)
        .status()
        .with_context(|| format!("run {cc}"))?;
    if !status.success() {
        bail!("the harness does not build");
    }
    let run = Command::new(&out).output().context("run the harness")?;
    if !run.status.success() {
        bail!("the harness failed: {}", String::from_utf8_lossy(&run.stderr));
    }
    let path = dir.join("testdata/configure.golden");
    let old = fs::read(&path).unwrap_or_default();
    let name = "crates/knixx-vl53l7cx/testdata/configure.golden";
    if check {
        if old != run.stdout {
            bail!("{name} differs from the log of the C driver; cargo xtask golden records it");
        }
        eprintln!("{name}: matches the C driver");
        return Ok(());
    }
    fs::write(&path, &run.stdout)?;
    eprintln!(
        "{name}: {} lines, {}",
        run.stdout.iter().filter(|&&b| b == b'\n').count(),
        if old == run.stdout { "unchanged" } else { "changed" }
    );
    Ok(())
}

/// The arrays of ST's vl53l7cx_buffers.h and the files of
/// crates/knixx-vl53l7cx/firmware/ they go to.
const FW_ARRAYS: [(&str, &str); 4] = [
    ("VL53L7CX_FIRMWARE", "firmware.bin"),
    ("VL53L7CX_DEFAULT_CONFIGURATION", "default_configuration.bin"),
    ("VL53L7CX_DEFAULT_XTALK", "default_xtalk.bin"),
    ("VL53L7CX_GET_NVM_CMD", "get_nvm_cmd.bin"),
];

/// The value of VL53L7CX_FW_NBTAR_RANGING, which the default configuration
/// uses by name, for VL53L7CX_NB_TARGET_PER_ZONE = 1.
const NBTAR_RANGING: u8 = 0x02;

/// Extracts the byte arrays of ST's vl53l7cx_buffers.h into the binary files
/// that knixx-vl53l7cx embeds. With --check it only compares them with the
/// files.
fn genfw(args: &[String]) -> Result<()> {
    let check = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: cargo xtask genfw [--check]"),
    };
    let dir = root().join("crates/knixx-vl53l7cx");
    let header = dir.join("VL53L7CX_ULD_driver_2.0.1/VL53L7CX_ULD_API/inc/vl53l7cx_buffers.h");
    let text = fs::read_to_string(&header).with_context(|| header.display().to_string())?;
    let arrays = c_arrays(&text);
    let mut differ = Vec::new();
    for (array, file) in FW_ARRAYS {
        let data = arrays
            .iter()
            .find(|(n, _)| n == array)
            .map(|(_, d)| d)
            .with_context(|| format!("{array} not found in {}", header.display()))?;
        let path = dir.join("firmware").join(file);
        let same = fs::read(&path).is_ok_and(|old| old == *data);
        if check {
            if !same {
                differ.push(file);
            }
            continue;
        }
        fs::write(&path, data)?;
        eprintln!(
            "crates/knixx-vl53l7cx/firmware/{file}: {} bytes, {}",
            data.len(),
            if same { "unchanged" } else { "changed" }
        );
    }
    if !differ.is_empty() {
        bail!(
            "crates/knixx-vl53l7cx/firmware/{{{}}} differ from vl53l7cx_buffers.h; cargo xtask genfw extracts them",
            differ.join(",")
        );
    }
    if check {
        eprintln!("crates/knixx-vl53l7cx/firmware: matches vl53l7cx_buffers.h");
    }
    Ok(())
}

/// Returns the `const uint8_t NAME[] = { ... };` arrays of a C header with
/// their bytes: the hex values of one or two digits on the lines after the
/// opening line, up to the line that ends with `};`, with
/// VL53L7CX_FW_NBTAR_RANGING taken as its value.
fn c_arrays(text: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let mut cur: Option<(String, Vec<u8>)> = None;
    for line in text.lines() {
        let Some((_, data)) = cur.as_mut() else {
            cur = array_start(line).map(|n| (n.to_string(), Vec::new()));
            continue;
        };
        let t = line.trim();
        let (body, end) = match t.strip_suffix("};") {
            Some(b) => (b, true),
            None => (t, false),
        };
        let body = body.replace("VL53L7CX_FW_NBTAR_RANGING", &format!("{NBTAR_RANGING:#04x}"));
        data.extend(hex_bytes(&body));
        if end {
            out.extend(cur.take());
        }
    }
    out
}

/// Returns NAME of a line that opens `const uint8_t NAME[] = {`.
fn array_start(line: &str) -> Option<&str> {
    let rest = line.strip_prefix("const uint8_t ")?;
    let n = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))?;
    (n > 0 && rest[n..].starts_with("[] = {")).then(|| &rest[..n])
}

/// Returns the values of the hex literals of one or two digits in s, such as
/// 0x2 and 0xA0; a literal with more digits is not one.
fn hex_bytes(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let word = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let mut out = Vec::new();
    let mut i = 0;
    while i + 2 < b.len() {
        if b[i] == b'0' && (b[i + 1] | 0x20) == b'x' {
            let digits = b[i + 2..].iter().take_while(|c| c.is_ascii_hexdigit()).count();
            if (1..=2).contains(&digits) && !b.get(i + 2 + digits).is_some_and(|&c| word(c)) {
                let v = &s[i + 2..i + 2 + digits];
                out.push(u8::from_str_radix(v, 16).unwrap());
                i += 2 + digits;
                continue;
            }
        }
        i += 1;
    }
    out
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
    use super::{c_arrays, hex_bytes};

    #[test]
    fn hex_literals() {
        assert_eq!(hex_bytes("0x00, 0xff,0X1a, 0x2,"), [0x00, 0xff, 0x1a, 0x02]);
        // Not bytes: more digits, or part of a longer word.
        assert_eq!(hex_bytes("0x123, 0x12u, 0xg1, 0x"), Vec::<u8>::new());
    }

    #[test]
    fn arrays() {
        let h = "/* x */\nconst uint8_t A[] = {\n\t0x01, 0x02,\n\t0x03, VL53L7CX_FW_NBTAR_RANGING,\n};\n\
                 static const uint8_t B[] = {\n0x09,\n};\nconst uint8_t C[] = {\n0xab };\n";
        assert_eq!(
            c_arrays(h),
            [("A".to_string(), vec![1, 2, 3, 2]), ("C".to_string(), vec![0xab])]
        );
    }
}
