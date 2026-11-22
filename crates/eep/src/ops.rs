//! The commands that change or report what's installed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use eep_core::card::{self, Card};
use eep_core::index::{App, Index};
use serde_json::{Value, json};

use crate::paths::{Paths, write_atomic};
use crate::{cards, env, integrate, journal, nix, patch, state, system};

/// How long the previous version stays for `eep rollback`.
const KEEP_PREVIOUS: Duration = Duration::from_secs(7 * 24 * 3600);

#[derive(Debug)]
pub struct Ctx {
    pub p: Paths,
    /// This eep binary (launcher entries call it by absolute path).
    pub eep: PathBuf,
}

/// How pieces are fetched.
#[derive(Debug, Clone, Copy)]
pub struct Fetch {
    /// Use update patches where possible.
    pub patches: bool,
    /// eep's own hash check of patched pieces (off only for the tamper test).
    pub verify: bool,
}

impl Default for Fetch {
    fn default() -> Self {
        Self {
            patches: true,
            verify: true,
        }
    }
}

fn store_paths(names: &[String]) -> Vec<String> {
    names.iter().map(|b| format!("/nix/store/{b}")).collect()
}

/// Bytes and pieces still to download for `app` (without patches).
fn plan(idx: &Index, app: &App) -> Result<(u64, usize)> {
    let missing = missing(app)?;
    Ok(idx.download_for(&app.closure, |b| !missing.contains(b)))
}

/// Base names of the app's pieces not in the local store.
fn missing(app: &App) -> Result<BTreeSet<String>> {
    missing_of(&app.closure)
}

/// Base names among `names` not in the local store (one Nix call).
fn missing_of(names: &[String]) -> Result<BTreeSet<String>> {
    Ok(nix::invalid(&store_paths(names))?
        .into_iter()
        .filter_map(|p| p.strip_prefix("/nix/store/").map(str::to_owned))
        .collect())
}

/// Split the missing pieces of several apps between them: each piece is
/// counted for the first app that needs it. → (bytes, pieces) per app.
fn attribute(idx: &Index, apps: &[&App], missing: &BTreeSet<String>) -> Vec<(u64, usize)> {
    let mut counted = BTreeSet::new();
    apps.iter()
        .map(|a| {
            a.closure
                .iter()
                .filter(|b| missing.contains(*b) && counted.insert((*b).clone()))
                .fold((0, 0), |(bytes, n), b| {
                    (bytes + idx.pieces.get(b).map_or(0, |x| x.dl), n + 1)
                })
        })
        .collect()
}

/// Releases the installed apps came from: where patches can start.
fn installed_releases(p: &Paths) -> BTreeSet<String> {
    cards::all(p)
        .0
        .into_iter()
        .filter(|f| !f.card.is_system())
        .map(|f| f.card.release)
        .collect()
}

fn one_or_many(mut results: Vec<Value>) -> Value {
    if results.len() == 1 {
        let mut v = results.remove(0);
        if let Some(o) = v.as_object_mut() {
            o.insert("ok".into(), json!(true));
        }
        return v;
    }
    json!({ "ok": true, "apps": results })
}

pub fn install(ctx: &Ctx, ids: &[String], fetch: Fetch) -> Result<Value> {
    let _lock = state::lock(&ctx.p)?;
    // Found apps (`sys-…`) only come back: nothing to fetch.
    let (found, ids): (Vec<String>, Vec<String>) = ids
        .iter()
        .cloned()
        .partition(|i| i.starts_with(card::SYSTEM_PREFIX));
    if !found.is_empty() {
        let find = state::config(&ctx.p)?.find_apps;
        let shown = found
            .iter()
            .map(|id| system::show(&ctx.p, id, find))
            .collect::<Result<Vec<_>>>()?;
        if ids.is_empty() {
            return Ok(one_or_many(shown));
        }
    }
    let idx = state::index_or_fetch(&ctx.p)?;
    let mut v = install_many(ctx, &idx, &ids, fetch)?;
    // One app: its fields at the top level, next to the totals.
    if let Some(apps) = v["apps"].as_array().filter(|a| a.len() == 1).cloned()
        && let (Some(o), Some(app)) = (v.as_object_mut(), apps[0].as_object())
    {
        o.remove("apps");
        for (k, val) in app {
            o.entry(k.clone()).or_insert_with(|| val.clone());
        }
    }
    Ok(v)
}

