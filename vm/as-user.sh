#!/usr/bin/env bash
# Run a command in the eep test VM as the desktop user, inside the running
# Hyprland session's environment (what a launcher-started app gets).
#   vm/as-user.sh eep doctor
#   EWE_VM_GA_TIMEOUT=600 vm/as-user.sh eep install firefox
set -euo pipefail
VM="${EWE_OS:-$HOME/Projects/ewe/ewe-os}/scripts/vm.sh"
export EWE_VM_DIR="${EWE_VM_DIR:-$HOME/vm/eep}"
user="${EEP_VM_USER:-tester}"
# shellcheck disable=SC2016  # expanded in the guest
exec "$VM" ga bash -c '
    u=$1; shift
    # A child of Hyprland (the shell): Hyprland itself lacks its own signature.
    pid=$(pgrep -u "$u" -x qs | head -1)
    [ -n "$pid" ] || { echo "no Hyprland session for $u" >&2; exit 1; }
    mapfile -d "" -t E < "/proc/$pid/environ"
    home=$(getent passwd "$u" | cut -d: -f6)
    exec runuser -u "$u" -- env -i "${E[@]}" PATH="$home/.local/bin:$home/.local/share/eep/bin:/usr/local/bin:/usr/bin" "$@"
' bash "$user" "$@"
