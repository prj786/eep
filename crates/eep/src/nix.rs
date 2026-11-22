//! The Nix engine, driven through its CLI. eep never evaluates: it only
//! asks the daemon to realise exact store paths (substituting from the
//! configured caches, checking every signature and hash itself), to keep
//! roots, and to collect garbage.

use std::path::Path;
use std::process::{Command, Stdio};

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

/// Realise several paths in one call (Nix downloads across all of them at
/// once). Roots: `root`, `root-2`, `root-3`… (Nix's naming), all in
/// `root`'s directory, which the caller removes once each path has its
/// own root.
pub fn realise_many(outs: &[&str], root: &Path) -> Result<()> {
    if let Some(dir) = root.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let st = nix_store()
        .arg("--realise")
        .args(outs)
        .arg("--add-root")
        .arg(root)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .context("running nix-store --realise")?;
    if !st.success() {
        bail!("nix-store --realise of {} paths failed ({st})", outs.len());
    }
    Ok(())
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
