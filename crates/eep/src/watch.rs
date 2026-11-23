//! The Apps place: `~/Applications` is the truth. `eep watch` (a systemd user
//! service) follows it with inotify; `eep open` handles a double-clicked
//! card; `eep setup` wires both into the session.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use eep_core::card::Card;
use inotify::{EventOwned, Inotify, WatchMask};
use serde_json::{Value, json};

use crate::activity::Activity;
use crate::ops::{self, Ctx};
use crate::paths::write_atomic;
use crate::{cards, run};

/// Quiet time before acting: a copy or a Trash move is several events.
const DEBOUNCE: Duration = Duration::from_millis(500);

/// Runs until killed; prints one JSON line per change it acted on.
pub fn watch(ctx: &Ctx) -> Result<Value> {
    let apps = &ctx.p.apps;
    let ino = Inotify::init().context("inotify")?;
    let mut watches = ino.watches();
    let mask = WatchMask::CREATE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO
        | WatchMask::DELETE_SELF
        | WatchMask::MOVE_SELF;
    // ~/Applications is made on the first run only. Later, a missing one
    // was moved or renamed by the user: wait for it to come back (watching
    // its parent) instead of making an empty one, which would block the
    // move back and say "no apps".
    if !apps.exists() && first_run(&ctx.p) {
        std::fs::create_dir_all(apps)?;
    }
    let apps_name = apps.file_name().map(ToOwned::to_owned);
    let parent_mask = WatchMask::CREATE | WatchMask::MOVED_TO | WatchMask::ONLYDIR;
    let mut waiting: Option<inotify::WatchDescriptor> = None;
    if apps.is_dir() {
        watches
            .add(apps, mask)
            .with_context(|| format!("watching {}", apps.display()))?;
    } else if let Some(parent) = apps.parent() {
        waiting = watches.add(parent, parent_mask).ok();
    }
    // The launcher entries of every app: a package installed or removed,
    // an AppImage added — its card follows. (Folders that don't exist yet,
    // like an unused /usr/local/share/applications, are not watched.)
    let dirs = crate::system::Dirs::from_env(&ctx.p);
    let _ = std::fs::create_dir_all(&dirs.user);
    let entry_mask = WatchMask::CREATE
        | WatchMask::CLOSE_WRITE
        | WatchMask::DELETE
        | WatchMask::MOVED_FROM
        | WatchMask::MOVED_TO;
    let entry_wds: Vec<inotify::WatchDescriptor> = dirs
        .apps_dirs()
        .iter()
        .filter_map(|d| watches.add(d, entry_mask).ok())
        .collect();
    // eep itself: when its file is replaced (an update), the watcher would
    // go on running the old code — it restarts as the new eep instead.
    let exe = std::env::current_exe().ok();
    let exe_name = exe
        .as_ref()
        .and_then(|e| e.file_name())
        .map(ToOwned::to_owned);
    let exe_wd = exe.as_ref().and_then(|e| e.parent()).and_then(|dir| {
        watches
            .add(dir, WatchMask::MOVED_TO | WatchMask::CLOSE_WRITE)
            .ok()
    });
    // One thread reads the kernel's events; this one acts on them, and can
    // wake by itself to try an install that failed again.
    let (tx, rx) = mpsc::channel::<EventOwned>();
    std::thread::spawn(move || pump(ino, &tx));
    // What this watcher says about installs: it outlives each pass, so an
    // app that could not be fetched stays "failed" until the next try.
    let mut act = Activity::new(&ctx.p);
    act.sweep();
    let mut retry = Retry::default();
    retry.after(&report(ops::reconcile_with(ctx, false, &mut act)));
    let mut seen = Seen::default();
    let me = Me {
        exe_wd: exe_wd.as_ref(),
        exe_name: exe_name.as_deref(),
        entry_wds: &entry_wds,
    };
    loop {
        let first = match retry.wait() {
            None => Some(rx.recv().context("inotify stopped")?),
            Some(wait) => match rx.recv_timeout(wait) {
                Ok(e) => Some(e),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => bail!("inotify stopped"),
            },
        };
        match first {
            // time to try the failed installs again
            None => seen.relevant = true,
            Some(e) => {
                let mut next = Some(e);
                while let Some(e) = next {
                    seen.take(&e, &me, waiting.as_ref());
                    seen.back |= is_back(&e, waiting.as_ref(), apps_name.as_deref());
                    next = match rx.recv_timeout(DEBOUNCE) {
                        Ok(e) => Some(e),
                        Err(RecvTimeoutError::Timeout) => None,
                        Err(RecvTimeoutError::Disconnected) => bail!("inotify stopped"),
                    };
                }
            }
        }
        if std::mem::take(&mut seen.upgraded)
            && let Some(exe) = &exe
        {
            use std::os::unix::process::CommandExt;
            // the new eep starts with the same pid: leave nothing said under it
            act = Activity::new(&ctx.p);
            act.sweep();
            let err = std::process::Command::new(exe).arg("watch").exec();
            eprintln!("eep: restarting as the new eep failed ({err}); going on");
        }
        if std::mem::take(&mut seen.lost_dir) && waiting.is_none() {
            // ~/Applications went away (moved, renamed): change nothing, wait
            // for it to come back.
            seen.relevant = false;
            if let Some(parent) = apps.parent() {
                waiting = watches.add(parent, parent_mask).ok();
            }
        }
        if std::mem::take(&mut seen.back) && apps.is_dir() {
            if let Some(wd) = waiting.take() {
                let _ = watches.remove(wd);
            }
            watches.add(apps, mask)?;
            seen.relevant = true;
        }
        if std::mem::take(&mut seen.relevant) {
            retry.after(&report(ops::reconcile_with(ctx, false, &mut act)));
        }
    }
}

