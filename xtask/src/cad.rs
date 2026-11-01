//! cargo xtask cad: the renders and the front plate, by the Python tools in
//! cad/, which run in a venv of their own, cad/.venv. All outputs go to
//! build/cad/ and are named after the model of the boards,
//! pcb/Knixx_Thunder_00.step unless --step names another; the model is not
//! in the repository.
//!
//!     render      five PNG views, with a transparent background, of the
//!                 model; needs the model
//!     frontplate  the STEP and the DXFs of the front plate and the sampler,
//!                 which need no model; with the model, the plate is first
//!                 checked against it and the assembly is written too
//!     readme      render and frontplate, then copies the PNGs that the
//!                 README shows to docs/img/
//!     clean       removes the venv

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use anyhow::{Context, Result, bail};

use crate::root;

const USAGE: &str = "usage: cargo xtask cad <render|frontplate|readme|clean> [--step <model.step>]";

/// The model of the boards, from the repository's root.
const MODEL: &str = "pcb/Knixx_Thunder_00.step";

/// The Python versions that cadquery-ocp has wheels for, newest first.
const PYTHONS: [&str; 4] = ["python3.14", "python3.13", "python3.12", "python3.11"];

/// The views of the renders that the README shows, and the sampler.
const README_PNGS: [&str; 4] = ["iso_front", "iso_back", "side", "sampler"];

pub fn run(args: &[String]) -> Result<()> {
    let (task, rest) = args.split_first().with_context(|| USAGE.to_string())?;
    let step = match rest {
        [] => root().join(MODEL),
        [flag, path] if flag == "--step" => env::current_dir()?.join(path),
        _ => bail!("{USAGE}"),
    };
    let name = step
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .with_context(|| format!("{}: no file name", step.display()))?;
    let model = step.exists().then_some(step.as_path());
    match task.as_str() {
        "render" => render(model, &step),
        "frontplate" => frontplate(model, &name),
        "readme" => {
            render(model, &step)?;
            frontplate(model, &name)?;
            let docs = root().join("docs/img");
            fs::create_dir_all(&docs)?;
            for png in README_PNGS {
                let file = format!("{name}_{png}.png");
                fs::copy(outdir().join(&file), docs.join(&file)).with_context(|| file.clone())?;
                eprintln!("docs/img/{file}");
            }
            Ok(())
        }
        "clean" => {
            let venv = cad().join(".venv");
            if venv.exists() {
                fs::remove_dir_all(&venv)?;
            }
            Ok(())
        }
        _ => bail!("{USAGE}"),
    }
}

fn cad() -> PathBuf {
    root().join("cad")
}

fn outdir() -> PathBuf {
    root().join("build/cad")
}

/// Renders the model from five angles into build/cad/.
fn render(model: Option<&Path>, step: &Path) -> Result<()> {
    let Some(model) = model else {
        bail!("render: no STEP model at {}; use --step", step.display());
    };
    let out = outdir();
    fs::create_dir_all(&out)?;
    python(&["render_step.py".as_ref(), model.as_os_str(), out.as_os_str()])
}

/// Makes the front plate into build/cad/, checked against the model and
/// with the assembly when there is one.
fn frontplate(model: Option<&Path>, name: &str) -> Result<()> {
    let out = outdir();
    fs::create_dir_all(&out)?;
    let mut args = vec!["frontplate.py".as_ref(), out.as_os_str(), name.as_ref()];
    if let Some(m) = model {
        args.push(m.as_os_str());
    }
    python(&args)
}

/// Runs the venv's Python in cad/ with args, setting the venv up first.
fn python(args: &[&std::ffi::OsStr]) -> Result<()> {
    let py = venv()?;
    let status = Command::new(&py)
        .args(args)
        .current_dir(cad())
        .status()
        .with_context(|| py.display().to_string())?;
    if !status.success() {
        bail!("{} failed", args[0].to_string_lossy());
    }
    Ok(())
}

/// Returns the venv's Python, after making the venv and installing
/// requirements.txt into it when there is no venv yet or the requirements
/// changed since.
fn venv() -> Result<PathBuf> {
    let dir = cad().join(".venv");
    let py = dir.join("bin/python");
    let marker = dir.join(".installed");
    let reqs = cad().join("requirements.txt");
    if mtime(&marker).is_some_and(|m| mtime(&reqs).is_some_and(|r| r <= m)) {
        return Ok(py);
    }
    let base = base_python()?;
    eprintln!("cad: setting up cad/.venv with {base}");
    if dir.exists() {
        fs::remove_dir_all(&dir)?;
    }
    let ok = |c: &mut Command, what: &str| -> Result<()> {
        if !c.status().with_context(|| what.to_string())?.success() {
            bail!("{what} failed");
        }
        Ok(())
    };
    ok(
        Command::new(&base).args(["-m", "venv"]).arg(&dir),
        "python -m venv",
    )?;
    ok(
        Command::new(&py)
            .args(["-m", "pip", "install", "-q", "-r"])
            .arg(&reqs),
        "pip install -r cad/requirements.txt",
    )?;
    fs::write(&marker, "")?;
    Ok(py)
}

/// The Python to make the venv with: $PY, else the newest of PYTHONS on the
/// PATH, else python3.
fn base_python() -> Result<String> {
    if let Ok(py) = env::var("PY") {
        return Ok(py);
    }
    let path = env::var_os("PATH").unwrap_or_default();
    for name in PYTHONS {
        if env::split_paths(&path).any(|d| d.join(name).is_file()) {
            return Ok(name.to_string());
        }
    }
    Ok("python3".to_string())
}

fn mtime(p: &Path) -> Option<SystemTime> {
    fs::metadata(p).and_then(|m| m.modified()).ok()
}
