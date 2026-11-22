# Install / run / remove / gc round trip for a CLI app and a GUI app.
# Runs inside the test container (test/run.sh).
source /test/lib.sh
start_daemon

out=$(eep index update);          check "index update"            '.ok and .changed' "$out"
out=$(eep index update);          check "index update again"      '.ok and (.changed|not)' "$out"
out=$(eep available);             check "ripgrep is available"    '.apps[] | select(.id=="ripgrep") | .version != ""' "$out"
out=$(eep info ripgrep);          check "info plans a download"   '.ok and .download_now > 0' "$out"

out=$(eep install ripgrep);       check "install ripgrep"         '.ok and .id=="ripgrep" and .pieces_new > 0' "$out"
echo "     $(jq -c '{download, pieces_new, seconds}' <<<"$out")"
v=$(as_user '/eep/eep run ripgrep -- --version' | head -1)
[[ $v == ripgrep* ]] && echo "ok   eep run ripgrep -- --version: $v" || { echo "FAIL eep run: $v"; exit 1; }
v=$(as_user '~/.local/share/eep/bin/rg --version' | head -1)
[[ $v == ripgrep* ]] && echo "ok   PATH shim rg" || { echo "FAIL shim: $v"; exit 1; }
out=$(eep list);                  check "list shows it present"   '.apps[] | select(.id=="ripgrep") | .present' "$out"
out=$(eep install ripgrep);       check "reinstall downloads 0"   '.ok and .pieces_new == 0' "$out"
out=$(eep run nope 2>&1 || true); check "run unknown app fails"   '.ok == false' "$out"
out=$(eep doctor);                check "doctor"                  '.ok and .local_cache.ready and .nix.store != null' "$out"

rg_out=$(jq -r '.out' <<<"$(eep list | jq '.apps[0]')")
out=$(eep remove ripgrep);        check "remove ripgrep"          '.ok and .roots >= 1 and .shims == 1' "$out"
as_user 'test ! -e ~/Applications/ripgrep.eep && test ! -e ~/.local/share/eep/bin/rg' && echo "ok   card and shim gone"
out=$(eep gc --dry-run);          check "gc dry run frees bytes"  '.ok and .freed > 0' "$out"
out=$(eep gc);                    check "gc frees bytes"          '.ok and .freed > 0' "$out"
echo "     $(jq -c '{freed, paths, seconds}' <<<"$out")"
[[ ! -e $rg_out ]] && echo "ok   ripgrep's store path collected" || { echo "FAIL $rg_out still there"; exit 1; }

# GUI app: launcher entry + icon. Calculator has the smallest GUI closure.
out=$(eep install calculator);    check "install calculator"      '.ok and (.integrated.launchers|length) >= 1' "$out"
echo "     $(jq -c '{download, pieces_new, seconds}' <<<"$out")"
l=/home/tester/.local/share/applications/org.gnome.Calculator.desktop
grep -q '^Exec=/eep/eep run calculator --exec ' "$l" && echo "ok   launcher Exec goes through eep run"
grep -q '^X-Eep-Id=calculator$' "$l" && echo "ok   launcher tagged"
icon=$(sed -n 's/^Icon=//p' "$l"); [[ -s $icon ]] && echo "ok   launcher icon $icon"
! grep -q '^TryExec=' "$l" && echo "ok   no TryExec"
desktop-file-validate "$l" && echo "ok   desktop-file-validate"

# Delete the card, put it back (Trash restore): no download.
as_user 'mv ~/Applications/Calculator.eep /tmp/'
out=$(eep repair);                check "repair forgets a deleted card" '.forgotten | length == 1' "$out"
[[ ! -e $l ]] && echo "ok   launcher gone with the card"
as_user 'mv /tmp/Calculator.eep ~/Applications/'
out=$(eep repair);                check "restored card: no download" '.apps[0].fetched == false' "$out"
[[ -e $l ]] && echo "ok   launcher back"

# eep watch: the Apps place, live (go/no-go #6: launcher gone in < 2 s).
as_user 'nohup /eep/eep watch > /tmp/watch.log 2>&1 &'
sleep 1
wait_for() { # wait_for <test-expr>; prints seconds
  local t0; t0=$(date +%s%N)
  for _ in $(seq 100); do eval "$1" && break; sleep 0.05; done
  awk -v a="$t0" -v b="$(date +%s%N)" 'BEGIN { printf "%.2f", (b - a) / 1e9 }'
}
as_user 'mv ~/Applications/Calculator.eep /tmp/'
s=$(wait_for "[[ ! -e $l ]]"); [[ ! -e $l ]] && echo "ok   watch: card deleted, launcher gone in ${s}s" || { echo "FAIL watch delete"; cat /tmp/watch.log; exit 1; }
as_user 'cp /tmp/Calculator.eep ~/Applications/Calc.eep'
s=$(wait_for "[[ -e $l ]]"); [[ -e $l ]] && echo "ok   watch: card copied in (renamed), launcher back in ${s}s" || { echo "FAIL watch restore"; cat /tmp/watch.log; exit 1; }
pkill -f 'eep watch' || true

out=$(eep updates --count);       check "no updates"              '.ok and .count == 0' "$out"
echo "all $pass checks passed"
