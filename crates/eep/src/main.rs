//! eep — apps from the Nix binary cache, the iOS way: one card per app in
//! `~/Applications`; delete the card and the app is gone.
//!
//! Every command prints one JSON object on stdout and exits 0 (house rule
//! 5); failures are `{"ok":false,"error":"…"}`. The exception is `eep run`,
//! which becomes the app (it prints JSON only if it can't start it).
//! Progress, including Nix's own, goes to stderr.

mod activity;
mod appimage;
mod cards;
mod driver;
mod env;
mod integrate;
mod journal;
mod nix;
mod ops;
mod patch;
mod paths;
mod run;
mod state;
mod system;
mod watch;

use std::ffi::OsString;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(
    name = "eep",
    version,
    about = "Apps from the Nix binary cache, one card per app"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// The index of available apps.
    Index {
        #[command(subcommand)]
        cmd: IndexCmd,
    },
    /// Apps in the index (with install state).
    Available,
    /// One app from the index: versions, sizes, what a download would cost now.
    Info { id: String },
    /// Installed apps (the cards).
    List {
        /// Also the apps found on the computer (pacman, AppImages, Steam…).
        #[arg(long)]
        all: bool,
    },
    /// Install apps: fetch, add to the launcher, write the card. A found
    /// app's id (`sys-…`) brings its card back and shows it again.
    Install {
        #[arg(required = true)]
        ids: Vec<String>,
        #[command(flatten)]
        fetch: FetchArgs,
    },
    /// Remove apps (card, launcher entry, roots). Disk is freed by `eep gc`.
    /// A found app (`sys-…`) is hidden, not uninstalled.
    Remove {
        #[arg(required = true)]
        ids: Vec<String>,
    },
    /// Start an app.
    Run {
        id: String,
        /// Program in the app's bin/ (or a /nix/store path) instead of the main one.
        #[arg(long)]
        exec: Option<String>,
        /// Extra VAR=value for the app (repeatable).
        #[arg(long = "env", value_name = "VAR=value")]
        env: Vec<String>,
        /// Arguments for the app (after `--`).
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<OsString>,
    },
    /// Updates available for installed apps.
    Updates {
        /// Only the number (cheap: no Nix call).
        #[arg(long)]
        count: bool,
    },
    /// Update some apps, or all of them.
    Update {
        ids: Vec<String>,
        #[command(flatten)]
        fetch: FetchArgs,
    },
    /// Go back to the version before the last update.
    Rollback { id: String },
    /// Rebuild launcher entries, icons, shims and roots from the cards.
    Repair,
    /// Follow ~/Applications: a card copied in gets installed, a deleted one removed.
    Watch,
    /// Open a card (double-click): start its app, installing it first if needed.
    Open {
        file: std::path::PathBuf,
        /// Not installed yet: let the installer (ewe Files) show the app
        /// and its Install button instead of installing right away.
        #[arg(long)]
        ask: bool,
    },
    /// Put a card into ~/Applications and install its app (what a file
    /// manager's "Install" does). The app is not started.
    Add { file: std::path::PathBuf },
    /// What a card file is, and what installing it would take.
    Card { file: std::path::PathBuf },
    /// One-time per-user setup: card file type, watcher and update timer.
    Setup,
    /// Free the disk space of removed apps (and old previous versions).
    Gc {
        #[arg(long)]
        dry_run: bool,
    },
    /// Check the setup (Nix, caches, driver, PATH, environment).
    Doctor {
        /// Also check one installed app.
        #[arg(long)]
        app: Option<String>,
    },
    /// The graphics driver for Nix apps (needs root).
    Driver {
        #[command(subcommand)]
        cmd: DriverCmd,
    },
}

#[derive(clap::Args, Debug)]
struct FetchArgs {
    /// Download whole pieces, no patches (for comparisons).
    #[arg(long)]
    no_patches: bool,
    /// Testing only: skip eep's hash check of patched pieces (Nix still checks).
    #[arg(long, hide = true)]
    unchecked_patches: bool,
}

impl FetchArgs {
    fn get(&self) -> ops::Fetch {
        ops::Fetch {
            patches: !self.no_patches,
            verify: !self.unchecked_patches,
        }
    }
}

#[derive(Subcommand, Debug)]
enum IndexCmd {
    /// Download the newest index from the eep server.
    Update,
}

#[derive(Subcommand, Debug)]
enum DriverCmd {
    /// Set up /run/opengl-driver from the index's driver set.
    Install,
}