/// Install apps from the index, or move them to the index's version.
///
/// Everything is planned together and fetched in ONE realise: Nix then
/// keeps all its download slots busy across apps, instead of waiting for
/// one app's slowest piece before starting the next app. Patches for all
/// of them are applied first, in one batch.
fn install_many(ctx: &Ctx, idx: &Index, ids: &[String], fetch: Fetch) -> Result<Value> {
    let p = &ctx.p;
    let t0 = Instant::now();
    let recovered = journal::recover(p)?;
    let mut todo: Vec<(&str, &App, Option<cards::Found>)> = Vec::new();
    for id in ids {
        let app = idx
            .app(id)
            .with_context(|| format!("no app {id:?} in the index"))?;
        if let Some(why) = &app.unavailable {
            bail!("{id} can't be installed from {}: {why}", idx.release);
        }
        if !todo.iter().any(|(i, ..)| i == id) {
            todo.push((id.as_str(), app, cards::find(p, id)));
        }
    }

    // Plan: one validity query for all closures together.
    let union: Vec<String> = todo
        .iter()
        .flat_map(|(_, a, _)| &a.closure)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let missing = missing_of(&union)?;
    let apps: Vec<&App> = todo.iter().map(|(_, a, _)| *a).collect();
    let shares = attribute(idx, &apps, &missing);
    let download_full: u64 = shares.iter().map(|s| s.0).sum();

    // Patches for what's missing, then the realise: Nix takes patched
    // pieces from the local cache and the rest from upstream.
    let mut applied = patch::Applied::default();
    if fetch.patches && !missing.is_empty() {
        let from = installed_releases(p);
        match patch::candidates(&state::config(p)?, &from, &idx.release, &missing)
            .and_then(|c| patch::apply(p, &c, fetch.verify))
        {
            Ok(a) => applied = a,
            Err(e) => eprintln!("eep: patches not used: {e:#}"),
        }
    }
    let outs: Vec<&str> = apps.iter().map(|a| a.out.as_str()).collect();
    let batch = p.roots.join(format!(".batch-{}", std::process::id()));
    let fetching = journal::begin(p, "install")?;
    let mut fell_back = false;
    let mut realised = if outs.is_empty() {
        Ok(())
    } else {
        nix::realise_many(&outs, &batch.join("r"))
    };
    patch::clean(&applied);
    if let Err(e) = &realised
        && applied.patched > 0
    {
        // Nix refused something we offered: fetch everything upstream.
        eprintln!("eep: {e:#}; retrying without patches");
        fell_back = true;
        realised = nix::realise_many(&outs, &batch.join("r"));
    }
    let per_app = realised.and_then(|()| {
        todo.into_iter()
            .zip(shares)
            .map(|((id, app, existing), share)| finish_one(ctx, idx, id, app, existing, share))
            .collect::<Result<Vec<_>>>()
    });
    // The batch roots only bridged the realise and the apps' own roots.
    let _ = std::fs::remove_dir_all(&batch);
    drop(fetching); // syncs the store, drops the journal
    let per_app = per_app?;

    let download = if fell_back {
        download_full
    } else {
        download_full - applied.full_bytes + applied.patch_bytes
    };
    Ok(json!({
        "ok": true,
        "release": idx.release,
        "apps": per_app,
        "download": download,
        "download_full": download_full,
        "pieces_new": missing.len(),
        "patches": applied,
        "fell_back": fell_back,
        "recovered": recovered,
        "seconds": t0.elapsed().as_secs_f64(),
    }))
}

