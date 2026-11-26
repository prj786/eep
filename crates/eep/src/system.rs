//! Every app gets a card, not only eep's own: apps that were on the
//! computer already — pacman packages, Komble's AppImages, Steam games,
//! the user's own launcher entries, ewe's apps — are *found* and get a
//! `source = "system"` card, so `~/Applications` is the whole app list.
//!
//! A found app is its launcher entry (`gimp.desktop`). Nothing is
//! downloaded and nothing is uninstalled:
//!
//! | The card…                         | eep…                                                     |
//! |-----------------------------------|----------------------------------------------------------|
//! | appears (a new app was installed) | — (it wrote the card)                                    |
//! | leaves (Trash, delete)            | hides the app: a copy of its entry with `NoDisplay=true` |
//! | comes back (restored)             | shows it again (the copy goes, the user's file returns)  |
//! | — the app was uninstalled         | deletes the card                                         |
//!
//! Hiding keeps the app's file types working (a `NoDisplay` entry still
//! opens files; `Hidden=true` would make it vanish for GIO too). The copy
//! is marked `X-Eep-Hides=<id>`, and eep only ever deletes or replaces a
//! launcher entry that carries its own mark. A user-level entry (in
//! `~/.local/share/applications`) is first copied byte for byte to
//! `~/.local/share/eep/hidden/` and put back from there. `found.json`
//! remembers which entries eep found, so a hidden app is not found again.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use eep_core::card::{self, Card, ICON_MAX, SYSTEM_PREFIX, Source};
use eep_core::catalog::Kind;
use eep_core::desktop;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cards;
use crate::paths::{Paths, write_atomic};

/// eep's mark on the copy that hides an app: `X-Eep-Hides=<card id>`.
const HIDES: &str = "X-Eep-Hides";

/// Another app's mark on an entry it owns: `X-Eep-Skip=true` — no card.
const SKIP: &str = "X-Eep-Skip";

/// Where launcher entries and icons are looked for.
#[derive(Debug, Clone)]
pub struct Dirs {
    /// `$XDG_DATA_HOME/applications`: first in line; eep's copies go here.
    pub user: PathBuf,
    /// `$XDG_DATA_DIRS/*/applications`, in order.
    pub system: Vec<PathBuf>,
    /// Icon theme bases: `$XDG_DATA_HOME/icons`, `~/.icons`, `$XDG_DATA_DIRS/*/icons`.
    pub icons: Vec<PathBuf>,
    /// `$XDG_DATA_DIRS/*/pixmaps`.
    pub pixmaps: Vec<PathBuf>,
    /// The icon theme (GTK's setting), searched after hicolor and pixmaps.
    pub theme: Option<String>,
}

impl Dirs {
    pub fn from_env(p: &Paths) -> Self {
        let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
        let data_home = p
            .launchers
            .parent()
            .map_or_else(|| home.join(".local/share"), Path::to_path_buf);
        let mut data: Vec<PathBuf> = Vec::new();
        let list = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        for d in list.split(':') {
            let d = PathBuf::from(d.trim_end_matches('/'));
            // ewe's shell puts LF copies of CRLF entries in a cache dir; the
            // originals are what counts.
            if d.is_absolute()
                && !d.to_string_lossy().contains("/.cache/")
                && d != data_home
                && !data.contains(&d)
            {
                data.push(d);
            }
        }
        Self {
            user: p.launchers.clone(),
            system: data.iter().map(|d| d.join("applications")).collect(),
            icons: [data_home.join("icons"), home.join(".icons")]
                .into_iter()
                .chain(data.iter().map(|d| d.join("icons")))
                .collect(),
            pixmaps: data.iter().map(|d| d.join("pixmaps")).collect(),
            theme: gtk_icon_theme(&home),
        }
    }

    /// The applications folders, first in line first.
    pub fn apps_dirs(&self) -> Vec<PathBuf> {
        std::iter::once(self.user.clone())
            .chain(self.system.iter().cloned())
            .collect()
    }
}

fn gtk_icon_theme(home: &Path) -> Option<String> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|c| c.is_absolute())
        .unwrap_or_else(|| home.join(".config"));
    let text = std::fs::read_to_string(config.join("gtk-3.0/settings.ini")).ok()?;
    desktop::get(&text, "Settings", "gtk-icon-theme-name")
        .filter(|t| !t.is_empty() && !t.contains('/') && t != "..")
}

/// Every desktop file id and its files, first in line first.
fn scan(dirs: &Dirs) -> BTreeMap<String, Vec<PathBuf>> {
    let mut out: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for base in dirs.apps_dirs() {
        collect(&base, "", 0, &mut out);
    }
    out
}

/// Desktop file ids of a subfolder are `sub-name.desktop` (the spec).
fn collect(dir: &Path, prefix: &str, depth: usize, out: &mut BTreeMap<String, Vec<PathBuf>>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut items: Vec<(String, PathBuf)> = rd
        .flatten()
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .filter(|(n, _)| !n.starts_with('.'))
        .collect();
    items.sort();
    for (name, path) in items {
        if path.is_dir() {
            if depth < 2 {
                collect(&path, &format!("{prefix}{name}-"), depth + 1, out);
            }
            continue;
        }
        if name.ends_with(".desktop") && path.is_file() {
            out.entry(format!("{prefix}{name}")).or_default().push(path);
        }
    }
}

/// What a launcher entry is to eep.
#[derive(Debug, PartialEq, Eq)]
enum Look {
    /// An app the launcher shows.
    App,
    /// eep's copy hiding the app with this card id.
    Ours(String),
    /// The launcher entry of an app eep installed from the cache.
    EepApp,
    /// Not shown in the launcher (helper, NoDisplay, another desktop's…).
    NotApp,
}

