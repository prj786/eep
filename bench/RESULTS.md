# eep prototype — benchmark results

Measured 2026-11-22. Everything ran in throwaway places only: podman
containers (Arch + `extra/nix` 2.36.0) and the ewe test VM. Nothing was
installed on the dev box.

## Setup

| | |
|---|---|
| Releases | A = `nixos-26.11pre1119549.d5cc68339997` (Nov 15) · A1 = `nixos-26.11pre1124497.0bb8272318b4` (Nov 20) · B = `nixos-26.11pre1125864.2786a2a72088` (Nov 21) |
| Catalog | Firefox, LibreOffice, GIMP, VLC, Telegram, VSCodium, Calculator, ripgrep (+ mesa-demos, vulkan-tools) |
| VM | ewe 0.17.0-beta ISO (CI release), installed by `vm/install-ewe.sh`; QEMU/KVM, 4 vCPU, 6 GB, btrfs (zstd:1), standard VGA = **software GL only** (llvmpipe) |
| Host | 8 cores, 30 GB RAM; link to cache.nixos.org 5.9 MB/s (IPv4; IPv6 is black-holed here) |
| Comparison | pacman on the same ewe system, same app versions where both exist (Firefox 160.0.1, Calculator 51.1) |

Bytes are the reliable numbers on this link; times depend on it (Gotcha 13).

## Results

| # | What | eep | pacman / plain Nix | Note |
|---|---|---|---|---|
| B1 | First install, 8 apps (+2 tools), release A | **1,927 MB**, 374 s (~download time) | pacman **664 MB** (126 pkgs) | 2.9× — Nix brings its own base libraries |
| B2 | Weekly update A → B (5 of 8 apps changed, real version bumps) | **129 MB**, 15 s | plain Nix **542 MB** | 23.9 %; 20 pieces patched; VM run, every app `nix store verify -r` ✔ |
| B2' | Same pair, whole catalog (builder) | 140 MB | 594 MB | 23.6 %; 16 of 39 new pieces are new dependencies with nothing to patch from |
| B3 | Daily update A1 → B, whole catalog (builder) | **29.6 MB** | plain Nix **400 MB** | 7.4 %; LibreOffice rebuild 293.5 MB → 3.6 MB, GIMP 23.8 MB → 25 KB |
| B4 | Launch, cold / warm (median of 5) | Firefox **1,007 / 759 ms** · Calculator 701 / 362 ms | Firefox 990 / 802 ms · Calculator 513 / 355 ms | Warm within +2 %; before eep's /etc/hosts line Firefox took 8.7 s (Gotcha 30) |
| B5 | `eep run` overhead | **2.0 ms** (1 card), 2.7 ms (51 cards) | — | hyperfine, container |
| B6 | Idle RAM (PSS) | Firefox 697 MB (60 s, 1 tab) · Calculator 198 MB | Firefox 830 MB · Calculator 158 MB | Small apps pay ~40 MB: their GTK/GLib isn't shared with the desktop's |
| B7 | Disk (uncompressed store bytes) | 8 apps **7,242 MB** · Firefox + Calculator 1,905 MB | 8 apps **2,756 MB** · 343 MB | 2.6× overall; btrfs zstd shrinks both on disk |
| B8 | Delete | Card to Trash → launcher gone **524 ms**; restore → back 662 ms, no download | — | gc freed 617 MB of VSCodium's 619 MB unique (99.7 %) in 0.27 s |
| B9 | Server cost | Index: 109 s first, 1.1 s rebuild (byte-identical), 33.6 KB. Patches A1→B: 185 s, 341 CPU s, 8.8 GB peak RAM; A→B: 281 s, 830 CPU s, **11.6 GB peak** | — | 4 patches at once; the 1 GB LibreOffice piece with a 2 GB window dominates RAM |
| B10 | Mixed releases | Firefox B on mesa A ✔ · Firefox A on mesa B ✔ (GL works both ways) | — | 6 days apart, same glibc; the driver timer keeps them together anyway |
| B11 | Real GPU | **not run** (owner gate) | — | |

Driver update A → B with a patch: 51.4 MB → 11.0 MB.

## Go / no-go

| # | Claim | Passes if | Result |
|---|---|---|---|
| 1 | No evaluation on the client | install never runs `nix eval`; ≤ download time + 15 s | ✔ never evaluates; B1 374 s vs ~370 s of pure download |
| 2 | Small updates | rebuild pair ≤ 10 %; version pair ≤ 50 % | ✔ 7.4 % · 23.6 % (VM 23.9 %) |
| 3 | Safe | patched pieces byte-identical, pass `nix store verify`; tampered never lands | ✔ every patch proven on the builder; verify ✔; tamper refused by eep, and by Nix with eep's check off |
| 4 | Apps work | all 8 pass the Step 2 checklist | ✔ in the VM with software GL (VLC's Qt 5 UI is light, as Arch's VLC is on ewe); real GPU = B11 |
| 5 | Fast to launch | warm within +10 % of pacman; `eep run` < 10 ms | ✔ Firefox −5 %, Calculator +2 %; 2 ms |
| 6 | iOS-style delete | launcher < 2 s; gc frees ≥ 95 % of unique bytes | ✔ 524 ms; 99.7 % |
| 7 | Affordable server | one release builds in < 30 min on a CI runner | ✔ ~7 min here for index + two patch sets; needs ≥ 16 GB RAM (or fewer parallel patches) |

**Known costs, recorded honestly:** first install 2.9× pacman's bytes; disk
2.6×; small GTK apps ~40 MB more RAM; unfree apps (Chrome, Steam, Discord,
NVIDIA userspace) are not in the cache; Nix apps can't use Arch's NSS
modules, so `.local` mDNS names and systemd's synthetic names won't resolve
for them (reasoned, not tested; the hostname does resolve, via eep's
/etc/hosts line).

## How to reproduce

```sh
eep-build index --release <R>; eep-build patches --from <A> --to <B>   # B2', B3, B9
bench/patch-cost.sh <A> <B>                                            # B9 with peak RAM
test/run.sh bench-run.sh                                               # B5
vm/install-ewe.sh <iso>; vm/login.sh; vm/setup-guest.sh (as root: EEP_USER=tester)
bench/launch.sh, bench/ram.sh                                          # B4, B6 (in the VM, as root)
```
