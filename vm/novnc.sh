#!/usr/bin/env bash
# Watch and drive the eep test VM in a browser, with nothing installed on
# the host: noVNC in a container on http://localhost:6080, talking to the
# VM's VNC server (start the VM with `vm.sh reboot-disk --vnc`).
#   vm/novnc.sh          start (idempotent)
#   vm/novnc.sh stop
set -euo pipefail
name=eep-novnc image=localhost/eep-novnc
if [ "${1:-}" = stop ]; then podman stop "$name" >/dev/null 2>&1 || true; exit 0; fi
if ! podman image exists "$image"; then
    printf 'FROM docker.io/library/alpine:latest\nRUN apk add --no-cache novnc websockify\n' \
        | podman build -q -t "$image" -f - >/dev/null
fi
podman container exists "$name" || podman run -d --rm --name "$name" --network host "$image" \
    websockify --web /usr/share/novnc 127.0.0.1:6080 127.0.0.1:5900 >/dev/null
echo "http://localhost:6080/vnc.html?autoconnect=1&resize=scale"