/// After the realise: the app's own GC root (instant, all local now), the
/// previous version kept for rollback, the card, the desktop integration.
fn finish_one(
    ctx: &Ctx,
    idx: &Index,
    id: &str,
    app: &App,
    existing: Option<cards::Found>,
    (download_full, pieces_new): (u64, usize),
) -> Result<Value> {
    let p = &ctx.p;
    nix::realise(&app.out, &p.root(id))?;
    let (mut previous, mut previous_version, mut previous_release) =
        existing.as_ref().map_or((None, None, None), |f| {
            let c = &f.card;
            (
                c.previous.clone(),
                c.previous_version.clone(),
                c.previous_release.clone(),
            )
        });
    if let Some(f) = &existing
        && f.card.out != app.out
        && Path::new(&f.card.out).exists()
    {
        nix::realise(&f.card.out, &p.prev_root(id))?;
        previous = Some(f.card.out.clone());
        previous_version = Some(f.card.version.clone());
        previous_release = Some(f.card.release.clone());
    }
    let mut card = Card {
        eep: card::VERSION,
        id: id.to_owned(),
        name: app.name.clone(),
        kind: app.kind,
        source: Default::default(),
        from: None,
        entry: None,
        package: None,
        channel: idx.channel.clone(),
        release: idx.release.clone(),
        version: app.version.clone(),
        out: app.out.clone(),
        exec: app.exec.clone(),
        desktop: app.desktop.clone(),
        icon: app.icon_path.clone(),
        bin: app.bin.clone(),
        previous,
        previous_version,
        previous_release,
        installed: card::datetime(&eep_core::time::now_rfc3339())?,
        icon_type: None,
        icon_data: None,
    };
    // The index was checked on load; the card's own checks run again here,
    // before anything is written from it.
    Card::parse(&card.to_toml()).with_context(|| format!("{id}: the index gave a bad card"))?;
    let integrated = integrate::integrate(p, &card, &ctx.eep)?;
    // The card carries its icon, so Files shows the app on the card itself.
    if let Some(icon) = &integrated.icon
        && let Ok(bytes) = std::fs::read(icon)
    {
        card.embed_icon(&icon.to_string_lossy(), &bytes);
    }
    let file = cards::save(p, &card, existing.as_ref().map(|f| f.file.as_path()))?;
    Ok(json!({
        "id": id,
        "version": card.version,
        "from": existing.map(|f| f.card.version),
        "out": card.out,
        "card": file,
        "download_full": download_full,
        "pieces_new": pieces_new,
        "integrated": integrated,
    }))
}

pub fn remove(ctx: &Ctx, ids: &[String]) -> Result<Value> {
    let _lock = state::lock(&ctx.p)?;
    let mut results = Vec::new();
    for id in ids {
        if id.starts_with(card::SYSTEM_PREFIX) {
            let find = state::config(&ctx.p)?.find_apps;
            results.push(system::remove(&ctx.p, id, find)?);
            continue;
        }
        // every card for the app (a copy would bring it back otherwise)
        let mine: Vec<PathBuf> = cards::all(&ctx.p)
            .0
            .into_iter()
            .filter(|f| f.card.id == *id)
            .map(|f| f.file)
            .collect();
        for f in &mine {
            std::fs::remove_file(f).with_context(|| format!("removing {}", f.display()))?;
        }
        let mut r = integrate::forget(&ctx.p, id)?;
        r["card"] = json!(mine.first());
        results.push(r);
    }
    Ok(one_or_many(results))
}

/// The cards. eep's own apps only, unless `all`: stores list found apps
/// (pacman's, AppImages…) under their own source already.
pub fn list(ctx: &Ctx, all: bool) -> Result<Value> {
    let (good, bad) = cards::all(&ctx.p);
    let apps: Vec<Value> = good
        .iter()
        .filter(|f| all || !f.card.is_system())
        .map(|f| {
            let c = &f.card;
            if c.is_system() {
                return json!({
                    "id": c.id, "name": c.name, "kind": c.kind, "source": c.source,
                    "from": c.from, "entry": c.entry, "package": c.package,
                    "version": c.version, "card": f.file,
                    "present": system::entry_file(&ctx.p, c).is_some(),
                });
            }
            json!({
                "id": c.id, "name": c.name, "kind": c.kind, "source": c.source,
                "version": c.version, "release": c.release, "out": c.out,
                "present": Path::new(&c.out).exists(), "card": f.file,
                "previous_version": c.previous_version,
            })
        })
        .collect();
    let bad: Vec<Value> = bad
        .into_iter()
        .map(|(f, e)| json!({ "card": f, "error": e }))
        .collect();
    Ok(json!({ "ok": true, "apps": apps, "bad_cards": bad }))
}

