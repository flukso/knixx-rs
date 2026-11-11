//! bolt fleet push and pull: the per-board files of an installation, its
//! boards' knixx.ini and scripts, kept in one folder per board, by its
//! individual address, under version control:
//!
//! ```text
//! <dir>/1.1.5/knixx.ini
//! <dir>/1.1.5/scripts/<name>.rhai   (or .rhgn; the source wins)
//! ```
//!
//! Each board is reached as fleet update reaches it: a scan through the
//! proxy, a Window request on the KNX control plane, then its transfer
//! service over BLE, `parallel` boards at a time, the proxy last, over its
//! own link. A push skips what a board has already, and a pull writes
//! what it got, so that pull, push, pull leaves the folder as it was.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use btleplug::api::Peripheral as _;
use btleplug::platform::Peripheral;
use knixx_knx::IndividualAddress;
use knixx_knx::fw::{Request, Response};
use knixx_xfer::Kind;

use crate::fleet::{Proxy, SCAN_WAIT, name, runtime};
use crate::xfer::{Link, write_file};

/// Where to find the proxy, and how to go about the boards.
pub struct Fleet<'a> {
    pub via: &'a str,
    pub via_addr: &'a str,
    pub wait: Duration,
    pub parallel: usize,
    /// How long each board's window stays open, in minutes.
    pub minutes: u8,
}

/// A board reached: its window open, found over BLE.
struct Reached {
    from: u16,
    p: Peripheral,
    proxy: bool,
}

/// What became of each board, file by file.
type Report = BTreeMap<u16, Vec<(String, String)>>;

/// Scans through the proxy, opens a window on each board that answers and
/// that want takes, and finds them over BLE. Boards that could not be
/// reached go into the report.
async fn reach(
    f: &Fleet<'_>,
    want: impl Fn(u16) -> bool,
    report: &mut Report,
) -> Result<(Proxy, Vec<Reached>)> {
    let adapter = knixx_host::blescan::adapter().await.map_err(anyhow::Error::msg)?;
    let advertising = knixx_host::blescan::find_boards(&adapter, &[], Duration::from_secs(3))
        .await
        .map_err(anyhow::Error::msg)?;
    let mut proxy = Proxy::connect(f.via, f.via_addr, f.wait).await?;
    let proxy_id = proxy.p.id();
    let proxy_ble = advertising
        .iter()
        .find(|(_, p)| p.id() == proxy_id)
        .map(|(a, _)| *a);

    proxy.send(0, &Request::Scan).await?;
    let mut seen: Vec<(u16, [u8; 6])> = proxy
        .answers(SCAN_WAIT, None)
        .await?
        .into_iter()
        .filter_map(|a| match a.response {
            Response::Info { ble, .. } => Some((a.from, ble)),
            _ => None,
        })
        .collect();
    seen.sort();
    seen.dedup_by_key(|s| s.0);
    eprintln!("{} board(s) answered", seen.len());

    let mut ready = Vec::new();
    for &(from, ble) in &seen {
        if !want(from) {
            report.insert(from, vec![("-".into(), "no folder".into())]);
            continue;
        }
        proxy.send(from, &Request::Window { minutes: f.minutes }).await?;
        let a = proxy.answers(Duration::from_secs(3), Some(from)).await?;
        match a.iter().find(|a| a.from == from).map(|a| a.response) {
            Some(Response::Ready { .. }) => ready.push((from, ble)),
            Some(Response::Refused { reason }) => {
                report.insert(from, vec![("-".into(), format!("window refused ({reason})"))]);
            }
            _ => {
                report.insert(
                    from,
                    vec![("-".into(), "no answer to window: its firmware is too old?".into())],
                );
            }
        }
    }

    let others: Vec<[u8; 6]> = ready
        .iter()
        .filter(|(_, b)| Some(*b) != proxy_ble)
        .map(|(_, b)| *b)
        .collect();
    let found = if others.is_empty() {
        Vec::new()
    } else {
        knixx_host::blescan::find_boards(&adapter, &others, Duration::from_secs(20))
            .await
            .map_err(anyhow::Error::msg)?
    };
    let mut reached = Vec::new();
    for (from, ble) in ready {
        if Some(ble) == proxy_ble {
            reached.push(Reached {
                from,
                p: proxy.p.clone(),
                proxy: true,
            });
        } else if let Some((_, p)) = found.iter().find(|(a, _)| *a == ble) {
            reached.push(Reached {
                from,
                p: p.clone(),
                proxy: false,
            });
        } else {
            report.insert(from, vec![("-".into(), "not seen over BLE".into())]);
        }
    }
    // The proxy last: a change to it could cut the run short.
    reached.sort_by_key(|r| r.proxy);
    Ok((proxy, reached))
}

