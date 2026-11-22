//! `sudo eep driver install`: the one root step. Nix-built apps look for
//! GL / Vulkan / VA-API drivers in `/run/opengl-driver`, as on NixOS. eep
//! realises the release's driver set (NixOS's default: mesa), builds the
//! same symlink farm NixOS builds, and keeps `/run/opengl-driver` pointing
//! at it with a tmpfiles rule (`/run` is emptied at every boot).

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::paths::{Paths, write_atomic};
use crate::{nix, state};

const LIB: &str = "/var/lib/eep";
const TMPFILES: &str = "/etc/tmpfiles.d/eep-driver.conf";
const GCROOTS: &str = "/nix/var/nix/gcroots";

pub fn install(p: &Paths) -> Result<Value> {
    if !is_root() {
        bail!("run this one as root: sudo eep driver install");
    }
    // The driver follows the newest index (offline: the one we have). The
    // release the current driver came from is where patches start.
    let before = state::load_index(p).ok().map(|i| i.release);
    if let Err(e) = state::update_index(p) {
        eprintln!("eep: index not refreshed ({e:#}); using the cached one");
    }
    let idx = state::index_or_fetch(p)?;
    let drivers = &idx.system.driver;
    if drivers.is_empty() {
        bail!("the index names no driver");
    }
    let recovered = crate::journal::recover(p)?;
    let fetching = crate::journal::begin(p, "driver")?;
    let missing: std::collections::BTreeSet<String> = nix::invalid(
        &idx.system
            .driver_closure
            .iter()
            .map(|b| format!("/nix/store/{b}"))
            .collect::<Vec<_>>(),
    )?
    .into_iter()
    .filter_map(|x| x.strip_prefix("/nix/store/").map(str::to_owned))
    .collect();
    let mut applied = crate::patch::Applied::default();
    if let Some(from) = before.filter(|b| *b != idx.release && !missing.is_empty()) {
        let from = std::collections::BTreeSet::from([from]);
        match crate::patch::candidates(&state::config(p)?, &from, &idx.release, &missing)
            .and_then(|c| crate::patch::apply(p, &c, true))
        {
            Ok(a) => applied = a,
            Err(e) => eprintln!("eep: patches not used: {e:#}"),
        }
    }
    let realised = drivers.iter().enumerate().try_for_each(|(i, d)| {
        nix::realise(d, &PathBuf::from(format!("{GCROOTS}/eep-driver-{i}")))
    });
    crate::patch::clean(&applied);
    if realised.is_err() && applied.patched > 0 {
        eprintln!("eep: retrying the driver without patches");
        for (i, d) in drivers.iter().enumerate() {
            nix::realise(d, &PathBuf::from(format!("{GCROOTS}/eep-driver-{i}")))?;
        }
    } else {
        realised?;
    }
    drop(fetching);
    // Stale roots from a longer driver list.
    for i in drivers.len()..32 {
        let _ = std::fs::remove_file(format!("{GCROOTS}/eep-driver-{i}"));
    }

    // One farm per driver set, swapped in atomically.
    let key = eep_core::StorePath::from_absolute(&drivers[0])?
        .hash()
        .to_owned();
    let farms = Path::new(LIB).join("drivers");
    let farm = farms.join(&key);
    if !farm.exists() {
        let tmp = farms.join(format!("{key}.tmp"));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp)?;
        for d in drivers {
            merge(Path::new(d), &tmp)?;
        }
        std::fs::rename(&tmp, &farm)?;
    }
    let link = &p.driver_farm;
    let tmp_link = link.with_extension("new");
    let _ = std::fs::remove_file(&tmp_link);
    symlink(&farm, &tmp_link)?;
    std::fs::rename(&tmp_link, link)?;
    for e in std::fs::read_dir(&farms)?.flatten() {
        if e.path() != farm {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }

    // Durable, or a power-off soon after leaves an empty rule and no
    // /run/opengl-driver at the next boot (happened in the VM).
    write_atomic(
        Path::new(TMPFILES),
        format!("L+ /run/opengl-driver - - - - {}\n", link.display()).as_bytes(),
    )?;
    // The farm is thousands of symlinks: one syncfs instead of a sync per dir.
    let _ = Command::new("sync").args(["-f", LIB]).status();
    let st = Command::new("systemd-tmpfiles")
        .args(["--create", TMPFILES])
        .status()?;
    if !st.success() {
        bail!("systemd-tmpfiles --create failed");
    }
    let timer = keep_current();
    let hostname = hostname_resolves().unwrap_or_else(|e| {
        eprintln!("eep: /etc/hosts not updated: {e:#}");
        false
    });
    Ok(json!({
        "ok": true, "release": idx.release, "driver": drivers, "farm": farm,
        "link": "/run/opengl-driver", "target": std::fs::read_link("/run/opengl-driver").ok(),
        "recovered": recovered,
        "patches": applied,
        "timer": timer,
        "hostname": hostname,
    }))
}

const HOSTS: &str = "/etc/hosts";
const HOSTS_MARK: &str = "# eep: Nix apps can't use nss-myhostname";

