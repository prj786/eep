//! Surviving a power cut in the middle of an install.
//!
//! Nix marks a downloaded path valid before its files have surely reached
//! the disk; a power cut right then leaves a "valid" path with empty files
//! (seen in the VM: mesa's EGL vendor file at 0 bytes, so no app had GL).
//! Nix's cure, `fsync-store-paths`, fsyncs every file of every package —
//! tens of thousands of fsyncs per install. eep's is cheaper:
//!
//! - before fetching, write a small journal (durably) saying when it began;
//! - after fetching, one `syncfs` of the store, then drop the journal;
//! - a journal still there at the next start means the machine died while
//!   fetching: verify only what Nix registered since then, and delete what
//!   is damaged (nothing roots it yet), so the next install fetches it again.
//!
//! An error or a panic is not a power cut: the guard still syncs and drops
//! the journal on the way out. Only a real crash leaves it behind.

use std::path::PathBuf;

use anyhow::{Context, Result};
use eep_core::par::par_map;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::nix;
use crate::paths::{Paths, write_atomic};

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Unix seconds when fetching began.
    since: i64,
    what: String,
}

fn file(p: &Paths) -> PathBuf {
    p.roots.parent().map_or_else(
        || p.roots.join("inflight.json"),
        |d| d.join("inflight.json"),
    )
}

/// Open while fetching; see the module docs.
#[derive(Debug)]
pub struct Guard {
    file: PathBuf,
}

pub fn begin(p: &Paths, what: &str) -> Result<Guard> {
    let file = file(p);
    let entry = Entry {
        since: eep_core::time::now_unix(),
        what: what.to_owned(),
    };
    write_atomic(&file, &serde_json::to_vec(&entry)?)?;
    Ok(Guard { file })
}

impl Drop for Guard {
    fn drop(&mut self) {
        nix::sync_store();
        let _ = std::fs::remove_file(&self.file);
    }
}

/// After a crash: check what was being fetched and remove the damage.
/// `None` when there was nothing to recover.
pub fn recover(p: &Paths) -> Result<Option<Value>> {
    let file = file(p);
    let Ok(bytes) = std::fs::read(&file) else {
        return Ok(None);
    };
    let entry: Option<Entry> = serde_json::from_slice(&bytes).ok();
    // An unreadable journal is itself crash damage: check the last day.
    let since = entry
        .as_ref()
        .map_or(eep_core::time::now_unix() - 86_400, |e| e.since)
        - 60;
    let paths = nix::registered_since(since)?;
    let checks = par_map(&paths, 4, |p| nix::verify_path(p).unwrap_or(false));
    let damaged: Vec<String> = paths
        .iter()
        .zip(&checks)
        .filter(|(_, ok)| !**ok)
        .map(|(p, _)| p.clone())
        .collect();
    let mut deleted = Vec::new();
    let mut error = None;
    if !damaged.is_empty() {
        // Anything that refers to a damaged path goes too (it is just as new).
        deleted = nix::referrers_closure(&damaged)?;
        if let Err(e) = nix::delete(&deleted) {
            error = Some(format!("{e:#}"));
        }
    }
    nix::sync_store();
    std::fs::remove_file(&file).context("removing the journal")?;
    Ok(Some(json!({
        "after": entry.map(|e| e.what),
        "checked": paths.len(),
        "damaged": damaged,
        "deleted": deleted.len(),
        "error": error,
    })))
}