pub fn available(ctx: &Ctx) -> Result<Value> {
    let idx = state::index_or_fetch(&ctx.p)?;
    let server = state::config(&ctx.p)?.server;
    let url = |rel: &Option<String>| {
        rel.as_ref()
            .map(|r| format!("{server}/{}/{r}", idx.release))
    };
    let installed: std::collections::BTreeMap<String, Card> = cards::all(&ctx.p)
        .0
        .into_iter()
        .map(|f| (f.card.id.clone(), f.card))
        .collect();
    let apps: Vec<Value> = idx
        .apps
        .iter()
        .filter(|(_, a)| a.kind != eep_core::catalog::Kind::System)
        .map(|(id, a)| {
            let card = installed.get(id);
            json!({
                "id": id, "name": a.name, "kind": a.kind, "version": a.version,
                "description": a.description, "homepage": a.homepage,
                "icon": a.icon, "icon_url": url(&a.icon),
                "icon_file": a.icon.as_ref().map(|i| state::icon_cache(&ctx.p, &idx.release, i))
                    .filter(|f| f.exists()),
                "download": a.download,
                "disk": a.disk, "unavailable": a.unavailable,
                "installed": card.map(|c| &c.version),
                "update": card.is_some_and(|c| c.out != a.out),
            })
        })
        .collect();
    Ok(json!({ "ok": true, "release": idx.release, "released": idx.released, "apps": apps }))
}

pub fn info(ctx: &Ctx, id: &str) -> Result<Value> {
    let idx = state::index_or_fetch(&ctx.p)?;
    let app = idx
        .app(id)
        .with_context(|| format!("no app {id:?} in the index"))?;
    let (download, pieces) = if app.unavailable.is_none() {
        plan(&idx, app)?
    } else {
        (0, 0)
    };
    let card = cards::find(&ctx.p, id);
    Ok(json!({
        "ok": true, "id": id, "release": idx.release, "app": app,
        "download_now": download, "pieces_now": pieces,
        "installed": card.map(|f| json!({ "version": f.card.version, "out": f.card.out, "card": f.file })),
    }))
}

/// Updates available for installed apps. `count` skips the download sizes
/// (no Nix call): that is what a bar badge polls. Otherwise one Nix query
/// and one manifest fetch cover all apps.
pub fn updates(ctx: &Ctx, count: bool) -> Result<Value> {
    let idx = state::load_index(&ctx.p)?;
    let pending = pending_updates(&ctx.p, &idx, &[]);
    if count {
        return Ok(json!({ "ok": true, "count": pending.len() }));
    }
    let apps: Vec<&App> = pending.iter().map(|(_, a)| *a).collect();
    let union: Vec<String> = apps
        .iter()
        .flat_map(|a| &a.closure)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let missing = missing_of(&union)?;
    let candidates = if missing.is_empty() {
        patch::Candidates::default()
    } else {
        let from = installed_releases(&ctx.p);
        patch::candidates(&state::config(&ctx.p)?, &from, &idx.release, &missing)
            .unwrap_or_default()
    };
    let shares = attribute(&idx, &apps, &missing);
    let mut counted = BTreeSet::new();
    let mut list = Vec::new();
    for ((card, app), (full, pieces)) in pending.iter().zip(shares) {
        let mine: BTreeSet<String> = app
            .closure
            .iter()
            .filter(|b| missing.contains(*b) && counted.insert((*b).clone()))
            .cloned()
            .collect();
        let (patch_bytes, replaced, patched) = candidates.savings(&mine);
        list.push(json!({
            "id": card.id, "name": card.name,
            "from": card.version, "to": app.version,
            "rebuild_only": card.version == app.version,
            "download": full - replaced + patch_bytes,
            "download_full": full,
            "pieces": pieces, "patched": patched,
        }));
    }
    let total = |k: &str| list.iter().map(|x| x[k].as_u64().unwrap_or(0)).sum::<u64>();
    let v = json!({
        "ok": true, "release": idx.release, "count": list.len(),
        "download": total("download"), "download_full": total("download_full"),
        "updates": list,
    });
    write_atomic(&ctx.p.cache.join("updates.json"), v.to_string().as_bytes())?;
    Ok(v)
}

