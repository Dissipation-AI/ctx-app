#!/bin/bash
# Build CTX.app, remove the installed one completely, and install the new build, so every run
# starts from a clean slate, including macOS privacy (TCC) permissions: the prompts come back.
#
#   scripts/install-mac.sh                 build (release), uninstall, reset TCC, install, launch,
#                                          reinstall ctx's Claude Code plugin if you use it
#   scripts/install-mac.sh --debug         a debug build
#   scripts/install-mac.sh --keep-tcc      leave privacy permissions as they are
#   scripts/install-mac.sh --keep-server   don't stop the running ctx server (keeps your tabs)
#   scripts/install-mac.sh --uninstall     only remove the installed CTX.app
#   scripts/install-mac.sh --dest DIR      install into DIR (default /Applications)
#
# Stopping the server ends the shells in every open tab (their output is kept, and the tabs can
# be reopened). The script asks first unless --yes is given.
#
# Needs the ctx checkout next to this one (../ctx) and Xcode's command-line tools.
set -euo pipefail

here=$(cd "$(dirname "$0")/.." && pwd)
dest=/Applications
profile=release
reset_tcc=1
stop_server=1
install=1
yes=0
while [ $# -gt 0 ]; do
    case "$1" in
        --debug) profile=debug ;;
        --keep-tcc) reset_tcc=0 ;;
        --keep-server) stop_server=0 ;;
        --uninstall) install=0 ;;
        --dest) dest=$2; shift ;;
        --yes|-y) yes=1 ;;
        -h|--help) sed -n 2,16p "$0"; exit 0 ;;
        *) echo "install-mac: unknown option $1" >&2; exit 2 ;;
    esac
    shift
done

[ "$(uname)" = Darwin ] || { echo "install-mac: this is for macOS" >&2; exit 1; }

app_id=ai.dissipation.ctx
label=com.ctx.server
uid=$(id -u)
step() { printf '\n==> %s\n' "$*"; }

# 1. Build first, so a failed build leaves the installed app alone.
if [ $install = 1 ]; then
    step "Building CTX.app ($profile)"
    if [ $profile = debug ]; then
        (cd "$here" && cargo xtask bundle --debug)
    else
        (cd "$here" && cargo xtask bundle)
    fi
    built=$here/target/CTX.app
    [ -d "$built" ] || { echo "install-mac: no $built after the build" >&2; exit 1; }
fi

# 2. Quit the running app (it saves its state continuously; a plain TERM is enough, and it
#    needs no Automation permission, unlike asking it to quit through AppleScript).
if pgrep -x CTX > /dev/null; then
    step "Quitting CTX"
    pkill -TERM -x CTX || true
    for _ in $(seq 50); do pgrep -x CTX > /dev/null || break; sleep 0.1; done
    pkill -KILL -x CTX 2> /dev/null || true
fi

# 3. Stop and unregister the ctx server (the old copy's LaunchAgent).
if [ $stop_server = 1 ] && launchctl print "gui/$uid/$label" > /dev/null 2>&1; then
    if [ $yes = 0 ]; then
        printf 'Stopping the ctx server ends the shells in your open tabs (their output is kept). Continue? [y/N] '
        read -r answer
        case "$answer" in y|Y|yes) ;; *) echo "Leaving the server running (use --keep-server to skip this question)."; stop_server=0 ;; esac
    fi
    if [ $stop_server = 1 ]; then
        step "Stopping the ctx server"
        launchctl bootout "gui/$uid/$label" 2> /dev/null || true
        rm -f "$HOME/Library/LaunchAgents/$label.plist"
    fi
fi

# 4. Remove the installed app and the command-line links that point into it.
old=""
for d in "$dest/CTX.app" /Applications/CTX.app "$HOME/Applications/CTX.app"; do
    [ -d "$d" ] && old="$old $d"
done
for d in $old; do
    step "Removing $d"
    rm -rf "$d"
done
for link in /usr/local/bin/ctx /usr/local/bin/bro "$HOME/.local/bin/ctx" "$HOME/.local/bin/bro"; do
    if [ -L "$link" ] && readlink "$link" | grep -q 'CTX.app/'; then
        rm -f "$link" && echo "removed $link"
    fi
done
# The first-run state, so the install offer and the LaunchAgent registration happen again.
state="$HOME/Library/Application Support/ctx/app-state.json"
if [ -f "$state" ] && command -v python3 > /dev/null; then
    python3 - "$state" <<'PY'
import json, sys
p = sys.argv[1]
s = json.load(open(p))
s["set_up"] = False
json.dump(s, open(p, "w"), indent=2)
PY
fi

# 5. Reset privacy permissions for the app, the tab launcher and scoped launchers, so macOS
#    asks again (CTX.app itself asks for none; the prompts come from the programs in tabs).
if [ $reset_tcc = 1 ]; then
    step "Resetting privacy permissions"
    ids="$app_id ctx-launch"
    ctx_bin=$(command -v ctx || true)
    [ -z "$ctx_bin" ] && [ -x "$here/../ctx/target/release/ctx" ] && ctx_bin=$here/../ctx/target/release/ctx
    if [ -n "$ctx_bin" ]; then
        launchers=$("$ctx_bin" list launchers as json 2> /dev/null | python3 -c '
import json, sys
try:
    for l in json.load(sys.stdin).get("launchers", []):
        print("com.ctx.launcher." + l["name"])
except Exception:
    pass' || true)
        ids="$ids $launchers"
    fi
    for id in $ids; do
        tccutil reset All "$id" > /dev/null 2>&1 && echo "reset $id" || echo "nothing to reset for $id"
    done
fi

[ $install = 1 ] || { step "Uninstalled"; exit 0; }

# 6. Install and launch.
step "Installing into $dest"
mkdir -p "$dest"
ditto "$built" "$dest/CTX.app"
xattr -dr com.apple.quarantine "$dest/CTX.app" 2> /dev/null || true
codesign --verify --deep --strict "$dest/CTX.app"
step "Launching"
open "$dest/CTX.app"
echo "Installed $dest/CTX.app. It registers the ctx server and offers the command-line tools on first run."

# 7. Claude Code keeps a copy of ctx's plugin per version, so a changed plugin only reaches new
#    sessions after a reinstall.
if grep -q '"ctx@ctx"' "$HOME/.claude/plugins/installed_plugins.json" 2> /dev/null; then
    step "Reinstalling ctx's Claude Code plugin"
    ctx_bin=$dest/CTX.app/Contents/Resources/bin/ctx
    for _ in $(seq 100); do "$ctx_bin" status > /dev/null 2>&1 && break; sleep 0.1; done
    "$ctx_bin" remove hooks for claude && "$ctx_bin" install hooks for claude
fi
