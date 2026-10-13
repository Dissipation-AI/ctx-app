#!/bin/bash
# Run CTX's self-test against a throwaway ctx server. Usage: tests/selftest.sh [path/to/CTX]
# On Linux without a display it runs on a virtual one (Xvfb).
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
app=${1:-$here/target/debug/CTX}
ctx=${CTX_BIN:-$here/../ctx/target/debug/ctx}
CTX_HOME=$(mktemp -d /tmp/ctx-app-selftest.XXXXXX)
export CTX_HOME
mkdir -p "$CTX_HOME/home"
printf 'shell = "/bin/zsh"\n' > "$CTX_HOME/config.toml"
pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null; rm -rf "$CTX_HOME"; }
trap cleanup EXIT
if [ "$(uname)" != Darwin ] && [ -z "${DISPLAY:-}" ]; then
    export DISPLAY=:$((90 + RANDOM % 9))
    Xvfb "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp > "$CTX_HOME/xvfb.log" 2>&1 &
    pids+=($!)
    for _ in $(seq 50); do xdpyinfo -display "$DISPLAY" > /dev/null 2>&1 && break; sleep 0.1; done
fi
LANG=C.UTF-8 HOME=$CTX_HOME/home "$ctx" server > "$CTX_HOME/server.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do [ -S "$CTX_HOME/run/server.sock" ] && break; sleep 0.1; done
out=$CTX_HOME/selftest.out
LANG=C.UTF-8 CTX_BIN="$ctx" CTX_APP_SELFTEST=1 "$app" > "$out" 2>&1 &
appid=$!
for _ in $(seq 120); do kill -0 $appid 2>/dev/null || break; sleep 1; done
if kill -0 $appid 2>/dev/null; then kill $appid; echo "FAIL the self-test didn't finish in 120 seconds"; cat "$out"; exit 1; fi
wait $appid; code=$?
cat "$out"
exit $code
