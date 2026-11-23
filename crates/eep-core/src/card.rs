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

/// Where a card's app comes from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// eep fetched it from the Nix binary cache (`out` is its store path).
    #[default]
    Nix,
    /// It was already on this computer (pacman, an AppImage, Steam, a
    /// launcher entry of the user's): the card stands for its launcher
    /// entry `entry`. Removing the card hides the app; restoring it shows
    /// the app again. Nothing is downloaded or uninstalled.
    System,
    /// An AppImage: one file from `url`, unpacked under eep's data folder
    /// (`out` is where, relative to it). Like an eep app in every way a
    /// person sees — the card installs it, the Trash removes it — but it
    /// brings everything it needs itself: nothing is shared, nothing is
    /// patched.
    Appimage,
}

impl Source {
    pub fn is_nix(&self) -> bool {
        *self == Source::Nix
    }
}

/// The folder under eep's data folder that holds unpacked AppImages:
/// `appimage/<id>/<key>`, the key being the first 16 hex digits of the
/// file's sha256.
pub const APPIMAGE_DIR: &str = "appimage";
/// Hex digits of the sha256 that name an unpacked AppImage's folder.
pub const APPIMAGE_KEY: usize = 16;

/// Where the AppImage `sha256` of app `id` is unpacked, relative to eep's
/// data folder: the `out` of its card.
pub fn appimage_out(id: &str, sha256: &str) -> String {
    let key = sha256.get(..APPIMAGE_KEY).unwrap_or(sha256);
    format!("{APPIMAGE_DIR}/{id}/{key}")
}

