//! The Nix engine, driven through its CLI. eep never evaluates: it only
//! asks the daemon to realise exact store paths (substituting from the
//! configured caches, checking every signature and hash itself), to keep
//! roots, and to collect garbage.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

fn nix_store() -> Command {
    let mut c = Command::new("nix-store");
    // Keep Nix's own progress off our stdout (stdout is JSON only).
    c.stdin(Stdio::null());
    c
}

/// Which of `paths` (absolute) are NOT valid in the local store.
pub fn invalid(paths: &[String]) -> Result<std::collections::BTreeSet<String>> {
    let mut missing = std::collections::BTreeSet::new();
    for chunk in paths.chunks(500) {
        let out = nix_store()
            .arg("--check-validity")
            .arg("--print-invalid")
            .args(chunk)
            .stderr(Stdio::piped())
            .output()
            .context("running nix-store (is nix installed?)")?;
        if !out.status.success() {
            bail!(
                "nix-store --check-validity: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        missing.extend(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_owned),
        );
    }
    Ok(missing)
}

/// Realise `out` and keep it with an indirect GC root at `root`.
/// Nix's progress goes to our stderr.
pub fn realise(out: &str, root: &Path) -> Result<()> {
    if let Some(dir) = root.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let st = nix_store()
        .arg("--realise")
        .arg(out)
        .arg("--add-root")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("running nix-store --realise")?;
    if !st.success() {
        bail!("nix-store --realise {out} failed ({st})");
    }
    Ok(())
}

/// How far a fetch is. `done` of `total` are bytes unpacked into the store
/// (what Nix counts as it copies), `download` the bytes the whole fetch
/// takes off the network; 0 = not known yet.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Progress {
    pub done: u64,
    pub total: u64,
    pub download: u64,
}

/// How often the observer of a watched fetch is called at most (and about
/// how often at least, so a cancel is noticed while a big piece unpacks).
const OBSERVE: Duration = Duration::from_millis(100);