/// Installed apps whose index entry moved (all, or only `ids`).
fn pending_updates<'a>(p: &Paths, idx: &'a Index, ids: &[String]) -> Vec<(Card, &'a App)> {
    cards::all(p)
        .0
        .into_iter()
        .filter(|f| ids.is_empty() || ids.contains(&f.card.id))
        .filter_map(|f| {
            let app = idx.app(&f.card.id)?;
            (app.unavailable.is_none() && app.out != f.card.out).then_some((f.card, app))
        })
        .collect()
}

pub fn update(ctx: &Ctx, ids: &[String], fetch: Fetch) -> Result<Value> {
    let _lock = state::lock(&ctx.p)?;
    let idx = state::load_index(&ctx.p)?;
    let ids: Vec<String> = pending_updates(&ctx.p, &idx, ids)
        .into_iter()
        .map(|(c, _)| c.id)
        .collect();
    let mut v = install_many(ctx, &idx, &ids, fetch)?;
    if let Some(o) = v.as_object_mut()
        && let Some(apps) = o.remove("apps")
    {
        o.insert("updated".into(), apps);
    }
    Ok(v)
}

pub fn rollback(ctx: &Ctx, id: &str) -> Result<Value> {
    let _lock = state::lock(&ctx.p)?;
    let p = &ctx.p;
    let f = cards::find(p, id).with_context(|| format!("{id} is not installed"))?;
    if f.card.is_system() {
        bail!(
            "{} is not an eep app: its own package manager updates it",
            f.card.name
        );
    }
    let mut card = f.card;
    let prev = card
        .previous
        .clone()
        .with_context(|| format!("{id} has no previous version"))?;
    if !Path::new(&prev).exists() {
        bail!("{id}'s previous version was already collected");
    }
    nix::realise(&prev, &p.root(id))?;
    nix::realise(&card.out, &p.prev_root(id))?;
    // Current ⇄ previous: store path, version and release travel together.
    std::mem::swap(&mut card.out, card.previous.get_or_insert_default());
    // a card from before eep kept these may lack them: keep what it has
    if let Some(v) = card.previous_version.as_mut() {
        std::mem::swap(&mut card.version, v);
    }
    if let Some(r) = card.previous_release.as_mut() {
        std::mem::swap(&mut card.release, r);
    }
    cards::save(p, &card, Some(&f.file))?;
    let integrated = integrate::integrate(p, &card, &ctx.eep)?;
    Ok(json!({
        "ok": true, "id": id, "version": card.version, "release": card.release,
        "out": card.out, "integrated": integrated,
    }))
}

