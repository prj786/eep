# The install flow a file manager drives: what a card is, Install (the card
# moves in, its app is fetched, progress is reported), a card dropped into
# ~/Applications, a failure that doesn't stop the others, a card that leaves
# mid-fetch, and the card handler's --ask.
# Runs inside the test container (test/run.sh install-flow.sh).
source /test/lib.sh
start_daemon
B=$(jq -r .release /dist/channels/nixos-unstable.json)
A=$(ls /dist | grep '^nixos-' | sort -V | head -1)      # an older release
H=/home/tester
ACT=$H/.cache/eep/activity.json    # no XDG_RUNTIME_DIR in here: the cache folder
bg() { su - tester -c "EEP_SERVER=$EEP_SERVER nohup /eep/eep $1 > $2 2> $2.err &"; }
entry() { jq -c --arg id "$1" '.apps[] | select(.id==$id)' "$ACT" 2>/dev/null || true; }
wait_for() { for _ in $(seq "${2:-100}"); do eval "$1" && return 0; sleep 0.1; done; return 1; }

as_user 'rm -rf ~/Applications ~/.cache/eep ~/.local/state/eep ~/.local/share/eep ~/.local/share/applications ~/dl; mkdir -p ~/dl'
nix-store --gc >/dev/null 2>&1
out=$(eep index update);          check "index"                    '.ok' "$out"

# 1. What a downloaded card is, before anything is installed.
as_user "cp /dist/$B/cards/ripgrep.eep ~/dl/"
out=$(eep card $H/dl/ripgrep.eep)
check "card: not installed, sizes known" \
  '.ok and .id=="ripgrep" and (.inside|not) and .installed==null and (.ready|not) and .download > 0 and .pieces > 0' "$out"

# 2. Install: the card moves into ~/Applications and its app is fetched.
out=$(eep add $H/dl/ripgrep.eep)
check "add: installed, the card moved" \
  '.ok and .id=="ripgrep" and .moved and (.card|endswith("/Applications/ripgrep.eep"))' "$out"
as_user 'test ! -e ~/dl/ripgrep.eep' && echo "ok   the download is not left behind"
v=$(as_user '~/.local/share/eep/bin/rg --version' | head -1)
[[ $v == ripgrep* ]] && echo "ok   it runs: $v" || { echo "FAIL rg: $v"; exit 1; }
out=$(eep card $H/Applications/ripgrep.eep)
check "card: installed and ready"  '.ok and .inside and .ready and (.installed|endswith("ripgrep.eep"))' "$out"
out=$(eep add $H/Applications/ripgrep.eep)
check "add again: nothing to do"   '.ok and (.moved|not)' "$out"
[[ ! -e $ACT ]] && echo "ok   nothing in flight: no activity file"

# 3. A real download says how far it is, and never goes backwards.
as_user "cp /dist/$B/cards/Calculator.eep ~/dl/"
bg "add $H/dl/Calculator.eep" /tmp/add.json
partial=0 last=0 back=0 card="" download=0
for _ in $(seq 6000); do
  [[ -s /tmp/add.json ]] && break
  e=$(entry calculator)
  if [[ -n $e && $(jq -r .state <<<"$e") == downloading ]]; then
    pct=$(jq -r 'if .total > 0 then (.done * 100 / .total | floor) else 0 end' <<<"$e")
    (( pct < last )) && back=1
    last=$pct
    (( pct > 0 && pct < 100 )) && partial=1
    card=$(jq -r '.card // ""' <<<"$e"); download=$(jq -r .download <<<"$e")
  fi
  sleep 0.1
done
out=$(cat /tmp/add.json);         check "add (a real download)"    '.ok and .id=="calculator"' "$out"
(( partial == 1 )) && echo "ok   progress was reported on the way (last seen ${last}%)" || { echo "FAIL no partial progress seen"; exit 1; }
(( back == 0 )) && echo "ok   it never went backwards" || { echo "FAIL progress went backwards"; exit 1; }
[[ $card == $H/Applications/Calculator.eep ]] && echo "ok   the entry names the card" || { echo "FAIL card: $card"; exit 1; }
(( download > 0 )) && echo "ok   the download size is known ($download bytes)" || { echo "FAIL download size"; exit 1; }
[[ ! -e $ACT ]] && echo "ok   done: the activity file is gone" || { echo "FAIL activity left: $(cat $ACT)"; exit 1; }
l=$H/.local/share/applications/org.gnome.Calculator.desktop
[[ -e $l ]] && echo "ok   launcher entry written"

