//! Making an installed app part of the desktop, and undoing it: launcher
//! entries, the icon copy, PATH shims. All of it is derived from the card,
//! so `eep repair` can always rebuild it.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use eep_core::card::Card;
use eep_core::desktop;
use serde::Serialize;

use crate::paths::{Paths, write_atomic};

#[derive(Debug, Default, Serialize)]
pub struct Integrated {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub launchers: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub icon: Option<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shims: Vec<String>,
    /// Programs another app already provides (left alone).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shim_conflicts: Vec<String>,
}

pub fn integrate(p: &Paths, card: &Card, eep: &Path) -> Result<Integrated> {
    if !eep_core::valid_id(&card.id) {
        anyhow::bail!("refusing a bad app id {:?}", card.id);
    }
    let out = PathBuf::from(&card.out);
    let mut done = Integrated::default();

    // Icon: copied, so the launcher entry survives the store path changing.
    delete_where(&p.icons(), |n| is_icon_of(n, &card.id))?;
    if let Some(icon) = &card.icon {
        let src = if icon.starts_with('/') {
            PathBuf::from(icon)
        } else {
            out.join(icon)
        };
        let ext = src
            .extension()
            .map_or("png".into(), |e| e.to_string_lossy().to_ascii_lowercase());
        let dst = p.icons().join(format!("{}.{ext}", card.id));
        match std::fs::read(&src) {
            Ok(bytes) => {
                write_atomic(&dst, &bytes)?;
                done.icon = Some(dst);
            }
            Err(e) => eprintln!("eep: {}: icon {}: {e}", card.id, src.display()),
        }
    }

    // Launcher entries: the main one, plus hidden helpers (URL and MIME
    // handlers marked NoDisplay) so links and file types still open.
    delete_launchers(p, &card.id)?;
    if let Some(rel) = &card.desktop {
        let main = out.join(rel);
        let icon = done.icon.as_ref().map(|i| i.to_string_lossy().into_owned());
        let text = std::fs::read_to_string(&main)
            .with_context(|| format!("reading {}", main.display()))?;
        let main_name = main
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dst = launcher_path(p, &card.id, &main_name);
        write_atomic(&dst, rewrite(&text, card, eep, icon.as_deref()).as_bytes())?;
        done.launchers.push(dst);
        if let (Some(dir), Some(main_name)) = (main.parent(), main.file_name())
            && let Ok(rd) = std::fs::read_dir(dir)
        {
            let mut extra: Vec<PathBuf> = rd
                .flatten()
                .map(|e| e.path())
                .filter(|f| {
                    f.extension().is_some_and(|e| e == "desktop")
                        && f.file_name() != Some(main_name)
                })
                .collect();
            extra.sort();
            for f in extra {
                let Ok(text) = std::fs::read_to_string(&f) else {
                    continue;
                };
                if desktop::get(&text, "Desktop Entry", "NoDisplay").as_deref() != Some("true") {
                    continue;
                }
                let name = f
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dst = launcher_path(p, &card.id, &name);
                write_atomic(&dst, rewrite(&text, card, eep, icon.as_deref()).as_bytes())?;
                done.launchers.push(dst);
            }
        }
        refresh_desktop_database(&p.launchers);
    }

    // PATH shims: `bin/<prog>` → `../shims/<id>/<prog>` → eep itself. eep
    // started under a program's name runs it through `eep run`, so a
    // command-line app gets the same clean environment as a launcher one;
    // the middle link says which app owns the name.
    delete_shims(p, &card.id)?;
    if !card.bin.is_empty() {
        std::fs::create_dir_all(p.bin())?;
        std::fs::create_dir_all(p.shims().join(&card.id))?;
    }
    for b in &card.bin {
        let link = p.bin().join(b);
        let owner_link = p.shims().join(&card.id).join(b);
        if let Some(other) = shim_owner(p, b)
            && other != card.id
            && p.shims().join(&other).join(b).symlink_metadata().is_ok()
        {
            done.shim_conflicts.push(b.clone());
            continue;
        }
        let _ = std::fs::remove_file(&link);
        let _ = std::fs::remove_file(&owner_link);
        symlink(eep, &owner_link).with_context(|| format!("shim {}", owner_link.display()))?;
        symlink(Path::new("../shims").join(&card.id).join(b), &link)
            .with_context(|| format!("shim {}", link.display()))?;
        done.shims.push(b.clone());
    }
    Ok(done)
}

