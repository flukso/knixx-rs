//! cargo xtask hil: the board tests, on a board on USB that the computer
//! also reaches over BLE.
//!
//! It builds and flashes the firmware (over USB, or SWD with --probe), then
//! goes through the transfers with bolt, the way a user would:
//! - a firmware push of the same build, signed again with a higher counter,
//!   then discarded;
//! - a script push of about 12 KiB of bytecode, and its pull, compared;
//! - a config pull, push and pull again, compared.
//!
//! It prints a table with the throughput of each push, end to end (bolt)
//! and on the flash (the board's own timing, when its firmware reports
//! one), and appends it to build/hil.log with the commit, so that runs
//! compare.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use knixx_host::flash;
use knixx_host::image::{Image, Stamp};
use knixx_host::sign;
use knixx_host::usbserial;

use crate::{ARTIFACT, Features, Version, root};

const USAGE: &str = "usage: cargo xtask hil [--port <serial port>] [--no-flash] [--probe [<GDB server port>]] [the build's flags]";

/// The name of the test script on the board.
const SCRIPT: &str = "hilpad";

/// How long the update window is opened for, in minutes.
const WINDOW_MINUTES: u32 = 15;

/// One push, as measured.
struct Rate {
    what: &'static str,
    bytes: usize,
    /// bolt's time, from the first ack to done.
    secs: f64,
    /// The board's own time in the flash, when it reports it.
    flash_secs: Option<f64>,
}

/// The outcome of a step.
struct Step {
    name: &'static str,
    result: Result<String>,
}

pub fn run(features: Features, args: &[String]) -> Result<()> {
    let mut port = None;
    let mut no_flash = false;
    let mut probe = None;
    let mut it = args.iter().peekable();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--port" => port = Some(it.next().context(USAGE)?.clone()),
            "--no-flash" => no_flash = true,
            "--probe" => {
                probe = Some(match it.peek() {
                    Some(p) if !p.starts_with("--") => it.next().unwrap().clone(),
                    _ => crate::PROBE.to_string(),
                })
            }
            _ => bail!(USAGE),
        }
    }

    // bolt runs as its own binary, so that its time is not cargo's.
    let status = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["build", "--quiet", "--package", "bolt"])
        .current_dir(root())
        .status()
        .context("build bolt")?;
    if !status.success() {
        bail!("bolt does not build");
    }

    if !no_flash {
        match &probe {
            Some(p) => crate::flash_probe(features, &["--probe".into(), p.clone()])?,
            None => crate::flash(
                features,
                &port
                    .iter()
                    .flat_map(|p| ["--port".to_string(), p.clone()])
                    .collect::<Vec<_>>(),
            )?,
        }
    }
    let port = match port {
        Some(p) => p,
        None => wait_app_port()?,
    };
    let version = console(&port, "version")?;
    eprintln!("hil: {} on {port}", version.trim());

    let dir = root().join("build/hil");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir)?;

    let mut steps = Vec::new();
    let mut rates = Vec::new();

    let opened = console(&port, &format!("upload window {WINDOW_MINUTES}"))?;
    if !opened.contains("window open") {
        bail!("the board did not open its update window: {}", opened.trim());
    }

    steps.push(Step {
        name: "firmware push",
        result: fw_push(&port, &dir, &mut rates),
    });
    steps.push(Step {
        name: "script push, pull",
        result: script(&port, &dir, &mut rates),
    });
    steps.push(Step {
        name: "config pull, push, pull",
        result: config(&dir),
    });

    let _ = console(&port, "upload window off");

    let table = report(version.trim(), &steps, &rates);
    eprint!("\n{table}");
    let log = root().join("build/hil.log");
    let mut f = fs::OpenOptions::new().create(true).append(true).open(&log)?;
    writeln!(f, "{table}")?;
    eprintln!("appended to {}", log.display());

    let failed = steps.iter().filter(|s| s.result.is_err()).count();
    if failed > 0 {
        bail!("{failed} of {} steps failed", steps.len());
    }
    Ok(())
}

