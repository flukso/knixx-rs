//! cargo xtask <task>: the build and flash tasks of knixx-rs.
//!
//!     build        build the bootloader and the firmware, every feature in,
//!                  and write build/thunder.{elf,kfw,hex} and
//!                  build/thunder-boot.{elf,hex}
//!     flash        build, flash over USB through the bootloader's recovery
//!                  and check the board runs the build
//!     flash-probe  build, write the bootloader and the firmware over SWD
//!     fw           build, open the update window over the console, push the
//!                  image over BLE and install it, with bolt fw push
//!     term         a terminal on the board's console with bolt term
//!     cycle        build, then flash n times, checking USB and BLE after each
//!     hil          build, flash, then push and pull firmware, a script and
//!                  the config through bolt, and measure the throughput
//!     golden       record the VL53L7CX golden log with ST's C driver
//!     genfw        extract the VL53L7CX firmware from ST's vl53l7cx_buffers.h
//!     cad          the renders and the front plate, with the Python tools in cad/
//!
//! The firmware and the bootloader are built in firmware/, a workspace of
//! its own for the nRF52840, stamped with the git commit and the build
//! time, and with the public keys they take signed images from.

mod cad;
mod hil;

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use knixx_host::flash;
use knixx_host::image::{Image, Ram, Stamp};
use knixx_host::sign;
use knixx_host::time::utc;
use knixx_image as kimage;

/// The name of the firmware's files in build/: thunder.kfw and the others,
/// and thunder-boot.* for the bootloader.
const ARTIFACT: &str = "thunder";

/// The default serial port of the SWD probe, a GDB server.
const PROBE: &str = "/dev/cu.usbmodemB5C8A6F11";

const USAGE: &str = "usage: cargo xtask <task>