/// Bring everything eep keeps in line with the cards: fetch and root what
/// a card names, (re)build its launcher entry, icon and shims, and drop the
/// leftovers of apps whose card is gone. `full` redoes every app (`eep
/// repair`); otherwise only apps that differ are touched (`eep watch`).
pub fn reconcile(ctx: &Ctx, full: bool) -> Result<Value> {
    let _lock = state::lock(&ctx.p)?;
    let p = &ctx.p;
    // Found apps first: it is quick (no download), so the cards of every
    // app on the computer are there before a slow fetch below starts.
    let found = match system::pass(p, state::config(p).map_or(true, |c| c.find_apps)) {
        Ok(r) => json!(r),
        Err(e) => json!({ "error": format!("{e:#}") }),
    };
    let recovered = journal::recover(p)?;
    let fetching = journal::begin(p, "repair")?;
    clean_stale_roots(p);
    let launchers = integrate::launcher_owners(p);
    let cards::Listing {
        good,
        bad,
        kept,
        sure,
    } = cards::listing(p);
    let mut touched = Vec::new();
    // a card that doesn't parse still keeps its app
    let mut ids: BTreeSet<String> = kept;
    for f in &good {
        let c = &f.card;
        if !ids.insert(c.id.clone()) || c.is_system() {
            continue; // a second card for the same app: the first one counts
        }
        let present = Path::new(&c.out).exists();
        let rooted =
            present && std::fs::read_link(p.root(&c.id)).is_ok_and(|t| t == Path::new(&c.out));
        if !full && rooted && integrated(p, c, &launchers) {
            continue;
        }
        if full || !rooted {
            nix::realise(&c.out, &p.root(&c.id))?;
        }
        if let Some(prev) = &c.previous
            && Path::new(prev).exists()
            && p.prev_root(&c.id).symlink_metadata().is_err()
        {
            nix::realise(prev, &p.prev_root(&c.id))?;
        }
        let integrated = integrate::integrate(p, c, &ctx.eep)?;
        // Cards from before icons travelled inside them get theirs now.
        if c.icon_data.is_none()
            && let Some(icon) = &integrated.icon
            && let Ok(bytes) = std::fs::read(icon)
        {
            let mut with_icon = c.clone();
            with_icon.embed_icon(&icon.to_string_lossy(), &bytes);
            if with_icon.icon_data.is_some() {
                cards::save(p, &with_icon, Some(&f.file))?;
            }
        }
        touched.push(json!({ "id": c.id, "fetched": !present }));
    }
    // Only a sure listing may forget: a moved or unreadable ~/Applications,
    // or a card whose id can't be read, must never uninstall anything.
    let mut forgotten = Vec::new();
    if sure {
        for id in integrate::known_ids(p).difference(&ids) {
            forgotten.push(integrate::forget(p, id)?);
        }
    }
    drop(fetching);
    let bad: Vec<Value> = bad
        .into_iter()
        .map(|(f, e)| json!({ "card": f, "error": e }))
        .collect();
    Ok(json!({
        "ok": true, "apps": touched, "forgotten": forgotten, "bad_cards": bad,
        "recovered": recovered, "found": found, "sure": sure,
    }))
}

/// `.batch-<pid>` / `.trash-<pid>` roots bridge one command; a command
/// killed half way leaves them, and their pieces would never be collected.
fn clean_stale_roots(p: &Paths) {
    let Ok(rd) = std::fs::read_dir(&p.roots) else {
        return;
    };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let pid = name
            .strip_prefix(".batch-")
            .or_else(|| name.strip_prefix(".trash-"))
            .and_then(|n| n.parse::<u32>().ok());
        if let Some(pid) = pid
            && pid != std::process::id()
            && !Path::new(&format!("/proc/{pid}")).exists()
        {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

/// Whether the app's launcher entry and shims are in place.
fn integrated(
    p: &Paths,
    c: &Card,
    launchers: &std::collections::BTreeMap<String, Vec<PathBuf>>,
) -> bool {
    (c.desktop.is_none() || launchers.contains_key(&c.id))
        && c.bin
            .iter()
            .all(|b| p.bin().join(b).symlink_metadata().is_ok())
}

pub fn gc(ctx: &Ctx, dry_run: bool) -> Result<Value> {
    let _lock = state::lock(&ctx.p)?;
    let t0 = Instant::now();
    if !dry_run {
        clean_stale_roots(&ctx.p);
    }
    let mut expired = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&ctx.p.roots) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(id) = name.strip_suffix(".prev") else {
                continue;
            };
            let age = e
                .path()
                .symlink_metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| SystemTime::now().duration_since(m).ok())
                .unwrap_or_default();
            if age > KEEP_PREVIOUS {
                if !dry_run {
                    std::fs::remove_file(e.path())?;
                    if let Some(f) = cards::find(&ctx.p, id) {
                        let mut c = f.card;
                        c.previous = None;
                        c.previous_version = None;
                        c.previous_release = None;
                        cards::save(&ctx.p, &c, Some(&f.file))?;
                    }
                }
                expired.push(id.to_owned());
            }
        }
    }
    // An app whose card is in the Trash stays until the Trash is emptied,
    // so a restore never downloads (macOS keeps an .app in the Trash whole).
    let keep = ctx.p.roots.join(format!(".trash-{}", std::process::id()));
    let mut kept = 0;
    for c in trash_cards() {
        if Path::new(&c.out).exists()
            && nix::realise(&c.out, &keep.join(format!("k{kept}"))).is_ok()
        {
            kept += 1;
        }
    }
    let g = nix::gc(dry_run);
    let _ = std::fs::remove_dir_all(&keep);
    let g = g?;
    Ok(json!({
        "ok": true, "dry_run": dry_run, "freed": g.freed_bytes, "paths": g.paths,
        "expired_previous": expired, "kept_for_trash": kept,
        "seconds": t0.elapsed().as_secs_f64(),
    }))
}

