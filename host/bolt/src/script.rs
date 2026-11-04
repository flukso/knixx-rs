//! bolt script: makes, builds and uploads the scripts of the board, and
//! lists and removes the ones on it.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use knixx_console::upload::{CHUNK, crc32, valid_name};
use knixx_host::time::utc_now;
use knixx_script::{Meta, compile, lines};
use md5::{Digest, Md5};

use crate::client::{Client, Target};

/// The template of `bolt script new`.
const TEMPLATE: &str = include_str!("../../../crates/knixx-script/template.rhai");

/// The extension of a built script.
const EXT: &str = "rhgn";

/// Writes the script name.rhai from the template, with its name, the git
/// user as its author and today as its date.
pub fn new(name: &str) -> Result<()> {
    if !valid_name(name) {
        bail!("{name:?}: a name is 1 to 24 letters, digits, - and _");
    }
    let path = PathBuf::from(format!("{name}.rhai"));
    if path.exists() {
        bail!("{} exists already", path.display());
    }
    let author = git(Path::new("."), &["config", "user.name"]).unwrap_or_default();
    let src = TEMPLATE
        .replacen("name: \"example\"", &format!("name: {name:?}"), 1)
        .replacen("author: \"\"", &format!("author: {author:?}"), 1)
        .replacen("date: \"\"", &format!("date: {:?}", &utc_now()[..10]), 1);
    fs::write(&path, src).with_context(|| path.display().to_string())?;
    eprintln!(
        "{}: fill in on_info, then: bolt script upload {}",
        path.display(),
        path.display()
    );
    Ok(())
}

/// Runs git in dir and returns its output, trimmed; None when it fails.
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The version of the script at path: the short commit hash of its git
/// repository, with -dirty when the file differs from the commit or is not
/// in it, or unversioned outside a repository.
fn version(path: &Path) -> String {
    let dir = match path.parent() {
        Some(d) if !d.as_os_str().is_empty() => d,
        _ => Path::new("."),
    };
    let Some(commit) = git(dir, &["rev-parse", "--short", "HEAD"]) else {
        return "unversioned".to_string();
    };
    let file = path
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or_default();
    match git(dir, &["status", "--porcelain", "--", &file]) {
        Some(s) if s.is_empty() => commit,
        _ => format!("{commit}-dirty"),
    }
}

/// The md5 of data, in hex.
fn md5_hex(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

/// Compiles the script at path and returns its bytecode.
fn build_file(path: &Path) -> Result<Vec<u8>> {
    let source = fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let compiled = utc_now();
    let version = version(path);
    let hash = md5_hex(source.as_bytes());
    let meta = Meta {
        compiled: &compiled,
        version: &version,
        hash: &hash,
    };
    let build = compile(&source, &meta).with_context(|| path.display().to_string())?;
    let i = &build.info;
    eprintln!(
        "{}: {} by {} ({}), version {}, hash {}, compiled {}: {} bytes of bytecode",
        path.display(),
        i.name,
        i.author,
        i.date,
        i.version,
        i.hash,
        i.compiled,
        build.artifact.len()
    );
    Ok(build.artifact)
}

/// Builds the script at path into output, by default next to it with the
/// extension rhgn.
pub fn build(path: &Path, output: Option<&Path>) -> Result<()> {
    let path = &resolve(path);
    let artifact = build_file(path)?;
    let out = output.map_or_else(|| path.with_extension(EXT), Path::to_path_buf);
    fs::write(&out, &artifact).with_context(|| out.display().to_string())?;
    eprintln!("wrote {}", out.display());
    Ok(())
}

/// The file path names: path itself, or without an extension, path.rhai or
/// else path.rhgn.
fn resolve(path: &Path) -> PathBuf {
    if path.exists() || path.extension().is_some() {
        return path.to_path_buf();
    }
    for ext in ["rhai", EXT] {
        let p = path.with_extension(ext);
        if p.exists() {
            return p;
        }
    }
    path.to_path_buf()
}

/// The bytecode of path, built when it is a source, and the name of the
/// script, from the file's name unless given. Without an extension, path is
/// path.rhai, or else path.rhgn.
fn bytecode(path: &Path, name: Option<&str>) -> Result<(String, Vec<u8>)> {
    let path = &resolve(path);
    let name = match name {
        Some(n) => n.to_string(),
        None => path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    if !valid_name(&name) {
        bail!("{name:?}: a name is 1 to 24 letters, digits, - and _; pick one with --name");
    }
    let data = if path.extension().is_some_and(|e| e == EXT) {
        fs::read(path).with_context(|| path.display().to_string())?
    } else {
        build_file(path)?
    };
    Ok((name, data))
}

/// Runs f on a client of the console at t, in a runtime of its own.
pub fn with_client<R>(t: &Target, f: impl AsyncFnOnce(&mut Client) -> Result<R>) -> Result<R> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let mut c = Client::connect(t).await?;
        c.wake().await?;
        let r = f(&mut c).await;
        c.close().await;
        r
    })
}

/// Reads lines until one of the upload's: the expected one, or an error.
/// Other lines, such as a sensor's error, are shown and skipped.
pub async fn reply(c: &mut Client, want: &str, wait: Duration) -> Result<()> {
    loop {
        let l = c.line(wait).await?;
        if l == want {
            return Ok(());
        }
        if let Some(e) = l.strip_prefix("error: ") {
            bail!("the board: {e}");
        }
        if !l.is_empty() {
            eprintln!("board: {l}");
        }
    }
}

