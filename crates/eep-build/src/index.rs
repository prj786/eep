//! `eep-build index`: one release + the catalog → `index.json.zst` + icons.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use eep_core::catalog::{self, Kind};
use eep_core::index::{self, App, Index, Piece, System};
use eep_core::listing::Entry;
use eep_core::narinfo::NarInfo;
use eep_core::{StorePath, desktop};
use serde_json::{Value, json};

use crate::nixeval::{self, EvalApp, Runner};
use crate::releases;
use crate::upstream::{Located, Upstream};
use crate::util::{par_map, write_atomic};

#[derive(Debug)]
pub struct Opts {
    pub channel: String,
    pub release: String,
    pub catalog: PathBuf,
    pub dist: PathBuf,
    pub runner: Runner,
    pub cache: PathBuf,
}

pub fn run(up: &Upstream, o: &Opts) -> Result<Value> {
    let t0 = Instant::now();
    let text = std::fs::read_to_string(&o.catalog)
        .with_context(|| format!("reading {}", o.catalog.display()))?;
    let cat = catalog::parse(&text)?;

    let (rev, released) = releases::meta(&up.http, &o.channel, &o.release)?;
    log(&format!("{} ({released}), rev {rev}", o.release));

    let attrs: Vec<String> = cat
        .values()
        .map(|e| e.attr.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let url = releases::release_url(&o.channel, &o.release, "nixexprs.tar.xz")?;
    let ev = nixeval::eval(o.runner, &url, &attrs, &o.cache)?;
    log(&format!("evaluated {} attrs", attrs.len()));

    // Roots: every app that evaluated, plus the driver.
    let mut outs: BTreeMap<String, StorePath> = BTreeMap::new();
    for (id, e) in &cat {
        if let Some(out) = ev.apps.get(&e.attr).and_then(|a| a.out.as_deref()) {
            outs.insert(id.clone(), StorePath::from_absolute(out)?);
        }
    }
    let driver: Vec<StorePath> = ev
        .driver
        .iter()
        .map(|d| StorePath::from_absolute(d))
        .collect::<Result<_>>()?;
    let roots: Vec<StorePath> = outs.values().chain(&driver).cloned().collect();
    let (infos, missing) = up.closure(&roots)?;
    log(&format!(
        "closure: {} pieces, {} missing from the cache",
        infos.len(),
        missing.len()
    ));

    // Per-app records (listings and icons are the slow part: in parallel).
    let ids: Vec<&String> = cat.keys().collect();
    let icons_dir = o.dist.join(&o.release).join("icons");
    let apps = par_map(&ids, 4, |id| {
        let e = &cat[*id];
        let evalled = ev.apps.get(&e.attr).cloned().unwrap_or_default();
        build_app(
            up,
            id,
            e,
            &evalled,
            outs.get(*id),
            &infos,
            &missing,
            &icons_dir,
        )
    });
    let apps: BTreeMap<String, App> = ids
        .into_iter()
        .cloned()
        .zip(apps.into_iter().collect::<Result<Vec<_>>>()?)
        .collect();

    let driver_closure = closure_of(&driver, &infos);
    let mut pieces = BTreeMap::new();
    for name in apps
        .values()
        .flat_map(|a| &a.closure)
        .chain(&driver_closure)
    {
        let p = StorePath::from_base_name(name)?;
        let info = &infos[&p];
        pieces.insert(
            name.clone(),
            Piece {
                dl: info.file_size().unwrap_or(0),
                nar: info.nar_size()?,
            },
        );
    }

    let idx = Index {
        eep_index: index::VERSION,
        channel: o.channel.clone(),
        release: o.release.clone(),
        rev,
        released,
        apps,
        pieces,
        system: System {
            driver: driver.iter().map(StorePath::absolute).collect(),
            driver_closure,
        },
    };
    let dir = o.dist.join(&o.release);
    remove_stale_icons(&icons_dir, &idx)?;
    let mut json_bytes = serde_json::to_vec(&idx)?;
    json_bytes.push(b'\n');
    let zst = zstd::encode_all(json_bytes.as_slice(), 19)?;
    write_atomic(&dir.join("index.json"), &json_bytes)?;
    write_atomic(&dir.join("index.json.zst"), &zst)?;

    let unavailable: BTreeMap<&str, &str> = idx
        .apps
        .iter()
        .filter_map(|(id, a)| a.unavailable.as_deref().map(|r| (id.as_str(), r)))
        .collect();
    let dl_all: u64 = idx.pieces.values().map(|p| p.dl).sum();
    let disk_all: u64 = idx.pieces.values().map(|p| p.nar).sum();
    Ok(json!({
        "ok": true,
        "release": idx.release,
        "released": idx.released,
        "index": dir.join("index.json.zst"),
        "index_bytes": zst.len(),
        "apps": idx.apps.len() - unavailable.len(),
        "unavailable": unavailable,
        "pieces": idx.pieces.len(),
        "download_all": dl_all,
        "disk_all": disk_all,
        "seconds": t0.elapsed().as_secs_f64(),
    }))
}

#[allow(clippy::too_many_arguments)]
fn build_app(
    up: &Upstream,
    id: &str,
    e: &catalog::Entry,
    ev: &EvalApp,
    out: Option<&StorePath>,
    infos: &BTreeMap<StorePath, NarInfo>,
    missing: &BTreeSet<StorePath>,
    icons_dir: &Path,
) -> Result<App> {
    let mut app = App {
        name: e.name.clone(),
        kind: e.kind,
        attr: e.attr.clone(),
        version: ev.version.clone(),
        description: ev.description.clone(),
        homepage: ev.homepage.clone(),
        unavailable: None,
        out: String::new(),
        exec: None,
        bin: Vec::new(),
        desktop: None,
        icon: None,
        icon_path: None,
        closure: Vec::new(),
        download: 0,
        disk: 0,
    };
    let Some(out) = out else {
        app.unavailable = Some(
            ev.error
                .clone()
                .unwrap_or_else(|| "failed to evaluate".into()),
        );
        return Ok(app);
    };
    app.out = out.absolute();
    if let Some(m) = missing_piece(out, infos, missing) {
        app.unavailable = Some(format!("not in the binary cache: {m}"));
        return Ok(app);
    }
    app.closure = closure_of(std::slice::from_ref(out), infos);
    for name in &app.closure {
        let info = &infos[&StorePath::from_base_name(name)?];
        app.download += info.file_size().unwrap_or(0);
        app.disk += info.nar_size()?;
    }

    let bin: Vec<String> = up
        .list_dir(out, "bin")?
        .into_iter()
        .filter(|b| !b.starts_with('.'))
        .collect();
    let main = (!ev.main_program.is_empty() && bin.contains(&ev.main_program))
        .then(|| ev.main_program.clone());
    app.exec = main
        .or_else(|| (bin.len() == 1).then(|| bin[0].clone()))
        .map(|b| format!("bin/{b}"));
    if e.kind.on_path() {
        app.bin = match &e.bin {
            Some(wanted) => {
                for w in wanted.iter().filter(|w| !bin.contains(w)) {
                    log(&format!(
                        "{id}: catalog bin {w:?} is not in the package; skipped"
                    ));
                }
                wanted.iter().filter(|w| bin.contains(w)).cloned().collect()
            }
            // The main program, else everything (a package of one tool).
            None => app
                .exec
                .as_deref()
                .and_then(|x| x.strip_prefix("bin/"))
                .map_or_else(|| bin.clone(), |main| vec![main.to_owned()]),
        };
    }
    if e.kind != Kind::Gui {
        return Ok(app);
    }

    // Launcher entry.
    let entries: Vec<String> = up
        .list_dir(out, "share/applications")?
        .into_iter()
        .filter(|n| n.ends_with(".desktop"))
        .collect();
    let Some(file) = pick_desktop(&entries, e.desktop.as_deref(), &[&ev.pname, id, &e.attr]) else {
        app.unavailable = Some(match &e.desktop {
            Some(d) => format!("{d} not found in share/applications"),
            None => "no launcher entry in share/applications".into(),
        });
        return Ok(app);
    };
    let rel = format!("share/applications/{file}");
    let loc = up
        .resolve(out, &rel)?
        .with_context(|| format!("{id}: {rel} vanished"))?;
    let text = String::from_utf8_lossy(&up.read_file(&loc)?).into_owned();
    app.desktop = Some(rel);
    if app.exec.is_none()
        && let Some(x) =
            desktop::get(&text, "Desktop Entry", "Exec").and_then(|v| desktop::split_exec(&v))
        && bin.contains(&x.program)
    {
        app.exec = Some(format!("bin/{}", x.program));
    }

    // Icon.
    if let Some(name) = desktop::get(&text, "Desktop Entry", "Icon")
        && let Some((icon_path, loc)) = find_icon(up, out, &name)?
    {
        let ext = loc
            .rel
            .rsplit('.')
            .next()
            .unwrap_or("png")
            .to_ascii_lowercase();
        let mut bytes = up.read_file(&loc)?;
        if ext == "png" && bytes.len() > eep_core::card::ICON_MAX {
            bytes = shrink_png(&bytes).unwrap_or(bytes);
        }
        let file = format!("{id}.{ext}");
        write_atomic(&icons_dir.join(&file), &bytes)?;
        app.icon = Some(format!("icons/{file}"));
        app.icon_path = Some(icon_path);
    }
    log(&format!(
        "{id}: {} {}",
        app.version,
        if app.icon.is_some() {
            "with icon"
        } else {
            "no icon"
        }
    ));
    Ok(app)
}

/// Scale a big PNG icon down until it fits inside a card (some apps ship
/// only a 1024 px icon): 256, 192, then 128 px, best compression.
/// Deterministic: same input, same bytes.
fn shrink_png(bytes: &[u8]) -> Option<Vec<u8>> {
    use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
    let img = image::load_from_memory_with_format(bytes, image::ImageFormat::Png).ok()?;
    let mut last = None;
    for px in [256, 192, 128] {
        let small = img.resize(px, px, image::imageops::FilterType::Lanczos3);
        let mut out = Vec::new();
        let enc =
            PngEncoder::new_with_quality(&mut out, CompressionType::Best, PngFilter::Adaptive);
        small.write_with_encoder(enc).ok()?;
        let fits = out.len() <= eep_core::card::ICON_MAX;
        last = Some(out);
        if fits {
            break;
        }
    }
    last
}

fn pick_desktop(entries: &[String], wanted: Option<&str>, names: &[&str]) -> Option<String> {
    if let Some(w) = wanted {
        return entries.iter().find(|e| *e == w).cloned();
    }
    if entries.len() == 1 {
        return entries.first().cloned();
    }
    for n in names.iter().filter(|n| !n.is_empty()) {
        if let Some(e) = entries.iter().find(|e| **e == format!("{n}.desktop")) {
            return Some(e.clone());
        }
    }
    for n in names.iter().filter(|n| !n.is_empty()) {
        let n = n.to_ascii_lowercase();
        if let Some(e) = entries.iter().find(|e| e.to_ascii_lowercase().contains(&n)) {
            return Some(e.clone());
        }
    }
    entries.first().cloned()
}

/// Find the icon `name` (a theme name or an absolute path). Returns the
/// path as the app sees it (relative to `out`, or absolute) and where the
/// file really is.
fn find_icon(up: &Upstream, out: &StorePath, name: &str) -> Result<Option<(String, Located)>> {
    if name.starts_with('/') {
        let Ok((piece, sub)) = StorePath::split_absolute(name) else {
            return Ok(None);
        };
        return Ok(up
            .resolve(&piece, &sub)?
            .filter(is_file)
            .map(|l| (name.to_owned(), l)));
    }
    let name = name
        .strip_suffix(".png")
        .or_else(|| name.strip_suffix(".svg"))
        .unwrap_or(name);
    let mut candidates = vec![format!("share/icons/hicolor/scalable/apps/{name}.svg")];
    let mut sizes: Vec<(u32, String)> = up
        .list_dir(out, "share/icons/hicolor")?
        .into_iter()
        .filter_map(|d| {
            let (w, h) = d.split_once('x')?;
            (w == h).then(|| w.parse().ok().map(|n| (n, d.clone())))?
        })
        .collect();
    // Biggest first, but anything up to 256 px before the bigger ones: the
    // icon also travels inside every card (card::ICON_MAX).
    sizes.sort_by_key(|(n, _)| (*n > 256, std::cmp::Reverse(*n)));
    candidates.extend(
        sizes
            .into_iter()
            .map(|(_, d)| format!("share/icons/hicolor/{d}/apps/{name}.png")),
    );
    candidates.push(format!("share/pixmaps/{name}.svg"));
    candidates.push(format!("share/pixmaps/{name}.png"));
    for c in candidates {
        if let Some(loc) = up.resolve(out, &c)?.filter(is_file) {
            return Ok(Some((c, loc)));
        }
    }
    Ok(None)
}

fn is_file(l: &Located) -> bool {
    matches!(
        l.entry,
        Entry::Regular {
            nar_offset: Some(_),
            ..
        }
    )
}

fn closure_of(roots: &[StorePath], infos: &BTreeMap<StorePath, NarInfo>) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut stack: Vec<StorePath> = roots.to_vec();
    while let Some(p) = stack.pop() {
        if !seen.insert(p.clone()) {
            continue;
        }
        if let Some(info) = infos.get(&p) {
            stack.extend(
                info.references()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|r| !seen.contains(r)),
            );
        }
    }
    seen.into_iter().map(String::from).collect()
}