/// 64 lower-case hex digits.
pub fn valid_sha256(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Prefix of the ids eep gives apps it finds on the computer (`sys-gimp`).
/// The catalog never uses it, so a found app and an eep app never share an id.
pub const SYSTEM_PREFIX: &str = "sys-";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Card {
    pub eep: u32,
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default, skip_serializing_if = "Source::is_nix")]
    pub source: Source,
    /// Where a found app came from: `pacman`, `appimage`, `steam`,
    /// `flatpak`, `local` (the user's own entry) or `system` (system cards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The launcher entry's desktop file id, `gimp.desktop` (system cards).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entry: Option<String>,
    /// The pacman package that owns the entry (system cards, when known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// Where an AppImage's file is downloaded from (`https://…`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// sha256 of the AppImage file, hex. A card that names it gets exactly
    /// that file; eep writes it into the card once the app is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// The AppImage file's size in bytes, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// GitHub `owner/name` whose releases carry the app's newer versions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repo: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub channel: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub release: String,
    #[serde(default)]
    pub version: String,
    /// The app's store path (eep apps).
    #[serde(default, skip_serializing_if = "String::is_empty")]
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
        if card.source == Source::System {
            return card.check_system();
        }
        if card.source == Source::Appimage {
            return card.check_appimage();
        }
        if card.url.is_some() || card.sha256.is_some() || card.repo.is_some() {
            bail!("only an AppImage card names a download");
        }
        if card.id.starts_with(SYSTEM_PREFIX) {
            bail!(
                "card id {:?} is reserved for apps found on the computer",
                card.id
            );
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
        card.check_icon()?;
        if card
            .bin
            .iter()
            .any(|b| b.is_empty() || b.contains('/') || b.starts_with('.'))
        {
            bail!("card bin entries must be plain program names");
        }
        Ok(card)
    }

    /// A system card names a launcher entry and nothing to fetch or run
    /// from the store.
    fn check_system(self) -> Result<Self> {
        if !self.id.starts_with(SYSTEM_PREFIX) {
            bail!(
                "a card for an app found on the computer needs an id starting with {SYSTEM_PREFIX:?}"
            );
        }
        let entry = self.entry.as_deref().unwrap_or_default();
        if !valid_entry(entry) {
            bail!("card has a bad launcher entry {entry:?}");
        }
        if !self.out.is_empty()
            || self.exec.is_some()
            || self.desktop.is_some()
            || self.icon.is_some()
            || !self.bin.is_empty()
            || self.previous.is_some()
        {
            bail!("a card for an app found on the computer names no store paths");
        }
        self.check_icon()?;
        Ok(self)
    }

    /// An AppImage card names a download (or the file it was made from, by
    /// its hash) and paths inside eep's own folder — never a store path,
    /// never a path that could leave that folder.
    fn check_appimage(self) -> Result<Self> {
        if self.id.starts_with(SYSTEM_PREFIX) {
            bail!(
                "card id {:?} is reserved for apps found on the computer",
                self.id
            );
        }
        if let Some(url) = &self.url
            && (!url.starts_with("https://")
                || url.len() > 2048
                || url.contains(|c: char| c.is_whitespace() || c.is_control()))
        {
            bail!("an AppImage card's url must be one https:// address");
        }
        if let Some(h) = &self.sha256
            && !valid_sha256(h)
        {
            bail!("an AppImage card's sha256 must be 64 hex digits");
        }
        if let Some(r) = &self.repo {
            let part = |s: &str| {
                !s.is_empty()
                    && s.len() <= 100
                    && s.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    && !s.starts_with('.')
            };
            if !r.split_once('/').is_some_and(|(a, b)| part(a) && part(b)) {
                bail!("an AppImage card's repo must be owner/name");
            }
        }
        if !self.out.is_empty()
            && self
                .sha256
                .as_deref()
                .is_none_or(|h| self.out != appimage_out(&self.id, h))
        {
            bail!("an AppImage card's out must be where its sha256 is unpacked");
        }
        if self.entry.is_some()
            || self.package.is_some()
            || !self.channel.is_empty()
            || !self.release.is_empty()
            || !self.bin.is_empty()
            || self.previous.is_some()
        {
            bail!("an AppImage card names no store paths, packages or releases");
        }
        for rel in [&self.exec, &self.desktop, &self.icon]
            .into_iter()
            .flatten()
        {
            if !inside(rel) {
                bail!("card paths must stay inside the app: {rel:?}");
            }
        }
        self.check_icon()?;
        Ok(self)
    }

    fn check_icon(&self) -> Result<()> {
        if let Some(d) = &self.icon_data {
            let ok_type = matches!(
                self.icon_type.as_deref(),
                Some("image/svg+xml" | "image/png")
            );
            if !ok_type || d.len() > ICON_MAX * 4 / 3 + 4 {
                bail!("card icon must be an SVG or PNG of at most {ICON_MAX} bytes");
            }
        }
        Ok(())
    }

    pub fn is_system(&self) -> bool {
        self.source == Source::System
    }

    pub fn is_appimage(&self) -> bool {
        self.source == Source::Appimage
    }

    /// An app from the Nix binary cache (what eep was made for).
    pub fn is_nix(&self) -> bool {
        self.source == Source::Nix
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

/// A desktop file id: `gimp.desktop`, `Dota 2.desktop` — one file name,
/// never a path.
pub fn valid_entry(entry: &str) -> bool {
    entry.len() > ".desktop".len()
        && entry.len() <= 255
        && entry.ends_with(".desktop")
        && !entry.starts_with('.')
        && !entry.contains(['/', '\0', '\n', '\r'])
}

/// A relative path that can't climb out of its base.
pub fn inside(rel: &str) -> bool {
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
            source: Default::default(),
            from: None,
            entry: None,
            package: None,
            url: None,
            sha256: None,
            size: None,
            repo: None,
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
    fn system_cards() {
        let text = "eep = 0\nid = \"sys-gimp\"\nname = \"GIMP\"\nsource = \"system\"\n\
                    from = \"pacman\"\nentry = \"gimp.desktop\"\npackage = \"gimp\"\n\
                    version = \"3.2.8-1\"\ninstalled = 2026-11-23T10:00:00Z\n";
        let c = Card::parse(text).expect("parses");
        assert!(c.is_system());
        assert_eq!(Card::parse(&c.to_toml()).expect("roundtrip"), c);
        assert!(
            !c.to_toml().contains("out ="),
            "no store fields: {}",
            c.to_toml()
        );
        // a path for an entry, a store path, or an eep id: refused
        for bad in [
            text.replace("gimp.desktop", "../../.bashrc.desktop"),
            text.replace("gimp.desktop", "gimp"),
            format!("{text}out = \"/nix/store/yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-gimp-3\"\n"),
            text.replace("sys-gimp", "gimp"),
        ] {
            assert!(Card::parse(&bad).is_err(), "{bad}");
        }
        // an eep card can't take a found app's id
        let mut nix = card();
        nix.id = "sys-firefox".into();
        assert!(Card::parse(&nix.to_toml()).is_err());
    }

    #[test]
    fn appimage_cards() {
        let sha = "ab".repeat(32);
        let text = "eep = 0\nid = \"obsidian\"\nname = \"Obsidian\"\nsource = \"appimage\"\n\
             version = \"1.8.4\"\nurl = \"https://github.com/o/r/releases/download/v1/Obsidian.AppImage\"\n\
             repo = \"obsidianmd/obsidian-releases\"\ninstalled = 2026-11-23T10:00:00Z\n"
            .to_owned();
        // a card that only says where the app comes from: not installed yet
        let c = Card::parse(&text).expect("parses");
        assert!(c.is_appimage() && !c.is_nix() && !c.is_system());
        assert!(c.out.is_empty());
        assert_eq!(Card::parse(&c.to_toml()).expect("roundtrip"), c);
        // installed: the hash, and where it is unpacked
        let installed = format!(
            "{text}sha256 = \"{sha}\"\nsize = 1234\nout = \"appimage/obsidian/{}\"\n\
             desktop = \"obsidian.desktop\"\nicon = \"obsidian.png\"\n",
            &sha[..16]
        );
        let c = Card::parse(&installed).expect("parses");
        assert_eq!(c.out, appimage_out("obsidian", &sha));
        assert_eq!(Card::parse(&c.to_toml()).expect("roundtrip"), c);
        for bad in [
            text.replace("https://", "http://"),
            text.replace("https://github.com", "file:///home"),
            text.replace("obsidianmd/obsidian-releases", "../../x"),
            text.replace("\"obsidian\"", "\"sys-obsidian\""),
            format!("{text}sha256 = \"xyz\"\n"),
            // unpacked somewhere else than its hash says, or outside eep's folder
            installed.replace(
                &format!("appimage/obsidian/{}", &sha[..16]),
                "appimage/obsidian/0000000000000000",
            ),
            installed.replace(&format!("appimage/obsidian/{}", &sha[..16]), "../../.ssh"),
            installed.replace("obsidian.desktop", "../../x.desktop"),
            format!("{text}out = \"/nix/store/yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-gimp-3\"\n"),
            format!("{text}entry = \"gimp.desktop\"\n"),
        ] {
            assert!(Card::parse(&bad).is_err(), "{bad}");
        }
        // a Nix card can't smuggle a download in
        let mut nix = card();
        nix.url = Some("https://example.org/x.AppImage".into());
        assert!(Card::parse(&nix.to_toml()).is_err());
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
