//! AppImages as cards (`source = "appimage"`): the same life as an eep app
//! — a card in `~/Applications` installs it, the Trash removes it, eep says
//! how far the download is — for an app that comes as one file.
//!
//! Where things are (all per user, under eep's data folder):
//!
//! ```text
//! ~/.local/share/eep/appimage/<id>/<key>/      the unpacked app (AppRun, …);
//!                                               <key> = first 16 hex of the
//!                                               file's sha256 = the card's `out`
//! ~/.local/share/eep/appimage/<id>/<key>/app.AppImage
//!                                               instead, for a file that
//!                                               would not unpack
//! ~/.local/state/eep/roots/<id>  →  that folder (the app is kept, and ready)
//! ```
//!
//! The file is unpacked once (`--appimage-extract`) and run from the tree:
//! an AppImage mounts and decompresses itself on every start otherwise
//! (Chrome: 1.1 s to a window from the file, 0.4 s from the tree — measured
//! for Komble, 2026-11-08). The downloaded file is not kept.
//!
//! A card carries no absolute path: `out` is relative to eep's data folder,
//! so the same card installs the same app on another computer.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use eep_core::card::{self, Card, Source};
use eep_core::catalog::Kind;
use eep_core::desktop;
use eep_core::http::Http;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::activity::{self, Activity};
use crate::integrate::{self, Integrated};
use crate::nix::Progress;
use crate::ops::Ctx;
use crate::paths::{Paths, write_atomic};
use crate::{cards, system};

/// The file kept in place of a tree when an AppImage would not unpack.
const WHOLE: &str = "app.AppImage";
/// The largest AppImage eep downloads (the biggest known are ~1.5 GB).
const MAX_BYTES: u64 = 8 * 1024 * 1024 * 1024;
/// How often a download says how far it is.
const OBSERVE: Duration = Duration::from_millis(100);

/// `~/.local/share/eep/appimage`.
pub fn store(p: &Paths) -> PathBuf {
    p.data.join(card::APPIMAGE_DIR)
}

/// Where a card's app is unpacked; `None` for a card that is not installed.
pub fn dir(p: &Paths, c: &Card) -> Option<PathBuf> {
    (!c.out.is_empty()).then(|| p.data.join(&c.out))
}

/// `Something.AppImage` (any case), not hidden: a file to turn into a card.
pub fn is_appimage_name(name: &str) -> bool {
    !name.starts_with('.') && name.len() > 9 && name.to_ascii_lowercase().ends_with(".appimage")
}

/// The app can start: it is unpacked, and kept (its root links to it).
pub fn ready(p: &Paths, c: &Card) -> bool {
    dir(p, c).is_some_and(|d| {
        std::fs::read_link(p.root(&c.id)).is_ok_and(|t| t == d) && runnable(&d).is_some()
    })
}

