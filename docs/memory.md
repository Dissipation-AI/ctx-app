# Memory: what was measured, what worked, what didn't

Everything tried so far to keep CTX small, so new work starts from here instead of repeating it.
macOS, Apple Silicon, release builds. "Footprint" is `phys_footprint` (`footprint -p`, what
Activity Monitor shows). Newest first.

## How to measure

- **Count the whole Mac, not just the app.** `scripts/membench/run.sh` runs the same workload in
  CTX, Ghostty and Terminal.app and adds the app, its per-tab launchers, the helper processes macOS
  starts for it, the ctx server, and its share of WindowServer (the drop when the app quits, and
  the rise over a reading taken before it started). See its README.
- **WindowServer** jumps by ~110 MB on its own when the screen goes quiet; take 15+ samples and
  compare medians and minimums. Anything animating in another app (your own CTX.app showing an
  agent's spinner) skews it. `top -pid <WindowServer> -stats mem` reads it without root.
- **App-only comparisons** (AppKit levers): launch an isolated copy (`CTX_HOME`) 5 times with one
  800×500 pt window and take the mean. Keep the window uncovered: a covered window releases its
  pixel buffer (by design) and reads ~6 MB low.
- **Distrust savings you can't find.** `footprint` doesn't charge copy-on-write pages shared
  between processes to anyone. If memory disappears from one process and shows up in none, it's
  hidden, not saved; check `vmmap -summary`.
- The app and the ctx server must come from matching sources, `ctx show` makes the server launch
  the installed CTX.app, and a bundle is needed for Info.plist keys. Details in
  `scripts/membench/README.md`.

## 2026-10-14: the whole-Mac count

Workload: one 1496×938 pt window, 3 Claude Code tabs resuming a transcript plus 1 agy tab; "active"
adds a tab printing 10 lines a second. Totals in MB (app + server + launchers + WindowServer share;
each also starts SetStoreUpdateService, ~3 MB, not included).

| | CTX before | CTX after | Ghostty 1.3.1 | Terminal.app |
|---|---|---|---|---|
| App | 41 | 41–46 | 302–331 | 62 idle, 244 active |
| WindowServer share | +130–160 | ≈ 0 (±15 noise) | +2–5 | +2 idle, +30–49 active |
| Helpers macOS started | SafariPlatformSupport 11–36 | none | none | none |
| Total, active | ~200 | **~59–63** | ~349 | ~307 |
| Total, idle | ~210 | **~73** | ~315 | ~75 |

What fixed it:

- **The borderless window cost WindowServer ~130–160 MB per window**, idle or not. A normal
  titled window with a see-through, full-size title bar and hidden title costs about nothing.
  Turning the borderless window's shadow off removed most of it, so the shadow (which WindowServer
  works out from the window's pixels for a borderless window) is the main part.
- **No second set of buttons.** A titled window makes the system's red/yellow/green buttons
  (SwiftUI views). Hiding them still cost ~3 MB (29 → 26 MB at 800×500 when removed). `BarWindow`
  overrides `+standardWindowButton:forStyleMask:` to return nil, so they're never made; CTX draws
  its own. The window still minimises (`miniaturize:`), zooms (`zoom:`) and goes full screen;
  only the `perform…` variants need the system buttons. Net cost of the title bar against the old
  borderless window: ~2 MB in the app, for ~130 MB less in WindowServer.
- **AppKit's AutoFill guessing** treated the terminal view as a text field and started
  `com.apple.SafariPlatformSupport.Helper` (11–36 MB). Info.plist
  `NSAutoFillRequiresTextContentTypeForOneTimeCodeOnMac = true` stops it on macOS 27. The
  `NSAutoFillHeuristicControllerEnabled = false` default (Ghostty, macOS 26) alone didn't; both are
  set. Found by breaking on `+[NSXPCSharedListener listenerEndpointForService:…]` in lldb.

Tried and dropped (each against the borderless window, unless noted):

| Tried | Result |
|---|---|
| Two IOSurfaces drawn in turn (never draw into the one on screen) | WindowServer unchanged; app +20 MB |
| CGImage as the layer's contents instead of the IOSurface | WindowServer unchanged; app looked 20 MB smaller only because the shared copy-on-write pages aren't charged to anyone. Also copies the whole window every frame |
| CAMetalLayer, GPU copy of the IOSurface into each drawable | WindowServer unchanged (+110); app 163 MB |
| Square corners (no `cornerRadius`/`masksToBounds`) | no change |
| Opaque window instead of clear background | no change |
| No sRGB tag on window and surface | no change (but see 2026-10-13: the tag saves memory elsewhere) |
| Rows aligned to 256 bytes | no change; IOSurface already picks 12032 = 47×256 |
| Layer with no contents at all | WindowServer +32: the cost comes with showing pixels in that window |

Things learned about WindowServer:
- Setting an IOSurface as a layer's contents is meant to be zero-copy across processes, and
  WindowServer keeps reading a surface until it has composited the next one (Ladybird keeps three
  and checks `IOSurfaceIsInUse`).
- It redraws a whole layer when any of it changes; there's no partial-damage API (Mozilla tiles
  layers to work around this).
