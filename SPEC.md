# CTX.app — spec

The native macOS window for CTX. It should feel like Terminal.app or Ghostty: fast, crisp, native
shortcuts. It holds **no shells and no scrollback of its own**. It is a viewer and keyboard for the
`ctx` server (see [../ctx/SPEC.md](../ctx/SPEC.md)). Anything you can do in the app, an agent can do
with the `ctx` CLI, and the app reflects it immediately.

It also builds and runs on Linux (X11). That build exists so the whole app can be run, inspected,
screenshotted and driven with synthetic input on a machine without macOS; it draws the same
pixels from the same code, and only the window, input and font plumbing differ (see Stack).

## Goals

1. **Low memory:** < 80 MB with 3 windows and 15 tabs. Hidden tabs cost ~nothing.
2. **Feels native:** Terminal.app keyboard shortcuts, native text input (accents, emoji picker,
   dictation, voice tools like Handy), smooth trackpad scrolling, system light/dark appearance.
3. **Fast:** keystroke-to-pixel latency at or below Ghostty's; 120 Hz on ProMotion displays; no
   flicker in Ink/React TUIs.
4. **Honest about agents:** you can always see when something other than you touched a tab.

## Non-goals

- Panes, split shells, or a tmux-style layout language.
- Settings UI beyond the basics (v1). Config is a file.
- Plugins, AI chat sidebar, or any built-in LLM. (`bro` handles AI assistance at the prompt.)

## Windows and tabs

- One CTX.app window per `ctx` window that you use. Windows you open (⌘N, `ctx open window` from
  your own terminal) appear; windows agents make (the **Agents** window) don't pop up. They're
  listed in the Window menu, and `ctx show "<tab>"` brings one forward. Closing an app window closes
  the `ctx` window (with the usual "processes are running" confirmation). The app does nothing for
  a window nobody can see (covered, minimised, on another Space).
- **The tab bar is the title bar** (`tabs_in_titlebar = true`, the default): on macOS the
  window's title bar is see-through and covered by the tab bar, and the system's buttons (SwiftUI
  views, ~3 MB) are never created (a borderless window cost ~130 MB in WindowServer; see
  [docs/memory.md](docs/memory.md)); on Linux the window is undecorated. The app draws the red/yellow/green
  buttons as three circles (grey when the window isn't focused, still there in full screen, ⌥-click
  on green zooms), and on Linux the window resizes from a band along its edges. Tabs sit next to
  those buttons, notebook style, rounded on top, with the selected tab flowing into the terminal
  below it. Each tab has two lines: its label in a large, uniform font on top, its folder smaller
  and dimmer underneath, and a status dot (idle, busy, agent working, agent waiting for you; agents
  are recognised by program name: claude, codex, gemini, aider, …). When tabs don't fit, the bar
  scrolls, with chevrons at the ends. At the right end are buttons for **+**, **History** and
  **Activity**. All of it is drawn in the window's own pixel buffer: no native controls.
