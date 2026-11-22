//! eep-build — eep's server side. Turns a nixpkgs release and the catalog
//! into the index clients read (and, later, update patches).
//!
//! Every command prints one JSON object and exits 0 (house rule 5);
//! failures are `{"ok":false,"error":"…"}`. Progress goes to stderr.

mod cards;
mod index;
mod nixeval;
mod patches;
mod releases;
mod upstream;
mod util;

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use eep_core::http::Http;
use eep_core::index::{ChannelPointer, Index};
use serde_json::{Value, json};

#[derive(Parser, Debug)]
#[command(
    name = "eep-build",
    version,
    about = "Build eep indexes from nixpkgs releases"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// List the newest releases of a channel (oldest first).
    Releases {
        #[arg(long, default_value = "nixos-unstable")]
        channel: String,
        #[arg(long, default_value_t = 12)]
        count: usize,
    },
    /// Build dist/<release>/index.json.zst and its icons.
    Index {
        #[arg(long)]
        release: String,
        #[arg(long, default_value = "nixos-unstable")]
        channel: String,
        #[arg(long, default_value = "catalog/apps.toml")]
        catalog: PathBuf,
        #[arg(long, default_value = "dist")]
        dist: PathBuf,
        /// Where Nix evaluation runs: auto, native or podman.
        #[arg(long, default_value = "auto")]
        nix: String,
    },
    /// Build update patches from release A to release B (both indexed).
    Patches {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        #[arg(long, default_value = "dist")]
        dist: PathBuf,
        /// Only pieces these apps need (comma-separated ids; default all).
        #[arg(long, value_delimiter = ',')]
        apps: Vec<String>,
        /// Patches made at once (each one is a zstd -19 run).
        #[arg(long, default_value_t = 4)]
        threads: usize,
    },
    /// Ready-to-install cards and catalog.json for a built release.
    Cards {
        #[arg(long)]
        release: String,
        #[arg(long, default_value = "dist")]
        dist: PathBuf,
    },
    /// Point dist/channels/<channel>.json at a built release.
    Publish {
        #[arg(long)]
        release: String,
        #[arg(long, default_value = "dist")]
        dist: PathBuf,
    },
}

fn main() {
    let cli = Cli::parse();
    let out = run(cli.cmd).unwrap_or_else(|e| json!({ "ok": false, "error": format!("{e:#}") }));
    println!("{out}");
}

fn run(cmd: Cmd) -> Result<Value> {
    let cache = util::cache_dir();
    match cmd {
        Cmd::Releases { channel, count } => {
            let list = releases::list(&Http::new(), &channel, count)?;
            Ok(json!({ "ok": true, "channel": channel, "releases": list }))
        }
        Cmd::Index {
            release,
            channel,
            catalog,
            dist,
            nix,
        } => {
            let up = upstream::Upstream::new(Http::new(), cache.clone());
            let runner = nixeval::Runner::pick(&nix)?;
            index::run(
                &up,
                &index::Opts {
                    channel,
                    release,
                    catalog,
                    dist,
                    runner,
                    cache,
                },
            )
        }
        Cmd::Patches {
            from,
            to,
            dist,
            apps,
            threads,
        } => {
            let up = upstream::Upstream::new(Http::new(), cache.clone());
            patches::run(
                &up,
                &patches::Opts {
                    from,
                    to,
                    dist,
                    apps,
                    threads,
                    cache,
                },
            )
        }
        Cmd::Cards { release, dist } => cards::run(&dist, &release),
        Cmd::Publish { release, dist } => publish(&dist, &release),
    }
}

fn publish(dist: &std::path::Path, release: &str) -> Result<Value> {
    let rel = format!("{release}/index.json.zst");
    let zst = std::fs::read(dist.join(&rel))
        .with_context(|| format!("no {rel} in {}; run index first", dist.display()))?;
    let idx: Index = serde_json::from_slice(&zstd::decode_all(zst.as_slice())?).context("index")?;
    let ptr = ChannelPointer {
        eep_channel: 0,
        channel: idx.channel.clone(),
        release: idx.release.clone(),
        released: idx.released.clone(),
        index: rel,
        sha256: util::sha256_hex(&zst),
    };
    let path = dist.join("channels").join(format!("{}.json", idx.channel));
    util::write_atomic(&path, &serde_json::to_vec_pretty(&ptr)?)?;
    Ok(json!({ "ok": true, "channel": idx.channel, "release": idx.release, "pointer": path }))
}
