# B5: what `eep run` adds to starting a program (go/no-go #5: < 10 ms).
source /test/lib.sh
start_daemon
eep index update >/dev/null
eep install ripgrep >/dev/null
rg=$(eep list | jq -r '.apps[] | select(.id=="ripgrep") | .out')/bin/rg
for n in 0 20 50; do
  # Cards to scan: eep run reads ~/Applications to find the app.
  as_user "mkdir -p /tmp/extra && for i in \$(seq $n); do sed \"s/^id = .*/id = \\\"fake\$i\\\"/\" ~/Applications/ripgrep.eep > ~/Applications/fake\$i.eep; done"
  as_user "hyperfine -N -w 20 -r 300 --export-json /tmp/h$n.json '/eep/eep run ripgrep -- --version' '$rg --version'" >/dev/null 2>&1
  python - "$n" /tmp/h$n.json <<'PY'
import json, sys
r = json.load(open(sys.argv[2]))["results"]
wall = lambda x: (x.get("mean") or x["summary"]["time_wall_clock"]["mean"]) * 1000
run, direct = wall(r[0]), wall(r[1])
print(f"cards={sys.argv[1]:>3}  eep run {run:.2f} ms  direct {direct:.2f} ms  overhead {run - direct:.2f} ms")
PY
done