/// What starts the app in `dir`: its `AppRun`, or the whole file.
fn runnable(dir: &Path) -> Option<PathBuf> {
    let executable = |f: &Path| {
        std::fs::metadata(f).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    [dir.join("AppRun"), dir.join(WHOLE)]
        .into_iter()
        .find(|f| executable(f))
}

fn link_root(p: &Paths, id: &str, target: &Path) -> Result<()> {
    std::fs::create_dir_all(&p.roots)?;
    let link = p.root(id);
    let tmp = p.roots.join(format!(".{id}.{}.tmp", std::process::id()));
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(target, &tmp).with_context(|| format!("link {}", tmp.display()))?;
    std::fs::rename(&tmp, &link).with_context(|| format!("link {}", link.display()))
}

// ── running ──────────────────────────────────────────────────────────────

/// `eep run <id>` for an AppImage app: become its `AppRun`.
pub fn run(p: &Paths, c: &Card, extra_env: &[String], args: &[std::ffi::OsString]) -> Result<()> {
    let dir = dir(p, c).with_context(|| format!("{} is not installed yet", c.name))?;
    let program = runnable(&dir).with_context(|| format!("{} is not installed yet", c.name))?;
    let mut cmd = std::process::Command::new(&program);
    cmd.args(args);
    if program.file_name().is_some_and(|n| n == "AppRun") {
        // what the AppImage's own start-up would have set — without APPIMAGE:
        // an app that finds no file of itself does not try to replace it
        // (eep updates it), and none is kept
        cmd.env("APPDIR", &dir);
        if let Ok(cwd) = std::env::current_dir() {
            cmd.env("OWD", cwd);
        }
        cmd.env_remove("APPIMAGE");
    }
    for kv in extra_env {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--env wants VAR=value, got {kv:?}"))?;
        cmd.env(k, v);
    }
    let err = cmd.exec(); // returns only on failure
    Err(err).with_context(|| format!("starting {}", program.display()))
}

// ── the download ─────────────────────────────────────────────────────────

/// What a download or a local file turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fetched {
    pub sha256: String,
    pub size: u64,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// sha256 and size of a file.
pub fn hash_file(file: &Path) -> Result<Fetched> {
    let mut f = std::fs::File::open(file).with_context(|| format!("reading {}", file.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut size = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok(Fetched {
        sha256: hex(&hasher.finalize()),
        size,
    })
}

/// Download `url` into `dest`, telling `on` how far it is; `on` stops it by
/// answering `false`. The file must be a program (an AppImage is an ELF
/// file with a file system behind it): a web page saying "not found" is
/// refused before it is ever run.
pub fn download(url: &str, dest: &Path, on: &mut dyn FnMut(Progress) -> bool) -> Result<Fetched> {
    let (mut body, meta) = Http::new()
        .get_reader(url)?
        .with_context(|| format!("{url}: not found"))?;
    let total = meta.content_length.unwrap_or(0);
    if total > MAX_BYTES {
        bail!("{url}: {total} bytes is more than eep downloads for one app");
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out =
        std::fs::File::create(dest).with_context(|| format!("writing {}", dest.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    let mut done = 0u64;
    let mut told = Instant::now() - OBSERVE;
    let result = (|| -> Result<()> {
        loop {
            let n = body
                .read(&mut buf)
                .with_context(|| format!("reading {url}"))?;
            if n == 0 {
                break;
            }
            if done == 0 && !buf[..n].starts_with(b"\x7fELF") {
                bail!("{url} is not an AppImage (the address answered something else)");
            }
            out.write_all(&buf[..n])?;
            hasher.update(&buf[..n]);
            done += n as u64;
            if done > MAX_BYTES {
                bail!("{url}: more than eep downloads for one app");
            }
            if told.elapsed() >= OBSERVE {
                told = Instant::now();
                let total = total.max(done);
                if !on(Progress {
                    done,
                    total,
                    download: total,
                }) {
                    bail!("stopped");
                }
            }
        }
        if done == 0 {
            bail!("{url}: nothing came");
        }
        if total > 0 && done != total {
            bail!("{url}: the download broke off ({done} of {total} bytes)");
        }
        out.sync_all()?;
        Ok(())
    })();
    drop(out);
    if let Err(e) = result {
        let _ = std::fs::remove_file(dest);
        return Err(e);
    }
    on(Progress {
        done,
        total: done,
        download: done,
    });
    Ok(Fetched {
        sha256: hex(&hasher.finalize()),
        size: done,
    })
}

// ── unpacking ────────────────────────────────────────────────────────────

/// Unpack the AppImage `file` into `dest` (which must not exist), moving
/// the file away: the tree `--appimage-extract` gives, or — for a file that
/// will not unpack or has no `AppRun` — the file itself as `dest/app.AppImage`.
fn unpack(file: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().context("no folder for the app")?;
    std::fs::create_dir_all(parent)?;
    let work = parent.join(format!(".work-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let mut perms = std::fs::metadata(file)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    std::fs::set_permissions(file, perms)?;

    let extracted = std::process::Command::new(file)
        .arg("--appimage-extract")
        .current_dir(&work)
        // an AppImage asked to unpack itself needs nothing of the session
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success());
    let tree = work.join("squashfs-root");
    let placed = if extracted && runnable(&tree).is_some_and(|r| r.ends_with("AppRun")) {
        std::fs::rename(&tree, dest).with_context(|| format!("placing {}", dest.display()))
    } else if is_program(file) {
        // it runs as it is (it mounts itself); slower to start, still the app
        std::fs::create_dir_all(dest)
            .and_then(|()| move_file(file, &dest.join(WHOLE)))
            .with_context(|| format!("placing {}", dest.display()))
    } else {
        // half a copy, a web page saved under the name: never "installed"
        Err(anyhow::anyhow!(
            "this is not an AppImage (it does not unpack and is no program)"
        ))
    };
    let _ = std::fs::remove_dir_all(&work);
    placed?;
    let _ = std::fs::remove_file(file);
    Ok(())
}

/// An ELF file: what every AppImage is.
fn is_program(file: &Path) -> bool {
    let mut head = [0u8; 4];
    std::fs::File::open(file)
        .and_then(|mut f| f.read_exact(&mut head))
        .is_ok()
        && head == *b"\x7fELF"
}

/// Rename, or copy and remove when the two are on different file systems.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    std::fs::copy(from, to)?;
    std::fs::remove_file(from)
}

/// What an unpacked AppImage says about itself.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct About {
    /// Its launcher entry, relative to the tree.
    desktop: Option<String>,
    /// Its icon file, relative to the tree.
    icon: Option<String>,
    name: Option<String>,
    version: Option<String>,
}

/// `rel` under `tree`, when it is a regular file that really is inside the
/// tree (an AppImage may ship links that point anywhere): the real file's
/// path, relative to the tree.
fn inside_file(tree: &Path, rel: &Path) -> Option<String> {
    let real = tree.join(rel).canonicalize().ok()?;
    let root = tree.canonicalize().ok()?;
    let inner = real.strip_prefix(&root).ok()?;
    (real.is_file() && !inner.as_os_str().is_empty()).then(|| inner.to_string_lossy().into_owned())
}

fn about(tree: &Path) -> About {
    let mut a = About::default();
    let Ok(rd) = std::fs::read_dir(tree) else {
        return a;
    };
    let mut top: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
    top.sort();
    let ext = |f: &Path, want: &str| {
        f.extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(want))
    };
    let mut icon_name = None;
    for f in top.iter().filter(|f| ext(f, "desktop")) {
        let Some(name) = f.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        // read through a link, but only one that stays inside the tree
        let Some(real) = inside_file(tree, Path::new(&name)) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(tree.join(&real)) else {
            continue;
        };
        let get = |k: &str| desktop::get(&text, "Desktop Entry", k).filter(|v| !v.is_empty());
        a.name = get("Name");
        a.version = get("X-AppImage-Version");
        icon_name = get("Icon");
        // the entry keeps the name the app gave it at the top of the tree
        // (windows are matched to their entry by that name)
        a.desktop = Some(name);
        break;
    }
    // the icon: by the entry's name (scalable first, then the largest), the
    // AppImage's own .DirIcon, any picture at the top
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(n) = &icon_name
        && !n.contains('/')
    {
        candidates.push(PathBuf::from(format!("{n}.svg")));
        for size in [
            "scalable", "512x512", "256x256", "128x128", "96x96", "64x64", "48x48",
        ] {
            for e in ["svg", "png"] {
                candidates.push(PathBuf::from(format!(
                    "usr/share/icons/hicolor/{size}/apps/{n}.{e}"
                )));
            }
        }
        candidates.push(PathBuf::from(format!("{n}.png")));
    }
    candidates.push(PathBuf::from(".DirIcon"));
    candidates.extend(
        top.iter()
            .filter(|f| ext(f, "svg") || ext(f, "png"))
            .filter_map(|f| f.file_name().map(PathBuf::from)),
    );
    a.icon = candidates.iter().find_map(|rel| {
        let inner = inside_file(tree, rel)?;
        icon_type(&tree.join(&inner)).map(|_| inner)
    });
    a
}

/// `png` or `svg`, by what the file holds (a `.DirIcon` has no extension).
fn icon_type(file: &Path) -> Option<&'static str> {
    let mut head = [0u8; 512];
    let n = std::fs::File::open(file).ok()?.read(&mut head).ok()?;
    let head = &head[..n];
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("png")
    } else if head.windows(4).any(|w| w == b"<svg") || head.starts_with(b"<?xml") {
        Some("svg")
    } else {
        None
    }
}

/// An app id from a name: `Obsidian` → `obsidian`, `org.gimp.GIMP` → `gimp`.
pub fn make_id(name: &str) -> Option<String> {
    // a reverse-DNS entry name: its last part says most
    let base = if name.matches('.').count() >= 2 && !name.contains(' ') {
        name.rsplit('.').next().unwrap_or(name)
    } else {
        name
    };
    let mut id = String::new();
    for c in base.chars() {
        if c.is_ascii_alphanumeric() {
            id.push(c.to_ascii_lowercase());
        } else if !id.is_empty() && !id.ends_with('-') {
            id.push('-');
        }
    }
    let id: String = id.trim_matches('-').chars().take(48).collect();
    let id = id.trim_matches('-').to_owned();
    let id = if id.starts_with(|c: char| c.is_ascii_digit()) {
        format!("app-{id}")
    } else {
        id
    };
    (eep_core::valid_id(&id) && !id.starts_with(card::SYSTEM_PREFIX)).then_some(id)
}

/// `Obsidian-1.8.4-x86_64.AppImage` → (`Obsidian`, `1.8.4`).
fn name_and_version(file_name: &str) -> (String, String) {
    let stem = file_name
        .rsplit_once('.')
        .map_or(file_name, |(s, _)| s)
        .to_owned();
    let mut name: Vec<&str> = Vec::new();
    let mut version = String::new();
    for part in stem.split(['-', '_', ' ']) {
        let lower = part.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "x86" | "64" | "x64" | "amd64" | "linux" | "x86_64" | "aarch64" | "arm64" | "appimage"
        ) {
            continue;
        }
        let v = part.trim_start_matches(['v', 'V']);
        if version.is_empty() && v.starts_with(|c: char| c.is_ascii_digit()) && v.contains('.') {
            version = v.to_owned();
        } else if version.is_empty() {
            name.push(part);
        }
    }
    let name = if name.is_empty() {
        stem.clone()
    } else {
        name.join(" ")
    };
    (name, version)
}

// ── the desktop ──────────────────────────────────────────────────────────

/// Make an installed AppImage part of the desktop: its launcher entry
/// (under the name the app gave it, starting it through `eep run`) and its
/// icon — what [`integrate::integrate`] does for an eep app.
pub fn integrate(p: &Paths, c: &Card, eep: &Path) -> Result<Integrated> {
    if !eep_core::valid_id(&c.id) {
        bail!("refusing a bad app id {:?}", c.id);
    }
    let tree = dir(p, c).with_context(|| format!("{} is not installed", c.name))?;
    let mut done = Integrated::default();

    integrate::delete_icons(p, &c.id)?;
    if let Some(rel) = &c.icon {
        let src = tree.join(rel);
        match (icon_type(&src), std::fs::read(&src)) {
            (Some(ext), Ok(bytes)) => {
                let dst = p.icons().join(format!("{}.{ext}", c.id));
                write_atomic(&dst, &bytes)?;
                done.icon = Some(dst);
            }
            _ => eprintln!("eep: {}: no icon at {}", c.id, src.display()),
        }
    }

    integrate::delete_launchers(p, &c.id)?;
    let icon = done.icon.as_ref().map(|i| i.to_string_lossy().into_owned());
    let eep_arg = desktop::quote_arg(&eep.to_string_lossy());
    let exec = |value: &str| {
        // the AppImage decides what runs (its AppRun); the entry's own
        // arguments and field codes come along
        let rest = desktop::split_exec(value).map_or(String::new(), |x| x.rest);
        if rest.is_empty() {
            format!("{eep_arg} run {}", c.id)
        } else {
            format!("{eep_arg} run {} -- {rest}", c.id)
        }
    };
    let (name, text) = match &c.desktop {
        Some(rel) => {
            let file = tree.join(rel);
            let text = std::fs::read_to_string(&file)
                .with_context(|| format!("reading {}", file.display()))?;
            let name = Path::new(rel)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| format!("{}.desktop", c.id));
            (name, desktop::rewrite(&text, &c.id, icon.as_deref(), exec))
        }
        // an AppImage without an entry of its own still gets a launcher
        None => {
            let icon_line = icon
                .as_ref()
                .map_or(String::new(), |i| format!("Icon={i}\n"));
            (
                format!("{}.desktop", c.id),
                format!(
                    "[Desktop Entry]\nX-Eep-Id={}\n{icon_line}Type=Application\nName={}\nExec={}\nTerminal=false\nCategories=Utility;\n",
                    c.id,
                    c.name.replace('\n', " "),
                    exec("app %U"),
                ),
            )
        }
    };
    let dst = integrate::launcher_path(p, &c.id, &name);
    write_atomic(&dst, text.as_bytes())?;
    done.launchers.push(dst);
    integrate::refresh_desktop_database(&p.launchers);
    Ok(done)
}