/// Hand the kernel's events to the watcher; ends when either side is gone.
fn pump(mut ino: Inotify, tx: &mpsc::Sender<EventOwned>) {
    let mut buf = [0u8; 8192];
    loop {
        let Ok(events) = ino.read_events_blocking(&mut buf) else {
            return;
        };
        for e in events {
            if tx.send(e.to_owned()).is_err() {
                return;
            }
        }
    }
}

/// When to try installs that failed again (no network when the card
/// arrived): soon at first, then less and less often. Any pass without a
/// failure starts over.
#[derive(Debug, Default)]
struct Retry {
    tries: usize,
    at: Option<Instant>,
}

impl Retry {
    /// Seconds until the next try, by how many failed in a row.
    const STEPS: [u64; 6] = [20, 60, 180, 600, 1800, 3600];

    fn after(&mut self, pass: &Value) {
        let failed = pass["ok"] != true || pass["failed"].as_array().is_some_and(|f| !f.is_empty());
        if failed {
            let step = Self::STEPS[self.tries.min(Self::STEPS.len() - 1)];
            self.tries += 1;
            self.at = Some(Instant::now() + Duration::from_secs(step));
        } else {
            *self = Self::default();
        }
    }

    /// How long to wait for events before trying again; `None`: nothing failed.
    fn wait(&self) -> Option<Duration> {
        self.at
            .map(|at| at.saturating_duration_since(Instant::now()))
    }
}

/// The watches that aren't `~/Applications`.
struct Me<'a> {
    exe_wd: Option<&'a inotify::WatchDescriptor>,
    exe_name: Option<&'a std::ffi::OsStr>,
    /// The applications folders (launcher entries).
    entry_wds: &'a [inotify::WatchDescriptor],
}

/// What a batch of inotify events asked for.
#[derive(Debug, Default)]
struct Seen {
    /// A card changed in ~/Applications.
    relevant: bool,
    /// ~/Applications itself was removed or moved.
    lost_dir: bool,
    /// ~/Applications appeared again (made, or moved back).
    back: bool,
    /// eep's own file was replaced.
    upgraded: bool,
}

impl Seen {
    fn take(&mut self, e: &EventOwned, me: &Me<'_>, waiting: Option<&inotify::WatchDescriptor>) {
        let name = e.name.as_deref();
        if Some(&e.wd) == me.exe_wd {
            self.upgraded |= name.is_some_and(|n| Some(n) == me.exe_name);
            return;
        }
        if waiting.is_some_and(|w| *w == e.wd) {
            return; // the parent, while waiting for ~/Applications (is_back)
        }
        if me.entry_wds.contains(&e.wd) {
            // a launcher entry (not eep's temp files, not mimeinfo.cache)
            self.relevant |= name.is_some_and(|n| {
                let n = n.to_string_lossy();
                n.ends_with(".desktop") && !n.starts_with('.')
            });
            return;
        }
        self.relevant |= name.is_none_or(|n| is_card_name(&n.to_string_lossy()));
        self.lost_dir |= e
            .mask
            .intersects(inotify::EventMask::DELETE_SELF | inotify::EventMask::MOVE_SELF);
    }
}

/// An event in the parent while waiting for ~/Applications: is it the folder?
fn is_back(
    e: &EventOwned,
    waiting: Option<&inotify::WatchDescriptor>,
    apps_name: Option<&std::ffi::OsStr>,
) -> bool {
    waiting.is_some_and(|w| *w == e.wd) && e.name.is_some() && e.name.as_deref() == apps_name
}

/// Nothing installed or found yet: making ~/Applications can't hide a move.
fn first_run(p: &crate::paths::Paths) -> bool {
    crate::integrate::known_ids(p).is_empty() && !p.state_dir().join("found.json").exists()
}