fn missing_piece(
    out: &StorePath,
    infos: &BTreeMap<StorePath, NarInfo>,
    missing: &BTreeSet<StorePath>,
) -> Option<String> {
    if missing.is_empty() {
        return None;
    }
    let mut seen = BTreeSet::new();
    let mut stack = vec![out.clone()];
    while let Some(p) = stack.pop() {
        if missing.contains(&p) {
            return Some(p.to_string());
        }
        if seen.insert(p.clone())
            && let Some(info) = infos.get(&p)
        {
            stack.extend(info.references().unwrap_or_default());
        }
    }
    None
}

fn remove_stale_icons(dir: &Path, idx: &Index) -> Result<()> {
    let keep: BTreeSet<String> = idx
        .apps
        .values()
        .filter_map(|a| a.icon.as_deref()?.strip_prefix("icons/").map(str::to_owned))
        .collect();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if !keep.contains(&e.file_name().to_string_lossy().into_owned()) {
                std::fs::remove_file(e.path())?;
            }
        }
    }
    Ok(())
}

pub fn log(msg: &str) {
    eprintln!("eep-build: {msg}");
}

#[cfg(test)]
mod tests {
    use super::pick_desktop;

    #[test]
    fn picks_desktop() {
        let e: Vec<String> = ["codium-url-handler.desktop", "codium.desktop"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            pick_desktop(&e, Some("codium.desktop"), &[]).as_deref(),
            Some("codium.desktop")
        );
        assert_eq!(pick_desktop(&e, Some("nope.desktop"), &[]), None);
        assert_eq!(
            pick_desktop(&e, None, &["codium"]).as_deref(),
            Some("codium.desktop")
        );
        let one = vec!["org.gnome.Calculator.desktop".to_owned()];
        assert_eq!(
            pick_desktop(&one, None, &["gnome-calculator"]).as_deref(),
            Some("org.gnome.Calculator.desktop")
        );
        let e: Vec<String> = ["a.desktop", "org.telegram.desktop.desktop"]
            .map(String::from)
            .to_vec();
        assert_eq!(
            pick_desktop(&e, None, &["telegram-desktop", "telegram"]).as_deref(),
            Some("org.telegram.desktop.desktop")
        );
    }
}
