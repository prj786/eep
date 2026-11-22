//! Client state: config, the cached index, the lock.

use std::fs::File;

use anyhow::{Context, Result, bail};
use eep_core::http::{Conditional, Http};
use eep_core::index::{ChannelPointer, Index};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest;

use crate::paths::{Paths, write_atomic};

/// The public eep server: the builder's `dist/`, published to GitHub Pages
/// (the `gh-pages` branch of prj786/eep, see `publish-index.yml`). A local
/// `dist/` (the VM, the test container) is set with `server =` in
/// config.toml or `EEP_SERVER`.
pub const DEFAULT_SERVER: &str = "https://prj786.github.io/eep";
pub const DEFAULT_CHANNEL: &str = "nixos-unstable";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub server: String,
    pub channel: String,
    /// Give every app on the computer a card (pacman, AppImages, Steam,
    /// the user's own launcher entries), not only eep's.
    pub find_apps: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: DEFAULT_SERVER.into(),
            channel: DEFAULT_CHANNEL.into(),
            find_apps: true,
        }
    }
}

/// `~/.config/eep/config.toml`, else `/etc/eep/config.toml`, then
/// `EEP_SERVER` / `EEP_CHANNEL` override.
pub fn config(p: &Paths) -> Result<Config> {
    let mut cfg = Config::default();
    for f in [p.config.join("config.toml"), "/etc/eep/config.toml".into()] {
        if let Ok(text) = std::fs::read_to_string(&f) {
            cfg = toml::from_str(&text).with_context(|| format!("reading {}", f.display()))?;
            break;
        }
    }
    if let Ok(s) = std::env::var("EEP_SERVER") {
        cfg.server = s;
    }
    if let Ok(c) = std::env::var("EEP_CHANNEL") {
        cfg.channel = c;
    }
    cfg.server = cfg.server.trim_end_matches('/').to_owned();
    if !eep_core::index::valid_name(&cfg.channel) {
        bail!("strange channel {:?} in the config", cfg.channel);
    }
    Ok(cfg)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct IndexMeta {
    /// The pointer URL the validators below came from.
    #[serde(default)]
    url: String,
    etag: Option<String>,
    last_modified: Option<String>,
    release: String,
    /// sha256 of the index we hold: the server may republish a release's
    /// index (a catalog change) under the same release name.
    #[serde(default)]
    sha256: String,
    checked: String,
}

pub fn load_index(p: &Paths) -> Result<Index> {
    let Ok(bytes) = std::fs::read(p.index_file()) else {
        bail!("no index yet: run `eep index update`");
    };
    let idx: Index = serde_json::from_slice(&bytes)
        .context("the cached index is damaged: run `eep index update`")?;
    idx.validate()?;
    Ok(idx)
}

/// The cached index, fetching it first if there is none.
pub fn index_or_fetch(p: &Paths) -> Result<Index> {
    if !p.index_file().exists() {
        update_index(p)?;
    }
    load_index(p)
}

/// Fetch the channel pointer (conditionally) and, when it moved, the index.
pub fn update_index(p: &Paths) -> Result<Value> {
    let cfg = config(p)?;
    let http = Http::new();
    let meta_file = p.cache.join("index.meta.json");
    let mut meta: IndexMeta = std::fs::read(&meta_file)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let have_index = p.index_file().exists();
    let url = format!("{}/channels/{}.json", cfg.server, cfg.channel);
    // Validators only mean something to the server that issued them, and
    // only when we know which index we hold (its sha256): otherwise, after
    // a server or channel change or an older eep's meta, fetch in full.
    let (etag, lm) = if have_index && meta.url == url && !meta.sha256.is_empty() {
        (meta.etag.as_deref(), meta.last_modified.as_deref())
    } else {
        (None, None)
    };
    let ptr_bytes = match http.conditional(&url, etag, lm)? {
        Conditional::NotModified => {
            meta.checked = eep_core::time::now_rfc3339();
            write_atomic(&meta_file, &serde_json::to_vec(&meta)?)?;
            return Ok(json!({ "ok": true, "changed": false, "release": meta.release }));
        }
        Conditional::Missing => {
            bail!("{url} not found (is the eep server right? see `eep doctor`)")
        }
        Conditional::Body(b, m) => {
            meta.url.clone_from(&url);
            meta.etag = m.etag;
            meta.last_modified = m.last_modified;
            b
        }
    };
    let ptr: ChannelPointer = serde_json::from_slice(&ptr_bytes).context("bad channel pointer")?;
    ptr.validate()?;
    let changed = !have_index || ptr.release != meta.release || ptr.sha256 != meta.sha256;
    if changed {
        let idx_url = format!("{}/{}", cfg.server, ptr.index);
        let zst = http
            .get_bytes(&idx_url)?
            .with_context(|| format!("{idx_url} not found"))?;
        let sum: String = sha2::Sha256::digest(&zst)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        if sum != ptr.sha256 {
            bail!("index checksum mismatch for {idx_url}");
        }
        let json_bytes = zstd::decode_all(zst.as_slice()).context("index is not zstd")?;
        let idx: Index = serde_json::from_slice(&json_bytes).context("index format")?;
        if idx.eep_index != eep_core::index::VERSION {
            bail!(
                "index format {} is not supported by this eep",
                idx.eep_index
            );
        }
        if idx.release != ptr.release || idx.channel != cfg.channel {
            bail!("index doesn't match its channel pointer");
        }
        idx.validate()?;
        write_atomic(&p.index_file(), &json_bytes)?;
        cache_icons(p, &cfg.server, &idx);
        meta.release.clone_from(&ptr.release);
        meta.sha256.clone_from(&ptr.sha256);
    }
    meta.checked = eep_core::time::now_rfc3339();
    write_atomic(&meta_file, &serde_json::to_vec(&meta)?)?;
    Ok(json!({ "ok": true, "changed": changed, "release": ptr.release, "released": ptr.released }))
}

/// One eep at a time changes state (the watcher and a CLI call can race).
pub fn lock(p: &Paths) -> Result<File> {
    let dir = p.roots.parent().context("state dir")?;
    std::fs::create_dir_all(dir)?;
    let f = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("lock"))?;
    if f.try_lock().is_err() {
        eprintln!("eep: waiting for another eep to finish…");
        f.lock().context("taking eep's lock")?;
    }
    Ok(f)
}

/// Where the catalog icon `rel` (`icons/firefox.png`) of `release` is kept.
pub fn icon_cache(p: &Paths, release: &str, rel: &str) -> std::path::PathBuf {
    p.cache
        .join("icons")
        .join(release)
        .join(rel.rsplit('/').next().unwrap_or(rel))
}

/// Keep the new index's icons on disk, so stores show apps that aren't
/// installed yet without a request per icon (and without loading http
/// images). Only missing ones are fetched, in parallel; older releases'
/// icons go. Best effort: a missing icon is a letter in the UI, not an error.
fn cache_icons(p: &Paths, server: &str, idx: &Index) {
    let rels: Vec<&String> = idx.apps.values().filter_map(|a| a.icon.as_ref()).collect();
    let http = Http::new();
    eep_core::par::par_map(&rels, 8, |rel| {
        let file = icon_cache(p, &idx.release, rel);
        if file.exists() || rel.contains("..") {
            return;
        }
        if let Ok(Some(bytes)) = http.get_bytes(&format!("{server}/{}/{rel}", idx.release)) {
            let _ = write_atomic(&file, &bytes);
        }
    });
    if let Ok(rd) = std::fs::read_dir(p.cache.join("icons")) {
        for e in rd.flatten() {
            if e.file_name() != idx.release.as_str() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}