# 4. The watcher: a card of an OLDER release dropped in is installed as the
#    index has the app today, and the card says so afterwards.
bg watch /tmp/watch.log
sleep 1
out=$(eep remove calculator);     check "remove calculator"        '.ok' "$out"
want=$(jq -r .apps.calculator.version "/dist/$B/index.json")
old=$(sed -n 's/^version = "\(.*\)"/\1/p' "/dist/$A/cards/Calculator.eep")
as_user "cp /dist/$A/cards/Calculator.eep ~/Applications/"
wait_for "[[ -e $l ]]" 600 && echo "ok   watch: dropped card installed" || { echo "FAIL watch install"; cat /tmp/watch.log; exit 1; }
got=$(sed -n 's/^version = "\(.*\)"/\1/p' $H/Applications/Calculator.eep)
[[ $got == "$want" ]] && echo "ok   the card of $old became today's $got" || { echo "FAIL card version $got, index $want"; exit 1; }
out=$(eep card $H/Applications/Calculator.eep); check "it is ready" '.ready' "$out"

# 5. One app that can't be fetched doesn't stop the others, says why while
#    the watcher lives, and is forgotten with its card.
out=$(eep remove ripgrep);        check "remove ripgrep"           '.ok' "$out"
as_user "cat > /tmp/Ghost.eep" <<'CARD'
eep = 0
id = "ghost"
name = "Ghost"
kind = "cli"
version = "1.0"
out = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ghost-1.0"
exec = "bin/ghost"
installed = 2026-11-23T10:00:00Z
CARD
as_user "cp /dist/$B/cards/ripgrep.eep /tmp/Ghost.eep ~/Applications/"
wait_for "[[ -e $H/.local/share/eep/bin/rg ]]" 300 && echo "ok   the good app landed next to a bad one" || { echo "FAIL ripgrep blocked"; cat /tmp/watch.log; exit 1; }
wait_for '[[ $(entry ghost | jq -r .state 2>/dev/null) == failed ]]' 300 \
  && echo "ok   the bad one is reported: $(entry ghost | jq -r .error | cut -c1-90)" || { echo "FAIL ghost not reported"; cat "$ACT" /tmp/watch.log; exit 1; }
out=$(eep card $H/Applications/Ghost.eep); check "the bad one is not ready" '.ok and (.ready|not)' "$out"
as_user 'rm ~/Applications/Ghost.eep'
wait_for '[[ -z $(entry ghost) ]]' 100 && echo "ok   its card left: nothing is said about it any more" || { echo "FAIL ghost entry stays"; exit 1; }

# 6. A card that leaves while its app downloads stops the download.
out=$(eep remove calculator);     check "remove calculator"        '.ok' "$out"
out=$(eep gc);                    check "gc"                       '.ok and .freed > 0' "$out"
as_user "cp /dist/$B/cards/Calculator.eep ~/Applications/"
wait_for '[[ $(entry calculator | jq -r "select(.state==\"downloading\" and .done > 0) | .id" 2>/dev/null) == calculator ]]' 600 \
  && echo "ok   downloading" || { echo "FAIL never downloading"; cat /tmp/watch.log; exit 1; }
as_user 'rm ~/Applications/Calculator.eep'
wait_for '! pgrep -u tester -f "nix-store --realise" >/dev/null' 50 && echo "ok   the fetch stopped" || { echo "FAIL still fetching"; exit 1; }
wait_for '[[ -z $(entry calculator) ]]' 50 && echo "ok   nothing is said about it" || { echo "FAIL entry stays: $(cat $ACT)"; exit 1; }
[[ ! -e $l ]] && echo "ok   and it is not installed"
! grep -q '"failed":\[{' /tmp/watch.log || ! tail -1 /tmp/watch.log | grep -q calculator && echo "ok   not counted as a failure"
pkill -u tester -f 'eep watch' || true

# 7. The card handler asks first (`eep open --ask`): a card whose app is not
#    installed goes to the installer; an installed app just starts.
printf '#!/bin/sh\necho "$@" > /tmp/ui.args\n' > /tmp/ui.sh; chmod 755 /tmp/ui.sh
as_user "cp /dist/$B/cards/Calculator.eep ~/dl/"
su - tester -c "EEP_SERVER=$EEP_SERVER EEP_INSTALLER=/tmp/ui.sh /eep/eep open --ask ~/dl/Calculator.eep" >/dev/null 2>&1 || true
[[ $(cat /tmp/ui.args 2>/dev/null) == "--install=$H/dl/Calculator.eep" ]] && echo "ok   not installed: the installer is asked" || { echo "FAIL ui.args: $(cat /tmp/ui.args 2>&1)"; exit 1; }
rm -f /tmp/ui.args
v=$(su - tester -c "EEP_SERVER=$EEP_SERVER EEP_INSTALLER=/tmp/ui.sh /eep/eep open --ask ~/Applications/ripgrep.eep" 2>&1 || true)
[[ ! -e /tmp/ui.args && $v == *pattern* ]] && echo "ok   installed: it starts (rg wants a pattern)" || { echo "FAIL open installed: $v"; exit 1; }
echo "all $pass checks passed"
