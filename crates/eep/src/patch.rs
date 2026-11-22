//! Applying update patches (PROTOTYPE.md Step 4).
//!
//! For each piece an update needs that has a patch from a piece we already
//! have: rebuild its NAR from that base, check the sha256 against the
//! signed NarHash, and put it in the local binary cache next to the
//! upstream narinfo (URL and Compression changed, signature untouched).
//! Nix then takes it from there (priority 10) and verifies it again; every
//! other piece comes from cache.nixos.org. Anything that goes wrong for a
//! piece just means that piece is downloaded in full.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use eep_core::http::Http;
use eep_core::narinfo::NarInfo;
use eep_core::par::par_map;
use eep_core::patches::{self, Manifest, Patch};
use eep_core::{StorePath, nixbase32};
use serde::Serialize;
use sha2::Digest;

use crate::nix;
use crate::paths::{Paths, write_atomic};
use crate::state::Config;

/// Patches applied at once: each needs RAM about the size of its base.
const PARALLEL: usize = 4;
const WORK: &str = "/var/tmp/eep";

/// Patches that could apply to some missing pieces.
#[derive(Debug, Default, Clone)]
pub struct Candidates {
    /// New piece → (manifest URL dir, patch).
    pub by_piece: BTreeMap<String, (String, Patch)>,
}

impl Candidates {
    /// Patch bytes and the full bytes they replace, for `pieces`.
    pub fn savings(&self, pieces: &BTreeSet<String>) -> (u64, u64, usize) {
        pieces
            .iter()
            .filter_map(|p| self.by_piece.get(p))
            .fold((0, 0, 0), |(s, f, n), (_, x)| {
                (s + x.size, f + x.full, n + 1)
            })
    }
}

/// Collect usable patches for `missing` (base names) into `to`, from any
/// of `from` releases the server has a manifest for. A patch is usable
/// when its base is valid in the local store.
pub fn candidates(
    cfg: &Config,
    from: &BTreeSet<String>,
    to: &str,
    missing: &BTreeSet<String>,
) -> Result<Candidates> {
    let http = Http::new();
    let mut found: BTreeMap<String, (String, Patch)> = BTreeMap::new();
    for f in from.iter().filter(|f| *f != to) {
        let rel = patches::manifest_path(f, to);
        let Some(bytes) = http.get_bytes(&format!("{}/{rel}", cfg.server))? else {
            continue;
        };
        let m: Manifest = serde_json::from_slice(&bytes).with_context(|| format!("bad {rel}"))?;
        if m.eep_patches != patches::VERSION || m.to != to {
            continue;
        }
        let dir = format!("{}/{}", cfg.server, rel.trim_end_matches("manifest.json"));
        for (piece, p) in m.patches {
            if missing.contains(&piece) && !found.contains_key(&piece) {
                found.insert(piece, (dir.clone(), p));
            }
        }
    }
    let bases: Vec<String> = found
        .values()
        .map(|(_, p)| format!("/nix/store/{}", p.base))
        .collect();
    let invalid = nix::invalid(&bases)?;
    found.retain(|_, (_, p)| !invalid.contains(&format!("/nix/store/{}", p.base)));
    Ok(Candidates { by_piece: found })
}

#[derive(Debug, Default, Serialize)]
pub struct Applied {
    pub patched: usize,
    pub patch_bytes: u64,
    pub full_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failed: Vec<String>,
    #[serde(skip)]
    pub files: Vec<PathBuf>,
}

/// Rebuild the candidate pieces into the local cache. `verify: false`
/// skips eep's own hash check (tamper test only: Nix must still refuse).
pub fn apply(p: &Paths, c: &Candidates, verify: bool) -> Result<Applied> {
    let local = &p.local_cache;
    if !local.join("nix-cache-info").is_file() {
        bail!("{} is not set up (see vm/setup-guest.sh)", local.display());
    }
    let work = work_dir()?;
    let jobs: Vec<(&String, &(String, Patch))> = c.by_piece.iter().collect();
    let http = Http::new();
    let results = par_map(&jobs, PARALLEL, |(piece, (dir, patch))| {
        let r = apply_one(&http, local, &work, piece, dir, patch, verify);
        if let Err(e) = &r {
            eprintln!("eep: patch for {piece} not used: {e:#}");
        }
        r
    });
    let mut done = Applied::default();
    for ((piece, (_, patch)), r) in jobs.iter().zip(results) {
        match r {
            Ok(files) => {
                done.patched += 1;
                done.patch_bytes += patch.size;
                done.full_bytes += patch.full;
                done.files.extend(files);
            }
            Err(_) => done.failed.push((*piece).clone()),
        }
    }
    Ok(done)
}

