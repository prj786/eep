//! `eep-build cards --release R`: a ready-to-install card per app
//! (`dist/<R>/cards/<Name>.eep`, icon inside) and `dist/<R>/catalog.json`
//! for Komble and the website. Putting such a card into ~/Applications —
//! from a download, from Komble — is installing the app.

use std::path::Path;

use anyhow::{Context, Result};
use eep_core::card::{self, Card};
use eep_core::catalog::Kind;
use eep_core::index::Index;
use serde_json::{Value, json};

use crate::util::write_atomic;

pub fn run(dist: &Path, release: &str) -> Result<Value> {
    let dir = dist.join(release);
    let idx: Index = serde_json::from_slice(
        &std::fs::read(dir.join("index.json")).context("build the index first")?,
    )?;
    let made = card::datetime(&idx.released)?;
    let cards_dir = dir.join("cards");
    if cards_dir.exists() {
        std::fs::remove_dir_all(&cards_dir)?; // only eep-build's own output
    }
    let mut apps = Vec::new();
    for (id, app) in &idx.apps {
        if app.unavailable.is_some() || matches!(app.kind, Kind::Tool | Kind::System) {
            continue;
        }
        let mut c = Card {
            eep: card::VERSION,
            id: id.clone(),
            name: app.name.clone(),
            kind: app.kind,
            channel: idx.channel.clone(),
            release: idx.release.clone(),
            version: app.version.clone(),
            out: app.out.clone(),
            exec: app.exec.clone(),
            desktop: app.desktop.clone(),
            icon: app.icon_path.clone(),
            bin: app.bin.clone(),
            previous: None,
            previous_version: None,
            previous_release: None,
            installed: made,
            icon_type: None,
            icon_data: None,
        };
        if let Some(icon) = &app.icon
            && let Ok(bytes) = std::fs::read(dir.join(icon))
        {
            c.embed_icon(icon, &bytes);
        }
        let file = c.file_name();
        write_atomic(&cards_dir.join(&file), c.to_toml().as_bytes())?;
        apps.push(json!({
            "id": id, "name": app.name, "kind": app.kind, "version": app.version,
            "description": app.description, "homepage": app.homepage,
            "download": app.download, "disk": app.disk,
            "icon": app.icon, "card": format!("cards/{file}"),
        }));
    }
    apps.sort_by_key(|a| a["name"].as_str().unwrap_or_default().to_lowercase());
    let catalog = json!({
        "eep_catalog": 0, "channel": idx.channel, "release": idx.release,
        "released": idx.released, "apps": apps,
    });
    write_atomic(
        &dir.join("catalog.json"),
        serde_json::to_string_pretty(&catalog)?.as_bytes(),
    )?;
    Ok(
        json!({ "ok": true, "release": idx.release, "cards": catalog["apps"].as_array().map_or(0, Vec::len), "catalog": dir.join("catalog.json") }),
    )
}