/// Make this machine's own name resolve for Nix apps.
///
/// Arch answers it through systemd's NSS modules (myhostname, resolve);
/// Nix's glibc can't load Arch's modules, so its lookup falls through to
/// DNS and waits for a timeout — Firefox started 8.7 s instead of 0.7 s in
/// the VM. One marked line in /etc/hosts fixes it; the driver timer runs
/// this again, so a renamed machine follows. Lines without the mark are
/// never touched.
fn hostname_resolves() -> Result<bool> {
    let name = std::fs::read_to_string("/proc/sys/kernel/hostname")?
        .trim()
        .to_owned();
    if name.is_empty() || name.contains(char::is_whitespace) {
        return Ok(false);
    }
    let hosts = std::fs::read_to_string(HOSTS).unwrap_or_default();
    let ours = |l: &str| l.trim_end().ends_with(HOSTS_MARK);
    let known = hosts.lines().filter(|l| !ours(l)).any(|l| {
        let l = l.split('#').next().unwrap_or("");
        l.split_whitespace().skip(1).any(|n| n == name)
    });
    let wanted = (!known).then(|| format!("127.0.1.1\t{name}\t{HOSTS_MARK}"));
    let current: Vec<&str> = hosts.lines().filter(|l| ours(l)).collect();
    if current == wanted.iter().map(String::as_str).collect::<Vec<_>>() {
        return Ok(true);
    }
    let mut out: Vec<String> = hosts
        .lines()
        .filter(|l| !ours(l))
        .map(str::to_owned)
        .collect();
    out.extend(wanted);
    write_atomic(Path::new(HOSTS), format!("{}\n", out.join("\n")).as_bytes())?;
    Ok(true)
}

const SERVICE: &str = "/etc/systemd/system/eep-driver.service";
const TIMER: &str = "/etc/systemd/system/eep-driver.timer";
/// The package ships both units (and enables the timer).
const PACKAGED_TIMER: &str = "/usr/lib/systemd/system/eep-driver.timer";

/// The driver must move with the apps (a newer app may need a newer
/// Mesa): a system timer repeats this step — index check, patches, no-op
/// when current — so nobody types sudo again. Installed once, by the first
/// `sudo eep driver install`, from the same unit files the package ships;
/// best effort.
fn keep_current() -> bool {
    if Path::new(TIMER).exists() || Path::new(PACKAGED_TIMER).exists() {
        return true;
    }
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let service = include_str!("../../../packaging/eep-driver.service")
        .replace(crate::watch::PACKAGED_EEP, &exe.to_string_lossy());
    let timer = include_str!("../../../packaging/eep-driver.timer");
    let written = write_atomic(Path::new(SERVICE), service.as_bytes())
        .and_then(|()| write_atomic(Path::new(TIMER), timer.as_bytes()));
    written.is_ok()
        && Command::new("systemctl")
            .arg("daemon-reload")
            .status()
            .is_ok_and(|s| s.success())
        && Command::new("systemctl")
            .args(["enable", "--now", "eep-driver.timer"])
            .status()
            .is_ok_and(|s| s.success())
}

/// Symlink `src`'s tree into `dst` the way `buildEnv` does: whole
/// directories as one link while nothing else needs them, split into real
/// directories when two sources share one. First source wins on a clash.
fn merge(src: &Path, dst: &Path) -> Result<()> {
    for e in std::fs::read_dir(src)
        .with_context(|| format!("reading {}", src.display()))?
        .flatten()
    {
        let s = e.path();
        let d = dst.join(e.file_name());
        let s_is_dir = s.is_dir();
        match std::fs::symlink_metadata(&d) {
            Err(_) => symlink(&s, &d)?,
            Ok(m) if s_is_dir && m.file_type().is_symlink() && d.is_dir() => {
                let old = std::fs::read_link(&d)?;
                std::fs::remove_file(&d)?;
                std::fs::create_dir(&d)?;
                merge(&old, &d)?;
                merge(&s, &d)?;
            }
            Ok(m) if s_is_dir && m.is_dir() => merge(&s, &d)?,
            Ok(_) => eprintln!(
                "eep: driver: {} already provided, skipping {}",
                d.display(),
                s.display()
            ),
        }
    }
    Ok(())
}

fn is_root() -> bool {
    std::fs::read_to_string("/proc/self/status").is_ok_and(|s| {
        s.lines()
            .find_map(|l| l.strip_prefix("Uid:"))
            .and_then(|l| l.split_whitespace().nth(1))
            == Some("0")
    })
}

#[cfg(test)]
mod tests {
    use super::merge;

    #[test]
    fn merges_like_buildenv() {
        let t = std::env::temp_dir().join(format!("eep-merge-{}", std::process::id()));
        let (a, b, out) = (t.join("a"), t.join("b"), t.join("out"));
        for d in [
            a.join("lib/dri"),
            b.join("lib/vdpau"),
            a.join("share/vulkan"),
            out.clone(),
        ] {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        std::fs::write(a.join("lib/dri/x.so"), b"x").expect("write");
        std::fs::write(b.join("lib/vdpau/y.so"), b"y").expect("write");
        merge(&a, &out).expect("merge a");
        assert!(
            out.join("lib")
                .symlink_metadata()
                .expect("lib")
                .file_type()
                .is_symlink()
        );
        merge(&b, &out).expect("merge b");
        assert!(
            out.join("lib").symlink_metadata().expect("lib").is_dir(),
            "split into a real dir"
        );
        assert!(out.join("lib/dri/x.so").exists());
        assert!(out.join("lib/vdpau/y.so").exists());
        assert!(out.join("share/vulkan").exists());
        std::fs::remove_dir_all(&t).expect("cleanup");
    }
}
