#!/bin/bash
# Run CTX on a virtual display against a throwaway ctx server, for developing and testing on
# Linux without a desktop. Everything lives under $CTX_HOME (default /tmp/ctx-dev).
#
#   scripts/dev-linux.sh start      Xvfb, a ctx server, and the app (built first)
#   scripts/dev-linux.sh shot F.png a screenshot of the whole display
#   scripts/dev-linux.sh stop       stop all three
#   scripts/dev-linux.sh ctx ARGS   run the ctx CLI against the test server
#
# Then drive it with xdotool (DISPLAY=:${CTX_DISPLAY:-99}): xdotool type, key, mousemove, click.
set -u
here=$(cd "$(dirname "$0")/.." && pwd)
export CTX_HOME=${CTX_HOME:-/tmp/ctx-dev}
export DISPLAY=:${CTX_DISPLAY:-99}
ctx=${CTX_BIN:-$here/../ctx/target/debug/ctx}
app=${CTX_APP:-$here/target/debug/CTX}
pids=$CTX_HOME/pids

stop() {
    if [ -f "$pids" ]; then
        while read -r p; do kill "$p" 2>/dev/null; done < "$pids"
        rm -f "$pids"
    fi
}

case "${1:-}" in
start)
    stop
    (cd "$here" && cargo build -q) || exit 1
    rm -rf "$CTX_HOME"; mkdir -p "$CTX_HOME/home"
    printf 'shell = "/bin/zsh"\n' > "$CTX_HOME/config.toml"
    Xvfb "$DISPLAY" -screen 0 ${CTX_SCREEN:-1600x1000x24} -nolisten tcp > "$CTX_HOME/xvfb.log" 2>&1 &
    echo $! >> "$pids"
    for _ in $(seq 50); do xdpyinfo -display "$DISPLAY" > /dev/null 2>&1 && break; sleep 0.1; done
    LANG=C.UTF-8 LC_ALL=C.UTF-8 HOME=$CTX_HOME/home "$ctx" server > "$CTX_HOME/server.log" 2>&1 &
    echo $! >> "$pids"
    for _ in $(seq 50); do [ -S "$CTX_HOME/run/server.sock" ] && break; sleep 0.1; done
    LANG=C.UTF-8 WINIT_X11_SCALE_FACTOR=${CTX_X11_SCALE:-2} CTX_BIN="$ctx" "$app" > "$CTX_HOME/app.log" 2>&1 &
    echo $! >> "$pids"
    sleep "${CTX_SETTLE:-2}"
    ;;
shot)
    import -display "$DISPLAY" -window root "${2:-/tmp/ctx-shot.png}"
    ;;
stop)
    stop
    ;;
ctx)
    shift
    LANG=C.UTF-8 "$ctx" "$@"
    ;;
*)
    sed -n 2,12p "$0"
    exit 2
    ;;
esac