/// The app that owns PATH name `prog` (from `bin/<prog>`'s link).
pub fn shim_owner(p: &Paths, prog: &str) -> Option<String> {
    let target = std::fs::read_link(p.bin().join(prog)).ok()?;
    let mut parts = target
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned());
    match (parts.next()?.as_str(), parts.next()?.as_str()) {
        ("..", "shims") => parts.next(),
        _ => None,
    }
}

/// Undo [`integrate`] and drop the app's GC roots. The pieces stay in the
/// store until `eep gc`, so a card restored from Trash comes back instantly.
pub fn forget(p: &Paths, id: &str) -> Result<serde_json::Value> {
    if !eep_core::valid_id(id) {
        anyhow::bail!("refusing a bad app id {id:?}");
    }
    let launchers = delete_launchers(p, id)?;
    if launchers > 0 {
        refresh_desktop_database(&p.launchers);
    }
    let shims = delete_shims(p, id)?;
    let icons = delete_where(&p.icons(), |n| is_icon_of(n, id))?;
    let mut roots = 0;
    for r in [p.root(id), p.prev_root(id)] {
        if r.symlink_metadata().is_ok() {
            std::fs::remove_file(&r).with_context(|| format!("removing {}", r.display()))?;
            roots += 1;
        }
    }
    Ok(serde_json::json!({
        "id": id, "launchers": launchers, "shims": shims, "icons": icons, "roots": roots
    }))
}

/// Apps eep has launcher entries, roots or icons for (to find leftovers).
pub fn known_ids(p: &Paths) -> std::collections::BTreeSet<String> {
    let mut ids = std::collections::BTreeSet::new();
    let names = |dir: &Path| -> Vec<String> {
        std::fs::read_dir(dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default()
    };
    ids.extend(launcher_owners(p).into_keys());
    for n in names(&p.roots).into_iter().chain(names(&p.shims())) {
        ids.insert(n.trim_end_matches(".prev").to_owned());
    }
    for n in names(&p.icons()) {
        if let Some((id, _)) = n.rsplit_once('.') {
            ids.insert(id.to_owned());
        }
    }
    ids.retain(|i| eep_core::valid_id(i));
    ids
}

fn rewrite(text: &str, card: &Card, eep: &Path, icon: Option<&str>) -> String {
    let eep = desktop::quote_arg(&eep.to_string_lossy());
    desktop::rewrite(text, &card.id, icon, |value| {
        let mut s = format!("{eep} run {}", card.id);
        if let Some(x) = desktop::split_exec(value) {
            for e in &x.env {
                s.push_str(" --env ");
                s.push_str(&desktop::quote_arg(e));
            }
            s.push_str(" --exec ");
            s.push_str(&desktop::quote_arg(&x.program));
            if !x.rest.is_empty() {
                s.push_str(" -- ");
                s.push_str(&x.rest);
            }
        }
        s
    })
}

/// Delete the icon copies eep keeps for `id`.
pub(crate) fn delete_icons(p: &Paths, id: &str) -> Result<usize> {
    delete_where(&p.icons(), |n| is_icon_of(n, id))
}

/// `firefox.png` is firefox's icon; `firefox-beta.png` is not.
fn is_icon_of(name: &str, id: &str) -> bool {
    name.strip_prefix(id).is_some_and(|r| r.starts_with('.'))
}

pub(crate) fn delete_launchers(p: &Paths, id: &str) -> Result<usize> {
    let mine = launcher_owners(p).remove(id).unwrap_or_default();
    for f in &mine {
        std::fs::remove_file(f).with_context(|| format!("removing {}", f.display()))?;
    }
    Ok(mine.len())
}

/// The launcher entries eep wrote, by app id (their `X-Eep-Id`).
pub fn launcher_owners(p: &Paths) -> std::collections::BTreeMap<String, Vec<PathBuf>> {
    let mut owners: std::collections::BTreeMap<String, Vec<PathBuf>> = Default::default();
    let Ok(rd) = std::fs::read_dir(&p.launchers) else {
        return owners;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.extension().is_none_or(|x| x != "desktop") {
            continue;
        }
        if let Some(id) = owner_of(&path) {
            owners.entry(id).or_default().push(path);
        }
    }
    owners
}

fn owner_of(entry: &Path) -> Option<String> {
    desktop::get(
        &std::fs::read_to_string(entry).ok()?,
        "Desktop Entry",
        "X-Eep-Id",
    )
}

/// Where an app's launcher entry `name` goes: under its own name, so the
/// window's app id matches it (the bar's title, the dock's icon, default
/// apps in mimeapps.list all go by that name). Only when the user or
/// another app already has a file of that name does it get a prefix.
pub(crate) fn launcher_path(p: &Paths, id: &str, name: &str) -> PathBuf {
    let dst = p.launchers.join(name);
    if !dst.exists() || owner_of(&dst).as_deref() == Some(id) {
        return dst;
    }
    p.launchers.join(format!("eep-{id}--{name}"))
}

/// Delete this app's shims (and ones from before the two-level layout,
/// which pointed straight into the app's GC root).
fn delete_shims(p: &Paths, id: &str) -> Result<usize> {
    if !eep_core::valid_id(id) {
        anyhow::bail!("refusing a bad app id {id:?}");
    }
    let root = p.root(id);
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(p.bin()) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let legacy = std::fs::read_link(e.path()).is_ok_and(|t| t.starts_with(&root));
            if legacy || shim_owner(p, &name).as_deref() == Some(id) {
                std::fs::remove_file(e.path())?;
                n += 1;
            }
        }
    }
    let dir = p.shims().join(id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(n)
}

