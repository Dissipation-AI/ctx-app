#!/bin/bash
# What a terminal costs the whole Mac, not just its own process: the app, the per-tab launchers,
# the system helper processes macOS starts for it, and its share of WindowServer (measured by
# quitting the app and watching WindowServer shrink). Every terminal gets the same workload: one
# window, 3 Claude Code tabs resumed from a transcript and 1 agy tab, plus a 10 Hz output loop in
# front in "active" mode. See README.md for how to read the numbers.
#
#   scripts/membench/run.sh ctx [idle|active]        a private ctx server and a CTX.app process
#   scripts/membench/run.sh ghostty [idle|active]    Ghostty (must not be running)
#   scripts/membench/run.sh terminal [idle|active]   Terminal.app (must not be running)
#   scripts/membench/run.sh all [idle|active]        the three in turn, then a table
#   scripts/membench/report.py --table target/membench/*/result.json
#
# Environment:
#   MEMBENCH_DIR             folder the Claude tabs start in (default: the current folder)
#   MEMBENCH_CLAUDE_SESSION  transcript to resume, forked so nothing is written to it
#                            (default: claude --continue --fork-session)
#   MEMBENCH_AGY             the 4th tab's command (default: agy --sandbox --continue, or Claude)
#   MEMBENCH_CTX_APP         CTX binary (default /Applications/CTX.app/Contents/MacOS/CTX)
#   MEMBENCH_CTX_BIN         ctx for the private server; must speak the same protocol as the app
#                            (default ../ctx/target/release/ctx)
#   MEMBENCH_SIZE            window size in points (default "1496 938")
#   MEMBENCH_SETTLE          seconds between starting and measuring (default 50)
#   MEMBENCH_OUT             results folder (default target/membench)
set -u
here=$(cd "$(dirname "$0")" && pwd)
repo=$(cd "$here/../.." && pwd)
which=${1:-}
mode=${2:-active}
case "$which:$mode" in
    ghostty:idle | ghostty:active | terminal:idle | terminal:active | ctx:idle | ctx:active | all:*) ;;
    *) sed -n 2,26p "$0"; exit 2 ;;
esac
[ "$(uname)" = Darwin ] || { echo "membench: macOS only" >&2; exit 1; }
if [ "$which" = all ]; then
    results=()
    for t in ctx ghostty terminal; do
        "$0" "$t" "$mode" || exit 1
        results+=("$(ls -td "${MEMBENCH_OUT:-$repo/target/membench}"/*-"$t-$mode" | head -1)/result.json")
    done
    echo
    python3 "$here/report.py" --table "${results[@]}"
    exit
fi

die() { echo "membench: $*" >&2; exit 1; }
read -r W H <<< "${MEMBENCH_SIZE:-1496 938}"
settle=${MEMBENCH_SETTLE:-50}
dir=${MEMBENCH_DIR:-$PWD}
out=${MEMBENCH_OUT:-$repo/target/membench}/$(date +%Y%m%d-%H%M%S)-$which-$mode
bin=$repo/target/membench-bin
mkdir -p "$out/tabs" "$bin"
[ "$bin/windows" -nt "$here/windows.swift" ] || swiftc -O "$here/windows.swift" -o "$bin/windows" || exit 1

# WindowServer's footprint over 15 one-second samples: "min max median" in MB.
ws() {
    top -l 15 -s 1 -pid "$(pgrep -x WindowServer)" -stats mem |
        awk '/^[0-9.]+[KMG][+-]?$/ { v = $1; sub(/[+-]$/, "", v); u = substr(v, length(v)); n = substr(v, 1, length(v) - 1)
             if (u == "G") n *= 1024; if (u == "K") n /= 1024; print n }' |
        sort -n | awk '{ a[NR] = $1 } END { print a[1], a[NR], a[int((NR + 1) / 2)] }'
}

