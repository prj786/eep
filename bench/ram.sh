#!/usr/bin/env bash
# B6: idle RAM of an app after it settles. Runs IN the VM as root, driving
# the desktop user's session: start, wait, sum PSS (shared pages split
# fairly between processes) of every process of that app, stop it.
#   ram.sh <name> <process-regex> <seconds> <command…>
set -euo pipefail
name=$1 match=$2 settle=$3; shift 3
user="${EEP_VM_USER:-tester}"
pid=$(pgrep -u "$user" -x qs | head -1)  # the shell: has the session env
mapfile -d "" -t E < "/proc/$pid/environ"
envs=(); for e in "${E[@]}"; do case $e in HYPRLAND_INSTANCE_SIGNATURE=*|XDG_RUNTIME_DIR=*|WAYLAND_DISPLAY=*) envs+=("$e");; esac; done
cmd=$(printf '%q ' "$@")
env "${envs[@]}" hyprctl dispatch "hl.dsp.exec_cmd(\"${cmd//\"/\\\"}\")" >/dev/null
sleep "$settle"
pss=0 rss=0 n=0
for p in $(pgrep -u "$user" -f "$match"); do
    r=/proc/$p/smaps_rollup
    [ -r "$r" ] || continue
    pss=$(( pss + $(awk '/^Pss:/ {print $2}' "$r") ))
    rss=$(( rss + $(awk '/^Rss:/ {print $2}' "$r") ))
    n=$(( n + 1 ))
done
pkill -u "$user" -f "$match" || true
sleep 3
printf '{"name":"%s","processes":%s,"pss_mb":%s,"rss_mb":%s,"settle_s":%s}\n' \
    "$name" "$n" "$(( pss / 1024 ))" "$(( rss / 1024 ))" "$settle"
