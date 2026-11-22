//! `.narinfo` files: `Key: value` lines describing one piece in a binary
//! cache. The signature (`Sig`) covers only StorePath, NarHash, NarSize and
//! References, so URL / Compression / FileHash / FileSize may be rewritten
//! (that is how eep serves patched pieces) and Nix still verifies them.

use anyhow::{Context, Result, bail};

use crate::{StorePath, nixbase32};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NarInfo {
    /// Fields in file order; keys may repeat (several `Sig` lines).
    fields: Vec<(String, String)>,
}

impl NarInfo {
    pub fn parse(text: &str) -> Result<Self> {
        let mut fields = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let (k, v) = line
                .split_once(": ")
                .with_context(|| format!("bad narinfo line {line:?}"))?;
            fields.push((k.to_owned(), v.to_owned()));
        }
        let info = Self { fields };
        info.store_path()?;
        info.nar_size()?;
        info.nar_hash()?;
        Ok(info)
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// Replace the first `key` (dropping repeats), or append it.
    pub fn set(&mut self, key: &str, value: &str) {
        let mut seen = false;
        self.fields.retain_mut(|(k, v)| {
            if k != key {
                return true;
            }
            if seen {
                return false;
            }
            seen = true;
            value.clone_into(v);
            true
        });
        if !seen {
            self.fields.push((key.to_owned(), value.to_owned()));
        }
    }

    pub fn remove(&mut self, key: &str) {
        self.fields.retain(|(k, _)| k != key);
    }

    pub fn store_path(&self) -> Result<StorePath> {
        StorePath::from_absolute(self.get("StorePath").context("narinfo without StorePath")?)
    }

    pub fn url(&self) -> Option<&str> {
        self.get("URL")
    }

    /// `none` when absent, as Nix does.
    pub fn compression(&self) -> &str {
        self.get("Compression").unwrap_or("none")
    }

    pub fn file_size(&self) -> Option<u64> {
        self.get("FileSize").and_then(|v| v.parse().ok())
    }

    pub fn nar_size(&self) -> Result<u64> {
        self.get("NarSize")
            .context("narinfo without NarSize")?
            .parse()
            .context("bad NarSize")
    }

    /// The NAR's sha256, decoded from `sha256:<nix32>`.
    pub fn nar_hash(&self) -> Result<[u8; 32]> {
        let v = self.get("NarHash").context("narinfo without NarHash")?;
        let Some(b32) = v.strip_prefix("sha256:") else {
            bail!("unsupported NarHash {v:?}")
        };
        let bytes = nixbase32::decode(b32).context("bad NarHash encoding")?;
        bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("NarHash is not 32 bytes"))
    }

    pub fn references(&self) -> Result<Vec<StorePath>> {
        self.get("References")
            .unwrap_or("")
            .split_whitespace()
            .map(StorePath::from_base_name)
            .collect()
    }

    /// The text of this narinfo, one `Key: value` per line.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        for (k, v) in &self.fields {
            s.push_str(k);
            s.push_str(": ");
            s.push_str(v);
            s.push('\n');
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    pub const GLIBC: &str = "StorePath: /nix/store/pagq7gjgxqbi09g0091x2cxwq2kwxyx8-glibc-2.42-103
URL: nar/0d4zl9l50iprs7yv9q80ggsc5saazxdz9sq6vrrp0j8zdc8sxsw1.nar.zst
Compression: zstd
FileHash: sha256:06ja93ypsi4bfhma9n5g2kjlvzcyjmmig5cwk2kvjr1f4awwnvxz
FileSize: 10249834
NarHash: sha256:0d4zl9l50iprs7yv9q80ggsc5saazxdz9sq6vrrp0j8zdc8sxsw1
NarSize: 35073152
References: 3y5kz09v351q95k45l2cn692skw2x910-xgcc-15.3.0-libgcc 9br0r2ha5l05vgz0vprx5x6qk95wb1xi-libidn2-2.3.8 pagq7gjgxqbi09g0091x2cxwq2kwxyx8-glibc-2.42-103
Deriver: imgp0s8wsiffdcm05y0pq88d1j5m80ja-glibc-2.42-103.drv
Sig: cache.nixos.org-1:qFaWgP40xhtSj+qTz4bIfRbRmHii90wKeVrmg7vDVEGAJYzM79PNpU2fIYKDvSCKxlfOgdWwDxR9KjBcAPmhCw==
";

    #[test]
    fn parse_and_read() {
        let n = NarInfo::parse(GLIBC).expect("parses");
        assert_eq!(n.store_path().expect("sp").name(), "glibc-2.42-103");
        assert_eq!(n.compression(), "zstd");
        assert_eq!(n.file_size(), Some(10_249_834));
        assert_eq!(n.nar_size().expect("size"), 35_073_152);
        assert_eq!(n.nar_hash().expect("hash")[0], 0x81);
        assert_eq!(n.references().expect("refs").len(), 3);
        assert_eq!(n.to_text(), GLIBC);
    }

    #[test]
    fn rewrite_for_local_cache() {
        let mut n = NarInfo::parse(GLIBC).expect("parses");
        n.set("URL", "nar/x.nar");
        n.set("Compression", "none");
        n.remove("FileHash");
        n.remove("FileSize");
        let t = n.to_text();
        assert!(t.contains("URL: nar/x.nar\nCompression: none\nNarHash:"));
        assert!(
            t.contains("Sig: cache.nixos.org-1:qFaW"),
            "signature untouched"
        );
        assert!(!t.contains("FileSize"));
    }

    #[test]
    fn set_collapses_repeats() {
        let mut n = NarInfo::parse(&format!("{GLIBC}Sig: other-1:abc\n")).expect("parses");
        n.set("Sig", "k:v");
        assert_eq!(n.to_text().matches("Sig: ").count(), 1);
    }
}
