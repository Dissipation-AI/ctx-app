# membench: what a terminal costs the whole Mac

A terminal's own process is only part of its memory. `run.sh` also counts:

- **WindowServer's share.** The compositor keeps buffers for the windows it shows, and an app can
  make it keep far more than the window's pixels. That memory is in WindowServer, not in the app.
  It's measured by quitting the app and watching WindowServer shrink.
- **Per-tab launchers**: `login` for Ghostty and Terminal.app, `ctx-launch` for ctx.
- **Helper processes macOS starts for the app**: XPC services such as AutoFill's
  `com.apple.SafariPlatformSupport.Helper` or `SetStoreUpdateService`. They're found in the system
  log (runningboardd's "Resolved XPC Service … with host pid"), not guessed from what exits.
- **ctx's server**, for CTX.

Shells and the programs in the tabs are reported but left out of the total, since they're the same
in every terminal.

## The workload

The same in every terminal: one window (1496×938 points by default) with 3 Claude Code tabs, each
resuming a transcript with `--fork-session` (nothing is written to it), and 1 agy tab. Resuming
a real transcript fills the screen with real output and makes AppKit load the colours and fonts a
terminal really uses, which an empty prompt doesn't. `active` mode adds a tab printing 10 lines a
second in front, standing in for an agent's spinner or streaming output.

```sh
MEMBENCH_CLAUDE_SESSION=<session id> MEMBENCH_DIR=~/code/project scripts/membench/run.sh all active
scripts/membench/run.sh ctx idle
MEMBENCH_CTX_APP=target/release/CTX scripts/membench/run.sh ctx active   # a development build
scripts/membench/report.py --table target/membench/*/result.json
```

Ghostty and Terminal.app mustn't be running: a run ends by quitting the app. CTX runs against a
private server (`CTX_HOME`), so your tabs are untouched, but **quit your own CTX.app first**: while
anything in it animates, WindowServer's buffers stay in use and every run's share looks smaller
than it is. The script warns about this.

## Reading WindowServer

- WindowServer frees a ~110 MB block by itself when the screen is quiet and takes it back when
  something draws, so single readings jump between two levels. Every reading is 15 samples; the
  report gives min, max and median. Compare medians, and look at the minimums: an app that keeps
  that block in use has a high minimum while it runs.
- Measure with nothing else animating: other apps' windows move WindowServer too.
- `top -pid <WindowServer> -stats mem` reads its footprint without root. `footprint` needs root
  for it, and nothing short of a kernel debugger splits it per client, hence quit-and-compare.

- "WS share" is the drop when the app quits; "WS vs before" compares with the reading taken before
  the app started. They should agree. When they don't, something else changed meanwhile, or
  WindowServer kept the app's memory for a while after it quit (it did when CTX handed it CGImages).

## What it found

Results and every experiment so far (what worked and what didn't) are in
[../../docs/memory.md](../../docs/memory.md). Add new findings there.

## Pitfalls hit while building this

- `ctx show` makes the server start the *installed* CTX.app, which inherits `CTX_HOME`. A run that
  uses it measures two apps. run.sh picks the window's front tab through `app-state.json` instead.
- The CTX app and the ctx server must be built from matching sources. A mismatched pair connects,
  then the app shows "Lost the ctx server" in a small window, and you measure that window.
- A test binary needs to be called `CTX` (or the window lister can't find it) and needs
  `ctx-launch` next to the `ctx` it uses.
- Terminal.app makes one window per `.command` file; run.sh sets `AppleWindowTabbingMode=always`
  for Terminal during the run (and puts it back) so they become tabs, launches with
  `-ApplePersistenceIgnoreState YES` so old windows don't come back, and resizes the window over
  AppleScript (Automation permission).
- Ghostty's window size can't be set from AppleScript; the report shows the size it got.