fn main() {
    cards::migrate_old_folder(&paths::Paths::from_env());
    // Started under a program's name: a PATH shim (see integrate.rs).
    if let Some(prog) = invoked_as() {
        shim(&prog);
    }
    // Rule 5 holds for a mistyped command too: help and --version print as
    // usual, every other parse error is one JSON line, exit 0.
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(e) => {
            use clap::error::ErrorKind;
            if matches!(e.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
                let _ = e.print();
                return;
            }
            // clap's first paragraph ("error: … not provided:\n  <IDS>..."), one line
            let msg = e.render().to_string();
            let words: Vec<&str> = msg
                .lines()
                .take_while(|l| !l.trim().is_empty())
                .map(str::trim)
                .collect();
            let text = words.join(" ");
            let text = text.strip_prefix("error: ").unwrap_or(&text);
            println!("{}", json!({ "ok": false, "error": text }));
            return;
        }
    };
    let out = std::panic::catch_unwind(|| dispatch(cli.cmd))
        .unwrap_or_else(|_| Err(anyhow::anyhow!("eep crashed (a bug): see stderr")))
        .unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") }));
    println!("{out}");
}

/// App ids name files (roots, shims, launchers): one bad one could reach
/// outside eep's folders, so every id from the command line is checked.
fn check_ids<'a>(ids: impl IntoIterator<Item = &'a String>) -> Result<()> {
    for id in ids {
        if !eep_core::valid_id(id) {
            anyhow::bail!("{id:?} is not an app id (a-z, 0-9 and -)");
        }
    }
    Ok(())
}

/// The program name eep was started as, when it isn't `eep`.
fn invoked_as() -> Option<String> {
    let argv0 = std::env::args_os().next()?;
    let name = std::path::Path::new(&argv0)
        .file_name()?
        .to_str()?
        .to_owned();
    (name != "eep").then_some(name)
}

/// Run `prog` from the app that owns the shim, like `eep run <id> --exec
/// <prog> -- <args>`. Errors go to stderr with status 127, as a shell's
/// "command not found" would: this is a program, not an eep command.
fn shim(prog: &str) -> ! {
    let p = paths::Paths::from_env();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let err = match integrate::shim_owner(&p, prog) {
        Some(id) => run::run(&p, &id, Some(prog), &[], &args).err(),
        None => Some(anyhow::anyhow!("no installed app provides {prog}")),
    };
    eprintln!(
        "{prog}: {:#}",
        err.unwrap_or_else(|| anyhow::anyhow!("could not start"))
    );
    std::process::exit(127);
}

fn dispatch(cmd: Cmd) -> Result<Value> {
    match &cmd {
        Cmd::Install { ids, .. } | Cmd::Remove { ids } | Cmd::Update { ids, .. } => {
            check_ids(ids)?;
        }
        Cmd::Run { id, .. } | Cmd::Info { id } | Cmd::Rollback { id } => check_ids([id])?,
        Cmd::Doctor { app: Some(id) } => check_ids([id])?,
        _ => {}
    }
    let p = paths::Paths::from_env();
    if let Cmd::Run {
        id,
        exec,
        env,
        args,
    } = cmd
    {
        run::run(&p, &id, exec.as_deref(), &env, &args)?;
        unreachable!("exec returned without an error");
    }
    let eep = std::env::current_exe().unwrap_or_else(|_| "eep".into());
    let ctx = ops::Ctx { p, eep };
    match cmd {
        Cmd::Index {
            cmd: IndexCmd::Update,
        } => state::update_index(&ctx.p),
        Cmd::Available => ops::available(&ctx),
        Cmd::Info { id } => ops::info(&ctx, &id),
        Cmd::List { all } => ops::list(&ctx, all),
        Cmd::Install { ids, fetch } => ops::install(&ctx, &ids, fetch.get()),
        Cmd::Remove { ids } => ops::remove(&ctx, &ids),
        Cmd::Updates { count } => ops::updates(&ctx, count),
        Cmd::Update { ids, fetch } => ops::update(&ctx, &ids, fetch.get()),
        Cmd::Rollback { id } => ops::rollback(&ctx, &id),
        Cmd::Repair => ops::reconcile(&ctx, true),
        Cmd::Watch => watch::watch(&ctx),
        Cmd::Open { file, ask } => watch::open(&ctx, &file, ask),
        Cmd::Add { file } => ops::add(&ctx, &file),
        Cmd::Card { file } => ops::card_info(&ctx, &file),
        Cmd::Setup => watch::setup(&ctx),
        Cmd::Gc { dry_run } => ops::gc(&ctx, dry_run),
        Cmd::Doctor { app } => ops::doctor(&ctx, app.as_deref()),
        Cmd::Driver {
            cmd: DriverCmd::Install,
        } => driver::install(&ctx.p),
        Cmd::Run { .. } => unreachable!(),
    }
}
