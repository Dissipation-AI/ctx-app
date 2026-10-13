#!/bin/bash
# Memory budget from SPEC.md § Acceptance tests: 3 windows, 15 tabs, `top` running.
# Uses a throwaway ctx server (CTX_HOME), so your own tabs are untouched. On macOS it reports
# `footprint` (what Activity Monitor shows); on Linux the proportional set size (PSS), on a
# virtual display when there's none.
# Usage: scripts/budgets.sh [path/to/CTX] [window width height in points]
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
if [ "$(uname)" = Darwin ]; then def_app=$here/target/CTX.app/Contents/MacOS/CTX; else def_app=$here/target/release/CTX; fi
app=${1:-$def_app}
ww=${2:-900}; wh=${3:-600}
ctx=${CTX_BIN:-$here/../ctx/target/release/ctx}
[ -x "$ctx" ] || ctx=$here/../ctx/target/debug/ctx
CTX_HOME=$(mktemp -d /tmp/ctx-app-budget.XXXXXX)
export CTX_HOME
mkdir -p "$CTX_HOME/home"
pids=()
appid=0
cleanup() {
    kill "${pids[@]}" "$appid" 2> /dev/null
    for _ in $(seq 30); do kill -0 "${pids[@]}" "$appid" 2> /dev/null || break; sleep 0.1; done
    rm -rf "$CTX_HOME"
}
trap cleanup EXIT
if [ "$(uname)" != Darwin ] && [ -z "${DISPLAY:-}" ]; then
    export DISPLAY=:$((70 + RANDOM % 9))
    Xvfb "$DISPLAY" -screen 0 3600x2400x24 -nolisten tcp > "$CTX_HOME/xvfb.log" 2>&1 &
    pids+=("$!")
    for _ in $(seq 50); do xdpyinfo -display "$DISPLAY" > /dev/null 2>&1 && break; sleep 0.1; done
fi
LANG=C.UTF-8 HOME=$CTX_HOME/home "$ctx" server > "$CTX_HOME/server.log" 2>&1 &
server=$!
pids+=("$server")
for _ in $(seq 50); do [ -S "$CTX_HOME/run/server.sock" ] && break; sleep 0.1; done
for _ in 1 2 3; do "$ctx" open window > /dev/null; done
for w in 1 2 3; do for _ in 1 2 3 4; do "$ctx" open tab in window $w > /dev/null; done; done
mkdir -p "$CTX_HOME/data"
cat > "$CTX_HOME/data/app-state.json" <<JSON
{"windows": {"1": {"frame": [80, 80, $ww, $wh]}, "2": {"frame": [140, 140, $ww, $wh]}, "3": {"frame": [200, 200, $ww, $wh]}}, "set_up": true}
JSON
WINIT_X11_SCALE_FACTOR=${CTX_X11_SCALE:-2} CTX_BIN="$ctx" "$app" > "$CTX_HOME/app.log" 2>&1 &
appid=$!
sleep 3
first=$("$ctx" list tabs as json | python3 -c 'import json,sys; print(json.load(sys.stdin)["windows"][0]["tabs"][0]["number"])')
"$ctx" run in tab "$first" "top" within 1 second > /dev/null 2>&1
sleep "${SETTLE:-15}"
mem() {
    if [ "$(uname)" = Darwin ]; then
        footprint "$1" 2>/dev/null | grep -oE 'Footprint: [0-9.]+ [MKG]B' | awk '{print $2, $3}'
    else
        awk '/^Pss:/ {printf "%.1f MB (PSS)", $2/1024}' "/proc/$1/smaps_rollup"
    fi
}
tabs=$("$ctx" list tabs | grep -c ' tab ')
echo "workload: 3 windows of ${ww}×${wh} pt, $tabs tabs, top running"
echo "CTX:        $(mem $appid)   (budget < 80 MB)"
echo "ctx server: $(mem $server)   (budget < 40 MB)"
echo "CTX CPU while top runs: $(ps -o %cpu= -p $appid)%"