/// An app is what ewe's launcher shows: `Type=Application`, a name, not
/// `NoDisplay`/`Hidden`, and a `TryExec` that exists (that app could not
/// start anyway). `OnlyShowIn`/`NotShowIn` are NOT checked: the launcher
/// (Quickshell) shows those entries, and every app it shows gets a card,
/// so the user can remove it like any other.
fn look(text: &str, _dirs: &Dirs) -> Look {
    let get = |k: &str| desktop::get(text, "Desktop Entry", k);
    if let Some(id) = get(HIDES) {
        return Look::Ours(id);
    }
    if get("X-Eep-Id").is_some() {
        return Look::EepApp;
    }
    let yes = |k: &str| get(k).is_some_and(|v| v.eq_ignore_ascii_case("true"));
    // Entries another ewe app writes and owns (ewe Mobile's phone apps,
    // `X-Ewe-Mobile-App`): shown in the launcher, but no card — the card's
    // Trash would hide an entry its owner rewrites and deletes by itself.
    if yes(SKIP) {
        return Look::NotApp;
    }
    let app = get("Type").as_deref() == Some("Application")
        && !yes("NoDisplay")
        && !yes("Hidden")
        && get("Name").is_some_and(|n| !n.is_empty())
        && get("TryExec").is_none_or(|t| on_path(&t));
    if app { Look::App } else { Look::NotApp }
}

fn on_path(prog: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let exe = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if prog.starts_with('/') {
        return exe(Path::new(prog));
    }
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/local/bin:/usr/bin".into());
    std::env::split_paths(&path).any(|d| exe(&d.join(prog)))
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok()
}

/// The name the launcher shows for `entry` (for reports).
fn entry_name(entries: &BTreeMap<String, Vec<PathBuf>>, entry: &str) -> String {
    entries
        .get(entry)
        .and_then(|paths| paths.iter().find_map(|f| read(f)))
        .and_then(|t| desktop::get(&t, "Desktop Entry", "Name"))
        .unwrap_or_else(|| entry.trim_end_matches(".desktop").to_owned())
}

// ── found.json ───────────────────────────────────────────────────────────

#[derive(Debug, Default, Serialize, Deserialize)]
struct Found {
    #[serde(default)]
    apps: BTreeMap<String, Rec>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Rec {
    /// The desktop file id.
    entry: String,
    /// Its card left `~/Applications`: eep hides the app.
    #[serde(default)]
    hidden: bool,
}

fn found_file(p: &Paths) -> PathBuf {
    p.state_dir().join("found.json")
}

/// An unreadable file starts empty: eep's marked copies bring the hidden
/// apps back into it (see [`pass_with`]).
fn load(p: &Paths) -> Found {
    std::fs::read(found_file(p))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn save(p: &Paths, f: &Found) -> Result<()> {
    write_atomic(&found_file(p), &serde_json::to_vec_pretty(f)?)
}

// ── one pass ─────────────────────────────────────────────────────────────

/// Where a found app stands right now.
#[derive(Debug)]
enum Now {
    /// Its entry is in the launcher.
    Shown(PathBuf),
    /// eep's copy hides it; `source` is what the copy was made from (the
    /// user's own file eep put aside, or the next entry in line), `None`
    /// when that is gone.
    Hidden {
        copy: PathBuf,
        source: Option<PathBuf>,
    },
    /// An app eep installed from the cache took its place.
    Shadowed,
    /// Uninstalled, or no longer an app the launcher shows.
    Gone,
}

fn now(
    p: &Paths,
    dirs: &Dirs,
    entries: &BTreeMap<String, Vec<PathBuf>>,
    id: &str,
    entry: &str,
) -> Now {
    let Some(paths) = entries.get(entry) else {
        return Now::Gone;
    };
    let Some(text) = read(&paths[0]) else {
        return Now::Gone;
    };
    match look(&text, dirs) {
        Look::App => Now::Shown(paths[0].clone()),
        Look::Ours(x) if x == id => {
            let stash = p.hidden().join(entry);
            let source = if paths[0] == dirs.user.join(entry) && stash.is_file() {
                Some(stash)
            } else {
                paths.get(1).cloned()
            };
            // the source must still be an app (its owner may have retired it)
            let source = source.filter(|s| read(s).is_some_and(|t| look(&t, dirs) == Look::App));
            Now::Hidden {
                copy: paths[0].clone(),
                source,
            }
        }
        Look::EepApp | Look::Ours(_) => Now::Shadowed,
        Look::NotApp => Now::Gone,
    }
}

/// Names of the apps one pass changed.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub found: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub shown: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub gone: Vec<String>,
}

impl Report {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.found.is_empty()
            && self.hidden.is_empty()
            && self.shown.is_empty()
            && self.gone.is_empty()
    }
}

/// Who owns these files (pacman), as (package, version).
pub type Owners = dyn Fn(&[PathBuf]) -> HashMap<PathBuf, (String, String)>;

/// Bring found apps in line with the cards: hide the apps whose card left,
/// show the ones whose card came back, drop the cards of uninstalled apps,
/// and (with `find`) give every new app a card. Run under eep's lock.
pub fn pass(p: &Paths, find: bool) -> Result<Report> {
    pass_with(p, &Dirs::from_env(p), find, &pacman_owners)
}

