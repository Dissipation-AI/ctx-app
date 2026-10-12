#!/bin/zsh
# Memory budget from SPEC.md § Acceptance tests: 3 windows, 15 tabs, `top` running.
# Uses a throwaway ctx server (CTX_HOME), so your own tabs are untouched.
# Usage: scripts/budgets.sh [path/to/CTX] [window width height in points]
set -u
here=${0:A:h}/..
app=${1:-$here/target/CTX.app/Contents/MacOS/CTX}
ww=${2:-900}; wh=${3:-600}
ctx=${CTX_BIN:-$here/../ctx/target/release/ctx}
export CTX_HOME=$(mktemp -d /tmp/ctx-app-budget.XXXXXX)
$ctx server > $CTX_HOME/server.log 2>&1 &
server=$!
trap 'kill $server $appid 2>/dev/null; rm -rf $CTX_HOME' EXIT
for i in {1..50}; do [[ -S $CTX_HOME/run/server.sock ]] && break; sleep 0.1; done
for i in 1 2 3; do $ctx open window > /dev/null; done
for w in 1 2 3; do for t in 1 2 3 4; do $ctx open tab in window $w > /dev/null; done; done
mkdir -p $CTX_HOME/data
cat > $CTX_HOME/data/app-state.json <<JSON
{"windows": {"1": {"frame": [80, 80, $ww, $wh]}, "2": {"frame": [140, 140, $ww, $wh]}, "3": {"frame": [200, 200, $ww, $wh]}}, "set_up": true}
JSON
$app > $CTX_HOME/app.log 2>&1 &
appid=$!
sleep 3
first=$($ctx list tabs as json | python3 -c 'import json,sys; print(json.load(sys.stdin)["windows"][0]["tabs"][0]["number"])')
$ctx run in tab $first "top" within 1 second > /dev/null 2>&1
sleep ${SETTLE:-15}
fp() { footprint $1 2>/dev/null | grep -oE 'Footprint: [0-9.]+ [MKG]B' | awk '{print $2, $3}'; }
tabs=$($ctx list tabs | grep -c ' tab ')
echo "workload: 3 windows of ${ww}×${wh} pt, $tabs tabs, top running"
echo "CTX.app:    $(fp $appid)   (budget < 80 MB)"
echo "ctx server: $(fp $server)   (budget < 40 MB)"
echo "CTX.app CPU while top runs: $(ps -o %cpu= -p $appid)%"
