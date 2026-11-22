//! The binary cache's `<hash>.ls` file listings: the whole tree of one
//! piece, with each regular file's size and its offset inside the NAR.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Listing {
    #[serde(default)]
    pub version: Option<u32>,
    pub root: Entry,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Entry {
    Directory {
        #[serde(default)]
        entries: BTreeMap<String, Entry>,
    },
    Regular {
        #[serde(default)]
        executable: bool,
        #[serde(default)]
        size: u64,
        #[serde(rename = "narOffset", default, skip_serializing_if = "Option::is_none")]
        nar_offset: Option<u64>,
    },
    Symlink {
        target: String,
    },
}

/// What a lookup found before reaching the end of the path.
#[derive(Debug, PartialEq, Eq)]
pub enum Step<'a> {
    /// The path names this entry (not a symlink, or the last component is one).
    Found(&'a Entry),
    /// A symlink sits at `at` (relative to the root); `rest` is still to walk.
    Symlink {
        at: String,
        target: &'a str,
        rest: String,
    },
    Missing,
}

impl Entry {
    /// Walk `rel` (`a/b/c`, no leading `/`) from this entry without following
    /// any symlink; a symlink part-way stops the walk and is reported.
    pub fn walk(&self, rel: &str) -> Step<'_> {
        let mut cur = self;
        let parts: Vec<&str> = rel
            .split('/')
            .filter(|p| !p.is_empty() && *p != ".")
            .collect();
        for (i, part) in parts.iter().enumerate() {
            match cur {
                Entry::Directory { entries } => match entries.get(*part) {
                    Some(Entry::Symlink { target }) if i + 1 < parts.len() => {
                        return Step::Symlink {
                            at: parts[..=i].join("/"),
                            target,
                            rest: parts[i + 1..].join("/"),
                        };
                    }
                    Some(e) => cur = e,
                    None => return Step::Missing,
                },
                _ => return Step::Missing,
            }
        }
        Step::Found(cur)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LS: &str = r#"{"version":1,"root":{"type":"directory","entries":{
      "bin":{"type":"directory","entries":{"rg":{"type":"regular","executable":true,"size":5,"narOffset":120}}},
      "share":{"type":"symlink","target":"/nix/store/yp5wq7w4b8ym5svs1fa8b5qnvn9wa9rk-x-1/share"}}}}"#;

    #[test]
    fn walks() {
        let l: Listing = serde_json::from_str(LS).expect("parses");
        assert!(matches!(
            l.root.walk("bin/rg"),
            Step::Found(Entry::Regular {
                size: 5,
                nar_offset: Some(120),
                executable: true
            })
        ));
        assert_eq!(l.root.walk("bin/nope"), Step::Missing);
        match l.root.walk("share/icons/a.png") {
            Step::Symlink { at, rest, .. } => {
                assert_eq!((at.as_str(), rest.as_str()), ("share", "icons/a.png"))
            }
            other => panic!("expected symlink, got {other:?}"),
        }
        assert!(matches!(
            l.root.walk("share"),
            Step::Found(Entry::Symlink { .. })
        ));
    }
}
