//! `index.json`: what one nixpkgs release offers, precomputed by
//! `eep-build index` so the client never evaluates Nix.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub use crate::catalog::Kind;

pub const VERSION: u32 = 0;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Index {
    /// Format version ([`VERSION`]).
    pub eep_index: u32,
    pub channel: String,
    /// `nixos-26.11pre1125864.2786a2a72088`
    pub release: String,
    /// nixpkgs git revision.
    pub rev: String,
    /// When the release was published (RFC 3339). Also how clients order
    /// indexes: there is no build timestamp, so rebuilding an index for the
    /// same release and catalog gives byte-identical output.
    pub released: String,
    pub apps: BTreeMap<String, App>,
    /// Every piece any app (or the driver) needs, by base name.
    pub pieces: BTreeMap<String, Piece>,
    pub system: System,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct App {
    pub name: String,
    pub kind: Kind,
    pub attr: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub homepage: String,
    /// Set when the app can't be installed from this release (unfree,
    /// failed to evaluate, a piece missing from the cache). Everything
    /// below may then be empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
    /// `/nix/store/<hash>-firefox-160.0.1`
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub out: String,
    /// Main program, relative to `out` (`bin/firefox`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// Programs in `out/bin` that get PATH shims (cli and tool kinds).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bin: Vec<String>,
    /// Launcher entry, relative to `out` (`share/applications/firefox.desktop`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
    /// Icon published next to the index (`icons/firefox.png`), for showing
    /// the app before it's installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The same icon inside the app, relative to `out`; installs copy it
    /// from the local store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_path: Option<String>,
    /// Base names of every piece the app needs, itself included, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closure: Vec<String>,
    /// Compressed bytes for the whole closure (nothing installed yet).
    #[serde(default)]
    pub download: u64,
    /// Unpacked bytes for the whole closure.
    #[serde(default)]
    pub disk: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Piece {
    /// Compressed download size.
    pub dl: u64,
    /// Unpacked (NAR) size.
    pub nar: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct System {
    /// What goes into `/run/opengl-driver` (NixOS's `hardware.graphics`
    /// default: mesa's outputs), absolute store paths.
    #[serde(default)]
    pub driver: Vec<String>,
    #[serde(default)]
    pub driver_closure: Vec<String>,
}

/// The small file a channel URL points at: which index is current.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChannelPointer {
    pub eep_channel: u32,
    pub channel: String,
    pub release: String,
    pub released: String,
    /// Path of the compressed index, relative to the server root.
    pub index: String,
    /// sha256 (hex) of the compressed index file.
    pub sha256: String,
}

impl Index {
    pub fn app(&self, id: &str) -> Option<&App> {
        self.apps.get(id)
    }

    /// Bytes to download for `closure`, skipping pieces `have` says are
    /// already in the local store.
    pub fn download_for<'a>(
        &self,
        closure: impl IntoIterator<Item = &'a String>,
        have: impl Fn(&str) -> bool,
    ) -> (u64, usize) {
        closure
            .into_iter()
            .filter(|p| !have(p))
            .fold((0, 0), |(bytes, n), p| {
                (bytes + self.pieces.get(p).map_or(0, |x| x.dl), n + 1)
            })
    }
}