tasks:
  build   build the bootloader and the firmware, every feature in, and
          write build/thunder.{elf,kfw,hex} and build/thunder-boot.{elf,hex};
          flags leave features out: [--no-scripts: without Rhai Grain and
          its heap, so that no allocator is linked] [--no-web-ble: without
          the knixx GATT service for the web app] [--no-coded-phy: without
          advertising and connections on the LE Coded PHY]. The image is
          signed with $KNIXX_SIGNING_KEY, else the developer's key
          (~/.config/knixx/dev-signing.key, made on first use)
  flash   build, flash over USB through the bootloader's recovery and check
          the board runs the build [--port <serial port>] [--force: take a
          counter not above the installed one] [the build's flags]
  flash-probe
          build, then write the bootloader and the firmware over SWD with
          arm-none-eabi-gdb [--probe <GDB server port>, default
          /dev/cu.usbmodemB5C8A6F11] [the build's flags]
  fw      build, then open the board's update window over its console,
          push the image over BLE and install it: bolt fw push --open 10
          --install [--port, --ble-name, --addr: bolt's flags] [the build's
          flags]
  term    a terminal on the console, over USB or BLE [bolt term flags]
  golden  record crates/knixx-vl53l7cx/testdata/configure.golden with ST's
          C driver and the harness; --check only compares [$CC, default cc]
  genfw   extract crates/knixx-vl53l7cx/firmware/*.bin from ST's
          vl53l7cx_buffers.h; --check only compares
  cad     render|frontplate|readme|clean: the renders and the front plate
          into build/cad/ [--step <model.step>, $PY]
  hil     build and flash, then the board tests through bolt: a firmware
          push (signed again with a higher counter, then discarded), a
          script push and pull, a config pull, push and pull; prints the
          throughput of each push and appends it to build/hil.log
          [--port <serial port>] [--no-flash] [--probe [<GDB server port>]:
          flash over SWD] [the build's flags]
  cycle   build, then flash <n> times; after each flash the board must
          report its version over USB and advertise as knixx over BLE
  web     serve the web app on localhost with trunk, rebuilt on changes;
          --build only builds it into web/dist/ [trunk flags]
  keygen  <number> <name> <seed file>: make a signing key; its seed goes
          to <seed file> (keep it secret, e.g. as the release workflow's
          KNIXX_SIGNING_KEY), its public key to keys/<number>-<name>.pub
";

/// The firmware features a build takes from its flags.
#[derive(Clone, Copy)]
struct Features {
    scripts: bool,
    web_ble: bool,
    coded_phy: bool,
}

impl Features {
    /// Takes --no-scripts, --no-web-ble and --no-coded-phy out of args:
    /// every feature is in unless a flag leaves it out. The flags that
    /// once added features are refused, as they are in already.
    fn take(args: &[String]) -> Result<(Features, Vec<String>)> {
        let mut f = Features {
            scripts: true,
            web_ble: true,
            coded_phy: true,
        };
        let mut rest = Vec::new();
        for a in args {
            match a.as_str() {
                "--no-scripts" => f.scripts = false,
                "--no-web-ble" => f.web_ble = false,
                "--no-coded-phy" => f.coded_phy = false,
                "--web-ble" | "--coded-phy" => bail!(
                    "{} is built in by default; --no-{} leaves it out",
                    &a[2..],
                    &a[2..]
                ),
                _ => rest.push(a.clone()),
            }
        }
        Ok((f, rest))
    }

    /// Whether every feature is in, the default build.
    fn all(self) -> bool {
        self.scripts && self.web_ble && self.coded_phy
    }
}

fn main() -> Result<()> {
    let args: Vec<String> = env::args().skip(1).collect();
    let (features, rest) = Features::take(args.get(1..).unwrap_or(&[]))?;
    match args.first().map(String::as_str) {
        Some("build") => {
            if !rest.is_empty() {
                bail!("usage: cargo xtask build [--no-scripts] [--no-web-ble] [--no-coded-phy]");
            }
            build_with(features).map(|_| ())
        }
        Some("flash") => flash(features, &rest),
        Some("flash-probe") => flash_probe(features, &rest),
        Some("cycle") => cycle(features, &rest),
        Some("hil") => hil::run(features, &rest),
        Some("golden") => golden(&args[1..]),
        Some("genfw") => genfw(&args[1..]),
        Some("cad") => cad::run(&args[1..]),
        Some("fw") => {
            let b = build_with(features)?;
            let mut a = vec![
                "fw".to_string(),
                "push".into(),
                b.kfw.display().to_string(),
                "--open".into(),
                "10".into(),
                "--install".into(),
            ];
            a.extend_from_slice(&rest);
            bolt(&a)
        }
        Some("web") => web(&rest),
        Some("keygen") => keygen(&args[1..]),
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
    /// The image's version counter: the commit's time, or the build's for
    /// a build with changes, so that it goes up from build to build.
    counter: u32,
    /// The build time in seconds since 1970.
    secs: u64,
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
        let dirty = git(&["diff", "--quiet", "HEAD"]).is_none() && commit != "unknown";
        if dirty {
            commit.push_str("-dirty");
        }
        let secs = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let committed = git(&["log", "-1", "--format=%ct"])
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse::<u64>().ok());
        let counter = match committed {
            Some(t) if !dirty => t,
            _ => secs,
        } as u32;
        Version {
            commit,
            date: utc(secs),
            counter,
            secs,
        }
    }
}

/// What a build made.
struct Built {
    kfw: PathBuf,
    v: Version,
}

/// Runs cargo build in firmware/ with args, stamped with v and the keys.
fn cargo_firmware(args: &[&str], v: &Version, keys: &str) -> Result<()> {
    let status = Command::new(env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .arg("build")
        .args(args)
        .current_dir(root().join("firmware"))
        .env("KNIXX_COMMIT", &v.commit)
        .env("KNIXX_BUILD_DATE", &v.date)
        .env("KNIXX_KEYS", keys)
        .status()
        .context("run cargo build in firmware/")?;
    if !status.success() {
        bail!("cargo build {} failed", args.join(" "));
    }
    Ok(())
}

/// Builds the bootloader and the firmware. It writes build/thunder.elf, the
/// signed build/thunder.kfw and build/thunder.hex (the header and the body
/// as they lie in flash, for the probe), and build/thunder-boot.elf and
/// .hex. It fails when an image does not fit its flash, and on the heap
/// rule: with scripts the allocator is the heap of Rhai Grain; without them
/// no allocator may be linked.
fn build_with(f: Features) -> Result<Built> {
    let scripts = f.scripts;
    let v = Version::now();
    let key = sign::signing_key().map_err(anyhow::Error::msg)?;
    let keys = sign::keys_env(&root(), &key).map_err(anyhow::Error::msg)?;

    let mut args = vec!["--release", "-p", "knixx-fw"];
    let features: Vec<&str> = [
        (f.scripts, "scripts"),
        (f.web_ble, "web-ble"),
        (f.coded_phy, "coded-phy"),
    ]
    .into_iter()
    .filter_map(|(on, name)| on.then_some(name))
    .collect();
    let joined = features.join(",");
    if !f.all() {
        args.push("--no-default-features");
        if !features.is_empty() {
            args.extend(["--features", &joined]);
        }
    }
    cargo_firmware(&args, &v, &keys)?;
    cargo_firmware(&["--profile", "boot", "-p", "knixx-boot"], &v, &keys)?;

    let target = root().join("firmware/target/thumbv7em-none-eabihf");
    let elf = target.join("release/knixx-fw");
    let data = fs::read(&elf).with_context(|| elf.display().to_string())?;
    let img = Image::from_elf(&data)?;
    let ram = Ram::from_elf(&data)?;
    if !scripts && ram.allocator {
        bail!("the firmware links a global allocator without scripts: something allocates on the heap");
    }
    if scripts && ram.heap == 0 {
        bail!("the firmware with scripts lacks its heap, KNIXX_HEAP");
    }
    let stamp = Stamp {
        counter: v.counter,
        version: &v.commit,
        built: v.secs,
    };
    let kfw = img.to_kfw(&stamp, &key)?;
    let (flash_img, _) = Image::from_kfw(&kfw)?;

    let boot_elf = target.join("boot/knixx-boot");
    let boot_data = fs::read(&boot_elf).with_context(|| boot_elf.display().to_string())?;
    let boot = Image::from_elf(&boot_data)?;
    if boot.start() != kimage::BOOT_START || boot.end() > kimage::BOOT_START + kimage::BOOT_SIZE {
        bail!(
            "the bootloader {:#x}..{:#x} is outside its flash {:#x}..{:#x}",
            boot.start(),
            boot.end(),
            kimage::BOOT_START,
            kimage::BOOT_START + kimage::BOOT_SIZE
        );
    }

    let out = root().join("build");
    fs::create_dir_all(&out)?;
    let kfw_file = out.join(format!("{ARTIFACT}.kfw"));
    fs::write(out.join(format!("{ARTIFACT}.elf")), &data)?;
    fs::write(&kfw_file, &kfw)?;
    fs::write(out.join(format!("{ARTIFACT}.hex")), flash_img.to_hex())?;
    fs::write(out.join(format!("{ARTIFACT}-boot.elf")), &boot_data)?;
    fs::write(out.join(format!("{ARTIFACT}-boot.hex")), boot.to_hex())?;

    let used = img.end() - kimage::APP_START;
    let app_size = kimage::MAX_BODY;
    eprintln!(
        "build/{ARTIFACT}.kfw: version {} built {}, counter {}, signed with key {}; {used} of {app_size} bytes of flash ({}%)",
        v.commit,
        v.date,
        v.counter,
        key.id,
        used * 100 / app_size
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
    let boot_used = boot.end() - kimage::BOOT_START;
    eprintln!(
        "build/{ARTIFACT}-boot.hex: {boot_used} of {} bytes of flash ({}%)",
        kimage::BOOT_SIZE,
        boot_used * 100 / kimage::BOOT_SIZE
    );
    Ok(Built { kfw: kfw_file, v })
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
/// Serves the web app with trunk, or builds it with --build. The cargo of
/// web/rust-toolchain.toml builds it: CARGO and RUSTUP_TOOLCHAIN, set for
/// xtask, are left out.
fn web(args: &[String]) -> Result<()> {
    let (cmd, rest) = match args.split_first() {
        Some((b, rest)) if b == "--build" => (["build", "--release"].as_slice(), rest),
        _ => (["serve", "--open"].as_slice(), args),
    };
    let status = Command::new("trunk")
        .args(cmd)
        .args(rest)
        .current_dir(root().join("web"))
        .env_remove("CARGO")
        .env_remove("RUSTUP_TOOLCHAIN")
        .status()
        .context("run trunk (install it: cargo install trunk --locked, or brew install trunk)")?;
    if !status.success() {
        bail!("trunk {} failed", cmd[0]);
    }
    Ok(())
}

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
fn flash(features: Features, args: &[String]) -> Result<()> {
    let mut port = None;
    let mut force = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = Some(it.next().context("--port <serial port>")?.clone()),
            "--force" => force = true,
            _ => bail!("usage: cargo xtask flash [--port <serial port>] [--force]"),
        }
    }
    let b = build_with(features)?;
    flash::kfw(flash::Options {
        file: &b.kfw,
        port,
        force,
        expect: Some(format!("version {} built {}", b.v.commit, b.v.date)),
        wait: Duration::from_secs(20),
    })
    .map_err(anyhow::Error::msg)
}

/// Builds the bootloader and the firmware and writes both over SWD, with
/// arm-none-eabi-gdb and a probe that is a GDB server on a serial port. The
/// QSPI flash, with the configuration, the scripts and the bonds, stays as
/// it is.
///
/// The probe does not always erase a page before it writes it, so the pages
/// are erased through the NVMC first, and both images are read back and
/// compared with the build, and written again if they differ, up to
/// PROBE_TRIES times. Only then is the board reset.
fn flash_probe(features: Features, args: &[String]) -> Result<()> {
    let probe = match args {
        [] => PROBE.to_string(),
        [flag, p] if flag == "--probe" => p.clone(),
        _ => bail!("usage: cargo xtask flash-probe [--probe <GDB server port>]"),
    };
    let b = build_with(features)?;
    let out = root().join("build");
    let boot_elf = out.join(format!("{ARTIFACT}-boot.elf"));
    let boot = Image::from_elf(&fs::read(&boot_elf)?)?;
    let boot_bin = boot.to_bin();
    let kfw = fs::read(&b.kfw)?;
    let app_hex = out.join(format!("{ARTIFACT}.hex"));
    let got_boot = out.join("probe-boot.bin");
    let got_app = out.join("probe-app.bin");
    // The probe does not erase reliably before it writes, so the pages of
    // both images are erased through the NVMC first.
    let mut script = String::from(
        "set mem inaccessible-by-default off\n\
         # Reset and halt before the bootloader runs: a running board has\n\
         # its bootloader write-protected by ACL until a reset.\n\
         set {int}0xE000EDFC = 0x01000001\n\
         set {int}0xE000ED0C = 0x05FA0004\n\
         set {int}0xE000EDFC = 0\n\
         set {int}0x4001E504 = 2\n",
    );
    let page = kimage::FLASH_PAGE;
    let ranges = [
        (boot.start(), boot.end()),
        (kimage::HEADER_ADDR, kimage::HEADER_ADDR + kfw.len() as u32),
    ];
    for (start, end) in ranges {
        let mut a = start & !(page - 1);
        while a < end {
            script.push_str(&format!(
                "set {{int}}0x4001E508 = {a:#x}\nwhile {{int}}0x4001E400 == 0\nend\n"
            ));
            a += page;
        }
    }
    script.push_str("set {int}0x4001E504 = 0\n");
    script.push_str(&format!(
        "load {}\nload {}\n",
        boot_elf.display(),
        app_hex.display()
    ));
    script.push_str(&format!(
        "dump binary memory {} {:#x} {:#x}\ndump binary memory {} {:#x} {:#x}\n",
        got_boot.display(),
        boot.start(),
        boot.end(),
        got_app.display(),
        ranges[1].0,
        ranges[1].1
    ));
    let script_file = out.join("flash-probe.gdb");
    fs::write(&script_file, script)?;
    for attempt in 1..=PROBE_TRIES {
        gdb(&probe, &[format!("source {}", script_file.display())])?;
        let bad: Vec<String> = [
            (boot.start(), &boot_bin, &got_boot),
            (kimage::HEADER_ADDR, &kfw, &got_app),
        ]
        .into_iter()
        .flat_map(|(base, want, file)| {
            let got = fs::read(file).unwrap_or_default();
            bad_pages(base, want, &got)
        })
        .map(|p| format!("{p:#x}"))
        .collect();
        if bad.is_empty() {
            gdb(&probe, &["kill".into()])?;
            eprintln!(
                "wrote and read back the bootloader and the firmware {} over SWD; the board resets",
                b.v.commit
            );
            return Ok(());
        }
        eprintln!(
            "attempt {attempt}: pages {} read back wrong; writing again",
            bad.join(", ")
        );
    }
    bail!("the probe did not write the images right in {PROBE_TRIES} attempts")
}

/// How often flash-probe writes the images before it gives up.
const PROBE_TRIES: u32 = 3;

/// The 4 KiB pages, by address from base, where got differs from want.
fn bad_pages(base: u32, want: &[u8], got: &[u8]) -> Vec<u32> {
    let mut pages: Vec<u32> = (0..want.len())
        .filter(|&i| got.get(i) != Some(&want[i]))
        .map(|i| (base + i as u32) & !(kimage::FLASH_PAGE - 1))
        .collect();
    pages.dedup();
    pages
}

/// Runs arm-none-eabi-gdb on the probe: attaches to the target and runs the
/// commands.
fn gdb(probe: &str, cmds: &[String]) -> Result<()> {
    let mut c = Command::new("arm-none-eabi-gdb");
    c.args(["-nx", "--batch"])
        .args(["-ex", &format!("target extended-remote {probe}")])
        .args(["-ex", "monitor swdp_scan"])
        .args(["-ex", "attach 1"]);
    for cmd in cmds {
        c.args(["-ex", cmd]);
    }
    let o = c.output().context("run arm-none-eabi-gdb")?;
    let text = String::from_utf8_lossy(&o.stdout).into_owned() + &String::from_utf8_lossy(&o.stderr);
    if !o.status.success() || text.contains("Error") {
        bail!("arm-none-eabi-gdb failed:\n{text}");
    }
    Ok(())
}

/// Makes a signing key: the seed to a file of its own, readable by its
/// owner only, the public key to keys/, for the builds to take.
fn keygen(args: &[String]) -> Result<()> {
    let [id, name, seed_file] = args else {
        bail!("usage: cargo xtask keygen <number> <name> <seed file>");
    };
    let id: u8 = id.parse().context("the key number")?;
    if id == sign::DEV_ID {
        bail!("key 0 is the developer's, made on first build");
    }
    let pub_file = root().join(format!("keys/{id}-{name}.pub"));
    if pub_file.exists() {
        bail!("{} exists", pub_file.display());
    }
    let seed = sign::random_seed().map_err(anyhow::Error::msg)?;
    sign::write_private(Path::new(seed_file), &format!("{}\n", sign::hex(&seed)))
        .map_err(anyhow::Error::msg)?;
    fs::create_dir_all(root().join("keys"))?;
    fs::write(&pub_file, format!("{}\n", sign::hex(&sign::public_key(&seed))))?;
    eprintln!(
        "key {id}: the seed in {seed_file} (keep it secret, store it as KNIXX_SIGNING_KEY), the public key in keys/{id}-{name}.pub (commit it)"
    );
    Ok(())
}

/// Builds once, then flashes n times. After each flash the board must
/// report the version over USB and advertise over BLE: every flash ends in
/// the bootloader booting the application, the path on which USB and the
/// BLE controller can hang.
fn cycle(features: Features, args: &[String]) -> Result<()> {
    let n: u32 = match args {
        [n] => n.parse().context("cycle count")?,
        _ => bail!("usage: cargo xtask cycle <n>"),
    };
    let b = build_with(features)?;
    let expect = format!("version {} built {}", b.v.commit, b.v.date);
    for i in 1..=n {
        eprintln!("--- cycle {i} of {n}");
        flash::kfw(flash::Options {
            file: &b.kfw,
            port: None,
            // The same image each time.
            force: true,
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
    use super::{bad_pages, c_arrays, hex_bytes};

    #[test]
    fn pages_that_differ() {
        let want = vec![1u8; 9000];
        let mut got = want.clone();
        got[5000] = 0;
        got.truncate(8500);
        assert_eq!(bad_pages(0x10000, &want, &got), vec![0x11000, 0x12000]);
        assert!(bad_pages(0, &want, &want).is_empty());
    }

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