/// Realise paths in one call (Nix downloads across all of them at once)
/// while somebody watches: `on` is told how far the fetch is, several
/// times a second, and stops it by answering `false` (Nix drops the piece
/// it was fetching; the finished ones stay in the store). A failure reads
/// as Nix's own last error, in short words.
///
/// Roots: `root`, `root-2`, `root-3`… (Nix's naming), all in `root`'s
/// directory, which the caller removes once each path has its own root.
pub fn realise_watched(
    outs: &[&str],
    root: &Path,
    on: &mut dyn FnMut(Progress) -> bool,
) -> Result<()> {
    if let Some(dir) = root.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut child = nix_store()
        .arg("--realise")
        .args(outs)
        .arg("--add-root")
        .arg(root)
        // Nix's activity as JSON lines (what its own progress bar reads)
        .args(["--log-format", "internal-json"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("running nix-store --realise (is nix installed?)")?;
    let log = child.stderr.take().context("nix-store's log")?;
    let (tx, rx) = mpsc::channel::<Progress>();
    let reader = std::thread::spawn(move || read_log(log, &tx));
    let mut now = Progress::default();
    let mut told: Option<Instant> = None;
    let mut cancelled = false;
    loop {
        match rx.recv_timeout(OBSERVE) {
            Ok(p) => now = p,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            // the log closed: nix-store is done
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if told.is_none_or(|t| t.elapsed() >= OBSERVE) {
            told = Some(Instant::now());
            if !on(now) {
                cancelled = true;
                let _ = child.kill();
                break;
            }
        }
    }
    let st = child.wait().context("waiting for nix-store")?;
    if cancelled {
        // the log's reader ends when the pipe closes; a helper of Nix's that
        // kept it open must not hold the cancel (and eep's lock) up
        drop(reader);
        bail!("stopped");
    }
    let said = reader.join().ok().flatten();
    if !st.success() {
        match said {
            Some(e) => bail!("{e}"),
            None => bail!("nix-store --realise of {} paths failed ({st})", outs.len()),
        }
    }
    on(now);
    Ok(())
}

/// Nix's activity types and result types (`logging.hh`), the ones read here.
const ACT_COPY_PATH: u64 = 100;
const ACT_FILE_TRANSFER: u64 = 101;
const RES_PROGRESS: u64 = 105;
const RES_SET_EXPECTED: u64 = 106;
/// Nix's `lvlInfo`: what it prints without `-v`.
const LVL_INFO: u64 = 3;

/// What Nix's log has said so far about a fetch.
#[derive(Debug, Default)]
struct Tracker {
    /// Pieces being copied now: activity id → (bytes done, its size).
    copying: HashMap<u64, (u64, u64)>,
    /// Bytes of the pieces already copied.
    copied: u64,
    expected: u64,
    download: u64,
    /// Nix's last error, cleaned up.
    error: Option<String>,
}

impl Tracker {
    fn progress(&self) -> Progress {
        let done = self.copied + self.copying.values().map(|(d, _)| d).sum::<u64>();
        Progress {
            done,
            total: self.expected.max(done),
            download: self.download,
        }
    }

    /// One log line → the text Nix would have printed for it (to pass on),
    /// and whether the progress changed.
    fn line(&mut self, line: &str) -> (Option<String>, bool) {
        let Some(v) = line
            .strip_prefix("@nix ")
            .and_then(|j| serde_json::from_str::<serde_json::Value>(j).ok())
        else {
            // not the JSON log (an older Nix, a wrapper's own words)
            return ((!line.trim().is_empty()).then(|| line.to_owned()), false);
        };
        let id = v["id"].as_u64().unwrap_or(0);
        let kind = v["type"].as_u64().unwrap_or(0);
        let field = |i: usize| v["fields"][i].as_u64().unwrap_or(0);
        let level = v["level"].as_u64().unwrap_or(u64::MAX);
        match v["action"].as_str().unwrap_or("") {
            "msg" => {
                let text = plain(v["msg"].as_str().unwrap_or(""));
                if level == 0 {
                    self.error = Some(short_error(&text));
                }
                (
                    (level <= LVL_INFO && !text.is_empty()).then_some(text),
                    false,
                )
            }
            "start" => {
                if kind == ACT_COPY_PATH {
                    self.copying.insert(id, (0, 0));
                }
                let text = v["text"].as_str().unwrap_or("");
                (
                    (level <= LVL_INFO && !text.is_empty()).then(|| format!("{text}...")),
                    false,
                )
            }
            "stop" => match self.copying.remove(&id) {
                Some((done, size)) => {
                    self.copied += size.max(done);
                    (None, true)
                }
                None => (None, false),
            },
            "result" if kind == RES_PROGRESS => match self.copying.get_mut(&id) {
                Some(c) => {
                    *c = (field(0), field(1));
                    (None, true)
                }
                None => (None, false),
            },
            "result" if kind == RES_SET_EXPECTED => {
                match field(0) {
                    ACT_COPY_PATH => self.expected = field(1),
                    ACT_FILE_TRANSFER => self.download = field(1),
                    _ => return (None, false),
                }
                (None, true)
            }
            _ => (None, false),
        }
    }
}

/// Read nix-store's log to its end: progress goes to `tx` (as it changes,
/// a few times a second at most), Nix's words to our stderr as before.
/// → Nix's last error.
fn read_log(log: impl std::io::Read, tx: &mpsc::Sender<Progress>) -> Option<String> {
    use std::io::BufRead;
    let mut t = Tracker::default();
    let mut sent: Option<(Instant, Progress)> = None;
    let mut log = std::io::BufReader::new(log);
    let mut raw = Vec::new();
    loop {
        // bytes, not `lines()`: one line that is not UTF-8 (a file name in an
        // error) must not end the reading — Nix would get a closed pipe
        raw.clear();
        match log.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let line = String::from_utf8_lossy(&raw);
        let (say, moved) = t.line(line.trim_end_matches(['\n', '\r']));
        if let Some(text) = say {
            eprintln!("{text}");
        }
        if moved {
            let p = t.progress();
            let due = sent.is_none_or(|(at, was)| was != p && at.elapsed() >= OBSERVE / 2);
            if due {
                sent = Some((Instant::now(), p));
                let _ = tx.send(p);
            }
        }
    }
    let _ = tx.send(t.progress());
    t.error
}

/// Without terminal colours.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // an escape sequence ends with a letter
            for e in chars.by_ref() {
                if e.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out.trim_end().to_owned()
}

/// Nix's error for people: no connection said as that, anything else as
/// Nix said it, on one line, without the `error:` and not a page long.
fn short_error(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let t = line.strip_prefix("error:").unwrap_or(&line).trim();
    let offline = [
        "Could not resolve host",
        "Couldn't resolve host",
        "Couldn't connect to server",
        "Could not connect to server",
        "Failed to connect",
        "Network is unreachable",
        "Timeout was reached",
        "Connection timed out",
    ];
    if offline.iter().any(|s| t.contains(s)) {
        return "No connection to the app server. Check the internet and try again.".into();
    }
    if t.contains("No space left on device") {
        return "The disk is full.".into();
    }
    let mut s: String = t.chars().take(240).collect();
    if s.len() < t.len() {
        s.push('…');
    }
    s
}

/// Flush the store to disk: one `syncfs` for the whole filesystem. Nix
/// registers a path as valid before its files are on disk (unless
/// `fsync-store-paths`, which fsyncs every file and is slow), so after a
/// power cut a "valid" path can hold empty files. See `journal`.
pub fn sync_store() {
    let _ = Command::new("sync")
        .args(["-f", "/nix/store"])
        .stdin(Stdio::null())
        .status();
}

/// Valid store paths registered at or after `since` (unix seconds).
pub fn registered_since(since: i64) -> Result<Vec<String>> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "path-info",
            "--all",
            "--json",
        ])
        .args(["--json-format", "2"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("running nix path-info --all")?;
    if !out.status.success() {
        bail!("nix path-info --all failed");
    }
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).context("nix path-info output")?;
    let Some(info) = v["info"].as_object() else {
        bail!("unexpected nix path-info output")
    };
    Ok(info
        .iter()
        .filter(|(_, i)| i["registrationTime"].as_i64().is_some_and(|t| t >= since))
        .map(|(base, _)| format!("/nix/store/{base}"))
        .collect())
}

/// Whether the path's files still match its registered hash.
pub fn verify_path(path: &str) -> Result<bool> {
    let st = nix_store()
        .arg("--verify-path")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("running nix-store --verify-path")?;
    Ok(st.success())
}

/// The paths plus everything that refers to them.
pub fn referrers_closure(paths: &[String]) -> Result<Vec<String>> {
    let out = nix_store()
        .args(["--query", "--referrers-closure"])
        .args(paths)
        .stderr(Stdio::null())
        .output()
        .context("running nix-store --query")?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// Delete dead paths (fails for anything still rooted).
pub fn delete(paths: &[String]) -> Result<()> {
    let out = nix_store()
        .arg("--delete")
        .args(paths)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()?;
    if !out.status.success() {
        bail!(
            "nix-store --delete: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

#[derive(Debug, Default)]
pub struct Gc {
    pub freed_bytes: u64,
    pub paths: u64,
}

/// `nix-store --gc` (or `--print-dead` for a dry run).
pub fn gc(dry_run: bool) -> Result<Gc> {
    let out = nix_store()
        .arg("--gc")
        .args(dry_run.then_some("--print-dead"))
        .stderr(Stdio::piped())
        .output()
        .context("running nix-store --gc")?;
    if !out.status.success() {
        bail!(
            "nix-store --gc: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let mut gc = Gc::default();
    if dry_run {
        let dead: Vec<&str> = stdout
            .lines()
            .filter(|l| l.starts_with("/nix/store/"))
            .collect();
        gc.paths = dead.len() as u64;
        gc.freed_bytes = sizes(&dead)?;
    } else {
        // "1234 store paths deleted, 567.89 MiB freed" (stdout in Nix 2.36;
        // look in both streams, older versions differ).
        let summary = stdout.lines().chain(stderr.lines()).find_map(parse_summary);
        (gc.paths, gc.freed_bytes) = summary.unwrap_or((0, 0));
    }
    Ok(gc)
}

/// Sum of NAR sizes of valid paths.
fn sizes(paths: &[&str]) -> Result<u64> {
    let mut total = 0;
    for chunk in paths.chunks(500) {
        let out = nix_store()
            .arg("--query")
            .arg("--size")
            .args(chunk)
            .stderr(Stdio::null())
            .output()?;
        total += String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| l.trim().parse::<u64>().ok())
            .sum::<u64>();
    }
    Ok(total)
}

/// `N store paths deleted, X.Y MiB freed` → (N, bytes).
fn parse_summary(line: &str) -> Option<(u64, u64)> {
    let (head, _) = line.split_once(" store paths deleted, ")?;
    Some((head.trim().parse().ok()?, parse_freed(line)?))
}

fn parse_freed(line: &str) -> Option<u64> {
    let (_, tail) = line.split_once(", ")?;
    let mut it = tail.split_whitespace();
    let n: f64 = it.next()?.parse().ok()?;
    let mult = match it.next()? {
        "B" | "bytes" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0_f64.powi(4),
        _ => return None,
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Some((n * mult) as u64)
}

/// Whether the store answers: `nix store info --json` (the plain form
/// prints to stderr), e.g. `{"url":"daemon","version":"2.36.0","trusted":false}`.
pub fn ping() -> Result<serde_json::Value> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "store",
            "info",
            "--json",
        ])
        .stdin(Stdio::null())
        .output()
        .context("running nix store info")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    serde_json::from_slice(&out.stdout).context("nix store info output")
}

/// `nix config show <key>`.
pub fn config(key: &str) -> Option<String> {
    let out = Command::new("nix")
        .args([
            "--extra-experimental-features",
            "nix-command",
            "config",
            "show",
            key,
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::{Progress, Tracker, plain, read_log, short_error};

    /// What Nix 2.36 really logs for a two-piece fetch through the daemon
    /// (ids shortened, the many progress lines thinned out).
    const LOG: &str = r#"@nix {"action":"start","id":1,"level":6,"parent":0,"text":"querying info about missing paths","type":0}
@nix {"action":"stop","id":1}
@nix {"action":"msg","level":3,"msg":"these 2 paths will be fetched (2.7 MiB download, 8.7 MiB unpacked):"}
@nix {"action":"msg","level":3,"msg":"  /nix/store/m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49"}
@nix {"action":"start","id":51,"level":0,"parent":0,"text":"","type":102}
@nix {"action":"start","id":53,"level":0,"parent":0,"text":"","type":103}
@nix {"action":"result","fields":[101,2127105],"id":51,"type":106}
@nix {"action":"result","fields":[100,7040576],"id":51,"type":106}
@nix {"action":"result","fields":[101,2825502],"id":51,"type":106}
@nix {"action":"result","fields":[100,9158664],"id":51,"type":106}
@nix {"action":"start","fields":["/nix/store/m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49","https://cache.nixos.org"],"id":56,"level":0,"parent":0,"text":"","type":108}
@nix {"action":"start","fields":["/nix/store/m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49","https://cache.nixos.org","local"],"id":57,"level":3,"parent":56,"text":"copying path '/nix/store/m9y00qgmfbzvlffgbx0hi5hc38dps46q-pcre2-10.49' from 'https://cache.nixos.org'","type":100}
@nix {"action":"start","fields":["https://cache.nixos.org/nar/0hb1.nar.zst"],"id":58,"level":4,"parent":57,"text":"downloading 'https://cache.nixos.org/nar/0hb1.nar.zst'","type":101}
@nix {"action":"result","fields":[0,0,0,0],"id":52,"type":105}
@nix {"action":"result","fields":[1564672,2118088,0,0],"id":57,"type":105}
@nix {"action":"stop","id":58}
@nix {"action":"stop","id":57}
@nix {"action":"stop","id":56}
@nix {"action":"start","fields":["/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0","https://cache.nixos.org","local"],"id":60,"level":3,"parent":59,"text":"copying path '/nix/store/whbrk657qmih065xal60m4khwf6cb9pj-ripgrep-15.2.0' from 'https://cache.nixos.org'","type":100}
@nix {"action":"result","fields":[2662400,7040576,0,0],"id":60,"type":105}
"#;

    #[test]
    fn progress_from_nixs_log() {
        let mut t = Tracker::default();
        let mut said = Vec::new();
        for l in LOG.lines() {
            if let (Some(text), _) = t.line(l) {
                said.push(text);
            }
        }
        // pcre2 whole (its size, not the last count seen) + ripgrep so far
        assert_eq!(
            t.progress(),
            Progress {
                done: 2_118_088 + 2_662_400,
                total: 9_158_664,
                download: 2_825_502
            }
        );
        // what Nix prints without -v is passed on; chatter is not
        assert_eq!(said.len(), 4, "{said:?}");
        assert!(said[0].starts_with("these 2 paths will be fetched"));
        assert!(said[2].starts_with("copying path '/nix/store/m9y0") && said[2].ends_with("..."));
        assert!(!said.iter().any(|s| s.contains("downloading")));
        assert!(t.error.is_none());
        t.line(r#"@nix {"action":"stop","id":60}"#);
        assert_eq!(t.progress().done, 2_118_088 + 7_040_576);
    }

    #[test]
    fn the_log_reader_reports_and_keeps_the_error() {
        let (tx, rx) = std::sync::mpsc::channel();
        let log = format!(
            "{LOG}@nix {{\"action\":\"msg\",\"level\":0,\"msg\":\"\\u001b[31;1merror:\\u001b[0m unable to download 'https://cache.nixos.org/x.narinfo': Could not resolve hostname (6)\"}}\nplain words\n"
        );
        let error = read_log(log.as_bytes(), &tx);
        assert_eq!(
            error.as_deref(),
            Some("No connection to the app server. Check the internet and try again.")
        );
        let last = rx.try_iter().last().expect("progress was sent");
        assert_eq!(last.total, 9_158_664);
    }

    #[test]
    fn errors_in_short_words() {
        let said = plain("\u{1b}[31;1merror:\u{1b}[0m  path\n   is  not valid\n");
        assert_eq!(said, "error:  path\n   is  not valid");
        assert_eq!(short_error(&said), "path is not valid");
        assert_eq!(
            short_error("error: writing to file: No space left on device"),
            "The disk is full."
        );
        let long = short_error(&format!("error: {}", "x".repeat(900)));
        assert!(long.chars().count() == 241 && long.ends_with('…'));
    }

    #[test]
    fn freed() {
        assert_eq!(
            super::parse_freed("12 store paths deleted, 1.50 MiB freed"),
            Some(1_572_864)
        );
        assert_eq!(
            super::parse_freed("0 store paths deleted, 0.00 MiB freed"),
            Some(0)
        );
        assert_eq!(super::parse_freed("nonsense"), None);
        assert_eq!(
            super::parse_summary("8 store paths deleted, 16.9 MiB freed"),
            Some((8, 17_720_934))
        );
        assert_eq!(
            super::parse_summary("0 store paths deleted, 0.0 KiB freed"),
            Some((0, 0))
        );
    }
}
