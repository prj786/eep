# eep — ewe executable package

Apps the iOS way, on ewe: every app is one small **card** in
`~/Applications`. The card is there, the app is in the launcher. Delete the
card, the app is gone. Cards carry their app's icon, so Files shows them as
apps; a card downloaded from the website or put there by Komble installs
the same way. The contract for Files, Komble and the website:
[docs/CARD.md](docs/CARD.md).

Underneath, apps come prebuilt and signed from the Nix binary cache
(cache.nixos.org). The client **never evaluates Nix**: an eep server
precomputes an index that says exactly which store paths an app needs, and
updates come as **binary patches** against what you already have. Nix stays
the engine: it downloads, checks every signature and hash, and collects
garbage.

> Status: **prototype, 0.1.0-beta** (see [PROTOTYPE.md](PROTOTYPE.md)).
> Tested in the ewe VM and the test container; the `eep` package (below)
> is opt-in. pacman stays the system's package manager (house rule 10;
> RFC-010 decides more).

## Layout

| Path | What |
|---|---|
| `crates/eep-core` | Shared formats: store paths, nix-base32, narinfo, `.ls` listings, index, card, catalog, patch manifest, desktop entries, HTTP |
| `crates/eep` | The client CLI |
| `crates/eep-build` | The server side: indexes and patches (drives Nix eval in podman) |
| `catalog/apps.toml` | The apps eep offers |
| `test/` | Container tests (Arch + `extra/nix`): `test/run.sh e2e.sh`, `test/run.sh update.sh` |
| `packaging/` | The `eep` pacman package: PKGBUILD, install script, units, config |
| `vm/setup-guest.sh` | Guest setup for the ewe VM (the package's system side, by hand) |
| `bench/` | Benchmark scripts and `RESULTS.md` |
| `dist/` | Builder output (git-ignored), served as-is by any static web server |

## Server side

```sh
cargo build --release
eep-build releases                       # newest nixos-unstable releases
eep-build index --release <R>            # dist/<R>/index.json.zst + icons (one Nix eval, in podman)
eep-build patches --from <A> --to <R>    # dist/patches/<A>__<R>/ (zstd --patch-from, each one proven)
eep-build cards --release <R>            # dist/<R>/cards/<Name>.eep (icon inside) + catalog.json
eep-build publish --release <R>          # dist/channels/nixos-unstable.json → <R>
python3 -m http.server -d dist 8000      # the VM sees it at http://10.0.2.2:8000
```

The public server is **<https://prj786.github.io/eep>** (eep's default):
GitHub Pages of this repo, from a `gh-pages` branch that is one orphan
commit replaced on every publish, so patch files never pile up in history.
`.github/workflows/publish-index.yml` rebuilds it every day (newest
nixos-unstable release, its cards, patches from the previous one) and
keeps the newest few releases. A client uses another server with
`server = "…"` in `~/.config/eep/config.toml` (or `/etc/eep/config.toml`)
or `EEP_SERVER`.

Caches live in `~/.cache/eep-build` (narinfos, listings and evals forever;
NARs for patch building, safe to delete). Indexes are deterministic: the
same release and catalog give byte-identical output.

## Client

```sh
eep index update            # fetch the newest index (conditional GET)
eep available               # what can be installed
eep install firefox         # → ~/Applications/Firefox.eep, launcher entry, icon
eep run firefox             # what the launcher entry calls: clean env, exec
eep list                    # the cards
eep updates [--count]       # what an update costs, with and without patches
eep update [ids…]           # patches where possible, the rest upstream
eep rollback firefox        # the previous version, instantly (kept 7 days)
eep remove firefox          # card, launcher entry, roots; disk freed by gc
eep gc [--dry-run]          # also runs by itself; keeps apps whose card is in the Trash
eep repair                  # rebuild everything from the cards
eep watch                   # follow ~/Applications (a systemd user service; the package enables it)
eep doctor                  # check the setup
sudo eep driver install     # /run/opengl-driver for Nix apps (the one root step)
```

## Package

`packaging/PKGBUILD` builds the `eep` pacman package (the client only):
`/usr/bin/eep`; `/etc/eep/config.toml` (the server); the driver timer
(system, started at install: no `sudo eep driver install`); the watcher
and the update timer for every desktop user (no `eep setup`); the card
file type and its handler; `~/.local/share/eep/bin` on PATH
(`/etc/profile.d/eep.sh`); the shared local cache `/var/cache/eep/local`
(sticky and world-writable like /tmp, a tmpfiles rule) and
`/etc/nix/eep.conf`, pulled into `/etc/nix/nix.conf` by one
`!include /etc/nix/eep.conf` line the package adds and removes. A tag
`v*` builds it in CI (`.github/workflows/release.yml`) and attaches it to
the GitHub release, where ewe-repo's publish picks it up.

Every command prints one JSON object and exits 0; errors are
`{"ok":false,"error":"…"}` (house rule 5). `eep run` becomes the app.

## Developing

```sh
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
test/run.sh e2e.sh          # install / run / remove / gc / Trash restore, in a container
EEP_A=<A> EEP_B=<B> test/run.sh update.sh   # patched update, rollback, tamper test
```

Rules this code keeps: no `unsafe`; JSON on stdout only; every write that a
watcher or another process can see is atomic (temp file + rename); one eep
process changes state at a time (a lock file); nothing outside eep's own
files is touched (house rule 9).