// ── installing ───────────────────────────────────────────────────────────

/// A newer version of an installed AppImage app, and where it comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Latest {
    pub id: String,
    pub version: String,
    pub url: String,
    #[serde(default)]
    pub size: u64,
    /// The file's sha256, when the place it comes from says it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// Bring the app of the AppImage card `f` (a card in `~/Applications`) onto
/// this computer, or move it to `to`: download (unless it is unpacked here
/// already), check, unpack, rewrite the card to say what was installed,
/// keep it, add it to the launcher. `stop` ends a download.
///
/// The card and the running app are only touched once the new one is
/// unpacked: a download that fails leaves what was there.
pub fn install(
    ctx: &Ctx,
    f: &cards::Found,
    to: Option<&Latest>,
    act: &mut Activity,
    stop: &dyn Fn() -> bool,
) -> Result<Card> {
    let p = &ctx.p;
    let c = &f.card;
    let id = [c.id.as_str()];
    // back from the Trash, or a repair: it is all here
    if to.is_none()
        && let Some(d) = dir(p, c)
        && runnable(&d).is_some()
    {
        link_root(p, &c.id, &d)?;
        finish(ctx, f, c.clone())?;
        return Ok(c.clone());
    }
    let url = to
        .map(|l| l.url.as_str())
        .or(c.url.as_deref())
        .with_context(|| {
            format!(
                "{} was made from a file on another computer: there is nothing to download",
                c.name
            )
        })?;
    let expect = match to {
        Some(l) => l.sha256.as_deref(),
        None => c.sha256.as_deref(),
    };
    let home = store(p).join(&c.id);
    std::fs::create_dir_all(&home)?;
    let part = home.join(format!(".download-{}", std::process::id()));
    act.state(&id, activity::State::Downloading);
    let got = download(url, &part, &mut |progress| {
        act.progress(&id, progress);
        !stop()
    })?;
    if let Some(want) = expect
        && want != got.sha256
    {
        let _ = std::fs::remove_file(&part);
        bail!(
            "{}: the download is not the file its card names (it was changed where it comes from)",
            c.name
        );
    }
    act.state(&id, activity::State::Finishing);
    let out = card::appimage_out(&c.id, &got.sha256);
    let tree = p.data.join(&out);
    if runnable(&tree).is_some() {
        let _ = std::fs::remove_file(&part); // the same file was unpacked before
    } else {
        let _ = std::fs::remove_dir_all(&tree);
        unpack(&part, &tree)?;
    }
    // its card left while the app came in: nothing is written back
    if !f.file.exists() {
        bail!("stopped");
    }
    let a = about(&tree);
    let mut card = c.clone();
    card.url = Some(url.to_owned());
    card.sha256 = Some(got.sha256);
    card.size = Some(got.size);
    card.out = out;
    card.desktop = a.desktop;
    card.icon = a.icon;
    card.exec = None;
    if let Some(l) = to {
        card.version.clone_from(&l.version);
    } else if card.version.is_empty() {
        card.version = a.version.unwrap_or_default();
    }
    card.installed = card::datetime(&eep_core::time::now_rfc3339())?;
    link_root(p, &card.id, &tree)?;
    finish(ctx, f, card.clone())?;
    Ok(card)
}

