//! nixpkgs releases on releases.nixos.org: which exist, when they were made.

use anyhow::{Context, Result, bail};
use eep_core::http::Http;
use serde::Serialize;

const RELEASES: &str = "https://releases.nixos.org";
const BUCKET: &str = "https://nix-releases.s3.amazonaws.com";
const CHANNELS: &str = "https://channels.nixos.org";

#[derive(Debug, Clone, Serialize)]
pub struct Release {
    pub release: String,
    pub released: String,
}

/// `nixos-unstable` → `nixos/unstable`, `nixos-26.05` → `nixos/26.05`.
pub fn channel_dir(channel: &str) -> Result<String> {
    match channel.strip_prefix("nixos-") {
        Some(rest) if !rest.is_empty() && !rest.contains('/') => Ok(format!("nixos/{rest}")),
        _ => bail!("unsupported channel {channel:?} (use nixos-unstable or nixos-<version>)"),
    }
}

pub fn release_url(channel: &str, release: &str, file: &str) -> Result<String> {
    if release.contains('/') || !release.starts_with("nixos-") {
        bail!("bad release name {release:?}");
    }
    Ok(format!(
        "{RELEASES}/{}/{release}/{file}",
        channel_dir(channel)?
    ))
}

/// The release the channel points at right now.
pub fn current(channel: &str) -> Result<String> {
    let meta = Http::with_redirects(0).head(&format!("{CHANNELS}/{channel}"))?;
    let loc = meta
        .location
        .context("channel URL did not redirect to a release")?;
    Ok(loc
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_owned())
}

/// `nixos-26.11pre1125864.2786a2a72088` → (`nixos-26.11pre`, 1125864).
pub fn series(release: &str) -> Option<(&str, u64)> {
    let (left, hex) = release.rsplit_once('.')?;
    if hex.is_empty() || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let digits = left.bytes().rev().take_while(u8::is_ascii_digit).count();
    let (prefix, num) = left.split_at(left.len() - digits);
    Some((prefix, num.parse().ok()?))
}

/// Git revision and publication time of a release.
pub fn meta(http: &Http, channel: &str, release: &str) -> Result<(String, String)> {
    let rev = http
        .get_bytes(&release_url(channel, release, "git-revision")?)?
        .with_context(|| format!("release {release} not found"))?;
    let rev = String::from_utf8(rev)?.trim().to_owned();
    let head = http.head(&release_url(channel, release, "nixexprs.tar.xz")?)?;
    let lm = head
        .last_modified
        .context("no Last-Modified on nixexprs.tar.xz")?;
    let t = eep_core::time::parse_http_date(&lm).with_context(|| format!("bad date {lm:?}"))?;
    Ok((rev, eep_core::time::rfc3339(t)))
}

/// The newest `count` releases of the channel's current series, oldest first.
pub fn list(http: &Http, channel: &str, count: usize) -> Result<Vec<Release>> {
    let cur = current(channel)?;
    let (prefix, _) = series(&cur).with_context(|| format!("can't parse release name {cur:?}"))?;
    let dir = channel_dir(channel)?;
    let url = format!("{BUCKET}/?delimiter=/&prefix={dir}/{prefix}");
    let xml = String::from_utf8(http.get_bytes(&url)?.context("release listing missing")?)?;
    let mut names: Vec<(u64, String)> = xml
        .split("<Prefix>")
        .skip(1)
        .filter_map(|s| s.split_once("</Prefix>"))
        .filter_map(|(p, _)| {
            p.strip_prefix(&format!("{dir}/"))?
                .strip_suffix('/')
                .map(str::to_owned)
        })
        .filter_map(|n| Some((series(&n)?.1, n)))
        .collect();
    names.sort();
    names.dedup();
    let newest = names.split_off(names.len().saturating_sub(count));
    let out = crate::util::par_map(&newest, 8, |(_, n)| -> Result<Release> {
        let (_, released) = meta(http, channel, n)?;
        Ok(Release {
            release: n.clone(),
            released,
        })
    });
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(
            series("nixos-26.11pre1125864.2786a2a72088"),
            Some(("nixos-26.11pre", 1_125_864))
        );
        assert_eq!(
            series("nixos-26.05.14753.3457b69ac4ab"),
            Some(("nixos-26.05.", 14_753))
        );
        assert_eq!(series("garbage"), None);
        assert_eq!(channel_dir("nixos-unstable").expect("ok"), "nixos/unstable");
        assert!(channel_dir("nixpkgs-unstable").is_err());
        assert!(release_url("nixos-unstable", "../x", "a").is_err());
    }
}
