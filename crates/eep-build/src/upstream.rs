//! cache.nixos.org, read the way the builder needs it: narinfos and file
//! listings (cached on disk forever: a store path's content never changes),
//! closures, and single files pulled out of a NAR by offset.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use eep_core::http::Http;
use eep_core::listing::{Entry, Listing, Step};
use eep_core::narinfo::NarInfo;
use eep_core::{StorePath, UPSTREAM_CACHE};

use crate::util::{par_map, write_atomic};

const THREADS: usize = 16;

#[derive(Debug)]
pub struct Upstream {
    pub http: Http,
    cache: PathBuf,
}

/// Where a path inside a piece really is, after following symlinks.
#[derive(Clone, Debug)]
pub struct Located {
    pub piece: StorePath,
    pub rel: String,
    pub entry: Entry,
}

impl Upstream {
    pub fn new(http: Http, cache: PathBuf) -> Self {
        Self { http, cache }
    }

    /// Downloaded NARs, as served (compressed). Large: one release of the
    /// catalog is ~2 GB. Safe to delete at any time.
    pub fn nar_cache(&self) -> PathBuf {
        self.cache.join("nar")
    }

    /// The narinfo of `p`; `None` when the cache doesn't have it.
    pub fn narinfo(&self, p: &StorePath) -> Result<Option<NarInfo>> {
        let file = self
            .cache
            .join("narinfo")
            .join(format!("{}.narinfo", p.hash()));
        if let Ok(text) = std::fs::read_to_string(&file) {
            return NarInfo::parse(&text).map(Some);
        }
        let url = format!("{UPSTREAM_CACHE}/{}.narinfo", p.hash());
        let Some(body) = self.http.get_bytes(&url)? else {
            return Ok(None);
        };
        let text = String::from_utf8(body).context("narinfo is not UTF-8")?;
        let info = NarInfo::parse(&text).with_context(|| format!("parsing {url}"))?;
        if info.store_path()? != *p {
            bail!("{url} describes {}, not {p}", info.store_path()?);
        }
        write_atomic(&file, text.as_bytes())?;
        Ok(Some(info))
    }

    /// The file listing of `p` (`<hash>.ls`).
    pub fn listing(&self, p: &StorePath) -> Result<Option<Listing>> {
        let file = self.cache.join("ls").join(format!("{}.ls.json", p.hash()));
        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(_) => {
                let url = format!("{UPSTREAM_CACHE}/{}.ls", p.hash());
                let Some(b) = self.http.get_bytes(&url)? else {
                    return Ok(None);
                };
                write_atomic(&file, &b)?;
                b
            }
        };
        Ok(Some(
            serde_json::from_slice(&bytes).with_context(|| format!("listing of {p}"))?,
        ))
    }

    /// Every piece reachable from `roots`, with its narinfo. Pieces the
    /// cache doesn't have are returned in the second set.
    pub fn closure(
        &self,
        roots: &[StorePath],
    ) -> Result<(BTreeMap<StorePath, NarInfo>, BTreeSet<StorePath>)> {
        let mut found = BTreeMap::new();
        let mut missing = BTreeSet::new();
        let mut frontier: Vec<StorePath> = roots
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        while !frontier.is_empty() {
            let infos = par_map(&frontier, THREADS, |p| self.narinfo(p));
            let mut next = BTreeSet::new();
            for (p, info) in frontier.iter().zip(infos) {
                match info.with_context(|| format!("narinfo of {p}"))? {
                    None => {
                        missing.insert(p.clone());
                    }
                    Some(info) => {
                        for r in info.references()? {
                            if !found.contains_key(&r) && !missing.contains(&r) && r != *p {
                                next.insert(r);
                            }
                        }
                        found.insert(p.clone(), info);
                    }
                }
            }
            next.retain(|p| !found.contains_key(p));
            frontier = next.into_iter().collect();
        }
        Ok((found, missing))
    }

    /// Resolve `rel` inside `piece`, following symlinks (across pieces too).
    pub fn resolve(&self, piece: &StorePath, rel: &str) -> Result<Option<Located>> {
        let (mut piece, mut rel) = (piece.clone(), normalize(rel).unwrap_or_default());
        for _ in 0..40 {
            let Some(listing) = self.listing(&piece)? else {
                return Ok(None);
            };
            let (at, target, rest) = match listing.root.walk(&rel) {
                Step::Missing => return Ok(None),
                Step::Found(Entry::Symlink { target }) => {
                    (rel.clone(), target.clone(), String::new())
                }
                Step::Found(entry) => {
                    return Ok(Some(Located {
                        piece,
                        rel,
                        entry: entry.clone(),
                    }));
                }
                Step::Symlink { at, target, rest } => (at, target.to_owned(), rest),
            };
            let Some((p2, base)) = follow(&piece, &at, &target) else {
                return Ok(None);
            };
            piece = p2;
            rel = join(&base, &rest);
        }
        bail!("symlink loop under {piece}");
    }

    /// Names in a directory (after resolving it), sorted.
    pub fn list_dir(&self, piece: &StorePath, rel: &str) -> Result<Vec<String>> {
        Ok(match self.resolve(piece, rel)? {
            Some(Located {
                entry: Entry::Directory { entries },
                ..
            }) => entries.into_keys().collect(),
            _ => Vec::new(),
        })
    }

    /// The bytes of one regular file. Downloads the piece's NAR only up to
    /// the end of that file. Cached by (piece name without hash, size, path):
    /// rebuilds of the same version don't download again.
    pub fn read_file(&self, loc: &Located) -> Result<Vec<u8>> {
        let Entry::Regular {
            size,
            nar_offset: Some(offset),
            ..
        } = loc.entry
        else {
            bail!(
                "{}/{} is not a regular file with a known offset",
                loc.piece,
                loc.rel
            );
        };
        let key = self
            .cache
            .join("files")
            .join(loc.piece.name())
            .join(size.to_string())
            .join(&loc.rel);
        if let Ok(b) = std::fs::read(&key)
            && b.len() as u64 == size
        {
            return Ok(b);
        }
        let info = self
            .narinfo(&loc.piece)?
            .with_context(|| format!("{} left the cache", loc.piece))?;
        let url = format!(
            "{UPSTREAM_CACHE}/{}",
            info.url().context("narinfo without URL")?
        );
        let (body, _) = self
            .http
            .get_reader(&url)?
            .with_context(|| format!("{url} is missing"))?;
        let mut nar = decompress(body, info.compression())?;
        std::io::copy(&mut (&mut nar).take(offset), &mut std::io::sink())?;
        let mut buf = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        (&mut nar).take(size).read_to_end(&mut buf)?;
        if buf.len() as u64 != size {
            bail!("{url}: NAR ended early");
        }
        write_atomic(&key, &buf)?;
        Ok(buf)
    }
}

