# A power cut during an install: a "valid" piece with an empty file and a
# journal left behind. The next install must find it, delete it, fetch it
# again, and end verified. Runs inside the test container.
source /test/lib.sh
start_daemon
eep index update >/dev/null
out=$(eep install calculator);          check "install calculator"         '.ok' "$out"
calc=$(eep list | jq -r '.apps[] | select(.id=="calculator") | .out')
out=$(eep remove calculator);           check "remove (pieces stay)"       '.ok' "$out"
[[ -d $calc ]] && echo "ok   pieces still in the store"
# The damage: one file of the app emptied, as an unflushed write leaves it.
f=$(find "$calc/share/applications" -name '*.desktop' | head -1)
chmod u+w "$(dirname "$f")" "$f"; : > "$f"
! nix-store --verify-path "$calc" 2>/dev/null && echo "ok   damage is real (verify-path fails)"
# The journal a crash leaves behind, dated when that piece was fetched.
reg=$(nix --extra-experimental-features nix-command path-info --json --json-format 2 "$calc" | jq '.info[].registrationTime')
as_user "printf '{\"since\":%s,\"what\":\"install\"}' $reg > ~/.local/state/eep/inflight.json"
out=$(eep install calculator);          check "install recovers"           '.ok and (.recovered.damaged | length) >= 1 and .recovered.deleted >= 1' "$out"
echo "     $(jq -c '.recovered | {checked, damaged: (.damaged | map(.[44:])), deleted}' <<<"$out")"
as_user 'test ! -e ~/.local/state/eep/inflight.json' && echo "ok   journal gone"
as_user "nix --extra-experimental-features nix-command store verify -r $calc 2>/dev/null" && echo "ok   app verifies again"
out=$(eep install calculator);          check "no journal, no recovery"    '.ok and .recovered == null' "$out"
echo "all $pass checks passed"