/// Waits for the board on USB, after a flash.
fn wait_app_port() -> Result<String> {
    for _ in 0..40 {
        if let Ok(p) = flash::app_port("") {
            std::thread::sleep(Duration::from_millis(500));
            return Ok(p);
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    flash::app_port("connect the board, or name its port with --port").map_err(anyhow::Error::msg)
}

/// Runs cmd on the console and returns what it printed after its echo.
fn console(port: &str, cmd: &str) -> Result<String> {
    let out =
        usbserial::query(port, cmd, Duration::from_millis(1200)).map_err(|e| anyhow::anyhow!("{e:?}"))?;
    // The reply follows the prompt and the echo of the command; the boot
    // log before it may hold the command's words too.
    let echo = format!(">>> {cmd}");
    Ok(match out.rfind(&echo) {
        Some(i) => out[i + echo.len()..].trim_matches(['>', ' ', '\n']).to_string(),
        None => out,
    })
}

/// Runs bolt with args: its output goes on to the terminal, and is
/// returned too.
fn bolt(args: &[&str]) -> Result<String> {
    let exe = root().join("target/debug/bolt");
    let mut child = Command::new(&exe)
        .args(args)
        .current_dir(root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| exe.display().to_string())?;
    let stdout = child.stdout.take().unwrap();
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut BufReader::new(stdout), &mut s);
        s
    });
    // bolt's progress comes on stderr, one \r-line after another.
    let mut err = String::new();
    let mut r = BufReader::new(child.stderr.take().unwrap());
    let mut buf = Vec::new();
    while r.read_until(b'\r', &mut buf)? > 0 {
        let piece = String::from_utf8_lossy(&buf);
        eprint!("{piece}");
        err.push_str(&piece);
        buf.clear();
    }
    let status = child.wait()?;
    let all = out.join().unwrap_or_default() + &err;
    if !status.success() {
        bail!("bolt {} failed: {}", args.join(" "), last_line(&all));
    }
    Ok(all)
}