pub fn pass_with(p: &Paths, dirs: &Dirs, find: bool, owners: &Owners) -> Result<Report> {
    // No ~/Applications (moved, renamed, not made yet): change nothing, and
    // don't make it either — the user may be moving it back.
    if !p.apps.is_dir() {
        return Ok(Report::default());
    }
    let listing = cards::listing(p);
    let mut st = load(p);
    let entries = scan(dirs);
    let mut cards: BTreeMap<String, cards::Found> = BTreeMap::new();
    for f in listing.good.iter() {
        if f.card.is_system() {
            cards
                .entry(f.card.id.clone())
                .or_insert_with(|| cards::Found {
                    file: f.file.clone(),
                    card: f.card.clone(),
                });
        }
    }
    let mut changed = false;
    let mut launchers = false;
    let mut report = Report::default();

    // eep's own copies say which apps it hides, even without found.json
    for (entry, paths) in &entries {
        if let Some(Look::Ours(id)) = read(&paths[0]).map(|t| look(&t, dirs))
            && eep_core::valid_id(&id)
            && id.starts_with(SYSTEM_PREFIX)
            && !st.apps.contains_key(&id)
        {
            let rec = Rec {
                entry: entry.clone(),
                hidden: true,
            };
            st.apps.insert(id, rec);
            changed = true;
        }
    }
    // cards eep has no record of (copied in, or found.json was lost)
    for (id, f) in &cards {
        let Some(entry) = &f.card.entry else { continue };
        if st.apps.contains_key(id) {
            continue;
        }
        let hidden = match now(p, dirs, &entries, id, entry) {
            Now::Shown(_) => false,
            Now::Hidden { .. } => true,
            // not on this computer: leave the card alone, never adopt it
            Now::Shadowed | Now::Gone => continue,
        };
        st.apps.insert(
            id.clone(),
            Rec {
                entry: entry.clone(),
                hidden,
            },
        );
        changed = true;
    }

    let ids: Vec<String> = st.apps.keys().cloned().collect();
    for id in ids {
        let rec = st.apps[&id].clone();
        let card = cards
            .get(&id)
            .filter(|f| f.card.entry.as_deref() == Some(rec.entry.as_str()));
        // a card that doesn't parse (a typo, a newer format) still keeps its app
        let present = card.is_some() || listing.kept.contains(&id);
        let name = card.map_or_else(|| entry_name(&entries, &rec.entry), |f| f.card.name.clone());
        match now(p, dirs, &entries, &id, &rec.entry) {
            Now::Gone | Now::Shadowed => {
                // uninstalled (or an eep app took its place): the card goes too
                if let Some(f) = card {
                    std::fs::remove_file(&f.file)
                        .with_context(|| format!("removing {}", f.file.display()))?;
                }
                st.apps.remove(&id);
                report.gone.push(name);
                changed = true;
            }
            Now::Shown(path) => {
                if present {
                    if rec.hidden {
                        set_hidden(&mut st, &id, false);
                        changed = true;
                    }
                } else if listing.sure {
                    hide(p, dirs, &id, &rec.entry, &path)?;
                    set_hidden(&mut st, &id, true);
                    report.hidden.push(name);
                    changed = true;
                    launchers = true;
                }
            }
            Now::Hidden { copy, source: None } => {
                // uninstalled while hidden: eep's copy goes, and the card
                remove_ours(&copy, &id)?;
                if let Some(f) = card {
                    std::fs::remove_file(&f.file)
                        .with_context(|| format!("removing {}", f.file.display()))?;
                }
                st.apps.remove(&id);
                report.gone.push(name);
                changed = true;
                launchers = true;
            }
            Now::Hidden {
                copy,
                source: Some(src),
            } => {
                if present {
                    unhide(p, dirs, &id, &rec.entry)?;
                    set_hidden(&mut st, &id, false);
                    report.shown.push(name);
                    changed = true;
                    launchers = true;
                } else {
                    if !rec.hidden {
                        set_hidden(&mut st, &id, true);
                        changed = true;
                    }
                    // a package update changed the entry: hide the new one
                    if src != p.hidden().join(&rec.entry) && newer(&src, &copy) {
                        hide(p, dirs, &id, &rec.entry, &src)?;
                        launchers = true;
                    }
                }
            }
        }
    }

    if find {
        let claimed: BTreeSet<String> = st
            .apps
            .values()
            .map(|r| r.entry.clone())
            .chain(cards.values().filter_map(|f| f.card.entry.clone()))
            .collect();
        let new: Vec<(String, PathBuf, String)> = entries
            .iter()
            .filter(|(e, _)| !claimed.contains(*e) && card::valid_entry(e))
            .filter_map(|(e, paths)| {
                let text = read(&paths[0])?;
                (look(&text, dirs) == Look::App).then(|| (e.clone(), paths[0].clone(), text))
            })
            .collect();
        if !new.is_empty() {
            let system: Vec<PathBuf> = new
                .iter()
                .filter(|(_, path, _)| !path.starts_with(&dirs.user))
                .map(|(_, path, _)| path.clone())
                .collect();
            let owned = owners(&system);
            let mut icons = IconFinder::new(dirs);
            let mut taken: BTreeSet<String> = st.apps.keys().chain(cards.keys()).cloned().collect();
            let installed = card::datetime(&eep_core::time::now_rfc3339())?;
            for (entry, path, text) in new {
                let id = make_id(&entry, &taken);
                taken.insert(id.clone());
                let c = make_card(
                    &id,
                    &entry,
                    &path,
                    &text,
                    dirs,
                    owned.get(&path),
                    &mut icons,
                    installed,
                );
                cards::save(p, &c, None)?;
                report.found.push(c.name.clone());
                st.apps.insert(
                    id,
                    Rec {
                        entry,
                        hidden: false,
                    },
                );
                changed = true;
            }
        }
    }

    if changed {
        save(p, &st)?;
    }
    if launchers {
        crate::integrate::refresh_desktop_database(&dirs.user);
    }
    Ok(report)
}