/// Scratch space for base NARs (up to a GB each: not /tmp, often RAM).
/// `/var/tmp/eep` is shared like /var/tmp itself (sticky, world-writable;
/// the package's tmpfiles rule makes it), and every user — root's driver
/// timer too — works in a private `<uid>` folder inside it, so nobody can
/// plant a file or a link where another one writes.
fn work_dir() -> Result<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
    let shared = Path::new(WORK);
    if !shared.is_dir() {
        std::fs::create_dir_all(shared).with_context(|| format!("mkdir {WORK}"))?;
        let _ = std::fs::set_permissions(shared, std::fs::Permissions::from_mode(0o1777));
    }
    let uid = std::fs::metadata("/proc/self").map(|m| m.uid())?;
    let mine = shared.join(uid.to_string());
    match std::fs::DirBuilder::new().mode(0o700).create(&mine) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e).with_context(|| format!("mkdir {}", mine.display())),
    }
    let m = std::fs::symlink_metadata(&mine)?;
    if !m.is_dir() || m.uid() != uid || m.mode() & 0o077 != 0 {
        bail!("{} is not this user's private folder", mine.display());
    }
    Ok(mine)
}

fn apply_one(
    http: &Http,
    local: &Path,
    work: &Path,
    piece: &str,
    dir: &str,
    patch: &Patch,
    verify: bool,
) -> Result<Vec<PathBuf>> {
    let sp = StorePath::from_base_name(piece)?;
    let base = StorePath::from_base_name(&patch.base)?;
    if patch.file.contains("..") || patch.file.starts_with('/') {
        bail!("strange patch path {:?}", patch.file);
    }
    let patch_file = work.join(format!("{}.zst", sp.hash()));
    let base_nar = work.join(format!("{}.base.nar", sp.hash()));
    let nar = local.join("nar").join(format!("{}.nar", sp.hash()));
    // rebuilt in the private folder, checked there, then moved in
    let tmp_nar = work.join(format!("{}.nar", sp.hash()));
    let result = (|| -> Result<()> {
        let (mut body, _) = http
            .get_reader(&format!("{dir}{}", patch.file))?
            .with_context(|| format!("patch {} missing on the server", patch.file))?;
        std::io::copy(&mut body, &mut std::fs::File::create(&patch_file)?)?;
        let st = Command::new("nix-store")
            .arg("--dump")
            .arg(base.absolute())
            .stdout(std::fs::File::create(&base_nar)?)
            .stderr(Stdio::inherit())
            .status()?;
        if !st.success() {
            bail!("nix-store --dump {base} failed");
        }
        let st = Command::new("zstd")
            .args(["-d", "-q", "-f", &format!("--long={}", patches::WINDOW_LOG)])
            .arg(format!("--patch-from={}", base_nar.display()))
            .arg(&patch_file)
            .arg("-o")
            .arg(&tmp_nar)
            .status()
            .context("running zstd")?;
        if !st.success() {
            bail!("zstd could not apply the patch");
        }
        if verify {
            let want = patch
                .nar_hash
                .strip_prefix("sha256:")
                .and_then(nixbase32::decode)
                .context("bad nar_hash")?;
            let mut h = sha2::Sha256::new();
            std::io::copy(&mut std::fs::File::open(&tmp_nar)?, &mut h)?;
            if h.finalize().as_slice() != want.as_slice() {
                bail!("rebuilt piece doesn't match its NarHash");
            }
        }
        publish_nar(&tmp_nar, &nar)?;
        let mut info = NarInfo::parse(&patch.narinfo)?;
        if info.store_path()? != sp {
            bail!("manifest narinfo is for another piece");
        }
        info.set("URL", &format!("nar/{}.nar", sp.hash()));
        info.set("Compression", "none");
        info.remove("FileHash");
        info.remove("FileSize");
        write_atomic(
            &local.join(format!("{}.narinfo", sp.hash())),
            info.to_text().as_bytes(),
        )?;
        Ok(())
    })();
    for f in [&patch_file, &base_nar, &tmp_nar] {
        let _ = std::fs::remove_file(f);
    }
    result.map(|()| vec![nar.clone(), local.join(format!("{}.narinfo", sp.hash()))])
}

/// Move a checked NAR into the shared local cache: a rename on the same
/// file system; otherwise a copy into a fresh, exclusively created file
/// first (never through a name someone else could have planted).
fn publish_nar(from: &Path, to: &Path) -> Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let dir = to.parent().context("nar dir")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        to.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    let mut out = std::fs::File::options()
        .write(true)
        .create_new(true)
        .open(&tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    let r = std::io::copy(&mut std::fs::File::open(from)?, &mut out)
        .and_then(|_| out.sync_all())
        .and_then(|()| std::fs::rename(&tmp, to));
    if r.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    r.with_context(|| format!("moving the rebuilt piece to {}", to.display()))
}

/// Remove what [`apply`] put in the local cache (after the realise).
pub fn clean(applied: &Applied) {
    for f in &applied.files {
        let _ = std::fs::remove_file(f);
    }
}
