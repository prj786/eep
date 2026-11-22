#!/usr/bin/env bash
# Step 0 guest setup: run once inside the ewe VM. Installs Nix as eep's
# engine, the local binary cache for patched pieces, eep itself and its
# per-user wiring — by hand, with the host's build of eep. The eep package
# (packaging/PKGBUILD) does the same for a real install. Never run this on a machine you care about: pacman stays
# the only package manager there until RFC-010 (house rule 10).
#
#   as the user:   bash setup-guest.sh                    (uses sudo)
#   as root:       EEP_USER=<name> bash setup-guest.sh    (e.g. via vm.sh ga)
#
# EEP_HOST (default http://10.0.2.2:8000, QEMU's view of the host) serves
# bin/eep and the builder's dist/ (see PROTOTYPE.md Step 2).
set -euo pipefail
HOST="${EEP_HOST:-http://10.0.2.2:8000}"

if [ "$(id -u)" = 0 ]; then
    user="${EEP_USER:?as root, set EEP_USER to the desktop user}"
    root() { "$@"; }
else
    user="$USER"
    root() { sudo "$@"; }
fi
uid="$(id -u "$user")"
home="$(getent passwd "$user" | cut -d: -f6)"
# The user's own commands, with their session bus when they are logged in.
as_user() {
    if [ "$(id -u)" = 0 ]; then
        runuser -u "$user" -- env HOME="$home" XDG_RUNTIME_DIR="/run/user/$uid" \
            DBUS_SESSION_BUS_ADDRESS="unix:path=/run/user/$uid/bus" PATH="$home/.local/bin:$PATH" "$@"
    else
        "$@"
    fi
}

root pacman -S --needed --noconfirm nix zstd hyperfine jq mesa-utils vulkan-tools
root systemctl enable --now nix-daemon.socket

# The same system side as the package (packaging/: eep.tmpfiles,
# nix-eep.conf, eep.install). The local cache is shared by every user like
# /tmp (sticky, world-writable): Arch's nix has no nix-users group.
root install -d -m 755 /var/cache/eep
root install -d -m 1777 /var/cache/eep/local /var/cache/eep/local/nar /var/tmp/eep
printf 'StoreDir: /nix/store\nWantMassQuery: 0\nPriority: 10\n' \
    | root tee /var/cache/eep/local/nix-cache-info >/dev/null
root tee /etc/nix/eep.conf >/dev/null <<'NIX'
extra-substituters = file:///var/cache/eep/local?priority=10
narinfo-cache-negative-ttl = 0
NIX
grep -qxF '!include /etc/nix/eep.conf' /etc/nix/nix.conf || \
    printf '\n%s\n' '!include /etc/nix/eep.conf' | root tee -a /etc/nix/nix.conf >/dev/null
# for nix commands by hand in the VM (eep passes the flag itself)
grep -q '^experimental-features' /etc/nix/nix.conf || \
    echo 'experimental-features = nix-command' | root tee -a /etc/nix/nix.conf >/dev/null
root systemctl restart nix-daemon.socket nix-daemon.service 2>/dev/null || true

# eep: the host's build, into the user's ~/.local/bin.
as_user mkdir -p "$home/.local/bin" "$home/.config/eep"
as_user curl -fsSL -o "$home/.local/bin/eep" "$HOST/bin/eep"
as_user chmod +x "$home/.local/bin/eep"
[ -e "$home/.config/eep/config.toml" ] || \
    as_user sh -c "printf 'server = \"%s\"\nchannel = \"nixos-unstable\"\n' '$HOST' > '$home/.config/eep/config.toml'"
for rc in "$home/.zshrc" "$home/.bashrc"; do
    [ -e "$rc" ] || continue
    grep -q '.local/share/eep/bin' "$rc" || \
        as_user sh -c "echo 'export PATH=\"\$HOME/.local/share/eep/bin:\$PATH\"' >> '$rc'"
done
as_user "$home/.local/bin/eep" index update
as_user "$home/.local/bin/eep" setup
# Root's eep (the driver step and its timer) reads /etc/eep/config.toml.
root install -d /etc/eep
printf 'server = "%s"\nchannel = "nixos-unstable"\n' "$HOST" | root tee /etc/eep/config.toml >/dev/null
# Root runs its own copy: the driver timer must not depend on a user's home.
root install -m 755 "$home/.local/bin/eep" /usr/local/bin/eep
root env HOME=/root /usr/local/bin/eep driver install
echo "done: eep doctor"