/// The launcher entry, the icon inside the card, the card itself.
fn finish(ctx: &Ctx, f: &cards::Found, mut card: Card) -> Result<()> {
    let p = &ctx.p;
    let integrated = integrate(p, &card, &ctx.eep)?;
    embed_icon(&mut card, integrated.icon.as_deref());
    Card::parse(&card.to_toml()).with_context(|| format!("{}: a bad card", card.id))?;
    if card != f.card || !f.file.exists() {
        cards::save(p, &card, Some(&f.file))?;
    }
    Ok(())
}

fn embed_icon(card: &mut Card, icon: Option<&Path>) {
    let Some(icon) = icon else { return };
    let Ok(bytes) = std::fs::read(icon) else {
        return;
    };
    card.icon_type = None;
    card.icon_data = None;
    card.embed_icon(&icon.to_string_lossy(), &bytes);
    if card.icon_data.is_none()
        && let Some((name, small)) = system::shrink(icon)
    {
        card.embed_icon(&name, &small);
    }
}

/// Turn an AppImage file into an app: unpack it, write its card into
/// `~/Applications`, add it to the launcher. The file itself goes (it
/// became the card), as a card that is installed moves into Applications.
pub fn adopt(ctx: &Ctx, file: &Path, act: &mut Activity) -> Result<cards::Found> {
    let p = &ctx.p;
    let file_name = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let (guess_name, guess_version) = name_and_version(&file_name);
    let got = hash_file(file)?;
    if got.size > MAX_BYTES {
        bail!("{file_name} is larger than eep installs");
    }
    // unpacked under a name of its own first: the app's id is inside
    let stage = store(p).join(format!(".adopt-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    std::fs::create_dir_all(&stage)?;
    let staged_tree = stage.join("tree");
    let staged_file = stage.join(&file_name);
    std::fs::copy(file, &staged_file).with_context(|| format!("reading {}", file.display()))?;
    let made = (|| -> Result<cards::Found> {
        unpack(&staged_file, &staged_tree)?;
        let a = about(&staged_tree);
        let name = a.name.clone().unwrap_or_else(|| guess_name.clone());
        let from_entry = a
            .desktop
            .as_deref()
            .map(|d| d.trim_end_matches(".desktop"))
            .and_then(make_id);
        let mut id = from_entry
            .or_else(|| make_id(&name))
            .or_else(|| make_id(&guess_name))
            .context("this AppImage has no usable name")?;
        // an app of another kind has that id (Firefox from eep's own list,
        // say): this one is its own app
        if cards::find(p, &id).is_some_and(|f| !f.card.is_appimage()) {
            id = format!("{}-appimage", id.chars().take(55).collect::<String>());
        }
        let existing = cards::find(p, &id).filter(|f| f.card.is_appimage());
        act.begin([activity::New {
            id: &id,
            name: &name,
            card: existing.as_ref().map(|f| f.file.as_path()),
            what: if existing.is_some() {
                activity::What::Update
            } else {
                activity::What::Install
            },
        }]);
        act.state(&[id.as_str()], activity::State::Finishing);
        let out = card::appimage_out(&id, &got.sha256);
        let tree = p.data.join(&out);
        if runnable(&tree).is_none() {
            let _ = std::fs::remove_dir_all(&tree);
            if let Some(parent) = tree.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&staged_tree, &tree)
                .with_context(|| format!("placing {}", tree.display()))?;
        }
        let version = a
            .version
            .clone()
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| guess_version.clone());
        let old = existing.as_ref().map(|f| &f.card);
        let card = Card {
            eep: card::VERSION,
            id: id.clone(),
            name,
            kind: Kind::Gui,
            source: Source::Appimage,
            from: None,
            entry: None,
            package: None,
            url: None,
            sha256: Some(got.sha256.clone()),
            size: Some(got.size),
            // a newer file of an app eep can update keeps where its updates come from
            repo: old.and_then(|c| c.repo.clone()),
            channel: String::new(),
            release: String::new(),
            version,
            out,
            exec: None,
            desktop: a.desktop,
            icon: a.icon,
            bin: Vec::new(),
            previous: None,
            previous_version: None,
            previous_release: None,
            installed: card::datetime(&eep_core::time::now_rfc3339())?,
            icon_type: None,
            icon_data: None,
        };
        let target = match &existing {
            Some(f) => f.file.clone(),
            None => {
                std::fs::create_dir_all(&p.apps)?;
                cards::free_name(p, &card)?
            }
        };
        let found = cards::Found {
            file: target,
            // what is there now (nothing, for a new app): `finish` writes
            // the card when it differs or is not there
            card: old.cloned().unwrap_or_else(|| card.clone()),
        };
        link_root(p, &id, &tree)?;
        finish(ctx, &found, card.clone())?;
        act.done(&id);
        // read back: `finish` put the icon into it
        let saved = std::fs::read_to_string(&found.file)
            .ok()
            .and_then(|t| Card::parse(&t).ok())
            .unwrap_or(card);
        Ok(cards::Found {
            file: found.file,
            card: saved,
        })
    })();
    let _ = std::fs::remove_dir_all(&stage);
    let found = made?;
    let _ = std::fs::remove_file(file);
    Ok(found)
}

