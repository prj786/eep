#!/usr/bin/env bash
# Install ewe into the eep test VM with no clicking: the GUI installer's own
# helper verbs, in the GUI's order (ewe-os installer/src/lib/steps/
# Install.svelte), run as root in the live system through the guest agent.
# Plain English, US keyboard, UTC, btrfs, no plugins, no apps, no account.
#
#   vm/install-ewe.sh <ewe.iso>      wipes $EWE_VM_DIR/target.qcow2 first
#   EEP_VM_FROM=<step> vm/install-ewe.sh <ewe.iso>
#                                    resume a failed run at that step, in
#                                    the live system that is still up
#
# Then: vm.sh reboot-disk [--gui], sign in as $EEP_VM_USER, run
# vm/setup-guest.sh. EWE_VM_DIR must be on a real disk (default ~/vm/eep).
set -euo pipefail
VM="${EWE_OS:-$HOME/Projects/ewe/ewe-os}/scripts/vm.sh"
export EWE_VM_DIR="${EWE_VM_DIR:-$HOME/vm/eep}"
# Hosts without QEMU's virtio display module get the standard VGA (the
# guest renders in software either way).
if [ -z "${EWE_VM_VGA:-}" ] && ! qemu-system-x86_64 -device help 2>/dev/null | grep -q '"virtio-vga"'; then
    export EWE_VM_VGA=VGA
fi
iso="$(realpath "${1:?usage: install-ewe.sh <iso>}")"
user="${EEP_VM_USER:-tester}"
pass="${EEP_VM_PASS:-eeptest}"
H=/usr/lib/ewe-installer/ewe-install-helper
disk=/dev/vda

ga() { EWE_VM_GA_TIMEOUT="${T:-300}" "$VM" ga "$@"; }
from="${EEP_VM_FROM:-}"
# step <name>: true when this step should run (resume skips until <from>).
step() {
    if [ -n "$from" ] && [ "$1" != "$from" ]; then return 1; fi
    from=""
    printf '\n== %s\n' "$1"
}
# Network steps: a stalled mirror fails the verb; the verbs are re-runnable.
retry() {
    local i
    for i in 1 2 3; do
        if "$@"; then return 0; fi
        echo "   attempt $i failed; retrying"
    done
    return 1
}

if [ -z "$from" ]; then
"$VM" down >/dev/null 2>&1 || true
"$VM" wipe >/dev/null 2>&1 || true
# vm.sh makes the disk with qemu-img; without one on the host, make it here.
command -v qemu-img >/dev/null || \
    "$(dirname "$0")/qemu-img.sh" create -f qcow2 "$EWE_VM_DIR/target.qcow2" 40G >/dev/null
"$VM" up "$iso"
step "waiting for the live system's guest agent"
for _ in $(seq 120); do
    if EWE_VM_GA_TIMEOUT=5 "$VM" ga true >/dev/null 2>&1; then break; fi
    sleep 3
done
fi
ga test -x "$H"

if step partition;  then ga "$H" partition "$disk" btrfs "ERASE=$disk" | tail -1; fi
if step mkfs;       then ga "$H" mkfs "$disk" btrfs "ERASE=$disk" | tail -1; fi
if step pacstrap;   then T=3600 retry ga "$H" pacstrap | tail -1; fi
if step settz;      then ga "$H" settz UTC | tail -1; fi
if step setlocale;  then T=600 ga "$H" setlocale en_US.UTF-8 en_US.UTF-8 | tail -1; fi
if step setkeymap;  then ga "$H" setkeymap us "" | tail -1; fi
if step hostname;   then ga "$H" sethostname eep-vm | tail -1; fi
if step user;       then ga sh -c "printf '%s\\n' '$pass' | $H user 'eep tester' '$user'" | tail -1; fi
if step layer;      then T=3600 retry ga "$H" layer | tail -1; fi
if step upgrade;    then T=3600 retry ga "$H" upgrade | tail -1; fi
if step bootloader; then T=600 ga "$H" bootloader | tail -1; fi
# A guest agent on the installed system too (TESTING.md "Install path"),
# with sync + umount so btrfs commits before the power-off.
step agent
T=900 retry ga sh -c 'arch-chroot /mnt/ewe-install pacman -S --noconfirm --needed qemu-guest-agent >/dev/null && sync && umount -R /mnt/ewe-install && echo ok'
"$VM" down
echo "installed: $VM reboot-disk [--gui]; user $user / $pass"