fn set_hidden(st: &mut Found, id: &str, hidden: bool) {
    if let Some(r) = st.apps.get_mut(id) {
        r.hidden = hidden;
    }
}

fn newer(a: &Path, b: &Path) -> bool {
    let m = |p: &Path| p.metadata().and_then(|m| m.modified()).ok();
    matches!((m(a), m(b)), (Some(x), Some(y)) if x > y)
}

/// Hide the app whose entry `entry` is now `path`: eep's marked copy with
/// `NoDisplay=true` goes first in line. The user's own entry is copied
/// aside first (byte for byte), and only then replaced, so a crash in
/// between leaves both.
fn hide(p: &Paths, dirs: &Dirs, id: &str, entry: &str, path: &Path) -> Result<()> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let dst = dirs.user.join(entry);
    if path == dst {
        write_atomic(&p.hidden().join(entry), &bytes)?;
    }
    let text = String::from_utf8_lossy(&bytes);
    let copy = desktop::set_keys(&text, &[("NoDisplay", "true"), (HIDES, id)]);
    write_atomic(&dst, copy.as_bytes())
}

/// Show the app again: eep's copy goes; the user's own entry comes back.
fn unhide(p: &Paths, dirs: &Dirs, id: &str, entry: &str) -> Result<()> {
    let dst = dirs.user.join(entry);
    if !is_ours(&dst, id) {
        return Ok(());
    }
    let stash = p.hidden().join(entry);
    if stash.is_file() {
        let bytes = std::fs::read(&stash)?;
        write_atomic(&dst, &bytes)?;
        std::fs::remove_file(&stash)?;
    } else {
        remove_ours(&dst, id)?;
    }
    Ok(())
}

fn is_ours(path: &Path, id: &str) -> bool {
    read(path).is_some_and(|t| desktop::get(&t, "Desktop Entry", HIDES).as_deref() == Some(id))
}

/// Delete a launcher entry, only if it is eep's copy for `id`.
fn remove_ours(path: &Path, id: &str) -> Result<()> {
    if is_ours(path, id) {
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    Ok(())
}

// ── cards for found apps ─────────────────────────────────────────────────

/// `sys-` + the desktop id in id letters: `org.gimp.GIMP.desktop` →
/// `sys-org-gimp-gimp`. Unique among `taken`.
fn make_id(entry: &str, taken: &BTreeSet<String>) -> String {
    let stem = entry.strip_suffix(".desktop").unwrap_or(entry);
    let mut slug = String::new();
    for c in stem.chars().map(|c| c.to_ascii_lowercase()) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let mut id = format!(
        "{SYSTEM_PREFIX}{}",
        if slug.is_empty() { "app" } else { slug }
    );
    if id.len() > 64 {
        id = format!("{}-{:08x}", id[..55].trim_end_matches('-'), fnv(entry));
    }
    if !taken.contains(&id) {
        return id;
    }
    let base = id[..id.len().min(58)].trim_end_matches('-').to_owned();
    (2..)
        .map(|n| format!("{base}-{n}"))
        .find(|c| !taken.contains(c))
        .unwrap_or(id)
}

fn fnv(s: &str) -> u32 {
    s.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

#[allow(clippy::too_many_arguments)]
fn make_card(
    id: &str,
    entry: &str,
    path: &Path,
    text: &str,
    dirs: &Dirs,
    owner: Option<&(String, String)>,
    icons: &mut IconFinder<'_>,
    installed: toml::value::Datetime,
) -> Card {
    let get = |k: &str| desktop::get(text, "Desktop Entry", k);
    let exec = get("Exec").unwrap_or_default();
    let (from, package, version) = if path.starts_with(&dirs.user) {
        let from = if entry.to_ascii_lowercase().starts_with("komble-") {
            "appimage"
        } else if exec.contains("steam://rungameid") {
            "steam"
        } else if exec.contains("flatpak run") {
            "flatpak"
        } else {
            "local"
        };
        (from, None, String::new())
    } else if path.to_string_lossy().contains("/flatpak/exports/") {
        ("flatpak", None, String::new())
    } else if let Some((pkg, ver)) = owner {
        ("pacman", Some(pkg.clone()), ver.clone())
    } else {
        ("system", None, String::new())
    };
    let mut c = Card {
        eep: card::VERSION,
        id: id.to_owned(),
        name: get("Name").unwrap_or_else(|| entry.trim_end_matches(".desktop").to_owned()),
        kind: Kind::Gui,
        source: Source::System,
        from: Some(from.to_owned()),
        entry: Some(entry.to_owned()),
        package,
        url: None,
        sha256: None,
        size: None,
        repo: None,
        channel: String::new(),
        release: String::new(),
        version,
        out: String::new(),
        exec: None,
        desktop: None,
        icon: None,
        bin: Vec::new(),
        previous: None,
        previous_version: None,
        previous_release: None,
        installed,
        icon_type: None,
        icon_data: None,
    };
    if let Some(icon) = get("Icon")
        && let Some((file, bytes)) = icons.find(&icon)
    {
        c.embed_icon(&file, &bytes);
    }
    c
}

/// `pacman -Qo` for many files in one call (pacman reads its file lists
/// once). Unowned files and a missing pacman give nothing.
pub fn pacman_owners(files: &[PathBuf]) -> HashMap<PathBuf, (String, String)> {
    let mut out = HashMap::new();
    if files.is_empty() {
        return out;
    }
    let Ok(o) = std::process::Command::new("pacman")
        .arg("-Qo")
        .args(files)
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return out;
    };
    for line in String::from_utf8_lossy(&o.stdout).lines() {
        if let Some((path, owner)) = line.split_once(" is owned by ")
            && let Some((pkg, ver)) = owner.trim().split_once(' ')
        {
            out.insert(PathBuf::from(path), (pkg.to_owned(), ver.to_owned()));
        }
    }
    out
}

/// Finds an app's icon file the way the launcher does (the freedesktop
/// icon theme spec): the user's theme, the themes it inherits, hicolor,
/// then pixmaps. Each theme's `index.theme` says which folders to look in,
/// so a theme of 200,000 files costs a few `stat`s per icon, not a walk.
/// A card carries SVG or PNG of at most [`ICON_MAX`] bytes; when the best
/// icon is bigger, ImageMagick (an ewe dependency) makes a 256 px PNG.
struct IconFinder<'a> {
    dirs: &'a Dirs,
    /// The theme chain with each theme's folders, read once.
    chain: Option<Vec<Theme>>,
}