/// The AppImage files lying in `~/Applications` (someone dropped them
/// there): each becomes a card. One that fails stays where it is and is
/// reported; it is not tried again until it changes.
pub fn adopt_dropped(ctx: &Ctx, act: &mut Activity) -> (Vec<Value>, Vec<Value>) {
    let (mut made, mut failed) = (Vec::new(), Vec::new());
    let Ok(rd) = std::fs::read_dir(&ctx.p.apps) else {
        return (made, failed);
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|f| {
            f.is_file()
                && f.file_name()
                    .is_some_and(|n| is_appimage_name(&n.to_string_lossy()))
        })
        .collect();
    files.sort();
    for file in files {
        // still being copied in: the pass its last write wakes takes it
        if refused(&ctx.p, &file) || !settled(&file) {
            continue;
        }
        match adopt(ctx, &file, act) {
            Ok(f) => made.push(json!({ "id": f.card.id, "name": f.card.name, "card": f.file })),
            Err(e) => {
                remember_refused(&ctx.p, &file);
                failed.push(json!({
                    "file": file,
                    "name": file.file_name().map(|n| n.to_string_lossy().into_owned()),
                    "error": format!("{e:#}"),
                }));
            }
        }
    }
    (made, failed)
}

/// The file is whole: not written to in the last moments. A copy into the
/// folder is seen when it starts; taking half of it would "install" a
/// broken app and remove the file under the copy.
fn settled(file: &Path) -> bool {
    let look = || {
        let m = std::fs::metadata(file).ok()?;
        Some((m.len(), m.modified().ok()?))
    };
    let Some((len, modified)) = look() else {
        return false;
    };
    if modified
        .elapsed()
        .is_ok_and(|age| age > Duration::from_secs(3))
    {
        return true;
    }
    std::thread::sleep(Duration::from_millis(900));
    look() == Some((len, modified))
}

/// Files that could not be made into apps, by path, size and time: asked
/// once, not on every pass of the watcher.
fn refused_file(p: &Paths) -> PathBuf {
    p.state_dir().join("appimage-refused.json")
}

fn stamp(file: &Path) -> Option<String> {
    let m = std::fs::metadata(file).ok()?;
    let t = m
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(format!("{}\n{}\n{t}", file.display(), m.len()))
}

fn refused_list(p: &Paths) -> Vec<String> {
    std::fs::read(refused_file(p))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn refused(p: &Paths, file: &Path) -> bool {
    stamp(file).is_some_and(|s| refused_list(p).contains(&s))
}

fn remember_refused(p: &Paths, file: &Path) {
    let Some(s) = stamp(file) else { return };
    let mut known = refused_list(p);
    known.retain(|k| Path::new(k.lines().next().unwrap_or_default()).exists());
    known.push(s);
    if let Ok(text) = serde_json::to_vec(&known) {
        let _ = write_atomic(&refused_file(p), &text);
    }
}

// ── updates ──────────────────────────────────────────────────────────────

/// What the last check for newer AppImages found (`eep updates --count`
/// reads it: a badge must not go to the network).
fn updates_file(p: &Paths) -> PathBuf {
    p.cache.join("appimage-updates.json")
}

/// The newer versions known for installed AppImage apps, from the last
/// check — only those that still apply (the card has not moved since).
pub fn pending(p: &Paths, ids: &[String]) -> Vec<(cards::Found, Latest)> {
    let known: Vec<Latest> = std::fs::read(updates_file(p))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    known
        .into_iter()
        .filter(|l| ids.is_empty() || ids.contains(&l.id))
        .filter_map(|l| {
            let f = cards::find(p, &l.id)?;
            (f.card.is_appimage() && f.card.url.as_deref() != Some(l.url.as_str()))
                .then_some((f, l))
        })
        .collect()
}

/// Ask where each installed AppImage app's updates come from (its `repo`'s
/// newest release) and remember the answers. An app that can't be asked
/// about now keeps what was known about it.
pub fn check_updates(p: &Paths) -> Vec<(cards::Found, Latest)> {
    let http = Http::new();
    let before: Vec<Latest> = pending(p, &[]).into_iter().map(|(_, l)| l).collect();
    let mut found = Vec::new();
    for f in cards::all(p).0 {
        let c = &f.card;
        let (true, Some(repo)) = (c.is_appimage(), c.repo.as_deref()) else {
            continue;
        };
        match latest_release(&http, repo, c) {
            Ok(Some(l)) if c.url.as_deref() != Some(l.url.as_str()) => found.push(l),
            Ok(_) => {}
            Err(e) => {
                eprintln!("eep: {}: updates not checked: {e:#}", c.id);
                found.extend(before.iter().filter(|l| l.id == c.id).cloned());
            }
        }
    }
    if let Ok(text) = serde_json::to_vec(&found) {
        let _ = write_atomic(&updates_file(p), &text);
    }
    pending(p, &[])
}

#[derive(Debug, Deserialize)]
struct GhRelease {
    #[serde(default)]
    tag_name: String,
    #[serde(default)]
    assets: Vec<GhAsset>,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    #[serde(default)]
    name: String,
    #[serde(default)]
    browser_download_url: String,
    #[serde(default)]
    size: u64,
    /// `sha256:<hex>` (GitHub says it for files uploaded since 2025).
    #[serde(default)]
    digest: Option<String>,
}

/// The AppImage for this computer in the newest release of `repo`.
fn latest_release(http: &Http, repo: &str, c: &Card) -> Result<Option<Latest>> {
    let id = c.id.as_str();
    let have = c.url.as_deref().and_then(|u| u.rsplit('/').next());
    let api = std::env::var("EEP_GITHUB_API").unwrap_or_else(|_| "https://api.github.com".into());
    let url = format!("{}/repos/{repo}/releases/latest", api.trim_end_matches('/'));
    let Some(body) = http.get_bytes(&url)? else {
        return Ok(None);
    };
    let rel: GhRelease = serde_json::from_slice(&body).context("GitHub's answer")?;
    Ok(
        pick_asset(&rel, std::env::consts::ARCH, have).map(|a| Latest {
            id: id.to_owned(),
            version: rel.tag_name.trim_start_matches(['v', 'V']).to_owned(),
            url: a.browser_download_url.clone(),
            size: a.size,
            sha256: a
                .digest
                .as_deref()
                .and_then(|d| d.strip_prefix("sha256:"))
                .filter(|h| card::valid_sha256(h))
                .map(str::to_owned),
        }),
    )
}

/// A file name without its numbers: `LiteXL-v2.1.8-x86_64.AppImage` and
/// `LiteXL-v2.1.9-x86_64.AppImage` are the same flavour of the same app,
/// `LiteXL-v2.1.9-addons-x86_64.AppImage` is another.
fn flavour(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let mut out = String::new();
    for part in lower.split(['-', '_', '.', ' ']) {
        let versionish = part
            .trim_start_matches('v')
            .trim_start_matches("rc")
            .chars()
            .all(|c| c.is_ascii_digit());
        // "x86_64" splits into "x86" and "64": the 64 stays with it
        if !versionish || out.ends_with("x86-") {
            out.push_str(part);
            out.push('-');
        }
    }
    out
}

/// The `.AppImage` of a release for `arch`: the same flavour as the file
/// installed now (`have`, its name) when the release has it, else one that
/// names the architecture, else one that names none (the only kind then).
fn pick_asset<'a>(rel: &'a GhRelease, arch: &str, have: Option<&str>) -> Option<&'a GhAsset> {
    const X86: [&str; 4] = ["x86_64", "x86-64", "amd64", "x64"];
    const ARM: [&str; 2] = ["aarch64", "arm64"];
    const NEITHER: [&str; 4] = ["i386", "i686", "armhf", "armv7"];
    let (mine, other): (&[&str], &[&str]) = if arch == "aarch64" {
        (&ARM, &X86)
    } else {
        (&X86, &ARM)
    };
    let images: Vec<&GhAsset> = rel
        .assets
        .iter()
        .filter(|a| {
            a.name.to_ascii_lowercase().ends_with(".appimage")
                && a.browser_download_url.starts_with("https://")
        })
        .collect();
    let has = |a: &GhAsset, words: &[&str]| {
        let n = a.name.to_ascii_lowercase();
        words.iter().any(|w| n.contains(w))
    };
    let same = have.map(flavour);
    images
        .iter()
        .find(|a| same.as_deref() == Some(flavour(&a.name).as_str()))
        .or_else(|| images.iter().find(|a| has(a, mine)))
        .or_else(|| images.iter().find(|a| !has(a, other) && !has(a, &NEITHER)))
        .copied()
}

