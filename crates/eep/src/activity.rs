//! What eep is fetching right now, for whoever shows it (ewe Files'
//! Applications, a store): one small JSON file,
//! `$XDG_RUNTIME_DIR/eep/activity.json` (docs/CARD.md, "While an app
//! installs").
//!
//! ```json
//! { "eep_activity": 0, "updated": 1763895000, "apps": [
//!   { "id": "blender", "name": "Blender",
//!     "card": "/home/you/Applications/Blender.eep",
//!     "what": "install", "state": "downloading",
//!     "done": 1048576, "total": 8388608, "download": 2097152, "pid": 1234 } ] }
//! ```
//!
//! Only the eep that holds the lock fetches, and it writes this file while
//! it holds the lock, so reading, merging and writing back need no lock of
//! their own. Every entry carries its writer's pid: one whose eep is gone
//! (killed, crashed) is dropped by the next writer and ignored by readers.
//! A file with no apps is removed.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::nix::Progress;
use crate::paths::{Paths, write_quick};

pub const VERSION: u32 = 0;
/// Progress is written at most this often; a change of state always is.
const EVERY: Duration = Duration::from_millis(200);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// In line: eep is busy with something before it.
    Waiting,
    Downloading,
    /// Fetched: the launcher entry and the card are being written.
    Finishing,
    /// It could not be fetched; `error` says why. `eep watch` tries again.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum What {
    Install,
    Update,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub name: String,
    /// The app's card in `~/Applications` (an install from a store has
    /// none until it is done).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<PathBuf>,
    pub what: What,
    pub state: State,
    /// Bytes unpacked so far and in all (0 = not known yet). Apps fetched
    /// together share one count.
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub total: u64,
    /// Bytes the whole fetch downloads (0 = not known yet).
    #[serde(default)]
    pub download: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The eep that says so.
    pub pid: u32,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    eep_activity: u32,
    updated: i64,
    apps: Vec<Entry>,
}

/// An app to start reporting.
#[derive(Debug)]
pub struct New<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub card: Option<&'a Path>,
    pub what: What,
}

/// This eep's part of the activity file. Dropping it takes its entries
/// out (a command that ends has nothing in flight).
#[derive(Debug)]
pub struct Activity {
    file: PathBuf,
    mine: Vec<Entry>,
    written: Option<Instant>,
}

impl Activity {
    pub fn new(p: &Paths) -> Self {
        Self {
            file: p.activity_file(),
            mine: Vec::new(),
            written: None,
        }
    }

    /// Start reporting `apps` as waiting (whatever was said about them
    /// before, a failure included, no longer holds).
    pub fn begin<'a>(&mut self, apps: impl IntoIterator<Item = New<'a>>) {
        let pid = std::process::id();
        for a in apps {
            self.mine.retain(|e| e.id != a.id);
            self.mine.push(Entry {
                id: a.id.to_owned(),
                name: a.name.to_owned(),
                card: a.card.map(Path::to_path_buf),
                what: a.what,
                state: State::Waiting,
                done: 0,
                total: 0,
                download: 0,
                error: None,
                pid,
            });
        }
        self.write();
    }

    pub fn state(&mut self, ids: &[&str], state: State) {
        let mut changed = false;
        for e in self
            .mine
            .iter_mut()
            .filter(|e| ids.contains(&e.id.as_str()))
        {
            changed |= e.state != state;
            e.state = state;
        }
        if changed {
            self.write();
        }
    }

    /// How far the fetch of `ids` is (they are fetched together).
    pub fn progress(&mut self, ids: &[&str], pr: Progress) {
        for e in self
            .mine
            .iter_mut()
            .filter(|e| ids.contains(&e.id.as_str()))
        {
            e.state = State::Downloading;
            // one fetch never goes backwards on screen
            e.total = pr.total.max(e.total);
            e.done = pr.done.max(e.done).min(e.total);
            e.download = pr.download.max(e.download);
        }
        if self.written.is_none_or(|t| t.elapsed() >= EVERY) {
            self.write();
        }
    }

    pub fn fail(&mut self, id: &str, error: &str) {
        for e in self.mine.iter_mut().filter(|e| e.id == id) {
            e.state = State::Failed;
            e.error = Some(error.to_owned());
        }
        self.write();
    }

    /// The app is installed (or its card left): nothing more to say.
    pub fn done(&mut self, id: &str) {
        let before = self.mine.len();
        self.mine.retain(|e| e.id != id);
        if self.mine.len() != before {
            self.write();
        }
    }

    /// Keep only what is said about `ids` (the apps still being worked on).
    pub fn keep_only(&mut self, ids: &BTreeSet<&str>) {
        let before = self.mine.len();
        self.mine.retain(|e| ids.contains(e.id.as_str()));
        if self.mine.len() != before {
            self.write();
        }
    }

    /// Take out what an eep that is gone left behind (and anything said
    /// under this pid before: a watcher that restarted itself keeps its pid).
    pub fn sweep(&mut self) {
        if !read_all(&self.file).is_empty() {
            self.write();
        }
    }

    /// Best effort: showing progress must never fail an install.
    fn write(&mut self) {
        self.written = Some(Instant::now());
        let me = std::process::id();
        let mut apps: Vec<Entry> = read(&self.file)
            .into_iter()
            .filter(|e| e.pid != me && !self.mine.iter().any(|m| m.id == e.id))
            .collect();
        apps.extend(self.mine.iter().cloned());
        if apps.is_empty() {
            let _ = std::fs::remove_file(&self.file);
            return;
        }
        let file = File {
            eep_activity: VERSION,
            updated: eep_core::time::now_unix(),
            apps,
        };
        if let Ok(bytes) = serde_json::to_vec(&file) {
            let _ = write_quick(&self.file, &bytes);
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        if !self.mine.is_empty() {
            self.mine.clear();
            self.write();
        }
    }
}

