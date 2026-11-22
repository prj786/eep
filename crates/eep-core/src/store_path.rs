//! Store paths: `/nix/store/<32-char hash>-<name>`.

use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::nixbase32;

pub const STORE_DIR: &str = "/nix/store";
const HASH_LEN: usize = 32;

/// A store path, kept as its base name (`<hash>-<name>`). Ordering and
/// equality are by base name, so sets of them sort deterministically.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct StorePath(String);

impl StorePath {
    /// From `<hash>-<name>`.
    pub fn from_base_name(s: &str) -> Result<Self> {
        if s.len() <= HASH_LEN + 1 || s.as_bytes()[HASH_LEN] != b'-' {
            bail!("not a store path base name: {s:?}");
        }
        let (hash, name) = (&s[..HASH_LEN], &s[HASH_LEN + 1..]);
        if !hash.bytes().all(nixbase32::is_valid_char) {
            bail!("bad hash in store path {s:?}");
        }
        if name.starts_with('.') || !name.bytes().all(valid_name_char) {
            bail!("bad name in store path {s:?}");
        }
        Ok(Self(s.to_owned()))
    }

    /// From `/nix/store/<hash>-<name>[/anything]`; the part after the base
    /// name is dropped (see [`StorePath::split_absolute`] to keep it).
    pub fn from_absolute(s: &str) -> Result<Self> {
        Ok(Self::split_absolute(s)?.0)
    }

    /// `/nix/store/<base>/a/b` → (`<base>`, `"a/b"`); the rest may be empty.
    pub fn split_absolute(s: &str) -> Result<(Self, String)> {
        let Some(rest) = s.strip_prefix(STORE_DIR).and_then(|r| r.strip_prefix('/')) else {
            bail!("not under {STORE_DIR}: {s:?}");
        };
        let (base, sub) = rest.split_once('/').unwrap_or((rest, ""));
        Ok((
            Self::from_base_name(base)?,
            sub.trim_end_matches('/').to_owned(),
        ))
    }

    pub fn base_name(&self) -> &str {
        &self.0
    }

    pub fn absolute(&self) -> String {
        format!("{STORE_DIR}/{}", self.0)
    }

    pub fn hash(&self) -> &str {
        &self.0[..HASH_LEN]
    }

    /// `firefox-unwrapped-160.0.1`
    pub fn name(&self) -> &str {
        &self.0[HASH_LEN + 1..]
    }

    /// The name without its version: everything before the first `-` that
    /// is followed by a digit (`firefox-unwrapped-160.0.1` → `firefox-unwrapped`).
    /// This is how Nix's own `builtins.parseDrvName` splits names.
    pub fn pname(&self) -> &str {
        split_name(self.name()).0
    }

    /// The version part of the name (may be empty, may carry an output
    /// suffix such as `-lib`).
    pub fn version(&self) -> &str {
        split_name(self.name()).1
    }
}

/// `parseDrvName`: split at the first `-` followed by a non-letter.
pub fn split_name(name: &str) -> (&str, &str) {
    let b = name.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'-' && b.get(i + 1).is_some_and(|c| !c.is_ascii_alphabetic()) {
            return (&name[..i], &name[i + 1..]);
        }
    }
    (name, "")
}

fn valid_name_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || b"+-._?=".contains(&c)
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for StorePath {
    type Error = anyhow::Error;
    fn try_from(s: String) -> Result<Self> {
        Self::from_base_name(&s)
    }
}

impl From<StorePath> for String {
    fn from(p: StorePath) -> Self {
        p.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FF: &str = "yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-firefox-unwrapped-160.0.1";

    #[test]
    fn parse() {
        let p = StorePath::from_base_name(FF).expect("valid");
        assert_eq!(p.hash(), "yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk");
        assert_eq!(p.name(), "firefox-unwrapped-160.0.1");
        assert_eq!(p.pname(), "firefox-unwrapped");
        assert_eq!(p.version(), "160.0.1");
        assert_eq!(p.absolute(), format!("/nix/store/{FF}"));
    }

    #[test]
    fn absolute_with_subpath() {
        let (p, sub) =
            StorePath::split_absolute(&format!("/nix/store/{FF}/lib/x.png")).expect("ok");
        assert_eq!(p.base_name(), FF);
        assert_eq!(sub, "lib/x.png");
        assert!(StorePath::from_absolute("/usr/bin/env").is_err());
    }

    #[test]
    fn names() {
        assert_eq!(split_name("gtk+3-3.24.52"), ("gtk+3", "3.24.52"));
        assert_eq!(split_name("krb5-1.22.2-lib"), ("krb5", "1.22.2-lib"));
        assert_eq!(split_name("xgcc-15.3.0-libgcc"), ("xgcc", "15.3.0-libgcc"));
        assert_eq!(split_name("source"), ("source", ""));
        assert_eq!(split_name("hello-world"), ("hello-world", ""));
    }

    #[test]
    fn rejects() {
        assert!(StorePath::from_base_name("short-name").is_err());
        assert!(StorePath::from_base_name("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x").is_err());
        assert!(StorePath::from_base_name("yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-a/b").is_err());
        assert!(StorePath::from_base_name("yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-.hidden").is_err());
    }
}