/// Decompress a NAR stream per the narinfo's `Compression`.
fn decompress(body: Box<dyn Read + Send>, compression: &str) -> Result<Box<dyn Read + Send>> {
    Ok(match compression {
        "none" => body,
        "zstd" => Box::new(zstd::Decoder::new(body)?),
        "xz" => Box::new(Piped::new("xz", body)?),
        other => bail!("unsupported NAR compression {other:?}"),
    })
}

/// A decompressor run as a child process, fed from a thread.
struct Piped {
    child: std::process::Child,
    out: std::process::ChildStdout,
}

impl Piped {
    fn new(tool: &str, mut input: Box<dyn Read + Send>) -> Result<Self> {
        let mut child = Command::new(tool)
            .arg("-dc")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {tool}"))?;
        let mut stdin = child.stdin.take().context("no stdin")?;
        let out = child.stdout.take().context("no stdout")?;
        std::thread::spawn(move || {
            // Ends with a broken pipe when the reader stops early: fine.
            let _ = std::io::copy(&mut input, &mut stdin).and_then(|_| stdin.flush());
        });
        Ok(Self { child, out })
    }
}

impl Read for Piped {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.out.read(buf)
    }
}

impl Drop for Piped {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Where a symlink at `at` (inside `piece`) pointing to `target` leads.
fn follow(piece: &StorePath, at: &str, target: &str) -> Option<(StorePath, String)> {
    if target.starts_with('/') {
        return StorePath::split_absolute(target).ok();
    }
    let dir = at.rsplit_once('/').map_or("", |(d, _)| d);
    Some((piece.clone(), normalize(&join(dir, target))?))
}

fn join(a: &str, b: &str) -> String {
    match (a.is_empty(), b.is_empty()) {
        (true, _) => b.to_owned(),
        (_, true) => a.to_owned(),
        _ => format!("{a}/{b}"),
    }
}

/// Collapse `.` and `..`; `None` if it climbs out of the piece.
fn normalize(p: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in p.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => parts.push(c),
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(normalize("a/./b/../c").as_deref(), Some("a/c"));
        assert_eq!(normalize("../x"), None);
        let p = StorePath::from_base_name("yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-x-1").expect("sp");
        assert_eq!(
            follow(&p, "bin/.f-wrapped", "../lib/f")
                .map(|x| x.1)
                .as_deref(),
            Some("lib/f")
        );
        let (q, sub) = follow(
            &p,
            "share/icons/a.png",
            "/nix/store/lr6rlibz9mi24mag008wxng40ii0q2kb-y-2/lib/a.png",
        )
        .expect("absolute");
        assert_eq!((q.name(), sub.as_str()), ("y-2", "lib/a.png"));
        assert!(follow(&p, "a", "/usr/share/x").is_none());
    }
}
