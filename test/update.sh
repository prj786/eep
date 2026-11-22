# Update A → B with patches, verify, roll back, and the tamper test.
# Runs inside the test container (test/run.sh update.sh). Needs dist/ with
# indexes for $EEP_A and $EEP_B and dist/patches/$EEP_A__$EEP_B.
source /test/lib.sh
start_daemon
A=${EEP_A:?} B=${EEP_B:?}
APPS=(${EEP_APPS:-firefox calculator})
R=${APPS[0]}   # rolled back; must print its version headless

# One server root per scenario, each with its own channel pointer.
mkroot() { # <dir> <release>
  mkdir -p "$1/channels"
  ln -sfn "/dist/$2" "$1/$2"
  [[ -e $1/patches ]] || ln -sfn /dist/patches "$1/patches"
  jq -n --arg r "$2" --arg s "$(sha256sum "/dist/$2/index.json.zst" | cut -d' ' -f1)" \
     --arg d "$(jq -r .released "/dist/$2/index.json")" \
     '{eep_channel:0, channel:"nixos-unstable", release:$r, released:$d, index:($r+"/index.json.zst"), sha256:$s}' \
     > "$1/channels/nixos-unstable.json"
}
serve() { python -m http.server -d "$1" "$2" >/dev/null 2>&1 & }
mkroot /srv/a "$A"; serve /srv/a 8801
mkroot /srv/b "$B"; serve /srv/b 8802
sleep 1
use() { EEP_SERVER=http://127.0.0.1:$1; }

# Clean start: nothing of A or B in the store.
as_user 'rm -rf ~/Applications ~/.cache/eep ~/.local/state/eep ~/.local/share/eep ~/.local/share/applications'
nix-store --gc >/dev/null 2>&1

use 8801
out=$(eep index update);                 check "index A"                 ".release == \"$A\"" "$out"
out=$(eep install "${APPS[@]}");         check "install from A"          '.ok' "$out"
use 8802
out=$(eep index update);                 check "index B"                 ".release == \"$B\"" "$out"
out=$(eep updates --count);              check "updates counted"         ".count == ${#APPS[@]}" "$out"
out=$(eep updates);                      check "updates priced"          '.ok and ([.updates[].patched] | add) > 0 and .download < .download_full' "$out"
echo "     $(jq -c '[.updates[] | {id, from, to, download, download_full, patched}]' <<<"$out")"
out=$(eep update);                       check "update with patches"     ".ok and .patches.patched > 0 and (.fell_back | not) and (.updated | length) == ${#APPS[@]}" "$out"
echo "     $(jq -c '{download, download_full, patched: .patches.patched, seconds, apps: [.updated[] | {id, from, version}]}' <<<"$out")"
for id in "${APPS[@]}"; do
  o=$(eep list | jq -r --arg id "$id" '.apps[] | select(.id==$id) | .out')
  as_user "nix --extra-experimental-features nix-command store verify -r $o 2>/dev/null" && echo "ok   nix store verify -r $id"
done
v=$(as_user "/eep/eep run $R -- --version" 2>/dev/null | head -1); echo "ok   runs after update: $v"

out=$(eep rollback "$R");                check "rollback"                '.ok' "$out"
out=$(eep list);                         check "rolled back to A's build" "[.apps[] | select(.id==\"$R\") | .release] == [\"$A\"]" "$out"
v=$(as_user "/eep/eep run $R -- --version" 2>/dev/null | head -1); echo "ok   runs after rollback: $v"
out=$(eep update "$R");                  check "re-update downloads 0"   '.pieces_new == 0 and (.updated | length) == 1' "$out"

# Tamper: every patch rebuilds a valid NAR of the wrong piece.
as_user 'rm -rf ~/Applications ~/.cache/eep ~/.local/state/eep ~/.local/share/eep ~/.local/share/applications'
nix-store --gc >/dev/null 2>&1
use 8801; eep index update >/dev/null; eep install calculator >/dev/null
wrong=$(eep list | jq -r '.apps[0].out')
mkroot /srv/bad "$B"; rm /srv/bad/patches; mkdir -p /srv/bad/patches
cp -r "/dist/patches/${A}__${B}" /srv/bad/patches/
for f in /srv/bad/patches/*/p/*.zst; do nix-store --dump "$wrong" | zstd -q -f -o "$f"; done
serve /srv/bad 8803; sleep 1
use 8803; eep index update >/dev/null
out=$(eep update calculator);            check "eep's check refuses bad patches" '.ok and .patches.patched == 0 and (.patches.failed | length) > 0' "$out"
o=$(eep list | jq -r '.apps[0].out'); as_user "nix --extra-experimental-features nix-command store verify -r $o 2>/dev/null" && echo "ok   still verifies"
eep remove calculator >/dev/null; nix-store --gc >/dev/null 2>&1
use 8801; eep index update >/dev/null; eep install calculator >/dev/null
use 8803; eep index update >/dev/null
out=$(eep update calculator --unchecked-patches); check "unchecked bad patches still end well" '.ok and .patches.patched > 0' "$out"
echo "     Nix refused the bad pieces; eep retried without patches: $(jq '.fell_back' <<<"$out")"
o=$(eep list | jq -r '.apps[0].out'); as_user "nix --extra-experimental-features nix-command store verify -r $o 2>/dev/null" && echo "ok   fallback result verifies"
echo "all $pass checks passed"
