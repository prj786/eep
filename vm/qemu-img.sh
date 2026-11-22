#!/usr/bin/env bash
# qemu-img for hosts without it (Arch ships it apart from qemu-system-x86):
# runs it in a container with $EWE_VM_DIR mounted at the same path.
#   vm/qemu-img.sh create -f qcow2 "$EWE_VM_DIR/target.qcow2" 40G
#   vm/qemu-img.sh snapshot -c clean "$EWE_VM_DIR/target.qcow2"
set -euo pipefail
dir="${EWE_VM_DIR:-$HOME/vm/eep}"
image=localhost/eep-qemu-img
if ! podman image exists "$image"; then
    printf 'FROM docker.io/library/archlinux:base\nRUN pacman -Syu --noconfirm --needed qemu-img && pacman -Scc --noconfirm\n' \
        | podman build -q -t "$image" -f - >/dev/null
fi
exec podman run --rm --userns=keep-id -v "$dir:$dir" "$image" qemu-img "$@"