/// Uploads the script at path, a source or bytecode, to the board at t.
pub fn upload(path: &Path, name: Option<&str>, t: &Target) -> Result<()> {
    let (name, data) = bytecode(path, name)?;
    with_client(t, async |c| {
        c.send_line(&format!("script put {name} {} {:08x}", data.len(), crc32(&data)))
            .await?;
        reply(c, "ready", Duration::from_secs(5)).await?;
        let mut sent = 0;
        for chunk in data.chunks(CHUNK) {
            c.write(chunk).await?;
            sent += chunk.len();
            if sent < data.len() {
                reply(c, &format!("ok {sent}"), Duration::from_secs(5)).await?;
                eprint!("\rsent {sent}/{} bytes", data.len());
            }
        }
        // The board writes the flash before it answers.
        reply(c, &format!("done {}", data.len()), Duration::from_secs(30)).await?;
        c.prompt(Duration::from_secs(5)).await?;
        eprintln!("\ruploaded {name}: {} bytes", data.len());
        Ok(())
    })
}

/// The value of the field key in the output of script info.
fn field<'a>(out: &'a str, key: &str) -> Option<&'a str> {
    out.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
        .map(str::trim)
}

/// The fault addresses of the last error in the output of script info:
/// "(at 0x1c 0x8)".
fn faults(out: &str) -> Vec<usize> {
    let Some(l) = out.lines().find(|l| l.starts_with("last error")) else {
        return Vec::new();
    };
    let Some(at) = l.rfind("(at ") else {
        return Vec::new();
    };
    l[at + 4..]
        .trim_end_matches(')')
        .split_whitespace()
        .filter_map(|a| usize::from_str_radix(a.trim_start_matches("0x"), 16).ok())
        .collect()
}

/// Shows script info of name on the board at t; with source, the script's
/// source, the lines of its last error too, by rebuilding it as the board's
/// was built.
pub fn info(name: &str, source: Option<&Path>, t: &Target) -> Result<()> {
    let out = with_client(t, async |c| {
        c.command(&format!("script info {name}"), Duration::from_secs(10))
            .await
    })?;
    println!("{out}");
    let Some(path) = source else {
        return Ok(());
    };
    let path = &resolve(path);
    let addresses = faults(&out);
    if addresses.is_empty() {
        return Ok(());
    }
    let text = fs::read_to_string(path).with_context(|| path.display().to_string())?;
    let (Some(compiled), Some(version), Some(hash)) = (
        field(&out, "compiled"),
        field(&out, "version"),
        field(&out, "hash"),
    ) else {
        bail!("the board does not show what {name} was built with");
    };
    if md5_hex(text.as_bytes()) != hash {
        eprintln!(
            "warning: {} is not the source the board runs (md5 {}, the board's {hash})",
            path.display(),
            md5_hex(text.as_bytes())
        );
    }
    let meta = Meta {
        compiled,
        version,
        hash,
    };
    let build = compile(&text, &meta).with_context(|| path.display().to_string())?;
    let src: Vec<&str> = text.lines().collect();
    println!("where, innermost first:");
    for (a, line) in addresses.iter().zip(lines(&build, &addresses)) {
        match line {
            Some(n) => println!(
                "  {}:{n}: {}",
                path.display(),
                src.get(n as usize - 1).map_or("", |l| l.trim())
            ),
            None => println!("  {a:#x}: no line"),
        }
    }
    Ok(())
}

/// Runs a script command on the board at t and prints its output.
pub fn command(line: &str, t: &Target) -> Result<()> {
    let out = with_client(t, async |c| c.command(line, Duration::from_secs(10)).await)?;
    println!("{out}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn info_output() {
        let out = "name       t\ncompiled   2026-11-01T12:00:00Z\nversion    abc1234-dirty\nhash       0123\nstate      running, 9 calls, 1 failed, longest call took 812 us\nlast error on_time: Division by zero (at 0x1c 0x8)";
        assert_eq!(field(out, "compiled"), Some("2026-11-01T12:00:00Z"));
        assert_eq!(field(out, "version"), Some("abc1234-dirty"));
        assert_eq!(field(out, "hash"), Some("0123"));
        assert_eq!(faults(out), [0x1c, 0x8]);
        assert!(faults("state      running").is_empty());
    }

    #[test]
    fn md5_of_source() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn template_builds() {
        let dir = std::env::temp_dir().join(format!("bolt-script-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.rhai");
        fs::write(&path, TEMPLATE).unwrap();
        let (name, data) = bytecode(&path, None).unwrap();
        assert_eq!(name, "t");
        assert!(!data.is_empty());
        // Outside a git repository the version says so.
        assert_eq!(version(&path), "unversioned");
        // Without an extension: the source, else the bytecode.
        assert_eq!(resolve(&dir.join("t")), path);
        fs::remove_file(&path).unwrap();
        fs::write(dir.join("t.rhgn"), &data).unwrap();
        assert_eq!(resolve(&dir.join("t")), dir.join("t.rhgn"));
        assert_eq!(bytecode(&dir.join("t"), None).unwrap(), ("t".to_string(), data));
        fs::remove_dir_all(&dir).unwrap();
    }
}
