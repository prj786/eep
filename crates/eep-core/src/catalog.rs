//! `catalog/apps.toml`: the hand-written list of apps eep offers.

use std::collections::BTreeMap;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::valid_id;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Has a launcher entry.
    #[default]
    Gui,
    /// Command-line app: its programs go on PATH.
    Cli,
    /// Helper programs (driver checks and the like): on PATH, no launcher.
    Tool,
    /// Part of the system (the graphics driver); never shown as an app.
    System,
}

impl Kind {
    /// Whether the app's `bin/` programs get PATH shims.
    pub fn on_path(self) -> bool {
        matches!(self, Kind::Cli | Kind::Tool)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub attr: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub desktop: Option<String>,
    /// Programs that go on PATH (cli and tool apps). Default: the main
    /// program only, so a package of 300 demos doesn't flood PATH and the
    /// launcher's search.
    #[serde(default)]
    pub bin: Option<Vec<String>>,
}

pub type Catalog = BTreeMap<String, Entry>;

pub fn parse(text: &str) -> Result<Catalog> {
    let cat: Catalog = toml::from_str(text).context("catalog is not valid TOML")?;
    for (id, e) in &cat {
        if !valid_id(id) {
            bail!("catalog: bad app id {id:?} (a-z, 0-9, '-')");
        }
        if e.attr.is_empty() || e.attr.contains(char::is_whitespace) {
            bail!("catalog: {id}: bad attr {:?}", e.attr);
        }
        if let Some(b) = &e.bin
            && b.iter()
                .any(|x| x.is_empty() || x.contains('/') || x.starts_with('.'))
        {
            bail!("catalog: {id}: bin entries must be plain program names");
        }
        if let Some(d) = &e.desktop
            && (d.contains('/') || !d.ends_with(".desktop"))
        {
            bail!("catalog: {id}: desktop must be a bare *.desktop file name");
        }
    }
    Ok(cat)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_repo_catalog() {
        let cat = parse(include_str!("../../../catalog/apps.toml")).expect("repo catalog parses");
        assert_eq!(cat["ripgrep"].kind, Kind::Cli);
        assert_eq!(cat["firefox"].desktop.as_deref(), Some("firefox.desktop"));
    }

    #[test]
    fn rejects_bad() {
        assert!(parse("[Bad]\nattr='x'\nname='x'").is_err());
        assert!(parse("[a]\nattr='x'\nname='x'\nwat=1").is_err());
        assert!(parse("[a]\nattr='x'\nname='x'\ndesktop='../x.desktop'").is_err());
        assert!(parse("[a]\nattr='x'\nname='x'\nbin=['../sh']").is_err());
    }
}