/// A card, or an AppImage file dropped into the folder (it becomes one).
fn is_card_name(n: &str) -> bool {
    (n.ends_with(".eep") && !n.starts_with('.')) || crate::appimage::is_appimage_name(n)
}

/// Print what a pass did (nothing for a pass that changed nothing).
fn report(r: Result<Value>) -> Value {
    let v = r.unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") }));
    let quiet = v["ok"] == true
        && v["apps"].as_array().is_some_and(Vec::is_empty)
        && v["failed"].as_array().is_some_and(Vec::is_empty)
        && v["forgotten"].as_array().is_some_and(Vec::is_empty)
        && v["found"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty);
    if !quiet {
        println!("{v}");
    }
    v
}

/// Double-clicked card: start its app. An app that is not installed yet
/// is installed first — after the installer has shown it and asked, when
/// the click came through the card's handler (`ask`).
pub fn open(ctx: &Ctx, file: &Path, ask: bool) -> Result<Value> {
    let p = &ctx.p;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let card = Card::parse(&text)?;
    // The usual click: the app is installed. Nothing to bring in line and
    // no lock to wait for (another app may be downloading right now).
    if cards::find(p, &card.id).is_some_and(|have| ops::ready(p, &have.card)) {
        run::run(p, &card.id, None, &[], &[])?;
        bail!("exec returned")
    }
    if ask
        && !card.is_system()
        && let Some(ui) = installer()
    {
        // The installer (ewe Files) shows the app, its Install button and
        // how far the install is. It takes over; nothing is installed here.
        use std::os::unix::process::CommandExt;
        let mut arg = std::ffi::OsString::from("--install=");
        arg.push(file.canonicalize().unwrap_or_else(|_| file.to_path_buf()));
        let err = std::process::Command::new(&ui).arg(arg).exec();
        eprintln!("eep: {}: {err}; installing without it", ui.display());
    }
    if !cards::in_apps(p, file) && cards::find(p, &card.id).is_none() {
        if card.is_system() {
            // another computer's app: only if this one has it too
            cards::save(p, &card, None)?;
        } else if card.is_appimage() {
            // An AppImage's card says where its file comes from (and, with
            // a sha256, exactly which file): it goes into Applications and
            // is fetched from there. Where it was unpacked on the computer
            // it came from means nothing here.
            let mut fresh = card.clone();
            if !ops::ready(p, &fresh) {
                fresh.desktop = None;
                fresh.icon = None;
            }
            cards::save(p, &fresh, None)?;
        } else {
            // A card from elsewhere (a download) is not trusted: the app
            // is installed as the index has it today, never from the store
            // paths the file names. The index card lands in Applications.
            let r = ops::install(ctx, std::slice::from_ref(&card.id), ops::Fetch::default())?;
            if r["ok"] != true {
                return Ok(r);
            }
        }
    }
    let r = ops::reconcile(ctx, false)?;
    if r["ok"] != true {
        return Ok(r);
    }
    let failure = r["failed"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["id"] == card.id.as_str()));
    if let Some(why) = failure.and_then(|x| x["error"].as_str()) {
        bail!("{why}");
    }
    if card.is_system() && cards::find(p, &card.id).is_none() {
        // a card for an app this computer doesn't have: nothing to start
        bail!(
            "{} is not on this computer (it came from another one)",
            card.name
        );
    }
    run::run(p, &card.id, None, &[], &[])?;
    bail!("exec returned")
}

/// The app that shows a card before it is installed — its icon and name,
/// the Install button, the progress: ewe Files. `EEP_INSTALLER` names
/// another one, or none when it is empty (then a click installs at once).
fn installer() -> Option<PathBuf> {
    let name = std::env::var_os("EEP_INSTALLER").unwrap_or_else(|| "ewe-files-app".into());
    if name.is_empty() {
        return None;
    }
    let direct = Path::new(&name);
    if direct.is_absolute() {
        return direct.is_file().then(|| direct.to_path_buf());
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(&name))
        .find(|f| f.is_file())
}

/// The card file type, the handler and the user units: the files the
/// package ships (`packaging/`), so a packaged install and `eep setup`
/// wire up the same thing. `eep setup` points them at its own eep.
const MIME_XML: &str = include_str!("../../../packaging/eep.xml");
const HANDLER: &str = include_str!("../../../packaging/eep-open.desktop");
const USER_UNITS: [(&str, &str); 3] = [
    (
        "eep-watch.service",
        include_str!("../../../packaging/eep-watch.service"),
    ),
    (
        "eep-updates.service",
        include_str!("../../../packaging/eep-updates.service"),
    ),
    (
        "eep-updates.timer",
        include_str!("../../../packaging/eep-updates.timer"),
    ),
];
/// The packaged binary every shipped file names.
pub const PACKAGED_EEP: &str = "/usr/bin/eep";
/// Present when the package installed eep: the units, the file type and
/// its handler are system-wide (enabled for every user), nothing to write.
const PACKAGED_UNITS: &str = "/usr/lib/systemd/user/eep-watch.service";