#[derive(Debug)]
struct Theme {
    /// `<base>/<theme>` for every base that has it.
    roots: Vec<PathBuf>,
    /// (folder, rank): SVG folders first, then bigger sizes, apps first.
    folders: Vec<(String, u32)>,
}

impl<'a> IconFinder<'a> {
    fn new(dirs: &'a Dirs) -> Self {
        Self { dirs, chain: None }
    }

    fn find(&mut self, icon: &str) -> Option<(String, Vec<u8>)> {
        let found = self.candidates(icon);
        let fits = found.iter().find_map(|f| {
            let ext = f.extension()?.to_str()?.to_ascii_lowercase();
            if ext != "svg" && ext != "png" {
                return None;
            }
            let bytes = std::fs::read(f).ok()?;
            (bytes.len() <= ICON_MAX).then(|| (f.to_string_lossy().into_owned(), bytes))
        });
        fits.or_else(|| found.first().and_then(|f| shrink(f)))
    }

    fn candidates(&mut self, icon: &str) -> Vec<PathBuf> {
        if icon.starts_with('/') {
            return vec![PathBuf::from(icon)];
        }
        let name = icon
            .strip_suffix(".png")
            .or_else(|| icon.strip_suffix(".svg"))
            .or_else(|| icon.strip_suffix(".xpm"))
            .unwrap_or(icon);
        if name.is_empty() || name.contains('/') {
            return Vec::new();
        }
        let chain = self.chain.get_or_insert_with(|| theme_chain(self.dirs));
        for theme in chain.iter() {
            let mut hits: Vec<(u32, PathBuf)> = Vec::new();
            for root in &theme.roots {
                for (folder, rank) in &theme.folders {
                    for ext in ["svg", "png"] {
                        let f = root.join(folder).join(format!("{name}.{ext}"));
                        if f.is_file() {
                            hits.push((*rank, f));
                        }
                    }
                }
            }
            if !hits.is_empty() {
                // the first theme that has the icon wins (the spec)
                hits.sort_by_key(|(r, _)| std::cmp::Reverse(*r));
                return hits.into_iter().map(|(_, f)| f).collect();
            }
        }
        self.dirs
            .pixmaps
            .iter()
            .flat_map(|d| [d.join(format!("{name}.svg")), d.join(format!("{name}.png"))])
            .filter(|f| f.is_file())
            .collect()
    }
}

/// The user's theme, what it inherits (breadth first), then hicolor.
fn theme_chain(dirs: &Dirs) -> Vec<Theme> {
    let mut names: Vec<String> = Vec::new();
    let mut queue: std::collections::VecDeque<String> = dirs.theme.iter().cloned().collect();
    while let Some(t) = queue.pop_front() {
        if names.contains(&t) || names.len() >= 8 || t.contains('/') || t.starts_with('.') {
            continue;
        }
        let index = dirs
            .icons
            .iter()
            .find_map(|b| read(&b.join(&t).join("index.theme")));
        if let Some(text) = &index
            && let Some(inh) = desktop::get(text, "Icon Theme", "Inherits")
        {
            queue.extend(
                inh.split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty()),
            );
        }
        names.push(t);
    }
    names.retain(|n| n != "hicolor");
    names.push("hicolor".into());
    names
        .into_iter()
        .filter_map(|t| {
            let roots: Vec<PathBuf> = dirs
                .icons
                .iter()
                .map(|b| b.join(&t))
                .filter(|r| r.is_dir())
                .collect();
            let index = roots.iter().find_map(|r| read(&r.join("index.theme")));
            let folders = match index {
                Some(text) => theme_folders(&text),
                // hicolor without an index (a bare test tree): its usual app folders
                None if t == "hicolor" => std::iter::once(("scalable/apps".to_owned(), u32::MAX))
                    .chain([512, 256, 128, 64, 48].map(|s| (format!("{s}x{s}/apps"), s * 2)))
                    .collect(),
                None => Vec::new(),
            };
            (!roots.is_empty() && !folders.is_empty()).then_some(Theme { roots, folders })
        })
        .collect()
}

