# CTX.app

The native macOS window for [ctx](../ctx) (it also builds on Linux, for development and testing). It holds no shells and no scrollback of its own: it
shows what the `ctx` server has and sends it your keys. Anything you do here, an agent can do with
the `ctx` command, and the app shows it at once. See [SPEC.md](SPEC.md).

## Build and run

Needs the ctx checkout next to this one (`../ctx`, which needs Zig; see its README) and Rust 1.99.

```sh
cargo build --release          # target/release/CTX (run it directly while developing)
cargo xtask bundle             # target/CTX.app with ctx, ctx-launch and bro inside, signed ad hoc
CTX_SIGN_IDENTITY="Developer ID Application: …" CTX_NOTARY_PROFILE=<notarytool profile> cargo xtask bundle
```

A development build finds `ctx` next to it (`../ctx/target/…`) and starts the server through
launchd the same way the command line does. The bundled app registers the server with
`SMAppService` and offers to link `ctx` and `bro` into your PATH the first time it runs.

To replace an installed copy with a fresh build, and have macOS ask its privacy questions again:

```sh
scripts/install-mac.sh          # build, quit CTX, stop the server (asks), remove the old app,
                                # reset TCC for CTX, ctx-launch and launchers, install, launch
scripts/install-mac.sh --help   # --debug, --keep-tcc, --keep-server, --uninstall, --dest, --yes
```

Set `CTX_HOME=/some/folder` to try it against a separate server (`ctx server` with the same
`CTX_HOME`) without touching your tabs, launchd or PATH.

### Linux

Needs the X11 and fontconfig development packages, and Xvfb, xdotool and ImageMagick for the
harness. `cargo build` makes `target/debug/CTX`; `scripts/dev-linux.sh start` runs it on a virtual
display against a throwaway server (`shot`, `ctx …`, `stop`; drive it with `xdotool`). Shortcuts are
Ctrl+Shift where macOS uses ⌘.

## Tests

```sh
cargo test                     # unit tests and reference screens (needs ../ctx built)
tests/selftest.sh              # the app drives itself with real events (Xvfb on Linux)
tests/crash.sh                 # kill -9 and relaunch restores windows, tabs and scrollback
scripts/budgets.sh             # memory: 3 windows, 15 tabs, top running
```

`CTX_APP_DUMP=<folder>` saves every window's pixels (and tab bar) as PNGs after each draw.
`CTX_APP_INSPECT=<file>` writes the app's state as JSON after each draw. `CTX --render <tab> out.png`
draws one tab without a window. All of these run on Linux too.

## Results (2026-10-11, M4 Pro, macOS 27, release build)

| Check | Result |
|---|---|
| 3 windows (900×600 pt), 15 tabs, `top` running | **app 49 MB** (budget < 80), **server 4.6 MB** (budget < 40), app CPU ~0% |
| Same with 3 near-full-screen windows (1700×1050 pt) | app 88 MB: over budget; the three window pixel buffers alone are ~85 MB |
| Keystroke to pixels (key → server → program echo → drawn), `cat` | median 0.5–1.3 ms, 95th percentile ~2 ms (plus the compositor's next frame) |
| Self-test (typing, IME path, selection, copy/paste, find, scrollback, ⌘T/⌘W, tiles, rename, History/Reopen, badges, panels, font size) | 21/21 pass |
| Reference screens (colours, emoji/CJK, box drawing, Ink-style redraw, vim) | pass |
| `kill -9` and relaunch | every window and tab back, scrollback kept |

## Results on Linux (2026-10-13, Xvfb at 2× scale, release build)

| Check | Result |
|---|---|
| 3 windows (900×600 pt), 15 tabs, `top` running | **app 48 MB PSS** (budget < 80), **server 7 MB**, app CPU ~3% (software presenting) |
| Keystroke to pixels | median 2.0 ms |
| Self-test (typing, arrow keys before typing, copy/paste, find, scrollback, the + / History / Activity buttons, tab clicks, tiles, rename, per-tab theme, Reopen, badges, Find All, Overview, drag out to a new window, window drag area, latency) | 30/30 pass |
| Reference screens (`tests/reference/linux`) | pass |
| `kill -9` and relaunch | 4/4 checks pass |

Windows have no window-system title bar on either platform: the tab bar is the top of the window,
with red/yellow/green buttons CTX draws itself (on macOS the system's title bar cost ~4 MB). Tried
on a Mac; on Linux the undecorated window, its drawn buttons and resizing from its edges haven't
been built or run yet and need a pass. `tabs_in_titlebar = false` brings back the window system's
title bar with the tabs below it.

## Not done yet

- Side-by-side comparison with Ghostty, and Claude Code, htop and lazygit by eye: the screens here
  were checked against references this app drew, not against Ghostty.
- Terminal.app's footprint for the same workload (the "< 25% of Terminal.app" check) wasn't
  measured: driving Terminal.app needs an Automation permission prompt.
- Manual pass with a person at the keyboard: dragging tabs between windows and out to a new window,
  tile divider dragging, real IME input (the self-test covers the text-input path, not a Japanese
  input method), dictation, the first-run install flow on a clean account.
- Developer ID signing and notarization (the xtask does it given an identity; none was available).
- VoiceOver.
- A faint dot under some emoji sequences (🏳️‍🌈, 👨‍👩‍👧).