/// Delete the files in `dir` whose names match.
fn delete_where(dir: &Path, matches: impl Fn(&str) -> bool) -> Result<usize> {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Ok(0);
    };
    for e in rd.flatten() {
        if matches(&e.file_name().to_string_lossy()) {
            std::fs::remove_file(e.path())
                .with_context(|| format!("removing {}", e.path().display()))?;
            n += 1;
        }
    }
    Ok(n)
}

/// MIME associations (`MimeType=`) need the cache rebuilt; best effort.
pub(crate) fn refresh_desktop_database(dir: &Path) {
    let _ = std::process::Command::new("update-desktop-database")
        .arg(dir)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
}

#[cfg(test)]
mod tests {
    use super::*;
    use eep_core::card::datetime;
    use eep_core::catalog::Kind;

    #[test]
    fn exec_lines() {
        let card = Card {
            eep: 0,
            id: "gimp".into(),
            name: "GIMP".into(),
            kind: Kind::Gui,
            source: Default::default(),
            from: None,
            entry: None,
            package: None,
            url: None,
            sha256: None,
            size: None,
            repo: None,
            channel: "c".into(),
            release: "r".into(),
            version: "3".into(),
            out: "/nix/store/yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-gimp-3".into(),
            exec: None,
            desktop: None,
            icon: None,
            bin: vec![],
            previous: None,
            previous_version: None,
            previous_release: None,
            icon_type: None,
            icon_data: None,
            installed: datetime("2026-11-23T10:00:00Z").expect("date"),
        };
        let text = "[Desktop Entry]\nExec=env GDK_BACKEND=x11 gimp-3.0 %U\nName=GIMP\n";
        let out = rewrite(
            text,
            &card,
            Path::new("/home/a b/.local/bin/eep"),
            Some("/i/gimp.png"),
        );
        assert!(
            out.contains(
                "Exec=\"/home/a b/.local/bin/eep\" run gimp --env GDK_BACKEND=x11 --exec gimp-3.0 -- %U\n"
            ),
            "{out}"
        );
        let out = rewrite(
            "[Desktop Entry]\nExec=gimp\n",
            &card,
            Path::new("/usr/bin/eep"),
            None,
        );
        assert!(
            out.contains("Exec=/usr/bin/eep run gimp --exec gimp\n"),
            "{out}"
        );
    }

    #[test]
    fn icon_names() {
        assert!(is_icon_of("firefox.png", "firefox"));
        assert!(!is_icon_of("firefox-beta.png", "firefox"));
        assert!(!is_icon_of("firefox", "firefox"));
    }
}