/// The entries of live eeps in the activity file (none when there is no
/// file, or it is from a newer eep).
pub fn read(file: &Path) -> Vec<Entry> {
    read_all(file)
        .into_iter()
        .filter(|e| Path::new(&format!("/proc/{}", e.pid)).exists())
        .collect()
}

/// Every entry in the file, the ones of eeps that are gone included.
fn read_all(file: &Path) -> Vec<Entry> {
    std::fs::read(file)
        .ok()
        .and_then(|b| serde_json::from_slice::<File>(&b).ok())
        .filter(|f| f.eep_activity == VERSION)
        .map(|f| f.apps)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(name: &str) -> (PathBuf, Activity) {
        let dir = std::env::temp_dir().join(format!("eep-activity-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let file = dir.join("activity.json");
        let act = Activity {
            file: file.clone(),
            mine: Vec::new(),
            written: None,
        };
        (file, act)
    }

    fn new<'a>(id: &'a str, card: &'a Path) -> New<'a> {
        New {
            id,
            name: id,
            card: Some(card),
            what: What::Install,
        }
    }

    #[test]
    fn a_fetch_from_waiting_to_done() {
        let (file, mut act) = at("fetch");
        let card = Path::new("/home/you/Applications/Blender.eep");
        act.begin([new("blender", card)]);
        let now = read(&file);
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].state, State::Waiting);
        assert_eq!(now[0].card.as_deref(), Some(card));

        act.written = None; // not throttled
        act.progress(
            &["blender"],
            Progress {
                done: 40,
                total: 100,
                download: 30,
            },
        );
        let now = read(&file);
        assert_eq!(
            (now[0].state, now[0].done, now[0].total, now[0].download),
            (State::Downloading, 40, 100, 30)
        );
        // the total grows while Nix still asks around; nothing goes backwards
        act.written = None;
        act.progress(
            &["blender"],
            Progress {
                done: 30,
                total: 80,
                download: 30,
            },
        );
        let now = read(&file);
        assert_eq!((now[0].done, now[0].total), (40, 100));

        act.state(&["blender"], State::Finishing);
        assert_eq!(read(&file)[0].state, State::Finishing);
        act.done("blender");
        assert!(!file.exists(), "no apps: no file");
        let _ = std::fs::remove_dir_all(file.parent().expect("dir"));
    }

    #[test]
    fn failures_stay_until_the_next_try_and_leave_with_the_card() {
        let (file, mut act) = at("fail");
        let card = Path::new("/a/A.eep");
        act.begin([new("a", card), new("b", card)]);
        act.fail("a", "no connection");
        let now = read(&file);
        let a = now.iter().find(|e| e.id == "a").expect("a");
        assert_eq!(
            (a.state, a.error.as_deref()),
            (State::Failed, Some("no connection"))
        );
        // b's card left ~/Applications
        act.keep_only(&BTreeSet::from(["a"]));
        assert_eq!(read(&file).len(), 1);
        // the next try starts clean
        act.begin([new("a", card)]);
        let now = read(&file);
        assert_eq!(
            (now[0].state, now[0].error.as_deref()),
            (State::Waiting, None)
        );
        drop(act);
        assert!(!file.exists(), "a command that ended reports nothing");
        let _ = std::fs::remove_dir_all(file.parent().expect("dir"));
    }

    #[test]
    fn another_eeps_entries_are_kept_and_a_dead_ones_dropped() {
        let (file, mut act) = at("merge");
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
        let other = |id: &str, pid: u32| Entry {
            id: id.into(),
            name: id.into(),
            card: None,
            what: What::Update,
            state: State::Failed,
            done: 0,
            total: 0,
            download: 0,
            error: Some("x".into()),
            pid,
        };
        // pid 1 is always alive; a pid this high never is
        let held = File {
            eep_activity: VERSION,
            updated: 0,
            apps: vec![
                other("kept", 1),
                other("dead", 4_000_000_000),
                other("mine", 1),
            ],
        };
        std::fs::write(&file, serde_json::to_vec(&held).expect("json")).expect("write");
        assert_eq!(read(&file).len(), 2, "the dead eep's entry is ignored");
        act.begin([new("mine", Path::new("/a/M.eep"))]);
        let now = read(&file);
        let ids: Vec<&str> = now.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["kept", "mine"]);
        assert_eq!(now[1].state, State::Waiting, "what this eep says wins");
        drop(act);
        assert_eq!(read(&file).len(), 1, "the other eep's entry stays");
        let _ = std::fs::remove_dir_all(file.parent().expect("dir"));
    }

    #[test]
    fn a_newer_format_reads_as_nothing() {
        let (file, _act) = at("newer");
        std::fs::create_dir_all(file.parent().expect("dir")).expect("mkdir");
        std::fs::write(&file, r#"{"eep_activity":9,"updated":0,"apps":[]}"#).expect("write");
        assert!(read(&file).is_empty());
        std::fs::write(&file, "not json").expect("write");
        assert!(read(&file).is_empty());
        let _ = std::fs::remove_dir_all(file.parent().expect("dir"));
    }
}
