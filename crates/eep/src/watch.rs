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
    std::fs::create_dir_all(apps)?;
    ino.watches()
        .add(apps, mask)
        .with_context(|| format!("watching {}", apps.display()))?;
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
    let me = (exe_wd.as_ref(), exe_name.as_deref());
    loop {
        for e in ino.read_events_blocking(&mut buf)? {
            seen.take(&e, me);
        }
        let mut quiet = Duration::ZERO;
        while quiet < DEBOUNCE {
            std::thread::sleep(TICK);
            match ino.read_events(&mut buf) {
                Ok(events) => {
                    let mut any = false;
                    for e in events {
                        any = true;
                        seen.take(&e, me);
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
        if std::mem::take(&mut seen.lost_dir) {
            // ~/Applications itself went away: recreate it and watch the new one.
            std::fs::create_dir_all(apps)?;
            ino.watches().add(apps, mask)?;
            seen.relevant = true;
        }
        if std::mem::take(&mut seen.relevant) {
            report(ops::reconcile(ctx, false));
        }
    }
}

/// What a batch of inotify events asked for.
#[derive(Debug, Default)]
struct Seen {
    /// A card changed in ~/Applications.
    relevant: bool,
    /// ~/Applications itself was removed or moved.
    lost_dir: bool,
    /// eep's own file was replaced.
    upgraded: bool,
}

impl Seen {
    fn take(
        &mut self,
        e: &inotify::Event<&std::ffi::OsStr>,
        (exe_wd, exe_name): (Option<&inotify::WatchDescriptor>, Option<&std::ffi::OsStr>),
    ) {
        if Some(&e.wd) == exe_wd {
            self.upgraded |= e.name.is_some_and(|n| Some(n) == exe_name);
            return;
        }
        self.relevant |= e.name.is_none_or(|n| is_card_name(&n.to_string_lossy()));
        self.lost_dir |= e
            .mask
            .intersects(inotify::EventMask::DELETE_SELF | inotify::EventMask::MOVE_SELF);
    }
}

fn is_card_name(n: &str) -> bool {
    n.ends_with(".eep") && !n.starts_with('.')
}

fn report(r: Result<Value>) {
    let v = r.unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") }));
    let quiet = v["ok"] == true
        && v["apps"].as_array().is_some_and(Vec::is_empty)
        && v["forgotten"].as_array().is_some_and(Vec::is_empty);
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
        cards::save(&ctx.p, &card, None)?;
    }
    let r = ops::reconcile(ctx, false)?;
    if r["ok"] != true {
        return Ok(r);
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

const MIME_XML: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<mime-info xmlns="http://www.freedesktop.org/standards/shared-mime-info">
  <mime-type type="application/x-eep">
    <comment>eep app</comment>
    <glob pattern="*.eep"/>
  </mime-type>
</mime-info>
"#;

/// Per-user wiring: the card file type and its handler, the watcher
/// service and the update timer. Writes only eep's own files.
pub fn setup(ctx: &Ctx) -> Result<Value> {
    let p = &ctx.p;
    let eep = ctx.eep.to_string_lossy();
    let data = p.launchers.parent().context("data dir")?;
    let systemd = p
        .config
        .parent()
        .context("config dir")?
        .join("systemd/user");

    write_atomic(&data.join("mime/packages/eep.xml"), MIME_XML.as_bytes())?;
    let handler = format!(
        "[Desktop Entry]\nType=Application\nName=eep\nNoDisplay=true\nMimeType=application/x-eep;\nExec={} open %f\n",
        eep_core::desktop::quote_arg(&eep)
    );
    write_atomic(&p.launchers.join("eep-open.desktop"), handler.as_bytes())?;
    write_atomic(
        &systemd.join("eep-watch.service"),
        format!(
            "[Unit]\nDescription=eep: follow ~/Applications\n\n[Service]\nExecStart={eep} watch\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n"
        )
        .as_bytes(),
    )?;
    write_atomic(
        &systemd.join("eep-updates.service"),
        format!(
            "[Unit]\nDescription=eep: check for app updates, free removed apps' space\n\n[Service]\nType=oneshot\nExecStart=sh -c '{eep} index update >/dev/null; {eep} updates; {eep} gc >/dev/null'\n"
        )
        .as_bytes(),
    )?;
    write_atomic(
        &systemd.join("eep-updates.timer"),
        b"[Unit]\nDescription=eep: check for app updates\n\n[Timer]\nOnStartupSec=2min\nOnUnitActiveSec=6h\n\n[Install]\nWantedBy=timers.target\n",
    )?;
    std::fs::create_dir_all(&p.apps)?;

    let mut steps = Vec::new();
    for (what, cmd) in [
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
        (
            "enable",
            vec![
                "systemctl".into(),
                "--user".into(),
                "enable".into(),
                "--now".into(),
                "eep-watch.service".into(),
                "eep-updates.timer".into(),
            ],
        ),
    ] {
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
        "steps": steps,
        "path_hint": format!("add {} to PATH for command-line apps", p.bin().display()),
    }))
}
