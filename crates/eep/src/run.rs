//! `eep run <id>`: start an app with a clean environment. This is the hot
//! path (every launcher click), so it does as little as possible: read the
//! card, fix the environment, `exec` — no parent process stays behind.

use std::ffi::OsString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::paths::Paths;
use crate::{cards, env, nix, state};

pub fn run(
    p: &Paths,
    id: &str,
    exec: Option<&str>,
    extra_env: &[String],
    args: &[OsString],
) -> Result<()> {
    let found = cards::find(p, id)
        .with_context(|| format!("{id} is not installed (no card in {})", p.apps.display()))?;
    let card = found.card;
    let out = PathBuf::from(&card.out);
    if !out.exists() {
        // A card copied in from elsewhere, or collected: fetch it now.
        let _lock = state::lock(p)?;
        crate::journal::recover(p)?;
        let _fetching = crate::journal::begin(p, "run")?;
        nix::realise(&card.out, &p.root(id))?;
    }
    let program = match exec {
        Some(e) if e.starts_with('/') => {
            if !e.starts_with("/nix/store/") {
                bail!("--exec must be a program name or a /nix/store path");
            }
            PathBuf::from(e)
        }
        Some(e) if e.is_empty() || e.contains('/') => bail!("bad --exec {e:?}"),
        Some(e) => out.join("bin").join(e),
        None => out.join(
            card.exec
                .as_deref()
                .with_context(|| format!("{id} has no main program"))?,
        ),
    };
    let home = std::env::var_os("HOME").map_or_else(|| PathBuf::from("/"), PathBuf::from);
    let ops = env::plan(|k| std::env::var_os(k), &home, Path::exists);
    let mut cmd = Command::new(&program);
    cmd.args(args);
    env::apply(&mut cmd, &ops);
    for kv in extra_env {
        let (k, v) = kv
            .split_once('=')
            .with_context(|| format!("--env wants VAR=value, got {kv:?}"))?;
        cmd.env(k, v);
    }
    let err = cmd.exec(); // returns only on failure
    Err(err).with_context(|| format!("starting {}", program.display()))
}