// ── the disk ─────────────────────────────────────────────────────────────

/// Remove unpacked AppImages nothing keeps: no root links to them (the
/// card is gone) and no card — in `~/Applications` or the Trash — names
/// them. Also what a killed eep left half done. → (bytes freed, folders
/// removed).
pub fn gc(p: &Paths, trash: &[Card], dry_run: bool) -> (u64, usize) {
    let mut keep: BTreeSet<PathBuf> = BTreeSet::new();
    if let Ok(rd) = std::fs::read_dir(&p.roots) {
        keep.extend(
            rd.flatten()
                .filter_map(|e| std::fs::read_link(e.path()).ok()),
        );
    }
    keep.extend(
        trash
            .iter()
            .chain(cards::all(p).0.iter().map(|f| &f.card))
            .filter(|c| c.is_appimage())
            .filter_map(|c| dir(p, c)),
    );
    let gone = |name: &str, prefixes: &[&str]| {
        prefixes.iter().any(|pre| {
            name.strip_prefix(pre)
                .and_then(|n| n.parse::<u32>().ok())
                .is_some_and(|pid| {
                    pid != std::process::id() && !Path::new(&format!("/proc/{pid}")).exists()
                })
        })
    };
    let (mut freed, mut removed) = (0, 0);
    let mut drop_it = |path: &Path| {
        freed += du(path);
        removed += 1;
        if !dry_run {
            let _ = if path.is_dir() {
                std::fs::remove_dir_all(path)
            } else {
                std::fs::remove_file(path)
            };
        }
    };
    let Ok(apps) = std::fs::read_dir(store(p)) else {
        return (0, 0);
    };
    for app in apps.flatten() {
        let app = app.path();
        let name = app
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if name.starts_with('.') {
            // `.adopt-<pid>`: a staging folder; only of an eep that is gone
            if gone(&name, &[".adopt-"]) {
                drop_it(&app);
            }
            continue;
        }
        let Ok(versions) = std::fs::read_dir(&app) else {
            continue;
        };
        let mut left = 0;
        for v in versions.flatten() {
            let v = v.path();
            let vname = v
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            let unused = if vname.starts_with('.') {
                gone(&vname, &[".work-", ".download-"])
            } else {
                !keep.contains(&v)
            };
            if unused {
                drop_it(&v);
            } else {
                left += 1;
            }
        }
        if left == 0 && !dry_run {
            let _ = std::fs::remove_dir(&app);
        }
    }
    (freed, removed)
}

