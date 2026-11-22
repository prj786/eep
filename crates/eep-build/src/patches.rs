//! `eep-build patches --from A --to B`: for every piece release B's apps
//! need that release A's didn't have, make a zstd `--patch-from` file
//! against the closest piece of A, prove it rebuilds the exact NAR, and
//! write the manifest clients read.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use eep_core::index::Index;
use eep_core::narinfo::NarInfo;
use eep_core::patches::{self, Manifest, Patch};
use eep_core::{StorePath, UPSTREAM_CACHE, store_path};
use serde_json::{Value, json};
use sha2::Digest;

use crate::index::log;
use crate::upstream::Upstream;
use crate::util::{par_map, write_atomic};

#[derive(Debug)]
pub struct Opts {
    pub from: String,
    pub to: String,
    pub dist: PathBuf,
    /// Only these apps (empty: all).
    pub apps: Vec<String>,
    pub threads: usize,
    pub cache: PathBuf,
}

/// One piece's outcome.
enum Made {
    Patch(Patch),
    Skipped(&'static str),
}

pub fn run(up: &Upstream, o: &Opts) -> Result<Value> {
    let t0 = Instant::now();
    let a = load(&o.dist, &o.from)?;
    let b = load(&o.dist, &o.to)?;
    let wanted: BTreeSet<&str> = o.apps.iter().map(String::as_str).collect();

    // New pieces: in B's closures, absent from A's.
    let old: BTreeSet<&String> = all_pieces(&a, &BTreeSet::new());
    let new: Vec<StorePath> = all_pieces(&b, &wanted)
        .into_iter()
        .filter(|p| !old.contains(p))
        .map(|p| StorePath::from_base_name(p))
        .collect::<Result<_>>()?;
    let old: Vec<StorePath> = old
        .into_iter()
        .map(|p| StorePath::from_base_name(p))
        .collect::<Result<_>>()?;

    let mut jobs: Vec<(StorePath, StorePath)> = Vec::new();
    let mut no_base = Vec::new();
    for n in &new {
        match choose_base(n, &old, &a) {
            Some(base) => jobs.push((n.clone(), base)),
            None => no_base.push(n.to_string()),
        }
    }
    // Biggest first, so the long ones overlap with many small ones.
    jobs.sort_by_key(|(n, _)| std::cmp::Reverse(b.pieces.get(n.base_name()).map_or(0, |p| p.nar)));
    log(&format!(
        "{} new pieces: {} with a base, {} without; {} threads",
        new.len(),
        jobs.len(),
        no_base.len(),
        o.threads
    ));

    let dir = o.dist.join(format!("patches/{}__{}", o.from, o.to));
    std::fs::create_dir_all(dir.join("p"))?;
    let work = o.cache.join(format!("patch-work-{}", std::process::id()));
    std::fs::create_dir_all(&work)?;
    let results = par_map(&jobs, o.threads, |(n, base)| {
        let r = make(up, n, base, &dir, &work);
        if let Err(e) = &r {
            log(&format!("{n}: {e:#}"));
        }
        r
    });
    let _ = std::fs::remove_dir_all(&work);

    let mut manifest = Manifest {
        eep_patches: patches::VERSION,
        from: o.from.clone(),
        to: o.to.clone(),
        patches: BTreeMap::new(),
    };
    let mut skipped: BTreeMap<String, &str> = BTreeMap::new();
    let mut failed = Vec::new();
    for ((n, _), r) in jobs.iter().zip(results) {
        match r {
            Ok(Made::Patch(p)) => {
                manifest.patches.insert(n.to_string(), p);
            }
            Ok(Made::Skipped(why)) => {
                skipped.insert(n.to_string(), why);
            }
            Err(_) => failed.push(n.to_string()),
        }
    }
    // Drop patch files no manifest entry names (an earlier run, a skip).
    let keep: BTreeSet<String> = manifest.patches.values().map(|p| p.file.clone()).collect();
    for e in std::fs::read_dir(dir.join("p"))?.flatten() {
        if !keep.contains(&format!("p/{}", e.file_name().to_string_lossy())) {
            std::fs::remove_file(e.path())?;
        }
    }
    write_atomic(
        &dir.join("manifest.json"),
        &serde_json::to_vec_pretty(&manifest)?,
    )?;

    // What an update of everything costs, three ways.
    let new_set: BTreeSet<&str> = new.iter().map(StorePath::base_name).collect();
    let full: u64 = new_set
        .iter()
        .map(|p| b.pieces.get(*p).map_or(0, |x| x.dl))
        .sum();
    let patched: u64 = new_set
        .iter()
        .map(|p| {
            manifest
                .patches
                .get(*p)
                .map_or_else(|| b.pieces.get(*p).map_or(0, |x| x.dl), |x| x.size)
        })
        .sum();
    Ok(json!({
        "ok": true,
        "from": o.from, "to": o.to,
        "manifest": dir.join("manifest.json"),
        "new_pieces": new.len(),
        "patches": manifest.patches.len(),
        "skipped": skipped,
        "no_base": no_base,
        "failed": failed,
        "download_full": full,
        "download_patched": patched,
        "ratio": if full > 0 { patched as f64 / full as f64 } else { 0.0 },
        "cost": {
            "seconds": t0.elapsed().as_secs_f64(),
            "child_cpu_seconds": children_cpu_seconds(),
        },
    }))
}

fn load(dist: &Path, release: &str) -> Result<Index> {
    let f = dist.join(release).join("index.json");
    let b = std::fs::read(&f).with_context(|| format!("{}: build the index first", f.display()))?;
    Ok(serde_json::from_slice(&b)?)
}

fn all_pieces<'a>(idx: &'a Index, only: &BTreeSet<&str>) -> BTreeSet<&'a String> {
    let mut s: BTreeSet<&String> = idx
        .apps
        .iter()
        .filter(|(id, _)| only.is_empty() || only.contains(id.as_str()))
        .flat_map(|(_, a)| &a.closure)
        .collect();
    if only.is_empty() {
        s.extend(&idx.system.driver_closure);
    }
    s
}