/// Cards in the user's Trash (`$XDG_DATA_HOME/Trash/files`).
fn trash_cards() -> Vec<Card> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")));
    let Some(files) = data.map(|d| d.join("Trash/files")) else {
        return Vec::new();
    };
    std::fs::read_dir(files)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|f| f.extension().is_some_and(|x| x == card::EXTENSION))
                .filter_map(|f| Card::parse(&std::fs::read_to_string(f).ok()?).ok())
                .filter(|c| !c.is_system())
                .collect()
        })
        .unwrap_or_default()
}

pub fn doctor(ctx: &Ctx, app: Option<&str>) -> Result<Value> {
    let p = &ctx.p;
    let cfg = state::config(p)?;
    let store = nix::ping();
    let driver_target = std::fs::read_link("/run/opengl-driver").ok();
    let path_ok =
        std::env::var_os("PATH").is_some_and(|v| std::env::split_paths(&v).any(|d| d == p.bin()));
    let local = &p.local_cache;
    let (good, bad) = cards::all(p);
    let index = state::load_index(p)
        .ok()
        .map(|i| json!({ "release": i.release, "released": i.released }));
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let mut v = json!({
        "ok": true,
        "server": cfg.server,
        "channel": cfg.channel,
        "index": index,
        "nix": {
            "store": store.as_ref().ok(),
            "error": store.as_ref().err().map(|e| format!("{e:#}")),
            "substituters": nix::config("substituters"),
            "narinfo_cache_negative_ttl": nix::config("narinfo-cache-negative-ttl"),
        },
        "local_cache": {
            "path": local,
            "ready": local.join("nix-cache-info").is_file(),
            "writable": dir_writable(local),
            // nix.conf's extra-substituters end up in `substituters`.
            "in_substituters": nix::config("substituters")
                .is_some_and(|s| s.contains(&format!("file://{}", local.display()))),
        },
        "driver": {
            "link": "/run/opengl-driver",
            "target": driver_target,
            "ok": Path::new("/run/opengl-driver").exists(),
        },
        "bin": { "dir": p.bin(), "on_path": path_ok },
        "apps_dir": p.apps,
        "cards": good.len(),
        "found_apps": { "on": cfg.find_apps, "cards": good.iter().filter(|f| f.card.is_system()).count() },
        "bad_cards": bad.iter().map(|(f, e)| json!({ "card": f, "error": e })).collect::<Vec<_>>(),
        "env": env::to_json(&env::plan(|k| std::env::var_os(k), &home, Path::exists)),
    });
    // One answer for UIs (Komble's "eep apps: Ready"): can eep install and
    // run apps here? Patches (the local cache) only make updates smaller.
    v["ready"] = json!(store.is_ok() && v["index"].is_object() && v["driver"]["ok"] == true);
    if let Some(id) = app {
        let f = cards::find(p, id).with_context(|| format!("{id} is not installed"))?;
        v["app"] = if f.card.is_system() {
            json!({ "id": id, "entry": system::entry_file(p, &f.card), "present": system::entry_file(p, &f.card).is_some() })
        } else {
            json!({ "id": id, "out": f.card.out, "present": Path::new(&f.card.out).exists() })
        };
    }
    Ok(v)
}

fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".eep-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}
