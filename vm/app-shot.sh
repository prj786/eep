#!/usr/bin/env bash
# Start an installed app the way a launcher click does (gio launch of its
# launcher entry — the visible one marked X-Eep-Id=<id> — spawned by
# Hyprland so it outlives this call), wait for
# its window, screenshot it, print the window's class / title / xwayland.
#   vm/app-shot.sh <id> <window-class-regex> [timeout-seconds]
set -euo pipefail
id=$1 class=$2 limit=${3:-60}
here="$(cd "$(dirname "$0")" && pwd)"
VM="${EWE_OS:-$HOME/Projects/ewe/ewe-os}/scripts/vm.sh"
export EWE_VM_DIR="${EWE_VM_DIR:-$HOME/vm/eep}"
U="$here/as-user.sh"
# shellcheck disable=SC2016  # expanded in the guest
"$U" sh -c "f=\$(grep -l -x 'X-Eep-Id=$id' \$HOME/.local/share/applications/*.desktop | xargs grep -L -x 'NoDisplay=true' | head -1)
    hyprctl dispatch \"hl.dsp.exec_cmd(\\\"gio launch \$f\\\")\"" >/dev/null
t0=$(date +%s)
until "$U" sh -c "hyprctl clients -j | jq -e --arg c '$class' 'any(.[]; (.class // \"\") | test(\$c; \"i\"))'" >/dev/null 2>&1; do
    if (( $(date +%s) - t0 > limit )); then echo "{\"id\":\"$id\",\"error\":\"no window after ${limit}s\"}"; exit 0; fi
    sleep 1
done
sleep "${SETTLE:-3}"
shot=$("$VM" shot "app-$id")
"$U" sh -c "hyprctl clients -j | jq -c --arg c '$class' --arg shot '$shot' --argjson s $(( $(date +%s) - t0 )) '{id: \"$id\", seconds: \$s, shot: \$shot, windows: [.[] | select((.class // \"\") | test(\$c; \"i\")) | {class, title: .title[0:50], xwayland}]}'"
