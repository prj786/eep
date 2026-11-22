# Shared by the in-container test scripts.
set -euo pipefail
pass=0
start_daemon() {
  nix-daemon >/tmp/daemon.log 2>&1 &
  for _ in $(seq 50); do [[ -S /nix/var/nix/daemon-socket/socket ]] && return; sleep 0.1; done
  echo "nix-daemon did not start"; cat /tmp/daemon.log; exit 1
}
# eep as the test user; prints eep's JSON.
eep() { su - tester -c "EEP_SERVER=$EEP_SERVER /eep/eep $(printf '%q ' "$@")"; }
as_user() { su - tester -c "$*"; }
# check <name> <jq-expression> <json>
check() {
  if jq -e "$2" >/dev/null <<<"$3"; then echo "ok   $1"; pass=$((pass + 1));
  else echo "FAIL $1: $2"; echo "$3" | jq . 2>/dev/null || echo "$3"; exit 1; fi
}