/// `Directories=` of an `index.theme`, ranked: scalable first, then by
/// size; an `Applications` folder above any other of the same kind.
fn theme_folders(index: &str) -> Vec<(String, u32)> {
    let list = desktop::get(index, "Icon Theme", "Directories").unwrap_or_default();
    let scaled = desktop::get(index, "Icon Theme", "ScaledDirectories").unwrap_or_default();
    let mut out: Vec<(String, u32)> = list
        .split(',')
        .chain(scaled.split(','))
        .map(str::trim)
        .filter(|d| !d.is_empty() && !d.contains("..") && !d.starts_with('/'))
        .map(|d| {
            let get = |k: &str| desktop::get(index, d, k);
            let size: u32 = get("Size").and_then(|s| s.parse().ok()).unwrap_or(0);
            let scale: u32 = get("Scale")
                .and_then(|s| s.parse().ok())
                .unwrap_or(1)
                .max(1);
            let apps = u32::from(get("Context").as_deref() == Some("Applications"));
            let base = if get("Type").as_deref() == Some("Scalable") {
                1 << 20
            } else {
                size.saturating_mul(scale).min((1 << 19) - 1)
            };
            (d.to_owned(), base * 2 + apps)
        })
        .collect();
    out.dedup_by(|a, b| a.0 == b.0);
    out
}

/// Too big for a card: a 256 px PNG through ImageMagick, if it is there.
pub(crate) fn shrink(f: &Path) -> Option<(String, Vec<u8>)> {
    let o = std::process::Command::new("magick")
        .args(["-background", "none", "-density", "192"])
        .arg(f)
        .args(["-resize", "256x256>", "-strip", "png:-"])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    (o.status.success() && !o.stdout.is_empty() && o.stdout.len() <= ICON_MAX)
        .then(|| ("icon.png".to_owned(), o.stdout))
}

// ── single apps: eep remove / install / run on a found app ───────────────

/// `eep remove sys-…`: the card goes, the app is hidden.
pub fn remove(p: &Paths, id: &str, find: bool) -> Result<Value> {
    let found = cards::find(p, id).filter(|f| f.card.is_system());
    if let Some(f) = &found {
        std::fs::remove_file(&f.file).with_context(|| format!("removing {}", f.file.display()))?;
    }
    let r = pass(p, find)?;
    Ok(json!({ "id": id, "card": found.map(|f| f.file), "hidden": r.hidden }))
}

/// `eep install sys-…`: a hidden app's card comes back (and the app with it).
pub fn show(p: &Paths, id: &str, find: bool) -> Result<Value> {
    if let Some(f) = cards::find(p, id) {
        let r = pass(p, find)?;
        return Ok(json!({ "id": id, "card": f.file, "shown": r.shown }));
    }
    let dirs = Dirs::from_env(p);
    let st = load(p);
    let Some(rec) = st.apps.get(id) else {
        bail!("no app {id:?} on this computer");
    };
    let entries = scan(&dirs);
    let path = match now(p, &dirs, &entries, id, &rec.entry) {
        Now::Shown(path) => path,
        Now::Hidden {
            source: Some(src), ..
        } => src,
        _ => bail!("{} is no longer on this computer", rec.entry),
    };
    let text = std::fs::read_to_string(&path)?;
    let owners = if path.starts_with(&dirs.user) || path.starts_with(p.hidden()) {
        HashMap::new()
    } else {
        pacman_owners(std::slice::from_ref(&path))
    };
    let mut icons = IconFinder::new(&dirs);
    let c = make_card(
        id,
        &rec.entry,
        &path,
        &text,
        &dirs,
        owners.get(&path),
        &mut icons,
        card::datetime(&eep_core::time::now_rfc3339())?,
    );
    let file = cards::save(p, &c, None)?;
    let r = pass(p, find)?;
    Ok(json!({ "id": id, "card": file, "shown": r.shown }))
}

/// The launcher entry a found app starts from: the first in line that is
/// not eep's hiding copy (the user's own file put aside counts).
pub fn entry_file(p: &Paths, card: &Card) -> Option<PathBuf> {
    let entry = card.entry.as_deref()?;
    let dirs = Dirs::from_env(p);
    let mut paths = scan(&dirs).remove(entry)?;
    let stash = p.hidden().join(entry);
    if stash.is_file() {
        paths.insert(0, stash);
    }
    paths
        .into_iter()
        .find(|f| read(f).is_some_and(|t| desktop::get(&t, "Desktop Entry", HIDES).is_none()))
}