fn last_line(s: &str) -> &str {
    s.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// The time of bolt's closing line: "sent N KiB in T s (R KiB/s)".
fn bolt_secs(out: &str) -> Result<f64> {
    let line = out
        .split(['\r', '\n'])
        .rev()
        .find(|l| l.contains(" KiB in ") && l.contains("KiB/s)"))
        .context("no closing line from bolt")?;
    let t = line.split(" KiB in ").nth(1).and_then(|r| r.split(" s").next());
    t.and_then(|t| t.trim().parse().ok())
        .with_context(|| format!("bolt's line {line:?}"))
}

/// The board's own time for its last push in the flash, from the upload
/// command's "last:" line, when its firmware has one.
fn board_flash_secs(port: &str) -> Option<f64> {
    let out = console(port, "upload").ok()?;
    let line = out.lines().find(|l| l.contains("last:"))?;
    let i = line.find(" s in flash")?;
    line[..i].rsplit(", ").next()?.trim().parse().ok()
}

/// Pushes the build again, signed with a counter above the running one,
/// checks that it is staged, and discards it.
fn fw_push(port: &str, dir: &Path, rates: &mut Vec<Rate>) -> Result<String> {
    let elf = root().join(format!("build/{ARTIFACT}.elf"));
    let data = fs::read(&elf).with_context(|| elf.display().to_string())?;
    let img = Image::from_elf(&data)?;
    let key = sign::signing_key().map_err(anyhow::Error::msg)?;
    let v = Version::now();
    let secs = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let counter = secs as u32;
    let kfw = img.to_kfw(
        &Stamp {
            counter,
            version: &v.commit,
            built: secs,
        },
        &key,
    )?;
    let file = dir.join("hil.kfw");
    fs::write(&file, &kfw)?;

    let out = bolt(&["fw", "push", &file.display().to_string(), "--port", port]);
    let staged = console(port, "fw status");
    let _ = console(port, "fw discard");
    let out = out?;
    let staged = staged?;
    if !staged.contains(&format!("staged {} (counter {counter}", v.commit)) {
        bail!("not staged: {}", staged.trim());
    }
    rates.push(Rate {
        what: "firmware",
        bytes: kfw.len(),
        secs: bolt_secs(&out)?,
        flash_secs: board_flash_secs(port),
    });
    Ok(format!("staged counter {counter}, discarded"))
}

/// Builds a script of about 12 KiB of bytecode, pushes it, pulls it back
/// and compares, then removes it.
fn script(port: &str, dir: &Path, rates: &mut Vec<Rate>) -> Result<String> {
    let template = fs::read_to_string(root().join("crates/knixx-script/template.rhai"))?;
    let pad: String = (0..11_000u32)
        .map(|i| char::from(b'a' + (i * 7 % 26) as u8))
        .collect();
    let src = dir.join(format!("{SCRIPT}.rhai"));
    fs::write(&src, format!("{template}\nfn pad() {{\n    \"{pad}\"\n}}\n"))?;
    let rhgn = dir.join(format!("{SCRIPT}.rhgn"));
    bolt(&[
        "script",
        "build",
        &src.display().to_string(),
        "--output",
        &rhgn.display().to_string(),
    ])?;
    let bytes = fs::read(&rhgn)?;

    let r = (|| {
        let out = bolt(&[
            "script",
            "push",
            &rhgn.display().to_string(),
            "--name",
            SCRIPT,
            "--always",
            "--port",
            port,
        ])?;
        let secs = bolt_secs(&out)?;
        let flash_secs = board_flash_secs(port);
        let ls = console(port, "script ls")?;
        if !ls.contains(SCRIPT) {
            bail!("not on the board: {}", ls.trim());
        }
        let pulled = dir.join("pulled");
        bolt(&[
            "script",
            "pull",
            SCRIPT,
            "--out",
            &pulled.display().to_string(),
            "--port",
            port,
        ])?;
        let back = find(&pulled, &format!("{SCRIPT}.rhgn"))?;
        if fs::read(&back)? != bytes {
            bail!("the pulled script differs from the pushed one");
        }
        rates.push(Rate {
            what: "script",
            bytes: bytes.len(),
            secs,
            flash_secs,
        });
        Ok(format!("{} bytes pushed and pulled back the same", bytes.len()))
    })();
    let _ = console(port, &format!("script rm {SCRIPT}"));
    r
}

/// Pulls the config, pushes it back, and pulls it again: both pulls must
/// be the same bytes.
fn config(dir: &Path) -> Result<String> {
    let a = dir.join("config-a");
    let b = dir.join("config-b");
    bolt(&["config", "pull", "--out", &a.display().to_string()])?;
    let first = find(&a, "knixx.ini")?;
    bolt(&["config", "push", &first.display().to_string()])?;
    bolt(&["config", "pull", "--out", &b.display().to_string()])?;
    let second = find(&b, "knixx.ini")?;
    let (x, y) = (fs::read(&first)?, fs::read(&second)?);
    if x != y {
        bail!("the second pull differs from the first");
    }
    Ok(format!("{} bytes, the same after the round trip", x.len()))
}

/// Finds the file named name under dir, which bolt puts in a folder per
/// board.
fn find(dir: &Path, name: &str) -> Result<PathBuf> {
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in fs::read_dir(&d).with_context(|| d.display().to_string())? {
            let p = e?.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == name) {
                return Ok(p);
            }
        }
    }
    bail!("no {name} under {}", dir.display())
}

/// The table of the steps and the rates.
fn report(version: &str, steps: &[Step], rates: &[Rate]) -> String {
    let mut t = String::new();
    let when = knixx_host::time::utc(SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs());
    let _ = writeln!(t, "hil {when}: {version}");
    for s in steps {
        match &s.result {
            Ok(what) => {
                let _ = writeln!(t, "  pass  {:<24} {what}", s.name);
            }
            Err(e) => {
                let _ = writeln!(t, "  FAIL  {:<24} {e:#}", s.name);
            }
        }
    }
    if !rates.is_empty() {
        let _ = writeln!(
            t,
            "  {:<10} {:>8} {:>8} {:>10} {:>10} {:>10}",
            "push", "bytes", "s", "KiB/s", "flash s", "flash KiB/s"
        );
        for r in rates {
            let kib = r.bytes as f64 / 1024.0;
            let (fs_, frate) = match r.flash_secs {
                Some(f) if f > 0.0 => (format!("{f:.1}"), format!("{:.1}", kib / f)),
                _ => ("-".into(), "-".into()),
            };
            let _ = writeln!(
                t,
                "  {:<10} {:>8} {:>8.1} {:>10.1} {:>10} {:>10}",
                r.what,
                r.bytes,
                r.secs,
                kib / r.secs,
                fs_,
                frate
            );
        }
    }
    t
}
