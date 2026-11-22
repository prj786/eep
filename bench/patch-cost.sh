#!/usr/bin/env bash
# B9: what building one patch set costs. Runs eep-build patches inside a
# systemd user scope and prints its JSON plus the scope's peak memory.
#   bench/patch-cost.sh <from> <to> [extra eep-build args…]
set -euo pipefail
cd "$(dirname "$0")/.."
from=$1 to=$2; shift 2
exec systemd-run --user --scope --quiet -p MemoryAccounting=yes bash -c '
  ./target/release/eep-build patches --from "$0" --to "$1" "${@:2}" > /tmp/eep-patch-cost.$$.json
  cg=/sys/fs/cgroup$(cut -d: -f3 /proc/self/cgroup)
  jq --argjson peak "$(cat "$cg/memory.peak" 2>/dev/null || echo null)" ". + {peak_memory_bytes: \$peak}" /tmp/eep-patch-cost.$$.json
  rm -f /tmp/eep-patch-cost.$$.json
' "$from" "$to" "$@"
