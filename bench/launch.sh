#!/usr/bin/env bash
# B4: launch time, from exec until the app's window maps (Hyprland lists a
# client with the class). Runs IN the VM as root (for drop_caches), driving
# the desktop user's Hyprland session; median of N runs, cold and warm.
#   launch.sh <name> <window-class-regex> <command…>
#   e.g. launch.sh eep-firefox firefox eep run firefox
set -euo pipefail
name=$1 class=$2; shift 2
runs="${RUNS:-5}"
user="${EEP_VM_USER:-tester}"
pid=$(pgrep -u "$user" -x qs | head -1)  # the shell: has the session env
mapfile -d "" -t E < "/proc/$pid/environ"
envs=() ; for e in "${E[@]}"; do case $e in HYPRLAND_INSTANCE_SIGNATURE=*|XDG_RUNTIME_DIR=*|WAYLAND_DISPLAY=*) envs+=("$e");; esac; done
hc() { env "${envs[@]}" hyprctl "$@"; }
mapped() { hc clients -j | jq -e --arg c "$class" 'any(.[]; (.class // "") | test($c; "i"))' >/dev/null; }
closeall() {
    hc clients -j | jq -r --arg c "$class" '.[] | select((.class // "") | test($c; "i")) | .pid' |
        while read -r p; do kill "$p" 2>/dev/null || true; done
    for _ in $(seq 100); do mapped || break; sleep 0.1; done
    sleep 1
}
# hyprctl's exec runs the command as the session user, in the session.
cmd=$(printf '%q ' "$@")
one() {
    local t0 t1
    t0=$(date +%s%N)
    hc dispatch "hl.dsp.exec_cmd(\"${cmd//\"/\\\"}\")" >/dev/null
    for _ in $(seq 1500); do mapped && break; sleep 0.02; done
    t1=$(date +%s%N)
    mapped || { echo "never mapped" >&2; echo 99999; return; }
    echo $(( (t1 - t0) / 1000000 ))
    closeall
}
median() { sort -n | awk '{a[NR]=$1} END {print a[int((NR+1)/2)]}'; }
closeall
cold=$(for _ in $(seq "$runs"); do sync; echo 3 > /proc/sys/vm/drop_caches; one; done | median)
one >/dev/null   # prime the cache
warm=$(for _ in $(seq "$runs"); do one; done | median)
printf '{"name":"%s","cold_ms":%s,"warm_ms":%s,"runs":%s}\n' "$name" "$cold" "$warm" "$runs"
