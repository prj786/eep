# `eep driver install` as root: /run/opengl-driver like NixOS's, kept by a
# tmpfiles rule. Runs inside the test container (test/run.sh driver.sh).
source /test/lib.sh
start_daemon
export HOME=/root
out=$(/eep/eep index update);         check "root index"            '.ok' "$out"
out=$(/eep/eep driver install);       check "driver install"        '.ok and .target != null' "$out"
echo "     $(jq -c '{driver, farm, target}' <<<"$out")"
[[ $(readlink /run/opengl-driver) == /var/lib/eep/opengl-driver ]] && echo "ok   /run/opengl-driver → /var/lib/eep/opengl-driver"
ls /run/opengl-driver/lib/dri/*_dri.so >/dev/null 2>&1 || ls /run/opengl-driver/lib/dri/ | head -3
[[ -d /run/opengl-driver/lib/dri ]] && echo "ok   lib/dri: $(ls /run/opengl-driver/lib/dri | head -4 | tr '\n' ' ')"
ls /run/opengl-driver/share/vulkan/icd.d/*.json >/dev/null && echo "ok   vulkan ICDs: $(ls /run/opengl-driver/share/vulkan/icd.d | tr '\n' ' ')"
ls /run/opengl-driver/share/glvnd/egl_vendor.d/*.json >/dev/null && echo "ok   EGL vendor: $(ls /run/opengl-driver/share/glvnd/egl_vendor.d | tr '\n' ' ')"
ls /nix/var/nix/gcroots/eep-driver-0 >/dev/null && echo "ok   driver GC root"
out=$(/eep/eep driver install);       check "driver install again (idempotent)" '.ok' "$out"
rm /run/opengl-driver                 # what a reboot does to /run
systemd-tmpfiles --create /etc/tmpfiles.d/eep-driver.conf
[[ -d /run/opengl-driver/lib/dri ]] && echo "ok   tmpfiles rule recreates it after a 'reboot'"
out=$(su - tester -c '/eep/eep driver install');  check "refuses without root" '.ok == false' "$out"
echo "all $pass checks passed"