/// The piece of the old release to patch from: the same name (a rebuild),
/// else the same pname with the same output (`-lib`, `-bin`…), else any
/// same pname; ties go to the closest size.
fn choose_base(new: &StorePath, old: &[StorePath], a: &Index) -> Option<StorePath> {
    if let Some(p) = old.iter().find(|p| p.name() == new.name()) {
        return Some(p.clone());
    }
    let want_out = output_of(new.name());
    let size = |p: &StorePath| a.pieces.get(p.base_name()).map_or(0, |x| x.nar);
    old.iter()
        .filter(|p| p.pname() == new.pname() && !new.pname().is_empty())
        .min_by_key(|p| (output_of(p.name()) != want_out, size(p).abs_diff(size(new))))
        .cloned()
}

/// `krb5-1.22.2-lib` → `lib`; `firefox-160.0.1` → ``.
fn output_of(name: &str) -> &str {
    let (_, version) = store_path::split_name(name);
    match version.rsplit_once('-') {
        Some((_, out)) if !out.is_empty() && out.bytes().all(|c| c.is_ascii_lowercase()) => out,
        _ => "",
    }
}

fn make(up: &Upstream, new: &StorePath, base: &StorePath, dir: &Path, work: &Path) -> Result<Made> {
    let ni = up.narinfo(new)?.context("new piece left the cache")?;
    let bi = up.narinfo(base)?.context("base piece left the cache")?;
    let full = ni.file_size().unwrap_or(ni.nar_size()?);
    let new_nar = work.join(format!("{}.nar", new.hash()));
    let base_nar = work.join(format!("{}.base.nar", new.hash()));
    let out = dir.join("p").join(format!("{}.zst", new.hash()));
    let tmp = work.join(format!("{}.zst", new.hash()));
    let cleanup = || {
        for f in [&new_nar, &base_nar, &tmp] {
            let _ = std::fs::remove_file(f);
        }
    };
    let result = (|| -> Result<Made> {
        fetch_nar(up, &bi, &base_nar)?;
        fetch_nar(up, &ni, &new_nar)?;
        let window = format!("--long={}", patches::WINDOW_LOG);
        let patch_from = format!("--patch-from={}", base_nar.display());
        zstd(&[
            "-19",
            "-q",
            "-f",
            &window,
            &patch_from,
            &new_nar.to_string_lossy(),
            "-o",
            &tmp.to_string_lossy(),
        ])?;
        let size = std::fs::metadata(&tmp)?.len();
        #[allow(clippy::cast_precision_loss)]
        if size as f64 > patches::MAX_RATIO * full as f64 {
            return Ok(Made::Skipped("patch not much smaller than the download"));
        }
        // Prove it: rebuild the NAR from base + patch and check the signed hash.
        std::fs::remove_file(&new_nar)?;
        zstd(&[
            "-d",
            "-q",
            "-f",
            &window,
            &patch_from,
            &tmp.to_string_lossy(),
            "-o",
            &new_nar.to_string_lossy(),
        ])?;
        if sha256_file(&new_nar)? != ni.nar_hash()? {
            bail!("rebuilt NAR doesn't match NarHash");
        }
        std::fs::rename(&tmp, &out).or_else(|_| std::fs::copy(&tmp, &out).map(|_| ()))?;
        Ok(Made::Patch(Patch {
            base: base.to_string(),
            file: format!("p/{}.zst", new.hash()),
            size,
            full,
            nar_hash: ni.get("NarHash").unwrap_or_default().to_owned(),
            nar_size: ni.nar_size()?,
            narinfo: ni.to_text(),
        }))
    })();
    cleanup();
    result
}