/// Runs job on each board reached, parallel at a time, the proxy last on
/// its own link; then shuts their windows.
async fn each<J>(
    f: &Fleet<'_>,
    proxy: &mut Proxy,
    reached: Vec<Reached>,
    report: &mut Report,
    job: J,
) -> Result<()>
where
    J: AsyncFn(&mut Link<'_>, u16, bool) -> Result<Vec<(String, String)>>,
{
    let run = async |r: &Reached| -> (u16, Result<Vec<(String, String)>>) {
        let res = async {
            if !r.p.is_connected().await.unwrap_or(false) {
                r.p.connect().await.context("connect")?;
                r.p.discover_services().await.context("services")?;
            }
            let mut link = Link::open(&r.p).await?;
            job(&mut link, r.from, r.proxy).await
        }
        .await;
        if !r.proxy {
            let _ = r.p.disconnect().await;
        }
        (r.from, res)
    };
    let (proxies, others): (Vec<&Reached>, Vec<&Reached>) = reached.iter().partition(|r| r.proxy);
    for group in others.chunks(f.parallel.max(1)) {
        for (from, r) in futures_util::future::join_all(group.iter().map(|r| run(r))).await {
            report.insert(from, r.unwrap_or_else(|e| vec![("-".into(), format!("{e:#}"))]));
        }
    }
    for r in proxies {
        let (from, res) = run(r).await;
        report.insert(from, res.unwrap_or_else(|e| vec![("-".into(), format!("{e:#}"))]));
    }
    for r in &reached {
        let _ = proxy.send(r.from, &Request::Close).await;
        let _ = proxy.answers(Duration::from_secs(1), Some(r.from)).await;
    }
    Ok(())
}

fn print(report: &Report) {
    println!("{:<10} {:<24} result", "board", "file");
    for (from, files) in report {
        for (file, what) in files {
            println!("{:<10} {:<24} {what}", name(*from), file);
        }
    }
}

/// A board's folder, as fleet push reads it.
struct Folder {
    config: Option<Vec<u8>>,
    /// Each script: its name, its bytecode and its source's hash.
    scripts: Vec<(String, Vec<u8>, Option<String>)>,
}

/// Reads dir: one folder per board, named by its individual address.
fn read_dir(dir: &Path) -> Result<BTreeMap<u16, Folder>> {
    let mut out = BTreeMap::new();
    let entries = fs::read_dir(dir).with_context(|| dir.display().to_string())?;
    let mut paths: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    paths.sort();
    for path in paths {
        if !path.is_dir() {
            continue;
        }
        let folder = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        let Ok(addr) = IndividualAddress::parse(&folder) else {
            if !folder.starts_with('.') {
                eprintln!("{}: not an individual address, left out", path.display());
            }
            continue;
        };
        let ini = path.join("knixx.ini");
        let config = if ini.exists() {
            let data = crate::xfer::read_config(&ini)?;
            let dev = knixx_device::config::parse(&data).map_err(|e| anyhow!("{}: {e}", ini.display()))?;
            if dev.individual != addr.0 {
                bail!(
                    "{}: for {}, in the folder of {folder}: a fleet run does not force",
                    ini.display(),
                    IndividualAddress(dev.individual)
                );
            }
            Some(data)
        } else {
            None
        };
        let mut stems: Vec<String> = fs::read_dir(path.join("scripts"))
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "rhai" || x == "rhgn"))
                    .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
                    .collect()
            })
            .unwrap_or_default();
        stems.sort();
        stems.dedup();
        let mut scripts = Vec::new();
        for stem in stems {
            // The source wins; resolve takes it first.
            let p = path.join("scripts").join(&stem);
            let (n, data) = crate::script::bytecode(&p, None)?;
            if data.len() > knixx_xfer::MAX_SCRIPT as usize {
                bail!(
                    "{}: {} bytes, over the {}",
                    p.display(),
                    data.len(),
                    knixx_xfer::MAX_SCRIPT
                );
            }
            scripts.push((n, data, crate::script::source_hash(&p)?));
        }
        out.insert(addr.0, Folder { config, scripts });
    }
    Ok(out)
}

