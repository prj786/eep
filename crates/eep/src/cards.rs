//! The cards in `~/Applications`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eep_core::card::{self, Card};

use crate::paths::{Paths, write_atomic};

#[derive(Debug)]
pub struct Found {
    pub file: PathBuf,
    pub card: Card,
}

/// Every readable card, sorted by file name; unreadable `.eep` files are
/// returned separately with the reason.
pub fn all(p: &Paths) -> (Vec<Found>, Vec<(PathBuf, String)>) {
    let mut good = Vec::new();
    let mut bad = Vec::new();
    let Ok(rd) = std::fs::read_dir(&p.apps) else {
        return (good, bad);
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|f| is_card_file(f))
        .collect();
    files.sort();
    for file in files {
        match std::fs::read_to_string(&file)
            .map_err(anyhow::Error::from)
            .and_then(|t| Card::parse(&t))
        {
            Ok(card) => good.push(Found { file, card }),
            Err(e) => bad.push((file, format!("{e:#}"))),
        }
    }
    (good, bad)
}

/// `Name.eep`, not hidden (our temp files are `.Name.eep.tmp`).
pub fn is_card_file(f: &Path) -> bool {
    f.extension().is_some_and(|e| e == card::EXTENSION)
        && f.file_name()
            .is_some_and(|n| !n.to_string_lossy().starts_with('.'))
        && f.is_file()
}

/// The card for `id`. With two cards for one id (a copy), the first by
/// file name wins.
pub fn find(p: &Paths, id: &str) -> Option<Found> {
    all(p).0.into_iter().find(|f| f.card.id == id)
}

/// Write `card` to `file`, or to a fresh `~/Applications/<Name>.eep` that doesn't
/// clobber another app's card.
pub fn save(p: &Paths, card: &Card, file: Option<&Path>) -> Result<PathBuf> {
    let path = match file {
        Some(f) => f.to_path_buf(),
        None => free_name(p, card)?,
    };
    write_atomic(&path, card.to_toml().as_bytes()).context("writing the card")?;
    Ok(path)
}

fn free_name(p: &Paths, card: &Card) -> Result<PathBuf> {
    let first = p.apps.join(card.file_name());
    if !first.exists() {
        return Ok(first);
    }
    let stem = card.file_name().trim_end_matches(".eep").to_owned();
    (2..1000)
        .map(|n| p.apps.join(format!("{stem} {n}.eep")))
        .find(|f| !f.exists())
        .context("no free card name")
}

/// The cards' folder was `~/Applications` until 2026-11-22; it is `~/Applications`
/// now. Move the cards over once (the whole folder when Applications
/// doesn't exist yet). Costs one `stat` when there is nothing to move.
pub fn migrate_old_folder(p: &Paths) {
    if std::env::var_os("EEP_APPS_DIR").is_some() {
        return;
    }
    let Some(old) = p.apps.parent().map(|h| h.join("Apps")) else {
        return;
    };
    if !old.is_dir() {
        return;
    }
    let cards: Vec<PathBuf> = std::fs::read_dir(&old)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|f| is_card_file(f))
                .collect()
        })
        .unwrap_or_default();
    if cards.is_empty() {
        return;
    }
    if !p.apps.exists() && std::fs::rename(&old, &p.apps).is_ok() {
        return;
    }
    let _ = std::fs::create_dir_all(&p.apps);
    for c in cards {
        if let Some(name) = c.file_name()
            && !p.apps.join(name).exists()
        {
            let _ = std::fs::rename(&c, p.apps.join(name));
        }
    }
}