- **+ (new tab) menu:** besides a plain new tab, it offers *starters* the server works out from
  history: the first commands people ran in tabs, with their folder ("+ jupyter lab in
  ~/code/nb", "+ npm run dev in ~/code/site"). Choosing one opens a tab in that folder with the
  command typed at the prompt but **not run**.
- **Per-tab themes:** a tab's context menu has a theme picker with swatches. The tinted built-in
  themes (`midnight`, `forest`, `plum`, `ember`, `ocean`, `graphite`) have backgrounds that are
  easy to tell apart, and the tab carries its colour in the bar.
- **Overview** (⌘⇧O): every window, its tabs and each tab's processes as a horizontal tree
  (window → tab → process), with each tab's status and agent. Drag a tab to another window in the
  tree to move it.
- A tab bar like Terminal.app/Ghostty. Drag to reorder, drag to another window to move, drag out
  to make a new window. All of these are `ctx` operations (`move tab …`, including
  `ctx move "<tab>" to new window`), so the CLI stays in sync.
- **Side-by-side tabs (GUI only):** drag a tab onto the left/right/top/bottom edge of the content
  area to show it next to the current one. This produces *tiles* within the window, each showing
  exactly one tab. Rules:
  - Tiles are purely a view. The `ctx` server doesn't know about them; it only knows the window's
    ordered tab list. `ctx` output never interleaves tabs.
  - A selection lives inside one tab. You can never select or copy across a tile boundary, so
    copied text never mixes lines from two tabs.
  - Tile layout is saved per window in the app's state file and restored on relaunch.
  - Closing a tab removes its tile; the neighbour takes the space.
- The app reconnects to all existing `ctx` windows on launch (they survive an app crash).
- **Tab labels** show the tab's name, if it has one, and its current folder, e.g.
  `Dev server — ~/code/site`; when space is tight the folder is shortened from the left and the
  name stays whole. A tab without a name
  shows its folder only. A program's terminal title (OSC 0/2) appears in the tooltip and in the
  window title, never in place of the name. Double-click a tab to rename it (same character rules
  as `ctx rename`).
- Tabs opened by an agent carry a small marker with the agent's name (e.g. `claude`) and land in
  the **Agents** window unless the agent picked a window. You can watch that window live, or open
  it later to audit what was run.
- **Closed tabs:** ⌘⇧T reopens the most recently closed tab (`ctx reopen`). The *History* menu
  (⌘Y) is a searchable list of saved tabs, filterable by folder, name, who opened it, and date.
  Selecting one shows its saved commands and output, with colours, read-only. A *Reopen* button
  brings it back. This is the human view of `ctx list closed tabs` / `ctx find`.
- On launch after a reboot or crash, a bar offers "Reopen 7 tabs from last time"
  (`reopen_on_launch`).
- **Each viewer chooses its own look.** The app is the viewer, the CLI the controller, the server
  the model. A window's colours (its theme; View ▸ Theme, per window) are sent to the server, which
  applies them to that window's tabs and answers programs' colour queries with them. Font size is
  per tab (⌘+ / ⌘-). A terminal attached with `ctx attach` keeps its own colours and font. A
  program has one grid size, so the viewer that last resized the tab sets its rows and columns.

## Rendering

The memory goes to window pixel buffers, not text. A window's on-screen image is a full grid of
pixels whatever it shows (a 1700×1050 pt window on a Retina display is ~28 MB per buffer), and
on Apple Silicon the GPU's memory is the same RAM, so the buffer count is the main lever.

- **Toolkit:** AppKit via `objc2` bindings for windows, the menu bar, text input and alerts. No
  Electron, no web view. Everything inside a window (tab bar, find and paste strips, panels,
  context menus) is drawn as rectangles and text into the window's one pixel buffer, so it costs no
  extra buffers and looks the same on both platforms. On Linux the window comes from `winit`, the
  buffer is presented with `softbuffer`, and dialogs are drawn too.
- **One text surface per window, whatever the tab count.** Switching tabs doesn't swap in a
  different view. The window's single surface just starts drawing another tab's text. The app
  asks the server for that tab's visible rows (a few KB), drops the previous tab's rows, and
  redraws. Tabs that aren't visible cost the app zero bytes; only the server holds their text.
- **Drawing (chosen by measurement, 2026-10-11):** Core Text glyphs drawn on the CPU into **one
  IOSurface per window**, handed to the window's layer as its contents. That is exactly one pixel
  buffer per window, and it stays one buffer while output streams. Side-by-side tiles are regions
  of the same surface. Measured on an M4 Pro (footprint, windows redrawing at 60 fps):

  | | 1 window 1700×1050 pt | 3 windows 1700×1050 pt | 3 windows 900×600 pt |
  |---|---|---|---|
  | Metal, 2 drawables | 200 MB | 327 MB | – |
  | AppKit layer-backed view (`drawRect`) | 284 MB | 555 MB | – |
  | **IOSurface + Core Text (chosen)** | **44 MB** | **101 MB** | **43 MB** |

  A full repaint of every cell takes 2–4 ms; normal updates redraw only changed rows.

  These are the app's own footprints. WindowServer's share has to be counted too
  (`scripts/membench`, 2026-10-14): with a borderless window WindowServer kept ~130–160 MB per
  1496×938 window, whichever way the pixels were handed over (IOSurface, CGImage, or a
  CAMetalLayer that also cost the app 163 MB). A titled window with a hidden, see-through title bar
  brought it to about zero. Handing over a CGImage made the app look 20 MB smaller only because
  the copy-on-write pages it shares aren't charged to any process; the IOSurface stays. All
  experiments: [docs/memory.md](docs/memory.md).
  - Draw only when the server sends a change or the user scrolls/selects. No render loop. Cursor
    blink uses a timer that redraws just the cursor's row, and only runs while needed.
  - When a window is minimised, fully covered or on another Space, its surface is released
    (`NSWindow.occlusionState`) and its tabs are unsubscribed.
  - Colours are sRGB: the surface is tagged sRGB, so a theme's hex values appear exactly as
    written (as in Ghostty), at no memory cost.
  - Colours: full truecolor plus the 256/16-colour palettes, bold/italic/underline/strikethrough,
    reverse video, dim. Each cell is drawn from the server's `(text, style)` runs.
  - **Box drawing (U+2500–257F) and block elements (U+2580–259F)** are drawn as shapes filling the
    cell, so lines join across rows whatever the font's line height (light, heavy, double, dashed,
    rounded corners, diagonals, eighths, shades, quadrants).