/// Start a found app (`eep run`, `eep open`): GIO launches its entry, field
/// codes, D-Bus activation and terminal apps included.
pub fn launch(p: &Paths, card: &Card, args: &[std::ffi::OsString]) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let file = entry_file(p, card).with_context(|| {
        format!(
            "{} is not on this computer (no launcher entry {})",
            card.name,
            card.entry.as_deref().unwrap_or("?")
        )
    })?;
    let err = std::process::Command::new("gio")
        .arg("launch")
        .arg(&file)
        .args(args)
        .exec();
    Err(err).context("starting gio launch")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway tree: a home and a /usr/share, removed on drop.
    struct T {
        root: PathBuf,
        p: Paths,
        dirs: Dirs,
    }

    impl Drop for T {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn setup(name: &str) -> T {
        let root = std::env::temp_dir().join(format!("eep-system-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let r = root.as_path();
        let p = Paths {
            apps: r.join("home/Applications"),
            cache: r.join("home/.cache/eep"),
            roots: r.join("home/.local/state/eep/roots"),
            data: r.join("home/.local/share/eep"),
            launchers: r.join("home/.local/share/applications"),
            config: r.join("home/.config/eep"),
            local_cache: r.join("var/cache/eep/local"),
            driver_farm: r.join("var/lib/eep/opengl-driver"),
            runtime: r.join("run/eep"),
        };
        let dirs = Dirs {
            user: p.launchers.clone(),
            system: vec![r.join("usr/share/applications")],
            icons: vec![r.join("usr/share/icons")],
            pixmaps: vec![r.join("usr/share/pixmaps")],
            theme: None,
        };
        for d in [&p.apps, &dirs.user, &dirs.system[0]] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        T { root, p, dirs }
    }

    fn entry(name: &str, extra: &str) -> String {
        format!("[Desktop Entry]\nType=Application\nName={name}\nExec={name} %U\nIcon=app\n{extra}")
    }

    fn no_owners(_: &[PathBuf]) -> HashMap<PathBuf, (String, String)> {
        HashMap::new()
    }

    fn owners(files: &[PathBuf]) -> HashMap<PathBuf, (String, String)> {
        files
            .iter()
            .map(|f| (f.clone(), ("gimp".to_owned(), "3.2.8-1".to_owned())))
            .collect()
    }

    fn cards_of(t: &T) -> Vec<Card> {
        cards::all(&t.p).0.into_iter().map(|f| f.card).collect()
    }

    /// Desktop ids the launcher shows.
    fn shown(t: &T) -> Vec<String> {
        scan(&t.dirs)
            .into_iter()
            .filter(|(_, paths)| read(&paths[0]).is_some_and(|x| look(&x, &t.dirs) == Look::App))
            .map(|(e, _)| e)
            .collect()
    }

    fn run(t: &T, o: &Owners) -> Report {
        pass_with(&t.p, &t.dirs, true, o).expect("pass")
    }

    #[test]
    fn finds_hides_shows_and_forgets() {
        let t = setup("cycle");
        let sys = t.dirs.system[0].join("gimp.desktop");
        std::fs::write(&sys, entry("GIMP", "")).expect("write");
        let helper = entry("Helper", "NoDisplay=true\n");
        std::fs::write(t.dirs.system[0].join("helper.desktop"), helper).expect("write");
        let gone = entry("Broken", "TryExec=/nonexistent/AppRun\n");
        std::fs::write(t.dirs.system[0].join("broken.desktop"), gone).expect("write");
        // another app's entry (ewe Mobile's phone apps): shown, never a card
        let phone = entry("WhatsApp", "X-Eep-Skip=true\n");
        std::fs::write(
            t.dirs.user.join("ewe-mobile-app-com.whatsapp.desktop"),
            phone,
        )
        .expect("write");
        let icon = t.dirs.icons[0].join("hicolor/scalable/apps/app.svg");
        std::fs::create_dir_all(icon.parent().expect("dir")).expect("mkdir");
        std::fs::write(&icon, "<svg/>").expect("icon");

        assert_eq!(run(&t, &owners).found, vec!["GIMP"]);
        let c = cards_of(&t).remove(0);
        assert_eq!(c.id, "sys-gimp");
        assert_eq!(c.source, Source::System);
        assert_eq!(c.package.as_deref(), Some("gimp"));
        assert_eq!(c.version, "3.2.8-1");
        assert_eq!(c.icon_type.as_deref(), Some("image/svg+xml"));
        assert!(t.p.apps.join("GIMP.eep").is_file());
        assert!(run(&t, &owners).is_empty(), "a second pass changes nothing");

        // the card goes to the Trash: the app leaves the launcher, its
        // entry still opens files (NoDisplay, not Hidden)
        std::fs::remove_file(t.p.apps.join("GIMP.eep")).expect("rm");
        assert_eq!(run(&t, &owners).hidden, vec!["GIMP"]);
        assert!(shown(&t).is_empty());
        let copy = std::fs::read_to_string(t.dirs.user.join("gimp.desktop")).expect("copy");
        assert!(copy.contains("NoDisplay=true") && copy.contains("X-Eep-Hides=sys-gimp"));
        assert!(copy.contains("Exec=GIMP %U"));
        assert!(run(&t, &owners).is_empty(), "not found again");
        assert!(cards_of(&t).is_empty());

        // restored: shown again, eep's copy gone
        std::fs::write(t.p.apps.join("GIMP.eep"), c.to_toml()).expect("restore");
        assert_eq!(run(&t, &owners).shown, vec!["GIMP"]);
        assert!(!t.dirs.user.join("gimp.desktop").exists());
        assert_eq!(shown(&t), vec!["gimp.desktop"]);

        // uninstalled: the card goes
        std::fs::remove_file(&sys).expect("rm");
        assert_eq!(run(&t, &owners).gone, vec!["GIMP"]);
        assert!(cards_of(&t).is_empty());
    }

    #[test]
    fn the_users_own_entry_comes_back_byte_for_byte() {
        let t = setup("own");
        let mine = t.dirs.user.join("Dota 2.desktop");
        let original = "[Desktop Entry]\r\nType=Application\r\nName=Dota 2\r\nExec=steam steam://rungameid/570\r\n# my note\r\n";
        std::fs::write(&mine, original).expect("write");
        assert_eq!(run(&t, &no_owners).found, vec!["Dota 2"]);
        let c = cards_of(&t).remove(0);
        assert_eq!(c.id, "sys-dota-2");
        assert_eq!(c.from.as_deref(), Some("steam"));

        std::fs::remove_file(t.p.apps.join("Dota 2.eep")).expect("rm");
        assert_eq!(run(&t, &no_owners).hidden, vec!["Dota 2"]);
        assert!(shown(&t).is_empty());
        let stash = std::fs::read_to_string(t.p.hidden().join("Dota 2.desktop")).expect("stash");
        assert_eq!(stash, original);
        // a lost found.json: eep's mark still knows the app is hidden
        std::fs::remove_file(found_file(&t.p)).expect("rm");
        assert!(run(&t, &no_owners).is_empty());
        assert!(shown(&t).is_empty());

        std::fs::write(t.p.apps.join("Dota 2.eep"), c.to_toml()).expect("restore");
        assert_eq!(run(&t, &no_owners).shown, vec!["Dota 2"]);
        assert_eq!(std::fs::read_to_string(&mine).expect("back"), original);
        assert!(!t.p.hidden().join("Dota 2.desktop").exists());
    }

    #[test]
    fn an_eep_app_takes_the_place_of_a_found_one() {
        let t = setup("shadow");
        std::fs::write(t.dirs.system[0].join("gimp.desktop"), entry("GIMP", "")).expect("write");
        run(&t, &no_owners);
        assert_eq!(cards_of(&t).len(), 1);
        // eep installs GIMP from the cache under the same desktop id
        let eep_entry = entry("GIMP", "X-Eep-Id=gimp\n");
        std::fs::write(t.dirs.user.join("gimp.desktop"), eep_entry).expect("write");
        assert_eq!(run(&t, &no_owners).gone, vec!["GIMP"]);
        assert!(cards_of(&t).is_empty());
    }

    #[test]
    fn a_package_update_while_hidden_stays_hidden() {
        let t = setup("update");
        let sys = t.dirs.system[0].join("vlc.desktop");
        std::fs::write(&sys, entry("VLC", "")).expect("write");
        run(&t, &no_owners);
        std::fs::remove_file(t.p.apps.join("VLC.eep")).expect("rm");
        run(&t, &no_owners);
        // pacman writes a new entry (a later mtime)
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&sys, entry("VLC", "Comment=new\n")).expect("write");
        run(&t, &no_owners);
        let copy = std::fs::read_to_string(t.dirs.user.join("vlc.desktop")).expect("copy");
        assert!(copy.contains("Comment=new") && copy.contains("NoDisplay=true"));
        assert!(shown(&t).is_empty());
        // uninstalled while hidden: eep's copy goes too
        std::fs::remove_file(&sys).expect("rm");
        assert_eq!(run(&t, &no_owners).gone, vec!["VLC"]);
        assert!(!t.dirs.user.join("vlc.desktop").exists());
    }

    #[test]
    fn foreign_cards_are_left_alone() {
        let t = setup("foreign");
        let c = Card::parse(
            "eep = 0\nid = \"sys-zed\"\nname = \"Zed\"\nsource = \"system\"\n\
             entry = \"dev.zed.Zed.desktop\"\ninstalled = 2026-11-23T10:00:00Z\n",
        )
        .expect("card");
        std::fs::write(t.p.apps.join("Zed.eep"), c.to_toml()).expect("write");
        assert!(run(&t, &no_owners).is_empty());
        assert!(t.p.apps.join("Zed.eep").is_file());
    }

    #[test]
    fn someone_elses_entry_is_never_deleted() {
        let t = setup("others");
        let theirs = t.dirs.user.join("x.desktop");
        std::fs::write(&theirs, entry("X", "NoDisplay=true\n")).expect("write");
        assert!(remove_ours(&theirs, "sys-x").is_ok());
        assert!(theirs.is_file());
        assert!(unhide(&t.p, &t.dirs, "sys-x", "x.desktop").is_ok());
        assert!(theirs.is_file());
    }

    #[test]
    fn a_broken_card_or_a_moved_folder_hides_nothing() {
        let t = setup("broken");
        std::fs::write(t.dirs.system[0].join("vlc.desktop"), entry("VLC", "")).expect("write");
        run(&t, &no_owners);
        // a typo in the card: its id still reads, the app stays
        let card = t.p.apps.join("VLC.eep");
        let text = std::fs::read_to_string(&card).expect("card");
        std::fs::write(
            &card,
            text.replace("source = \"system\"", "source = \"sytsem\""),
        )
        .expect("typo");
        assert!(run(&t, &no_owners).is_empty());
        assert_eq!(shown(&t), vec!["vlc.desktop"]);
        // not TOML at all: nothing is hidden while it is broken
        std::fs::write(&card, "id = \"sys-vlc").expect("broken");
        assert!(run(&t, &no_owners).is_empty());
        assert_eq!(shown(&t), vec!["vlc.desktop"]);
        // the folder moved away: nothing changes, and it isn't made again
        std::fs::remove_file(&card).expect("rm");
        let moved = t.root.join("home/Applications-old");
        std::fs::rename(&t.p.apps, &moved).expect("mv");
        assert!(run(&t, &no_owners).is_empty());
        assert!(!t.p.apps.exists());
        assert_eq!(shown(&t), vec!["vlc.desktop"]);
    }

    #[test]
    fn ids() {
        let none = BTreeSet::new();
        assert_eq!(make_id("org.gimp.GIMP.desktop", &none), "sys-org-gimp-gimp");
        assert_eq!(make_id("Dota 2.desktop", &none), "sys-dota-2");
        assert_eq!(make_id("..desktop", &none), "sys-app");
        let taken: BTreeSet<String> = ["sys-dota-2".to_owned()].into();
        assert_eq!(make_id("dota-2.desktop", &taken), "sys-dota-2-2");
        let long = format!("{}.desktop", "a".repeat(100));
        let id = make_id(&long, &none);
        assert!(id.len() <= 64 && eep_core::valid_id(&id), "{id}");
    }
}