- Metal drawables and AppKit backing stores are charged to the app ("Owned physical footprint
  (graphics)": ~115–120 MB in Ghostty and Terminal), not to WindowServer. So app-only numbers
  compare terminals unfairly in both directions.

## 2026-10-13: AppKit's fixed cost (800×500 window, app only)

- A bare AppKit app with one titled window and nothing drawn is **20.6 MB**. CTX then was 28.0 MB
  including its 6.1 MB window buffer, so CTX itself added ~1.3 MB.
- **The system title bar was the one big lever: 4.1 MB**, because its buttons are SwiftUI views.
  This led to the borderless window with drawn buttons (28.84 → 24.18 MB, mean of 5), which
  2026-10-14 found cost ~130 MB in WindowServer, and replaced with the button-less titled window.
- **sRGB window tag: 0.66 MB** even on one display. Without it AppKit builds colour-conversion
  tables for every display a window visits and never frees them (18 MB after a couple of hours on
  two displays).
- The menu bar costs 0.5–1.5 MB (kept).
- Any bundled app connects to App Intents (Shortcuts/Siri) at launch: +1.2 MB, no switch.
- Made no difference: the Liquid Glass opt-out (`UIDesignRequiresCompatibility`), removing the
  title bar blur or hidden title field, dropping unused framework links (AppKit loads CloudKit,
  CoreData… anyway), turning off state restoration, window animations, dictation/emoji menu items.
- `MallocNanoZone=0`: +0.45 MB (worse).
- Memory that only grew, now bounded: a tab's cached scrollback is dropped when it's no longer
  shown; keystroke-latency samples are capped.

## 2026-10-11: how pixels are drawn (app only, windows redrawing at 60 fps)

| | 1 window 1700×1050 pt | 3 windows 1700×1050 pt | 3 windows 900×600 pt |
|---|---|---|---|
| Metal, 2 drawables | 200 MB | 327 MB | – |
| AppKit layer-backed view (`drawRect`) | 284 MB | 555 MB | – |
| **IOSurface + Core Text on the CPU (chosen)** | **44 MB** | **101 MB** | **43 MB** |

One IOSurface per window, shared by all its tabs; tabs that aren't shown cost the app nothing; a
covered, minimised or other-Space window releases its surface. The pixel buffer is the floor:
width × height × scale² × 4 bytes (22.5 MB at 1496×938 pt on a 2× display). These numbers don't
include WindowServer's share (see 2026-10-14).

## The ctx server

- Runs with `MallocSpaceEfficient=1` (set at start), so memory freed after an output flood goes
  back to the system; idle tabs are compacted. 3–5 MB with 15 idle tabs.
- A command's saved output tail is one byte buffer plus line lengths; floods are written out in
  256 KB pieces; 32 KB per tab per turn so one flooding tab can't stall the others.
- `ctx-launch` is ~1.3 MB per tab, against ~2.9 MB for the `login` that Ghostty and Terminal.app
  put in each tab.

## Ideas not tried yet

- Window buffer in a 16-bit format (half the floor) would band truecolor themes.
- SetStoreUpdateService (~3 MB) starts for every AppKit app, CTX included; no switch found.
