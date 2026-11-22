//! The Apps place: `~/Applications` is the truth. `eep watch` (a systemd user
//! service) follows it with inotify; `eep open` handles a double-clicked
//! card; `eep setup` wires both into the session.

use std::io::ErrorKind;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use eep_core::card::Card;
use inotify::{Inotify, WatchMask};
use serde_json::{Value, json};

use crate::ops::{self, Ctx};
use crate::paths::write_atomic;
use crate::{cards, run};

/// Quiet time before acting: a copy or a Trash move is several events.
const DEBOUNCE: Duration = Duration::from_millis(500);
const TICK: Duration = Duration::from_millis(100);

/// Runs until killed; prints one JSON line per change it acted on.
pub fn watch(ctx: &Ctx) -> Result<Value> {
    let apps = &ctx.p.apps;
    let mut ino = Inotify::init().context("inotify")?;
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
        ino.watches()
            .add(apps, mask)
            .with_context(|| format!("watching {}", apps.display()))?;
    } else if let Some(parent) = apps.parent() {
        waiting = ino.watches().add(parent, parent_mask).ok();
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
        .filter_map(|d| ino.watches().add(d, entry_mask).ok())
        .collect();
    // eep itself: when its file is replaced (an update), the watcher would
    // go on running the old code — it restarts as the new eep instead.
    let exe = std::env::current_exe().ok();
    let exe_name = exe
        .as_ref()
        .and_then(|e| e.file_name())
        .map(ToOwned::to_owned);
    let exe_wd = exe.as_ref().and_then(|e| e.parent()).and_then(|dir| {
        ino.watches()
            .add(dir, WatchMask::MOVED_TO | WatchMask::CLOSE_WRITE)
            .ok()
    });
    report(ops::reconcile(ctx, false));
    let mut buf = [0u8; 8192];
    let mut seen = Seen::default();
    let me = Me {
        exe_wd: exe_wd.as_ref(),
        exe_name: exe_name.as_deref(),
        entry_wds: &entry_wds,
    };
    loop {
        for e in ino.read_events_blocking(&mut buf)? {
            seen.take(&e, &me, waiting.as_ref());
            seen.back |= is_back(&e, waiting.as_ref(), apps_name.as_deref());
        }
        let mut quiet = Duration::ZERO;
        while quiet < DEBOUNCE {
            std::thread::sleep(TICK);
            match ino.read_events(&mut buf) {
                Ok(events) => {
                    let mut any = false;
                    for e in events {
                        any = true;
                        seen.take(&e, &me, waiting.as_ref());
                        seen.back |= is_back(&e, waiting.as_ref(), apps_name.as_deref());
                    }
                    quiet = if any { Duration::ZERO } else { quiet + TICK };
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => quiet += TICK,
                Err(e) => return Err(e.into()),
            }
        }
        if std::mem::take(&mut seen.upgraded)
            && let Some(exe) = &exe
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new(exe).arg("watch").exec();
            eprintln!("eep: restarting as the new eep failed ({err}); going on");
        }
        if std::mem::take(&mut seen.lost_dir) && waiting.is_none() {
            // ~/Applications went away (moved, renamed): change nothing, wait
            // for it to come back.
            seen.relevant = false;
            if let Some(parent) = apps.parent() {
                waiting = ino.watches().add(parent, parent_mask).ok();
            }
        }
        if std::mem::take(&mut seen.back) && apps.is_dir() {
            if let Some(wd) = waiting.take() {
                let _ = ino.watches().remove(wd);
            }
            ino.watches().add(apps, mask)?;
            seen.relevant = true;
        }
        if std::mem::take(&mut seen.relevant) {
            report(ops::reconcile(ctx, false));
        }
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
    fn take(
        &mut self,
        e: &inotify::Event<&std::ffi::OsStr>,
        me: &Me<'_>,
        waiting: Option<&inotify::WatchDescriptor>,
    ) {
        if Some(&e.wd) == me.exe_wd {
            self.upgraded |= e.name.is_some_and(|n| Some(n) == me.exe_name);
            return;
        }
        if waiting.is_some_and(|w| *w == e.wd) {
            return; // the parent, while waiting for ~/Applications (is_back)
        }
        if me.entry_wds.contains(&e.wd) {
            // a launcher entry (not eep's temp files, not mimeinfo.cache)
            self.relevant |= e.name.is_some_and(|n| {
                let n = n.to_string_lossy();
                n.ends_with(".desktop") && !n.starts_with('.')
            });
            return;
        }
        self.relevant |= e.name.is_none_or(|n| is_card_name(&n.to_string_lossy()));
        self.lost_dir |= e
            .mask
            .intersects(inotify::EventMask::DELETE_SELF | inotify::EventMask::MOVE_SELF);
    }
}

/// An event in the parent while waiting for ~/Applications: is it the folder?
fn is_back(
    e: &inotify::Event<&std::ffi::OsStr>,
    waiting: Option<&inotify::WatchDescriptor>,
    apps_name: Option<&std::ffi::OsStr>,
) -> bool {
    waiting.is_some_and(|w| *w == e.wd) && e.name.is_some() && e.name == apps_name
}

/// Nothing installed or found yet: making ~/Applications can't hide a move.
fn first_run(p: &crate::paths::Paths) -> bool {
    crate::integrate::known_ids(p).is_empty() && !p.state_dir().join("found.json").exists()
}

fn is_card_name(n: &str) -> bool {
    n.ends_with(".eep") && !n.starts_with('.')
}

fn report(r: Result<Value>) {
    let v = r.unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") }));
    let quiet = v["ok"] == true
        && v["apps"].as_array().is_some_and(Vec::is_empty)
        && v["forgotten"].as_array().is_some_and(Vec::is_empty)
        && v["found"]
            .as_object()
            .is_some_and(serde_json::Map::is_empty);
    if !quiet {
        println!("{v}");
    }
}

/// Double-clicked card: bring it into `~/Applications` if it isn't there, install
/// what it names, then become the app.
pub fn open(ctx: &Ctx, file: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let card = Card::parse(&text)?;
    let inside = file.parent().is_some_and(|d| same_dir(d, &ctx.p.apps));
    if !inside && cards::find(&ctx.p, &card.id).is_none() {
        if card.is_system() {
            // another computer's app: only if this one has it too
            cards::save(&ctx.p, &card, None)?;
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
    if card.is_system() && cards::find(&ctx.p, &card.id).is_none() {
        // a card for an app this computer doesn't have: nothing to start
        bail!(
            "{} is not on this computer (it came from another one)",
            card.name
        );
    }
    run::run(&ctx.p, &card.id, None, &[], &[])?;
    bail!("exec returned")
}

fn same_dir(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
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
    use super::{HANDLER, PACKAGED_EEP, USER_UNITS};

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
        assert!(HANDLER.contains(&format!("Exec={PACKAGED_EEP} open %f")));
        let driver = include_str!("../../../packaging/eep-driver.service");
        assert!(driver.contains(&format!("ExecStart={PACKAGED_EEP} driver install")));
    }
}