# The workload: one script per tab, so every terminal runs exactly the same thing.
if [ -n "${MEMBENCH_CLAUDE_SESSION:-}" ]; then
    claude_cmd="claude --resume $MEMBENCH_CLAUDE_SESSION --fork-session"
else
    claude_cmd="claude --continue --fork-session"
fi
if [ -n "${MEMBENCH_AGY:-}" ]; then
    agy_cmd=$MEMBENCH_AGY
elif command -v agy > /dev/null; then
    agy_cmd="agy --sandbox --continue"
else
    agy_cmd=$claude_cmd
fi
tab() { printf '#!/bin/zsh -l\ncd %q && %s\nexec zsh -l\n' "$dir" "$2" > "$out/tabs/$1.command"; }
tab c1 "$claude_cmd"
tab c2 "$claude_cmd"
tab c3 "$claude_cmd"
tab a1 "$agy_cmd"
printf '#!/bin/zsh\nwhile :; do date +%%T.%%N; sleep 0.1; done\n' > "$out/tabs/anim.command"
chmod +x "$out"/tabs/*.command
tabs=(c1 c2 c3 a1)
[ "$mode" = active ] && tabs+=(anim)

# Other terminals on screen skew WindowServer: anything animating keeps its spare block in use.
for p in $(pgrep -x 'ghostty|Terminal|CTX'); do
    echo "membench: warning: $(ps -o comm= -p "$p" | xargs basename) (pid $p) is running; if anything in it" \
        "is animating, WindowServer's numbers for this run are skewed. Quit it for clean numbers." >&2
done

server=-
# Set by start_terminal and start_ctx; read by their cleanup when the script exits.
prev=
ctx=
cleanup() { :; }
trap 'cleanup' EXIT

start_ghostty() {
    pgrep -xq ghostty && die "quit Ghostty first: the run ends by quitting it"
    local files="" t
    for t in "${tabs[@]}"; do files+="${files:+, }\"$out/tabs/$t.command\""; done
    open -a Ghostty || die "can't start Ghostty"
    for _ in $(seq 50); do app=$(pgrep -x ghostty) && break; sleep 0.2; done
    sleep 3
    osascript > /dev/null <<APPLESCRIPT || die "can't script Ghostty (Automation permission?)"
tell application "Ghostty"
    set old to id of every window
    set w to missing value
    repeat with f in {$files}
        set c to new surface configuration
        set initial input of c to (f as text) & linefeed
        if w is missing value then
            set w to new window with configuration c
        else
            new tab in w with configuration c
        end if
    end repeat
    repeat with i in old
        close window (first window whose id is (contents of i))
    end repeat
    activate
end tell
APPLESCRIPT
}

start_terminal() {
    pgrep -xq Terminal && die "quit Terminal first: the run ends by quitting it"
    # Each .command opens a window; with tabbing set to "always" they become tabs of one window.
    prev=$(defaults read com.apple.Terminal AppleWindowTabbingMode 2> /dev/null) || prev=
    cleanup() {
        if [ -n "$prev" ]; then defaults write com.apple.Terminal AppleWindowTabbingMode "$prev"
        else defaults delete com.apple.Terminal AppleWindowTabbingMode 2> /dev/null; fi
    }
    defaults write com.apple.Terminal AppleWindowTabbingMode always
    open -a Terminal "$out/tabs/${tabs[0]}.command" --args -ApplePersistenceIgnoreState YES || die "can't start Terminal"
    for _ in $(seq 50); do app=$(pgrep -x Terminal) && break; sleep 0.2; done
    sleep 3
    for t in "${tabs[@]:1}"; do open -a Terminal "$out/tabs/$t.command"; sleep 1.5; done
    # Needs the Automation permission for Terminal; without it the window keeps its default size.
    osascript -e "tell application \"Terminal\" to set bounds of front window to {100, 60, $((100 + W)), $((60 + H))}" \
        > /dev/null 2>&1 &
    local p=$!
    for _ in $(seq 40); do kill -0 $p 2> /dev/null || break; sleep 0.2; done
    kill $p 2> /dev/null
    local n
    n=$(pgrep -P "$app" | wc -l | tr -d ' ')
    [ "$n" = "${#tabs[@]}" ] || echo "membench: warning: Terminal has $n tabs, expected ${#tabs[@]}" >&2
}

start_ctx() {
    ctx=${MEMBENCH_CTX_BIN:-$repo/../ctx/target/release/ctx}
    local ctx_app=${MEMBENCH_CTX_APP:-/Applications/CTX.app/Contents/MacOS/CTX}
    [ -x "$ctx" ] || die "no ctx at $ctx (set MEMBENCH_CTX_BIN)"
    [ -x "$(dirname "$ctx")/ctx-launch" ] || die "no ctx-launch next to $ctx (cargo build --release --bins in ../ctx)"
    export CTX_HOME=$out/ctx-home
    mkdir -p "$CTX_HOME/data"
    "$ctx" server > "$CTX_HOME/server.log" 2>&1 &
    server=$!
    cleanup() {
        "$ctx" close window 1 without asking > /dev/null 2>&1
        kill "$server" 2> /dev/null
    }
    for _ in $(seq 50); do "$ctx" list tabs > /dev/null 2>&1 && break; sleep 0.1; done
    kill -0 "$server" 2> /dev/null || die "ctx server didn't start: $(cat "$CTX_HOME/server.log")"
    for t in "${tabs[@]}"; do
        "$ctx" open tab named "$t" > /dev/null || die "can't open a tab"
        "$ctx" run in "$t" "$out/tabs/$t.command" without waiting > /dev/null 2>&1
    done
    # The window's size and front tab come from the app's saved state. (`ctx show` would have the
    # server start the installed CTX.app as well, so it isn't used.)
    cat > "$CTX_HOME/data/app-state.json" << JSON
{"windows": {"1": {"frame": [100, 100, $W, $H], "selected": ${#tabs[@]}}}, "set_up": true}
JSON
    "$ctx_app" > "$CTX_HOME/app.log" 2>&1 &
    app=$!
    disown "$app"
}

echo "membench: $which $mode -> $out"
ws_pre=$(ws)
before=" $(ps -axo pid= | tr -d ' ' | tr '\n' ' ')"
since=$(date '+%Y-%m-%d %H:%M:%S')
"start_$which"
sleep "$settle"
kill -0 "$app" 2> /dev/null || die "the app exited early"
window=$("$bin/windows" | awk -F'\t' -v p="$app" '$1 == p { print $2 "x" $3; exit }')
[ -n "$window" ] || die "no window on screen for pid $app"
[ "$window" = "${W}x${H}" ] || echo "membench: warning: window is $window, not ${W}x${H}" >&2
python3 "$here/snap.py" "$out/with.json" 10
ws_with=$(ws)
footprint -p "$app" > "$out/footprint.txt" 2>&1
# Services macOS started for the app (e.g. AutoFill's SafariPlatformSupport.Helper): processes
# that logged a connection from it, weren't running before it, and exit with it (checked below).
peers=$(log show --start "$since" --style compact --predicate "eventMessage CONTAINS \"peer[$app]\"" 2> /dev/null |
    sed -nE 's/^[^ ]+ [^ ]+ +[A-Za-z]+ +[^[]+\[([0-9]+):.*/\1/p' | sort -u)
kill -TERM "$app"
for _ in $(seq 75); do kill -0 "$app" 2> /dev/null || break; sleep 0.2; done
sleep 10
xpc=
for p in $peers; do
    case "$before" in *" $p "*) continue ;; esac
    kill -0 "$p" 2> /dev/null || xpc+="$p "
done
ws_after=$(ws)
python3 "$here/report.py" "$out/result.json" "$which-$mode" "$out/with.json" "$app" "$server" \
    "$ws_pre" "$ws_with" "$ws_after" "$xpc" "$window"
