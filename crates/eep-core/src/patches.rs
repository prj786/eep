//! Update patches: `patches/<from>__<to>/manifest.json` plus one zstd
//! `--patch-from` file per piece. A patch turns a piece the client already
//! has (`base`) into the new piece's NAR; the client then offers that NAR
//! to Nix with the upstream narinfo, signature untouched, and Nix checks
//! it like any download.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 0;

/// zstd window for patches: 2 GiB, so a whole piece fits as reference.
pub const WINDOW_LOG: u32 = 31;

/// A patch bigger than this share of the full download isn't worth it.
pub const MAX_RATIO: f64 = 0.6;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Manifest {
    pub eep_patches: u32,
    pub from: String,
    pub to: String,
    /// By the new piece's base name.
    pub patches: BTreeMap<String, Patch>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Patch {
    /// Base name of the piece to patch from.
    pub base: String,
    /// Relative to the manifest (`p/<hash>.zst`).
    pub file: String,
    /// Patch bytes.
    pub size: u64,
    /// What a full download of the piece costs.
    pub full: u64,
    /// `sha256:<nix32>` of the NAR the patch produces.
    pub nar_hash: String,
    pub nar_size: u64,
    /// The upstream narinfo, verbatim (its `Sig` is what Nix checks).
    pub narinfo: String,
}

/// `patches/<from>__<to>/manifest.json`
pub fn manifest_path(from: &str, to: &str) -> String {
    format!("patches/{from}__{to}/manifest.json")
}
