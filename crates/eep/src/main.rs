//! eep — apps from the Nix binary cache, the iOS way: one card per app in
//! `~/Applications`; delete the card and the app is gone.
//!
//! Every command prints one JSON object on stdout and exits 0 (house rule
//! 5); failures are `{"ok":false,"error":"…"}`. The exception is `eep run`,
//! which becomes the app (it prints JSON only if it can't start it).
//! Progress, including Nix's own, goes to stderr.

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
    List,
    /// Install apps: fetch, add to the launcher, write the card.
    Install {
        #[arg(required = true)]
        ids: Vec<String>,
        #[command(flatten)]
        fetch: FetchArgs,
    },
    /// Remove apps (card, launcher entry, roots). Disk is freed by `eep gc`.
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
    /// Open a card (double-click): install it if needed, then start the app.
    Open { file: std::path::PathBuf },
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
    let cli = Cli::parse();
    let out =
        dispatch(cli.cmd).unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") }));
    println!("{out}");
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
        Cmd::List => ops::list(&ctx),
        Cmd::Install { ids, fetch } => ops::install(&ctx, &ids, fetch.get()),
        Cmd::Remove { ids } => ops::remove(&ctx, &ids),
        Cmd::Updates { count } => ops::updates(&ctx, count),
        Cmd::Update { ids, fetch } => ops::update(&ctx, &ids, fetch.get()),
        Cmd::Rollback { id } => ops::rollback(&ctx, &id),
        Cmd::Repair => ops::reconcile(&ctx, true),
        Cmd::Watch => watch::watch(&ctx),
        Cmd::Open { file } => watch::open(&ctx, &file),
        Cmd::Setup => watch::setup(&ctx),
        Cmd::Gc { dry_run } => ops::gc(&ctx, dry_run),
        Cmd::Doctor { app } => ops::doctor(&ctx, app.as_deref()),
        Cmd::Driver {
            cmd: DriverCmd::Install,
        } => driver::install(&ctx.p),
        Cmd::Run { .. } => unreachable!(),
    }
}