/// Bytes under `path` (links not followed).
fn du(path: &Path) -> u64 {
    let Ok(meta) = path.symlink_metadata() else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path)
        .map(|rd| rd.flatten().map(|e| du(&e.path())).sum())
        .unwrap_or(0)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A stand-in AppImage: a program that unpacks "itself" when asked to,
    /// the way the real runtime does.
    pub(crate) fn fake_appimage(dir: &Path, file: &str, entry: &str, version: &str) -> PathBuf {
        let path = dir.join(file);
        let script = format!(
            "#!/bin/sh\n\
             [ \"$1\" = --appimage-extract ] || exit 1\n\
             mkdir -p squashfs-root/usr/share/icons/hicolor/256x256/apps\n\
             printf '#!/bin/sh\\necho \"ran $APPDIR $*\"\\n' > squashfs-root/AppRun\n\
             chmod +x squashfs-root/AppRun\n\
             printf '[Desktop Entry]\\nType=Application\\nName=Demo App\\nExec=demo --flag %%U\\nIcon=demo\\nX-AppImage-Version={version}\\nTryExec=demo\\n' > squashfs-root/{entry}.desktop\n\
             printf '\\211PNG\\r\\n\\032\\npicture {version}' > squashfs-root/usr/share/icons/hicolor/256x256/apps/demo.png\n\
             ln -s usr/share/icons/hicolor/256x256/apps/demo.png squashfs-root/.DirIcon\n\
             ln -s /etc/passwd squashfs-root/escape.png\n"
        );
        std::fs::write(&path, script).expect("write");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        path
    }

    pub(crate) fn test_ctx(root: &Path) -> Ctx {
        let p = Paths {
            apps: root.join("Applications"),
            cache: root.join("cache"),
            roots: root.join("state/eep/roots"),
            data: root.join("data/eep"),
            launchers: root.join("data/applications"),
            config: root.join("config"),
            local_cache: root.join("local"),
            driver_farm: root.join("driver"),
            runtime: root.join("run"),
        };
        std::fs::create_dir_all(&p.apps).expect("mkdir");
        Ctx {
            p,
            eep: PathBuf::from("/usr/bin/eep"),
        }
    }

    pub(crate) fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("eep-appimage-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    #[test]
    fn a_dropped_file_becomes_an_app() {
        let root = tmp("adopt");
        let ctx = test_ctx(&root);
        let p = &ctx.p;
        let file = fake_appimage(
            &p.apps,
            "Demo-App-1.2.3-x86_64.AppImage",
            "org.demo.DemoApp",
            "1.2.3",
        );
        let mut act = Activity::new(p);
        let found = adopt(&ctx, &file, &mut act).expect("adopted");
        let c = &found.card;
        assert_eq!(
            (c.id.as_str(), c.name.as_str(), c.version.as_str()),
            ("demoapp", "Demo App", "1.2.3")
        );
        assert!(c.is_appimage() && c.url.is_none());
        assert_eq!(c.desktop.as_deref(), Some("org.demo.DemoApp.desktop"));
        // the icon is the real file inside the tree, never the link out of it
        assert_eq!(
            c.icon.as_deref(),
            Some("usr/share/icons/hicolor/256x256/apps/demo.png")
        );
        assert_eq!(c.icon_type.as_deref(), Some("image/png"));
        // the file became the card
        assert!(!file.exists());
        assert_eq!(found.file, p.apps.join("Demo App.eep"));
        let on_disk =
            Card::parse(&std::fs::read_to_string(&found.file).expect("card")).expect("parses");
        assert_eq!(&on_disk, c);
        // kept and ready, by the rule a file manager reads: the root links to `out`
        assert!(ready(p, c));
        assert!(
            std::fs::read_link(p.root("demoapp"))
                .expect("root")
                .ends_with(&c.out)
        );
        // in the launcher under the app's own entry name, started through eep
        let entry =
            std::fs::read_to_string(p.launchers.join("org.demo.DemoApp.desktop")).expect("entry");
        assert!(entry.contains("X-Eep-Id=demoapp\n"), "{entry}");
        assert!(
            entry.contains("Exec=/usr/bin/eep run demoapp -- --flag %U\n"),
            "{entry}"
        );
        assert!(!entry.contains("TryExec"), "{entry}");
        assert!(
            entry.contains(&format!(
                "Icon={}\n",
                p.icons().join("demoapp.png").display()
            )),
            "{entry}"
        );
        // nothing half done is left
        let left: Vec<_> = std::fs::read_dir(store(p))
            .expect("store")
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("demoapp")]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_newer_file_updates_the_same_app_and_gc_frees_the_old_one() {
        let root = tmp("update");
        let ctx = test_ctx(&root);
        let p = &ctx.p;
        let mut act = Activity::new(p);
        let one = adopt(
            &ctx,
            &fake_appimage(&p.apps, "Demo-1.0.AppImage", "demo", "1.0"),
            &mut act,
        )
        .expect("one");
        let two = adopt(
            &ctx,
            &fake_appimage(&p.apps, "Demo-2.0.AppImage", "demo", "2.0"),
            &mut act,
        )
        .expect("two");
        assert_eq!(one.card.id, two.card.id);
        assert_eq!(one.file, two.file, "one card for the app");
        assert_eq!(two.card.version, "2.0");
        assert_ne!(one.card.out, two.card.out);
        assert!(ready(p, &two.card) && !ready(p, &one.card));
        // the old tree is still there until the disk is tidied
        assert!(p.data.join(&one.card.out).exists());
        assert_eq!(gc(p, &[], true).1, 1, "a dry run says so, removes nothing");
        assert!(p.data.join(&one.card.out).exists());
        assert_eq!(gc(p, &[], false).1, 1);
        assert!(!p.data.join(&one.card.out).exists());
        assert!(p.data.join(&two.card.out).exists());
        // a card in the Trash keeps its app (a restore needs no download)
        std::fs::remove_file(&two.file).expect("trash");
        integrate::forget(p, "demo").expect("forget");
        assert_eq!(gc(p, std::slice::from_ref(&two.card), false).1, 0);
        assert!(p.data.join(&two.card.out).exists());
        assert_eq!(gc(p, &[], false).1, 1, "the Trash was emptied");
        assert!(!store(p).join("demo").exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_card_back_from_the_trash_needs_no_download() {
        let root = tmp("restore");
        let ctx = test_ctx(&root);
        let p = &ctx.p;
        let mut act = Activity::new(p);
        let found = adopt(
            &ctx,
            &fake_appimage(&p.apps, "Demo.AppImage", "demo", "1"),
            &mut act,
        )
        .expect("adopted");
        integrate::forget(p, "demo").expect("forget");
        assert!(!ready(p, &found.card));
        assert!(!p.launchers.join("demo.desktop").exists());
        // no url in the card: only what is on the disk can bring it back
        let card = install(&ctx, &found, None, &mut act, &|| false).expect("restored");
        assert!(ready(p, &card));
        assert!(p.launchers.join("demo.desktop").exists());
        // and one whose app is gone says why
        let _ = std::fs::remove_dir_all(store(p));
        let err =
            install(&ctx, &found, None, &mut act, &|| false).expect_err("nothing to download");
        assert!(format!("{err:#}").contains("another computer"), "{err:#}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_file_that_will_not_unpack_runs_as_it_is() {
        let root = tmp("whole");
        let ctx = test_ctx(&root);
        let p = &ctx.p;
        let file = p.apps.join("Stubborn.AppImage");
        // a program that does not answer --appimage-extract (an old AppImage)
        std::fs::write(&file, b"\x7fELF not really a program").expect("write");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let mut act = Activity::new(p);
        let found = adopt(&ctx, &file, &mut act).expect("adopted");
        assert_eq!(found.card.id, "stubborn");
        assert!(ready(p, &found.card));
        assert!(p.data.join(&found.card.out).join(WHOLE).is_file());
        // it has no entry of its own: eep writes one
        let entry = std::fs::read_to_string(p.launchers.join("stubborn.desktop")).expect("entry");
        assert!(
            entry.contains("Name=Stubborn\n")
                && entry.contains("Exec=/usr/bin/eep run stubborn -- %U\n"),
            "{entry}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn names_ids_and_assets() {
        assert_eq!(make_id("Obsidian").as_deref(), Some("obsidian"));
        assert_eq!(make_id("org.gimp.GIMP").as_deref(), Some("gimp"));
        assert_eq!(make_id("My  Cool App!").as_deref(), Some("my-cool-app"));
        assert_eq!(make_id("7zip").as_deref(), Some("app-7zip"));
        assert_eq!(make_id("sys-thing"), None);
        assert_eq!(make_id("…"), None);
        assert_eq!(
            name_and_version("Obsidian-1.8.4-x86_64.AppImage"),
            ("Obsidian".to_owned(), "1.8.4".to_owned())
        );
        assert_eq!(
            name_and_version("krita_5.2.9-x86_64.appimage"),
            ("krita".to_owned(), "5.2.9".to_owned())
        );
        assert_eq!(
            name_and_version("Demo.AppImage"),
            ("Demo".to_owned(), String::new())
        );
        assert!(is_appimage_name("X-1.AppImage") && is_appimage_name("x.appimage"));
        assert!(
            !is_appimage_name(".x.AppImage")
                && !is_appimage_name("x.eep")
                && !is_appimage_name(".AppImage")
        );

        let rel: GhRelease = serde_json::from_str(
            r#"{"tag_name":"v2.1.0","assets":[
              {"name":"app-2.1.0-arm64.AppImage","browser_download_url":"https://x/arm","size":1},
              {"name":"app-2.1.0.AppImage.zsync","browser_download_url":"https://x/zsync","size":1},
              {"name":"app-2.1.0-x86_64.AppImage","browser_download_url":"https://x/amd","size":9,
               "digest":"sha256:abababababababababababababababababababababababababababababababab"},
              {"name":"app-2.1.0.deb","browser_download_url":"https://x/deb","size":1}]}"#,
        )
        .expect("json");
        let url =
            |arch: &str| pick_asset(&rel, arch, None).map(|a| a.browser_download_url.as_str());
        assert_eq!(url("x86_64"), Some("https://x/amd"));
        assert_eq!(url("aarch64"), Some("https://x/arm"));
        let plain: GhRelease = serde_json::from_str(
            r#"{"tag_name":"1","assets":[{"name":"App.AppImage","browser_download_url":"https://x/a","size":1}]}"#,
        )
        .expect("json");
        assert!(
            pick_asset(&plain, "x86_64", None).is_some(),
            "no architecture named: the only kind"
        );
        // two flavours for one architecture: the one that is installed stays
        let two: GhRelease = serde_json::from_str(
            r#"{"tag_name":"v2.1.8","assets":[
              {"name":"LiteXL-v2.1.8-addons-x86_64.AppImage","browser_download_url":"https://x/addons","size":1},
              {"name":"LiteXL-v2.1.8-x86_64.AppImage","browser_download_url":"https://x/plain","size":1}]}"#,
        )
        .expect("json");
        let pick = |have: &str| {
            pick_asset(&two, "x86_64", Some(have)).map(|a| a.browser_download_url.as_str())
        };
        assert_eq!(
            pick("LiteXL-v2.1.8-rc3-x86_64.AppImage"),
            Some("https://x/plain")
        );
        assert_eq!(
            pick("LiteXL-v2.1.7-addons-x86_64.AppImage"),
            Some("https://x/addons")
        );
        // no match: the first one for this computer
        assert_eq!(pick("Renamed.AppImage"), Some("https://x/addons"));
    }

    #[test]
    fn a_download_that_is_not_the_file_the_card_names_is_refused() {
        use std::io::{Read as _, Write as _};
        let root = tmp("pinned");
        let ctx = test_ctx(&root);
        let p = &ctx.p;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut req = [0u8; 1024];
                let _ = stream.read(&mut req);
                let body = b"\x7fELF another program than the card pinned";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(body);
            }
        });
        // a card as it would sit in Applications, pinned to another file
        let mut act = Activity::new(p);
        let made = adopt(
            &ctx,
            &fake_appimage(&p.apps, "Demo.AppImage", "demo", "1"),
            &mut act,
        )
        .expect("adopted");
        let mut card = made.card.clone();
        card.out = String::new();
        card.url = Some(format!("http://127.0.0.1:{port}/app"));
        card.sha256 = Some("0".repeat(64));
        let _ = std::fs::remove_dir_all(store(p));
        let _ = std::fs::remove_file(p.root("demo"));
        let found = cards::Found {
            file: made.file.clone(),
            card,
        };
        let err = install(&ctx, &found, None, &mut act, &|| false).expect_err("refused");
        assert!(
            format!("{err:#}").contains("not the file its card names"),
            "{err:#}"
        );
        // nothing of it is kept, and the card is as it was
        assert!(p.root("demo").symlink_metadata().is_err());
        let left: Vec<_> = std::fs::read_dir(store(p).join("demo"))
            .map(|rd| rd.flatten().map(|e| e.file_name()).collect())
            .unwrap_or_default();
        assert!(left.is_empty(), "{left:?}");
        assert_eq!(
            std::fs::read_to_string(&made.file).expect("card"),
            made.card.to_toml()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_download_is_checked_and_can_be_stopped() {
        use std::io::{Read as _, Write as _};
        use std::net::TcpListener;
        let root = tmp("download");
        // a tiny server: /app answers a "program", /page a web page
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let body: Vec<u8> = [b"\x7fELF".as_slice(), &vec![7u8; 600_000]].concat();
        let served = body.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut req = [0u8; 2048];
                let n = stream.read(&mut req).unwrap_or(0);
                let page = String::from_utf8_lossy(&req[..n]).contains("GET /page");
                let payload: &[u8] = if page {
                    b"<html>not found</html>"
                } else {
                    &served
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    payload.len()
                );
                let _ = stream.write_all(payload);
            }
        });
        let dest = root.join("got");
        let got = download(&format!("http://127.0.0.1:{port}/app"), &dest, &mut |_| {
            true
        })
        .expect("downloaded");
        assert_eq!(got.size, body.len() as u64);
        assert_eq!(got, hash_file(&dest).expect("hash"));
        // a web page where the app should be: refused, nothing left
        let err = download(&format!("http://127.0.0.1:{port}/page"), &dest, &mut |_| {
            true
        })
        .expect_err("a page");
        assert!(format!("{err:#}").contains("not an AppImage"), "{err:#}");
        assert!(!dest.exists());
        // stopped half way: nothing left
        let err = download(&format!("http://127.0.0.1:{port}/app"), &dest, &mut |_| {
            false
        })
        .expect_err("stopped");
        assert!(format!("{err:#}").contains("stopped"), "{err:#}");
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
