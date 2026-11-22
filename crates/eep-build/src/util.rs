//! Small helpers: the cache directory, hashing, atomic writes.

use std::path::PathBuf;

pub use eep_core::par::par_map;

/// `$EEP_BUILD_CACHE`, else `$XDG_CACHE_HOME/eep-build`, else `~/.cache/eep-build`.
pub fn cache_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("EEP_BUILD_CACHE") {
        return PathBuf::from(d);
    }
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| "/tmp".into())).join(".cache")
        });
    base.join("eep-build")
}

pub fn sha256_hex(data: &[u8]) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Write `data` to `path` via a temp file + rename (never a half file).
pub fn write_atomic(path: &std::path::Path, data: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    }
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("rename to {}", path.display()))?;
    Ok(())
}
