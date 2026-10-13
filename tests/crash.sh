#!/bin/bash
# Acceptance: killing CTX with `kill -9` and relaunching restores every window and tab (with
# scrollback: the server holds it). Uses a throwaway ctx server; on Linux without a display it
# runs on a virtual one. Usage: tests/crash.sh [path/to/CTX]
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
app=${1:-$here/target/release/CTX}
ctx=${CTX_BIN:-$here/../ctx/target/release/ctx}
[ -x "$ctx" ] || ctx=$here/../ctx/target/debug/ctx
CTX_HOME=$(mktemp -d /tmp/ctx-app-crash.XXXXXX)
export CTX_HOME
dump=$CTX_HOME/dump; mkdir -p "$dump" "$CTX_HOME/home"
printf 'shell = "/bin/zsh"\n' > "$CTX_HOME/config.toml"
pids=()
appid=0
cleanup() { kill "${pids[@]}" "$appid" 2>/dev/null; [ -n "${CTX_KEEP:-}" ] || rm -rf "$CTX_HOME"; }
trap cleanup EXIT
if [ "$(uname)" != Darwin ] && [ -z "${DISPLAY:-}" ]; then
    export DISPLAY=:$((80 + RANDOM % 9))
    Xvfb "$DISPLAY" -screen 0 1600x1000x24 -nolisten tcp > "$CTX_HOME/xvfb.log" 2>&1 &
    pids+=($!)
    for _ in $(seq 50); do xdpyinfo -display "$DISPLAY" > /dev/null 2>&1 && break; sleep 0.1; done
fi
LANG=C.UTF-8 HOME=$CTX_HOME/home "$ctx" server > "$CTX_HOME/server.log" 2>&1 &
pids+=($!)
for _ in $(seq 50); do [ -S "$CTX_HOME/run/server.sock" ] && break; sleep 0.1; done
json() { python3 -c "import json,sys; print($1)"; }
CTX_APP_DUMP=$dump CTX_BIN="$ctx" "$app" > "$CTX_HOME/app1.log" 2>&1 &
appid=$!
sleep 3
w=$("$ctx" list tabs as json | json 'json.load(sys.stdin)["windows"][0]["number"]')
for _ in 1 2; do "$ctx" open tab in window "$w" > /dev/null; done
t=$("$ctx" list tabs as json | json 'json.load(sys.stdin)["windows"][0]["tabs"][0]["number"]')
"$ctx" run in tab "$t" "seq 1 3000" > /dev/null
sleep 1
state=$CTX_HOME/data/app-state.json
before=$(python3 -c 'import json,sys; print(sorted(json.load(open(sys.argv[1]))["windows"]))' "$state")
tabs_before=$("$ctx" list tabs | grep -c ' tab ')
kill -9 $appid; wait $appid 2>/dev/null
rm -f "$dump"/*
CTX_APP_DUMP=$dump CTX_BIN="$ctx" "$app" > "$CTX_HOME/app2.log" 2>&1 &
appid=$!
sleep 3
after=$(python3 -c 'import json,sys; print(sorted(json.load(open(sys.argv[1]))["windows"]))' "$state")
tabs_after=$("$ctx" list tabs | grep -c ' tab ')
last=$("$ctx" read last 2 lines in tab "$t" | head -1)
fail=0
[ "$before" == "$after" ] && echo "ok   the same windows are shown after kill -9 ($after)" || { echo "FAIL windows $before → $after"; fail=1; }
[ "$tabs_before" == "$tabs_after" ] && echo "ok   all $tabs_after tabs are still open" || { echo "FAIL tabs $tabs_before → $tabs_after"; fail=1; }
[ -f "$dump/window-$w.png" ] && echo "ok   window $w was drawn again" || { echo "FAIL window $w wasn't drawn"; fail=1; }
[ "$last" == 3000 ] && echo "ok   scrollback kept (last line $last)" || { echo "FAIL last line is '$last'"; fail=1; }
exit $fail
