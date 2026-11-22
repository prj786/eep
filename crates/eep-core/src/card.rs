//! The card: `~/Applications/<Name>.eep`, one small TOML file per installed app.
//! The card is what the user sees and owns; everything else eep keeps
//! (GC roots, icons, launcher entries) is rebuilt from the cards.

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::catalog::Kind;
use crate::{StorePath, valid_id};

pub const VERSION: u32 = 0;
pub const EXTENSION: &str = "eep";

/// Largest icon a card carries (decoded bytes).
pub const ICON_MAX: usize = 96 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub eep: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    pub channel: String,
    pub release: String,
    pub version: String,
    /// The app's store path.
    pub out: String,
    /// Main program relative to `out`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<String>,
    /// Launcher entry relative to `out` (gui apps).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<String>,
    /// Icon relative to `out` (or an absolute store path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// Programs that get PATH shims (cli and tool apps).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub bin: Vec<String>,
    /// The store path before the last update (`eep rollback`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_release: Option<String>,
    pub installed: toml::value::Datetime,
    /// The app's icon inside the card (`image/svg+xml` or `image/png`), so
    /// the card shows its app before it is installed — in Files, Komble,
    /// on the website. Optional; at most [`ICON_MAX`] bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_type: Option<String>,
    /// base64 (standard alphabet) of the icon file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_data: Option<String>,
}

impl Card {
    pub fn parse(text: &str) -> Result<Self> {
        let card: Card = toml::from_str(text).context("not an eep card")?;
        if card.eep != VERSION {
            bail!(
                "card format {} is newer than this eep understands",
                card.eep
            );
        }
        if !valid_id(&card.id) {
            bail!("card has a bad id {:?}", card.id);
        }
        StorePath::from_absolute(&card.out).context("card out")?;
        if let Some(p) = &card.previous {
            StorePath::from_absolute(p).context("card previous")?;
        }
        for rel in [&card.exec, &card.desktop].into_iter().flatten() {
            if !inside(rel) {
                bail!("card paths must stay inside the app: {rel:?}");
            }
        }
        if let Some(i) = &card.icon
            && !inside(i)
            && StorePath::from_absolute(i).is_err()
        {
            bail!("card icon must be inside the store: {i:?}");
        }
        if let Some(d) = &card.icon_data {
            let ok_type = matches!(
                card.icon_type.as_deref(),
                Some("image/svg+xml" | "image/png")
            );
            if !ok_type || d.len() > ICON_MAX * 4 / 3 + 4 {
                bail!("card icon must be an SVG or PNG of at most {ICON_MAX} bytes");
            }
        }
        if card
            .bin
            .iter()
            .any(|b| b.is_empty() || b.contains('/') || b.starts_with('.'))
        {
            bail!("card bin entries must be plain program names");
        }
        Ok(card)
    }

    pub fn to_toml(&self) -> String {
        // Serializing a plain struct of strings can't fail.
        toml::to_string(self).unwrap_or_default()
    }

    pub fn out_path(&self) -> Result<StorePath> {
        StorePath::from_absolute(&self.out)
    }

    /// The file name eep gives a new card: the display name, minus
    /// characters that are awkward in file names.
    pub fn file_name(&self) -> String {
        let clean: String = self
            .name
            .chars()
            .filter(|c| {
                !matches!(
                    c,
                    '/' | '\\' | '\0' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
                )
            })
            .collect();
        let clean = clean.trim().trim_start_matches('.');
        let stem = if clean.is_empty() {
            self.id.as_str()
        } else {
            clean
        };
        format!("{stem}.{EXTENSION}")
    }
}

/// A relative path that can't climb out of its base.
fn inside(rel: &str) -> bool {
    !rel.is_empty() && !rel.starts_with('/') && !rel.split('/').any(|c| c == "..")
}

impl Card {
    /// Put the icon file `bytes` (named `file`, for its type) into the card;
    /// too big or not SVG/PNG → no icon (the card still works).
    pub fn embed_icon(&mut self, file: &str, bytes: &[u8]) {
        use base64::Engine;
        let kind = match file
            .rsplit('.')
            .next()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("svg") => "image/svg+xml",
            Some("png") => "image/png",
            _ => return,
        };
        if bytes.len() > ICON_MAX {
            return;
        }
        self.icon_type = Some(kind.to_owned());
        self.icon_data = Some(base64::engine::general_purpose::STANDARD.encode(bytes));
    }

    /// The embedded icon: (MIME type, bytes).
    pub fn icon_bytes(&self) -> Option<(&str, Vec<u8>)> {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD
            .decode(self.icon_data.as_ref()?)
            .ok()?;
        Some((self.icon_type.as_deref()?, data))
    }
}

pub fn datetime(rfc3339: &str) -> Result<toml::value::Datetime> {
    rfc3339
        .parse()
        .with_context(|| format!("bad date {rfc3339:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card() -> Card {
        Card {
            eep: 0,
            id: "firefox".into(),
            name: "Firefox".into(),
            kind: Kind::Gui,
            channel: "nixos-unstable".into(),
            release: "nixos-26.11pre1125864.2786a2a72088".into(),
            version: "160.0.1".into(),
            out: "/nix/store/lr6rlibz9mi24mag008wxng40ii0q2kb-firefox-160.0.1".into(),
            exec: Some("bin/firefox".into()),
            desktop: Some("share/applications/firefox.desktop".into()),
            icon: Some("share/icons/hicolor/128x128/apps/firefox.png".into()),
            bin: Vec::new(),
            previous: None,
            previous_version: None,
            previous_release: None,
            installed: datetime("2026-11-23T10:00:00Z").expect("date"),
            icon_type: None,
            icon_data: None,
        }
    }

    #[test]
    fn roundtrip() {
        let c = card();
        let text = c.to_toml();
        assert!(text.contains("installed = 2026-11-23T10:00:00Z"), "{text}");
        assert_eq!(Card::parse(&text).expect("parses"), c);
    }

    #[test]
    fn rejects_escapes() {
        let mut c = card();
        c.exec = Some("../../bin/sh".into());
        assert!(Card::parse(&c.to_toml()).is_err());
        let mut c = card();
        c.out = "/usr/bin".into();
        assert!(Card::parse(&c.to_toml()).is_err());
        let mut c = card();
        c.id = "../x".into();
        assert!(Card::parse(&c.to_toml()).is_err());
        let mut c = card();
        c.bin = vec!["../../evil".into()];
        assert!(Card::parse(&c.to_toml()).is_err());
        let mut c = card();
        c.icon = Some("/etc/shadow".into());
        assert!(Card::parse(&c.to_toml()).is_err());
    }

    #[test]
    fn icons_roundtrip() {
        let mut c = card();
        c.embed_icon("firefox.png", b"\x89PNG fake");
        let back = Card::parse(&c.to_toml()).expect("parses");
        assert_eq!(
            back.icon_bytes(),
            Some(("image/png", b"\x89PNG fake".to_vec()))
        );
        let mut big = card();
        big.embed_icon("x.png", &vec![0u8; ICON_MAX + 1]);
        assert!(big.icon_data.is_none(), "too big: left out");
        let mut odd = card();
        odd.embed_icon("x.xpm", b"x");
        assert!(odd.icon_data.is_none());
    }

    #[test]
    fn file_names() {
        let mut c = card();
        assert_eq!(c.file_name(), "Firefox.eep");
        c.name = "A/B: C?".into();
        assert_eq!(c.file_name(), "AB C.eep");
        c.name = "...".into();
        assert_eq!(c.file_name(), "firefox.eep");
    }
}