/// The BLE settings that decide whether bolt reaches a board again.
fn reach_settings(ini: &[u8]) -> Option<(bool, bool)> {
    knixx_device::config::parse(ini)
        .ok()
        .map(|d| (d.ble.knixx, d.ble.pair))
}

/// Pushes each folder of dir to its board: the knixx.ini, then the scripts,
/// each skipped when the board has it already. A change to the proxy's
/// ble.knixx or ble.pair needs force_proxy: it could strand the next run.
pub fn push(dir: &Path, f: &Fleet<'_>, force_proxy: bool) -> Result<()> {
    let folders = read_dir(dir)?;
    if folders.is_empty() {
        bail!(
            "{}: no board folders (named by individual address)",
            dir.display()
        );
    }
    runtime()?.block_on(async {
        let mut report = Report::new();
        let (mut proxy, reached) = reach(f, |a| folders.contains_key(&a), &mut report).await?;
        let job = async |l: &mut Link<'_>, from: u16, is_proxy: bool| -> Result<Vec<(String, String)>> {
            let folder = &folders[&from];
            let label = name(from);
            let mut done = Vec::new();
            if let Some(ini) = &folder.config {
                let now = l.get(Kind::Config, "").await?;
                let what = if now == *ini {
                    "unchanged".to_string()
                } else if is_proxy && !force_proxy && reach_settings(&now) != reach_settings(ini) {
                    "left out: it changes the proxy's ble.knixx or ble.pair; --force-proxy".to_string()
                } else {
                    match l.put(Kind::Config, 0, "", ini, |_| {}).await {
                        Ok(_) => "pushed".to_string(),
                        Err(e) => format!("{e:#}"),
                    }
                };
                eprintln!("{label}: knixx.ini {what}");
                done.push(("knixx.ini".to_string(), what));
            }
            if !folder.scripts.is_empty() {
                let list = l.list().await?;
                for (n, data, src) in &folder.scripts {
                    let what = if l.has_script(&list, n, src.as_deref(), data).await? {
                        "unchanged".to_string()
                    } else {
                        match l.put(Kind::Script, 0, n, data, |_| {}).await {
                            Ok(_) => "pushed".to_string(),
                            Err(e) => format!("{e:#}"),
                        }
                    };
                    eprintln!("{label}: {n} {what}");
                    done.push((format!("scripts/{n}"), what));
                }
            }
            Ok(done)
        };
        each(f, &mut proxy, reached, &mut report, job).await?;
        proxy.close().await;
        for addr in folders.keys() {
            report
                .entry(*addr)
                .or_insert_with(|| vec![("-".into(), "did not answer the scan".into())]);
        }
        print(&report);
        Ok(())
    })
}

/// Pulls each board's knixx.ini and scripts into `<dir>/<board>/`,
/// overwriting, and says what changed against what was there.
pub fn pull(dir: &Path, f: &Fleet<'_>) -> Result<()> {
    runtime()?.block_on(async {
        let mut report = Report::new();
        let (mut proxy, reached) = reach(f, |_| true, &mut report).await?;
        let job = async |l: &mut Link<'_>, from: u16, _: bool| -> Result<Vec<(String, String)>> {
            let folder = dir.join(name(from));
            let mut done = Vec::new();
            let ini = l.get(Kind::Config, "").await?;
            done.push((
                "knixx.ini".to_string(),
                write_file(&folder.join("knixx.ini"), &ini)?.to_string(),
            ));
            let list = l.list().await?;
            for n in list.keys() {
                let data = l.get(Kind::Script, n).await?;
                let what = write_file(&folder.join("scripts").join(format!("{n}.rhgn")), &data)?;
                done.push((format!("scripts/{n}"), what.to_string()));
            }
            Ok(done)
        };
        each(f, &mut proxy, reached, &mut report, job).await?;
        proxy.close().await;
        print(&report);
        Ok(())
    })
}