- **The grid is the emulator's.** The server sends each run with the terminal's cells (where each
  cell's text starts and its width, from ghostty-vt); the app places text by that grid and never
  guesses widths, so wide characters, emoji and combining marks land where the emulator put them.
- **Glyphs:** shaped and rasterised with Core Text (on Linux: `swash`, fonts found with
  `fontdb`/fontconfig, a bounded glyph cache), so text looks like Terminal.app (SF Mono
  default, emoji via Apple Color Emoji, fallback fonts for symbols and CJK, shaped clusters for
  emoji sequences and combining marks). Text-style symbols (✔ ☐ ★) take the text colour; only
  emoji-presentation characters use the emoji font. Glyph lookups are cached per font set
  (bounded); Core Graphics caches the rasterised glyphs, shared by the whole app.
- **Cell data:** the app keeps only the rows of tabs on screen. Scrolling back fetches scrollback
  lines from the server on demand, with a read-ahead cache of a few thousand lines.

## Input

- `NSTextInputClient` for full IME, dead keys, emoji picker, and dictation/voice tools that insert
  text.
- Keys are forwarded as key events to the server, which encodes them (xterm or kitty keyboard
  protocol, whichever the program asked for). The app doesn't encode escape sequences itself.
- Option-as-Meta: configurable per side (`left_option_is_meta = true` default), like Terminal.app.
- Mouse: selection, word/line selection, ⌘-click opens links (OSC 8 hyperlinks from programs, and URLs
  found in the text), and mouse reporting to programs that request it (hold ⌥ to select anyway). The app sends
  mouse events; the server encodes them for the program.
- Dropping files on a tab types their paths, shell-quoted (as Terminal.app does).
- Paste goes through the server's bracketed paste. Pasting more than one line into a shell prompt
  (not a full-screen program) shows a one-line preview first. Configurable.
- Passwords: while CTX.app is active and the tab you're typing into is reading a password (the
  server's `password_input`), Secure Keyboard Entry is on, so no other process sees the keys. It
  goes off when the prompt ends, the tab changes or the app goes to the background. The app logs
  no keystrokes. See ctx's SPEC, Passwords.

## Keyboard shortcuts (Terminal.app conventions)

| Shortcut | Action |
|---|---|
| ⌘N | New window |
| ⌘Q | Quit; tabs keep running (with `keep_tabs_when_app_quits = false`: closes them, confirming if commands are running) |
| ⌘C / ⌘V | Copy selection / paste |
| ⌘A | Select all in this tab |
| ⌘T | New tab (same folder as current tab) |
| ⌘W | Close tab |
| ⌘⇧W | Close window |
| ⌘1…⌘9 | Go to tab N in this window |
| ⌘⇧[ / ⌘⇧] | Previous / next tab |
| ⌘K | Clear scrollback (the saved record is kept) |
| ⌘⇧T | Reopen last closed tab |
| ⌘Y | History of saved tabs |
| ⌘F | Find in this tab |
| ⌘⇧F | Find in all tabs (`ctx find`) |
| ⌘+ / ⌘- / ⌘0 | Font size |
| ⌘⌥← → ↑ ↓ | Move focus between side-by-side tiles |
| ⌘⇧I | Rename tab |
| ⌘⇧A | Activity panel |
| ⌘⇧O | Overview of windows, tabs and processes |
| ⌘, | Open the config file |
| ⌃⌘F | Full screen |
| ⌘M, ⌘H, ⌘` | Minimise, hide, cycle windows (standard macOS) |

All shortcuts are listed in the menus (so they're discoverable) and can be rebound in config
(`[app.keys]`). On Linux, ⌘ is Ctrl+Shift and ⌘⇧ is Ctrl+Shift+Alt, so every plain Ctrl key
still reaches the program. If a terminal program needs a key the app would take, ⌥-less ⌃ combinations
always go to the program, as in Terminal.app.

## Agent visibility

- A small dot badge on a tab when another client (not the app) has typed into it or read it in the
  last 60 seconds. Its colour differs for "typed" and "read only".
- Click the badge (or ⌘⇧A) to open the **Activity** panel: a plain-language list from the server's
  log, e.g. *"claude (in tab 2) typed `npm test` and pressed Enter in tab 4 — 12s ago"*.
- When an agent runs `ctx show "Dev server"`, the app brings that tab forward but doesn't steal keyboard
  focus from another app.

## Permissions

- CTX.app requests **no** privacy permissions (no Full Disk Access, Accessibility, Screen Recording,
  Automation). It reads only its config and state files. Shells are spawned by the `ctx` server
  with their own macOS identity (see [ctx spec § Permissions](../ctx/SPEC.md#permissions)), so
  nothing granted to the app could reach them anyway.
- On first launch, registers the `ctx` server as a LaunchAgent via `SMAppService.agent` (the
  bundle carries `ctx`, `ctx-launch` and `bro`) and installs the `ctx` and `bro` command-line
  tools (symlinks into `/usr/local/bin` when writable, else `~/.local/bin`, with consent). When the
  command line registered the server earlier, the app's registration replaces it; the command line
  then only starts it. A trial or test setup (`CTX_HOME`) never touches launchd or PATH.
- Signed with Developer ID, hardened runtime, notarized. No App Sandbox (it must talk to the
  server's socket and launch it), but no entitlements beyond what that needs.

## Configuration

`~/.config/ctx/config.toml`, `[app]` section. Reloaded on change.

```toml
[app]
font = "SF Mono"
font_size = 13
theme = "ghostty"           # Ghostty's default colours; "system" follows macOS light/dark
left_option_is_meta = true
right_option_is_meta = false
confirm_multiline_paste = true
cursor = "block"            # block | bar | underline
cursor_blink = false
tabs_in_titlebar = true     # false: a plain title bar with the tabs below it
```

Themes: Ghostty's default colours by default. Built in: `ghostty`, `system` (Terminal.app's Basic
or Basic dark, following macOS), `basic`, `basic-dark`, and the tinted `midnight`, `forest`,
`plum`, `ember`, `ocean`, `graphite`. A window's theme can be overridden per tab. Theme files live in `~/.config/ctx/themes/<name>.toml`:

```toml
palette = ["#000000", "#990000", …]   # 16 ANSI colours
foreground = "#ffffff"
background = "#1e1e1e"
cursor = "#929292"                    # optional
selection = "#3f638b"                 # optional
dark = true                           # optional; worked out from the background otherwise
```

Shortcuts are rebound by action name in `[app.keys]` (`new_tab = "cmd-shift-n"`, `"none"` to
remove one); a rebinding takes the key away from the action that had it. The server ignores the
`[app]` section and the app ignores the rest.

## Stack

| Concern | Choice | Why |
|---|---|---|
| Language | Rust, edition 2024, pinned stable toolchain | |
| macOS UI | `objc2` + `objc2-app-kit`, `objc2-foundation`, `objc2-quartz-core`, `objc2-core-text`, `objc2-core-graphics`, `objc2-io-surface`, `objc2-service-management`, `block2`, `dispatch2` | native AppKit, typed bindings, no Xcode project |
| Rendering | Core Text on the CPU into one IOSurface per window | measured: one buffer per window (see Rendering) |
| Linux (dev and test) | `winit` (X11), `softbuffer`, `swash`, `fontdb`, `tiny-skia`, `arboard` | the same app core and drawing, a software canvas |
| Text | `unicode-segmentation`, `unicode-width` | graphemes and cell widths for scrollback lines |
| Server connection | `ctx-protocol` crate (the one cross-project dependency) | protocol versions checked on connect |
| Config | `toml` + `serde`, `serde_json` for the state file, file watching via `FSEvents` FFI | |
| Bundling | small `xtask` that builds the `.app`, signs (Developer ID, hardened runtime) and notarizes | reproducible in CI |

No cross-platform GUI toolkit, no web view, no async runtime. The app core (`src/app/*.rs`)
draws through a small `Canvas` trait and talks to the platform through `src/app/mac` or
`src/app/linux`.

## Testing

**Unit tests** (`cargo test`, every commit; no window server needed):

- layout: tile splitting, closing, dividers, neighbours, drop edges;
- selection and copy: word/line selection, wide characters and emoji, wrapped lines joined
  correctly, trailing spaces trimmed; a selection belongs to one tab, so it can't cross a tile;
- screen model: row updates, scrollback cache, scrolling, wrapping, clamping;
- rendering: cell metrics, glyph plans and fallbacks (cached, never looked up twice), pixels of a
  drawn row; box-drawing tables;
- input mapping: key → key name for the server, Option-as-Meta per side, IME composition;
- shortcut table: no two actions share a key; every action appears in a menu; rebinding;
- config, themes, the state file, link finding.

**Integration tests** (need a `ctx` build; a throwaway server under `CTX_HOME`):

- `tests/selftest.sh`: the app drives itself with real AppKit events (keys through text input,
  clicks, menu shortcuts) and checks: typing, double-click + Copy, Paste, find in a tab, scrollback
  fetched on demand, ⌘T, tiles resizing their tabs, ⌘W, rename, History + Reopen, a badge from
  another client's read, the Activity and Find-all panels, font size, and keystroke-to-pixels
  latency. No Accessibility or Screen Recording permission is needed: the events never leave the
  app.
- `cargo test --test screens`: colours, emoji/CJK, box drawing, an Ink-style redraw and vim are
  drawn by the app's renderer (`CTX --render`) and compared with approved PNGs in
  `tests/reference/` (tolerant diff).
- `tests/crash.sh`: `kill -9` the app and relaunch; every window and tab comes back, with
  scrollback.
- `scripts/budgets.sh`: the memory budget workload.

All of these also run on Linux under Xvfb (`CTX_APP_SELFTEST=1` runs the same self-test there).
`scripts/dev-linux.sh` starts a virtual display, a throwaway server and the app, to be driven with
`xdotool` and screenshotted; `CTX_APP_INSPECT=<file>` writes the app's state (windows, tabs, focus,
hit areas) as JSON after each draw, and `CTX_APP_DUMP=<folder>` saves each window's pixels.

## Crate layout

```
ctx-app/
  src/app/        the app core: windows and tabs, drawing, input, actions, the drawn chrome
                  (chrome.rs: tab bar and strips; panels.rs; ui.rs: menus, fields, dialogs),
                  self-test
  src/app/mac/    AppKit: windows, content view and text input, menus, alerts, first-run
                  install, config watching
  src/app/linux/  winit + softbuffer: the same, for Linux
  src/render/     the Canvas trait and terminal drawing (mod.rs); cg.rs Core Text into
                  IOSurfaces; soft.rs the software canvas; src/boxes.rs box drawing
  src/model.rs    a tab's screen, scrollback cache and scroll position
  src/selection.rs, src/layout.rs (tiles), src/keys.rs, src/shortcuts.rs, src/links.rs
  src/client.rs   connection to the ctx server (ctx-protocol)
  src/headless.rs drawing a tab to a PNG without a window (`CTX --render`)
  bundle/         Info.plist, entitlements, LaunchAgent plist
  xtask/          `cargo xtask bundle`: builds, signs and notarizes CTX.app
  scripts/        install-mac.sh (build, uninstall the old app, reset TCC, install),
                  dev-linux.sh, budgets.sh
  tests/          self-test, reference screens (macos/ and linux/), crash test
```

## Protocol additions (ctx protocol version 2)

What the viewer needs from the model, added to `ctx-protocol` and the server:

- Every request from the app carries an id, and its reply the same id (some replies come later
  than others).
- `SetLook`: a window's palette, colours and default cursor for a tab.
- `History`: scrollback lines by number (logical lines; the app wraps them).
- `SearchTab`: matches in one tab, with positions. `SavedTab`: a closed tab's records, coloured.
- Screen updates carry: the row's soft-wrap flag, cursor shape and blink, mouse tracking, whether
  the shell is at its prompt, and the scrollback's line range.
- `AppInput::Mouse`: mouse events, encoded by the server for the program.
- `StreamOut::Activity`: another client typed into or read a tab (badges, Activity panel).
- `WindowRef::New`: `ctx move "<tab>" to new window`.
- The app is always "you" to the server, whoever launched it; its input and colour requests are
  never held while a tab's shell starts, and opening a tab from the app answers at once.

## Protocol version 3

- Runs carry the terminal's cell grid (`StyledRun.cells`: byte offset and width per cell; left out
  for plain one-cell ASCII). Scrollback lines are measured by the server with ghostty-vt's widths.
- Screen updates carry OSC 8 hyperlinks (`RowUpdate.links`).
- A cursor move alone is sent as an update (arrow keys move the cursor at once).
- `Starters`: the first commands run in tabs, with folder, count and when last used.
- `Processes`: a tab's process tree.

## Protocol version 4

- Screen updates carry `password_input`: the program in front is reading a password.
- Activity entries carry `typed_hidden`: text was typed but kept out of the log.

## Acceptance tests

Results (2026-10-11, M4 Pro, macOS 27, release build) are in the README.

- Claude Code, vim, htop/top, `lazygit`, and an Ink (React) demo app render and behave identically
  to Ghostty side by side (screenshot diff + manual check).
- 15 tabs / 3 windows / `top` running: app < 80 MB, server < 40 MB, total < 25% of Terminal.app's
  footprint for the same workload.
- Copying a selection from a tile never contains text from another tile.
- Killing CTX.app with `kill -9` and relaunching restores every window and tab with scrollback.
