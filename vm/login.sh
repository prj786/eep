#!/usr/bin/env bash
# Boot the installed eep test VM (if it's down) and sign in at the greeter.
set -euo pipefail
VM="${EWE_OS:-$HOME/Projects/ewe/ewe-os}/scripts/vm.sh"
export EWE_VM_DIR="${EWE_VM_DIR:-$HOME/vm/eep}"
if [ -z "${EWE_VM_VGA:-}" ] && ! qemu-system-x86_64 -device help 2>/dev/null | grep -q '"virtio-vga"'; then
    export EWE_VM_VGA=VGA
fi
pass="${EEP_VM_PASS:-eeptest}"
"$VM" status | grep -q "^running" || "$VM" reboot-disk "$@"
for _ in $(seq 60); do
    EWE_VM_GA_TIMEOUT=5 "$VM" ga systemctl is-active -q greetd >/dev/null 2>&1 && break
    sleep 2
done
sleep 4  # the greeter's window maps a moment after greetd starts
"$VM" click 620 460
"$VM" type "$pass"
"$VM" key ret
for _ in $(seq 60); do
    if EWE_VM_GA_TIMEOUT=5 "$VM" ga sh -c 'ls /run/user/1000/hypr' 2>/dev/null | grep -q .; then
        echo "signed in"; exit 0
    fi
    sleep 2
done
echo "no Hyprland session after 2 minutes" >&2; exit 1
