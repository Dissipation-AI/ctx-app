#!/bin/zsh
# Run CTX.app's self-test against a throwaway ctx server. Usage: tests/selftest.sh [path/to/CTX]
set -u
here=${0:A:h}/..
app=${1:-$here/target/debug/CTX}
ctx=${CTX_BIN:-$here/../ctx/target/debug/ctx}
export CTX_HOME=$(mktemp -d /tmp/ctx-app-selftest.XXXXXX)
$ctx server > $CTX_HOME/server.log 2>&1 &
server=$!
trap 'kill $server 2>/dev/null; rm -rf $CTX_HOME' EXIT
for i in {1..50}; do [[ -S $CTX_HOME/run/server.sock ]] && break; sleep 0.1; done
out=$CTX_HOME/selftest.out
CTX_APP_SELFTEST=1 $app > $out 2>&1 &
appid=$!
for i in {1..90}; do kill -0 $appid 2>/dev/null || break; sleep 1; done
if kill -0 $appid 2>/dev/null; then kill $appid; echo "FAIL the self-test didn't finish in 90 seconds"; cat $out; exit 1; fi
wait $appid; code=$?
cat $out
exit $code
