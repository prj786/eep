//! `index.json`: what one nixpkgs release offers, precomputed by
//! `eep-build index` so the client never evaluates Nix.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::card::{SYSTEM_PREFIX, inside};
use crate::{StorePath, valid_id};

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

impl App {
    fn validate(&self) -> Result<()> {
        if !self.out.is_empty() {
            StorePath::from_absolute(&self.out).context("out")?;
        }
        for b in &self.closure {
            StorePath::from_base_name(b).context("closure")?;
        }
        for r in [&self.exec, &self.desktop].into_iter().flatten() {
            if !inside(r) {
                bail!("path {r:?} climbs out of the app");
            }
        }
        if let Some(i) = &self.icon_path
            && !inside(i)
            && StorePath::from_absolute(i).is_err()
        {
            bail!("icon path {i:?} is outside the store");
        }
        if let Some(i) = &self.icon
            && !valid_rel(i)
        {
            bail!("strange icon {i:?}");
        }
        if self
            .bin
            .iter()
            .any(|b| b.is_empty() || b.contains('/') || b.starts_with('.'))
        {
            bail!("bin entries must be plain program names");
        }
        Ok(())
    }
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

/// A release or channel name, `nixos-26.11pre1125864.2786a2a72088`. It
/// names folders (the icon cache) and URLs, so it is one plain name.
pub fn valid_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && !s.starts_with('.')
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"._-+".contains(&c))
}

/// A server-relative file (`icons/firefox.svg`, `index.json.zst`): plain
/// names joined by `/`, never `..`, never absolute.
pub fn valid_rel(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 512
        && s.split('/').all(|c| {
            !c.is_empty()
                && !c.starts_with('.')
                && c.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-+@".contains(&b))
        })
}

impl ChannelPointer {
    /// The pointer comes from the network: its names become paths and URLs.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.release) || !valid_name(&self.channel) || !valid_rel(&self.index) {
            bail!(
                "the channel pointer names something strange: release {:?}, index {:?}",
                self.release,
                self.index
            );
        }
        Ok(())
    }
}

impl Index {
    /// An index comes from the network, and root's driver timer reads it
    /// too: every field the client turns into a path, a file name or a
    /// program argument must be what it claims to be.
    pub fn validate(&self) -> Result<()> {
        if !valid_name(&self.release) || !valid_name(&self.channel) {
            bail!("the index names a strange release {:?}", self.release);
        }
        for (id, a) in &self.apps {
            if !valid_id(id) || id.starts_with(SYSTEM_PREFIX) {
                bail!("the index has a bad app id {id:?}");
            }
            a.validate().with_context(|| format!("index app {id}"))?;
        }
        for k in self.pieces.keys() {
            StorePath::from_base_name(k).context("index piece")?;
        }
        for d in &self.system.driver {
            StorePath::from_absolute(d).context("index driver")?;
        }
        for b in &self.system.driver_closure {
            StorePath::from_base_name(b).context("index driver piece")?;
        }
        Ok(())
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> Index {
        serde_json::from_value(serde_json::json!({
            "eep_index": 0, "channel": "nixos-unstable",
            "release": "nixos-26.11pre1125864.2786a2a72088", "rev": "x", "released": "y",
            "apps": { "rg": {
                "name": "ripgrep", "kind": "cli", "attr": "ripgrep",
                "out": "/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0",
                "exec": "bin/rg", "bin": ["rg"], "icon": "icons/rg.svg",
                "closure": ["whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0"]
            }},
            "pieces": {}, "system": { "driver": [], "driver_closure": [] }
        }))
        .expect("index")
    }

    #[test]
    fn a_good_index_passes() {
        index().validate().expect("valid");
    }

    #[test]
    fn hostile_indexes_are_refused() {
        let mut i = index();
        i.release = "/etc/cron.d".into();
        assert!(i.validate().is_err(), "absolute release");
        let mut i = index();
        i.release = "../../..".into();
        assert!(i.validate().is_err(), "climbing release");
        let mut i = index();
        let app = i.apps.remove("rg").expect("rg");
        i.apps.insert("../../Trash".into(), app.clone());
        assert!(i.validate().is_err(), "bad id");
        let mut i = index();
        i.apps.insert("sys-gimp".into(), app.clone());
        assert!(i.validate().is_err(), "reserved id");
        let mut i = index();
        i.apps.get_mut("rg").expect("rg").bin = vec!["../../../../.ssh/authorized_keys".into()];
        assert!(i.validate().is_err(), "bin path");
        let mut i = index();
        i.apps.get_mut("rg").expect("rg").icon = Some("icons/../../x".into());
        assert!(i.validate().is_err(), "icon path");
        let mut i = index();
        i.apps.get_mut("rg").expect("rg").out = "/usr/bin".into();
        assert!(i.validate().is_err(), "out outside the store");
        let mut i = index();
        i.system.driver = vec!["--option".into()];
        assert!(i.validate().is_err(), "driver as a flag");
    }

    #[test]
    fn names() {
        assert!(valid_name("nixos-26.11pre1125864.2786a2a72088"));
        assert!(!valid_name(".."));
        assert!(!valid_name("a/b"));
        assert!(valid_rel("icons/firefox.svg"));
        assert!(!valid_rel("icons/../x"));
        assert!(!valid_rel("/etc/passwd"));
        assert!(!valid_rel("icons//x"));
    }
}