/// Download a piece's NAR (cached compressed, as served) and unpack it to `to`.
fn fetch_nar(up: &Upstream, info: &NarInfo, to: &Path) -> Result<()> {
    let url_path = info.url().context("narinfo without URL")?;
    let cached = up
        .nar_cache()
        .join(url_path.rsplit('/').next().unwrap_or(url_path));
    if !cached.exists() {
        let url = format!("{UPSTREAM_CACHE}/{url_path}");
        let (mut body, _) = up
            .http
            .get_reader(&url)?
            .with_context(|| format!("{url} missing"))?;
        let part = cached.with_extension("part");
        std::fs::create_dir_all(up.nar_cache())?;
        let mut f = std::fs::File::create(&part)?;
        std::io::copy(&mut body, &mut f)?;
        f.flush()?;
        std::fs::rename(&part, &cached)?;
    }
    let tool = match info.compression() {
        "zstd" => vec!["zstd", "-d", "-q", "-c"],
        "xz" => vec!["xz", "-d", "-c"],
        "none" => vec!["cat"],
        c => bail!("unsupported compression {c}"),
    };
    let st = Command::new(tool[0])
        .args(&tool[1..])
        .arg(&cached)
        .stdout(std::fs::File::create(to)?)
        .stderr(Stdio::inherit())
        .status()?;
    if !st.success() {
        let _ = std::fs::remove_file(&cached); // damaged download: fetch again next time
        bail!("unpacking {} failed", cached.display());
    }
    Ok(())
}

fn zstd(args: &[&str]) -> Result<()> {
    let st = Command::new("zstd")
        .args(args)
        .stderr(Stdio::inherit())
        .status()
        .context("running zstd")?;
    if !st.success() {
        bail!("zstd {} failed", args.join(" "));
    }
    Ok(())
}

/// CPU time of finished child processes (zstd, xz): `cutime + cstime`
/// from /proc/self/stat, in clock ticks of 1/100 s (Linux's USER_HZ).
/// Peak RAM is measured from outside (bench/: a systemd scope's memory.peak).
fn children_cpu_seconds() -> f64 {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return 0.0;
    };
    // Fields after the parenthesised command name; cutime/cstime are 16 and 17.
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<f64> = rest
        .split_whitespace()
        .filter_map(|x| x.parse().ok())
        .collect();
    // `rest` starts at field 3 (state, not numeric, so filtered out): field N is f[N - 4].
    f.get(12)
        .zip(f.get(13))
        .map_or(0.0, |(u, s)| (u + s) / 100.0)
}

fn sha256_file(p: &Path) -> Result<[u8; 32]> {
    let mut f = std::fs::File::open(p)?;
    let mut h = sha2::Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(h.finalize().into())
}

#[cfg(test)]
mod tests {
    use super::output_of;

    #[test]
    fn outputs() {
        assert_eq!(output_of("krb5-1.22.2-lib"), "lib");
        assert_eq!(output_of("xgcc-15.3.0-libgcc"), "libgcc");
        assert_eq!(output_of("firefox-160.0.1"), "");
        assert_eq!(output_of("glibc-2.42-103"), "");
        assert_eq!(output_of("source"), "");
    }
}