/// Per-user wiring: the card file type and its handler, the watcher
/// service and the update timer. Writes only eep's own files, and none
/// when the package already provides them (it only starts them then).
pub fn setup(ctx: &Ctx) -> Result<Value> {
    let p = &ctx.p;
    let eep = ctx.eep.to_string_lossy();
    let data = p.launchers.parent().context("data dir")?;
    let systemd = p
        .config
        .parent()
        .context("config dir")?
        .join("systemd/user");
    let packaged = Path::new(PACKAGED_UNITS).exists();

    if !packaged {
        write_atomic(&data.join("mime/packages/eep.xml"), MIME_XML.as_bytes())?;
        let handler = HANDLER.replace(PACKAGED_EEP, &eep_core::desktop::quote_arg(&eep));
        write_atomic(&p.launchers.join("eep-open.desktop"), handler.as_bytes())?;
        for (name, text) in USER_UNITS {
            write_atomic(
                &systemd.join(name),
                text.replace(PACKAGED_EEP, &eep).as_bytes(),
            )?;
        }
    }
    std::fs::create_dir_all(&p.apps)?;

    // Packaged: enabled for every user already, only start them now.
    let start = if packaged {
        &["start"][..]
    } else {
        &["enable", "--now"][..]
    };
    let start: Vec<String> = ["systemctl", "--user"]
        .iter()
        .chain(start)
        .chain(&["eep-watch.service", "eep-updates.timer"])
        .map(|s| (*s).to_owned())
        .collect();
    let mut steps = Vec::new();
    let wiring = [
        (
            "mime",
            vec![
                "update-mime-database".to_owned(),
                data.join("mime").to_string_lossy().into_owned(),
            ],
        ),
        (
            "handler",
            vec![
                "xdg-mime".into(),
                "default".into(),
                "eep-open.desktop".into(),
                "application/x-eep".into(),
            ],
        ),
        (
            "reload",
            vec!["systemctl".into(), "--user".into(), "daemon-reload".into()],
        ),
        ("enable", start),
    ];
    for (what, cmd) in wiring {
        if packaged && what != "enable" {
            continue;
        }
        let ok = std::process::Command::new(&cmd[0])
            .args(&cmd[1..])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        steps.push(json!({ "step": what, "ok": ok }));
    }
    Ok(json!({
        "ok": true,
        "packaged": packaged,
        "steps": steps,
        "path_hint": format!("add {} to PATH for command-line apps", p.bin().display()),
    }))
}

#[cfg(test)]
mod tests {
    use super::{HANDLER, PACKAGED_EEP, Retry, USER_UNITS};
    use serde_json::json;
    use std::time::Duration;

    /// A pass with a failed install is tried again, later and later; a
    /// clean pass (or one that never ran) waits for events only.
    #[test]
    fn failed_installs_are_tried_again_less_and_less_often() {
        let mut r = Retry::default();
        assert!(r.wait().is_none(), "nothing failed: only events wake it");
        let failed = json!({ "ok": true, "failed": [{ "id": "ghost", "error": "x" }] });
        let mut last = Duration::ZERO;
        for step in Retry::STEPS {
            r.after(&failed);
            let wait = r.wait().expect("a retry is due");
            assert!(wait <= Duration::from_secs(step) && wait > Duration::from_secs(step - 2));
            assert!(wait > last, "each wait is longer than the one before");
            last = wait;
        }
        // past the last step it stays there
        r.after(&failed);
        assert!(r.wait().expect("still due") > Duration::from_secs(3598));
        // a pass that could not even start counts as failed; a clean one starts over
        r.after(&json!({ "ok": false, "error": "nix is not there" }));
        assert!(r.wait().is_some());
        r.after(&json!({ "ok": true, "failed": [] }));
        assert!(r.wait().is_none() && r.tries == 0);
    }

    /// `eep setup` points the shipped files at its own eep by replacing
    /// the packaged path: each must name it.
    #[test]
    fn shipped_files_name_the_packaged_eep() {
        for (name, text) in USER_UNITS.iter().filter(|(n, _)| n.ends_with(".service")) {
            let exec = text.lines().find(|l| l.starts_with("ExecStart="));
            assert!(
                exec.is_some_and(|l| l.contains(PACKAGED_EEP)),
                "{name} without a packaged ExecStart"
            );
        }
        assert!(HANDLER.contains(&format!("Exec={PACKAGED_EEP} open --ask %f")));
        let driver = include_str!("../../../packaging/eep-driver.service");
        assert!(driver.contains(&format!("ExecStart={PACKAGED_EEP} driver install")));
    }
}
