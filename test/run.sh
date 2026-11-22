#!/usr/bin/env bash
# Host side of the container tests.
#   test/run.sh [script]   build the image (once), serve dist/, run test/<script> (default e2e.sh)
# EEP_TEST_FRESH=1 starts from an empty /nix (otherwise the store volume is
# kept between runs, so downloads happen once).
set -euo pipefail
cd "$(dirname "$0")/.."
script="${1:-e2e.sh}"
port="${EEP_TEST_PORT:-8786}"
image=localhost/eep-test
podman image exists "$image" || podman build -t "$image" -f test/Containerfile test
cargo build --release -q
[[ "${EEP_TEST_FRESH:-0}" == 1 ]] && podman volume rm -f eep-test-nix >/dev/null
podman volume exists eep-test-nix || podman volume create eep-test-nix >/dev/null
python3 -m http.server -d dist "$port" >/dev/null 2>&1 &
server=$!
trap 'kill $server' EXIT
podman run --rm --network host \
  -v eep-test-nix:/nix \
  -v "$PWD/target/release:/eep:ro" \
  -v "$PWD/test:/test:ro" \
  -v "$PWD/dist:/dist:ro" \
  -e EEP_SERVER="http://127.0.0.1:$port" -e EEP_A -e EEP_B -e EEP_APPS \
  "$image" bash "/test/$script"
