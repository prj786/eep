//! Where eep keeps things. Everything is per user except the local binary
//! cache and the driver link (see PROTOTYPE.md §5, "eep's own places").

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    /// `~/Applications`: the cards (one per app; the folder is the app list).
    pub apps: PathBuf,
    /// `~/.cache/eep`: index, updates.json.
    pub cache: PathBuf,
    /// `~/.local/state/eep/roots`: GC roots, `<id>` and `<id>.prev`.
    pub roots: PathBuf,
    /// `~/.local/share/eep`: icons/, bin/, data/.
    pub data: PathBuf,
    /// `~/.local/share/applications`: launcher entries under the app's own
    /// file names, marked `X-Eep-Id=<id>`.
    pub launchers: PathBuf,
    /// `~/.config/eep`: config.toml.
    pub config: PathBuf,
    /// `/var/cache/eep/local`: the local binary cache for patched pieces.
    pub local_cache: PathBuf,
    /// `/var/lib/eep/opengl-driver`: the driver symlink farm.
    pub driver_farm: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = env_path("HOME").unwrap_or_else(|| PathBuf::from("/"));
        let xdg = |var: &str, dflt: &str| env_path(var).unwrap_or_else(|| home.join(dflt));
        let data_home = xdg("XDG_DATA_HOME", ".local/share");
        Self {
            apps: env_path("EEP_APPS_DIR").unwrap_or_else(|| home.join("Applications")),
            cache: xdg("XDG_CACHE_HOME", ".cache").join("eep"),
            roots: xdg("XDG_STATE_HOME", ".local/state").join("eep/roots"),
            data: data_home.join("eep"),
            launchers: data_home.join("applications"),
            config: xdg("XDG_CONFIG_HOME", ".config").join("eep"),
            local_cache: env_path("EEP_LOCAL_CACHE")
                .unwrap_or_else(|| "/var/cache/eep/local".into()),
            driver_farm: "/var/lib/eep/opengl-driver".into(),
        }
    }

    pub fn root(&self, id: &str) -> PathBuf {
        self.roots.join(id)
    }

    pub fn prev_root(&self, id: &str) -> PathBuf {
        self.roots.join(format!("{id}.prev"))
    }

    pub fn icons(&self) -> PathBuf {
        self.data.join("icons")
    }

    pub fn bin(&self) -> PathBuf {
        self.data.join("bin")
    }

    /// `shims/<id>/<prog>` → eep: which app owns a PATH name.
    pub fn shims(&self) -> PathBuf {
        self.data.join("shims")
    }

    pub fn index_file(&self) -> PathBuf {
        self.cache.join("index.json")
    }
}

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// Write via a hidden temp file + rename in the same directory, so a
/// watcher never sees half a file (`.Name.eep.tmp` → `Name.eep`), and
/// durably: the data is synced before the rename and the directory after
/// it. Without that, a power cut soon after can leave a zero-length file
/// (seen in the VM: an empty tmpfiles rule after an unclean power-off).
pub fn write_atomic(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    use std::io::Write;
    let dir = path.parent().context("path without a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let name = path
        .file_name()
        .context("path without a file name")?
        .to_string_lossy();
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::File::create(&tmp).with_context(|| format!("write {}", tmp.display()))?;
    f.write_all(data)
        .and_then(|()| f.sync_all())
        .with_context(|| format!("write {}", tmp.display()))?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("rename {}", path.display()))?;
    // The rename itself is only durable once the directory is synced.
    std::fs::File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("sync {}", dir.display()))?;
    Ok(())
}
