//! `CTX_APP_SELFTEST=1`: the app drives itself with real AppKit events (keys through text input,
//! mouse clicks, menu shortcuts) against a running server, checks what it sees, prints a report
//! and quits with 0 when everything passed. Used by the integration tests; it needs no
//! Accessibility or Screen Recording permission because the events never leave the app.

use super::{App, later};
use crate::model::{Place, ViewPos};
use objc2::rc::Retained;
use objc2_app_kit::*;
use objc2_foundation::*;
use std::time::Duration;

type Step = Box<dyn FnOnce(&mut App, &mut Report)>;

#[derive(Default)]
pub struct Report {
    pub passed: Vec<String>,
    pub failed: Vec<String>,
    pub window: u64,
    pub tab: u64,
    pub saved_clipboard: Option<String>,
    pub tabs_before: usize,
    pub saved_cols: u16,
}

impl Report {
    fn check(&mut self, ok: bool, what: &str) {
        if ok {
            self.passed.push(what.to_string());
        } else {
            self.failed.push(what.to_string());
        }
    }
}

fn after(ms: u64, f: impl FnOnce() + Send + 'static) {
    let when = dispatch2::DispatchTime::NOW.time(Duration::from_millis(ms).as_nanos() as i64);
    let _ = dispatch2::DispatchQueue::main().after(when, f);
}

struct SendBox<T>(T);
// SAFETY: only ever touched on the main queue.
unsafe impl<T> Send for SendBox<T> {}

/// Run the steps one after another, `gap` ms apart.
fn run(mut steps: std::collections::VecDeque<Step>, report: Report) {
    let Some(step) = steps.pop_front() else {
        finish(report);
        return;
    };
    let b = SendBox((step, steps, report));
    later(move |app| {
        let b = b;
        let SendBox((step, steps, mut report)) = b;
        step(app, &mut report);
        let b = SendBox((steps, report));
        after(700, move || {
            let b = b;
            let SendBox((steps, report)) = b;
            run(steps, report);
        });
    });
}

fn finish(report: Report) {
    for p in &report.passed {
        println!("ok   {p}");
    }
    for f in &report.failed {
        println!("FAIL {f}");
    }
    println!("{} passed, {} failed", report.passed.len(), report.failed.len());
    if let Some(c) = &report.saved_clipboard {
        super::cocoa::set_clipboard(c);
    }
    let code = if report.failed.is_empty() { 0 } else { 1 };
    // Leave the tabs as they are (the test server is thrown away).
    later(move |a| {
        a.quitting = true;
        a.loaded.keep_tabs_when_app_quits = true;
        let _ = std::io::Write::flush(&mut std::io::stdout());
        std::process::exit(code);
    });
}

fn window(app: &App, w: u64) -> Option<Retained<NSWindow>> {
    app.wins.get(&w).map(|win| win.views.window.clone())
}

fn key(app: &App, w: u64, chars: &str, keycode: u16, flags: u64) {
    let Some(win) = window(app, w) else { return };
    let mtm = app.mtm;
    for ty in [NSEventType::KeyDown, NSEventType::KeyUp] {
        let e = NSEvent::keyEventWithType_location_modifierFlags_timestamp_windowNumber_context_characters_charactersIgnoringModifiers_isARepeat_keyCode(
            ty,
            NSPoint::ZERO,
            NSEventModifierFlags(flags as usize),
            0.0,
            win.windowNumber(),
            None,
            &NSString::from_str(chars),
            &NSString::from_str(&chars.to_lowercase()),
            false,
            keycode,
        );
        if let Some(e) = e {
            if flags & crate::keys::COMMAND != 0 {
                if ty == NSEventType::KeyDown
                    && let Some(menu) = NSApplication::sharedApplication(mtm).mainMenu()
                {
                    menu.performKeyEquivalent(&e);
                }
            } else {
                win.sendEvent(&e);
            }
        }
    }
}

fn type_text(app: &App, w: u64, text: &str) {
    for c in text.chars() {
        key(app, w, &c.to_string(), 0, 0);
    }
}

/// A click in the terminal view at (column, drawn row) of tile `tab`.
fn click(app: &mut App, w: u64, tab: u64, col: usize, row: usize, count: isize) {
    let Some((r, scale)) =
        app.wins.get(&w).and_then(|win| win.tile_rects.iter().find(|(t, _)| *t == tab).map(|x| (x.1, win.scale)))
    else {
        return;
    };
    let size = app.state.font_sizes.get(&tab).copied().unwrap_or(app.loaded.app.font_size);
    let Some(fonts) = app.fonts_peek(size, scale) else { return };
    let Some(win) = app.wins.get(&w) else { return };
    let x = r.x + crate::render::PAD_X + (col as f64 + 0.5) * fonts.cell_w / scale;
    let y = r.y + crate::render::PAD_Y + (row as f64 + 0.5) * fonts.cell_h / scale;
    let p = win.views.term.convertPoint_toView(NSPoint::new(x, y), None);
    for ty in [NSEventType::LeftMouseDown, NSEventType::LeftMouseUp] {
        let e = NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
            ty,
            p,
            NSEventModifierFlags(0),
            0.0,
            win.views.window.windowNumber(),
            None,
            0,
            count,
            1.0,
        );
        if let Some(e) = e {
            win.views.window.sendEvent(&e);
        }
    }
}

fn screen_text(app: &App, tab: u64) -> Vec<String> {
    app.views.get(&tab).map(|v| v.screen.lines.iter().map(|r| r.text()).collect()).unwrap_or_default()
}

fn find_row(app: &App, tab: u64, exact: &str) -> Option<usize> {
    let v = app.views.get(&tab)?;
    let y = v.screen.lines.iter().position(|r| r.text().trim_end() == exact)?;
    v.drawn.iter().position(|p| *p == Place::Screen(y as u16))
}

pub fn start() {
    let mut steps: std::collections::VecDeque<Step> = std::collections::VecDeque::new();
    steps.push_back(Box::new(|app, r| {
        r.saved_clipboard = super::cocoa::get_clipboard();
        r.window = app.wins.keys().next().copied().unwrap_or(0);
        r.tab = app.focused_tab(r.window).unwrap_or(0);
        r.check(r.window != 0 && r.tab != 0, "a window with a tab is shown");
        if let Some(win) = app.wins.get(&r.window) {
            super::cocoa::show_window(&win.views, true);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        type_text(app, r.window, "echo ctx-self''test-ok");
        key(app, r.window, "\r", 36, 0);
    }));
    steps.push_back(Box::new(|app, r| {
        let lines = screen_text(app, r.tab);
        r.check(lines.iter().any(|l| l.trim_end() == "ctx-selftest-ok"), "typing goes through text input and runs");
    }));
    steps.push_back(Box::new(|app, r| match find_row(app, r.tab, "ctx-selftest-ok") {
        Some(row) => click(app, r.window, r.tab, 3, row, 2),
        None => r.check(false, "found the output row to click"),
    }));
    steps.push_back(Box::new(|app, r| {
        let w = r.window;
        app.copy(w);
        r.check(
            super::cocoa::get_clipboard().as_deref() == Some("ctx-selftest-ok"),
            "double-click selects a word; Copy copies it",
        );
    }));
    steps.push_back(Box::new(|app, r| {
        app.find_changed(r.window, "test-ok".into());
        let n = app.views.get(&r.tab).and_then(|v| v.find.as_ref()).map(|f| f.matches.len()).unwrap_or(0);
        r.check(n >= 2, "find in this tab finds the command and its output");
        app.close_find(r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        super::cocoa::set_clipboard("echo pasted-ok");
        app.paste(r.window);
        key(app, r.window, "\r", 36, 0);
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(screen_text(app, r.tab).iter().any(|l| l.trim_end() == "pasted-ok"), "paste goes to the program");
        type_text(app, r.window, "seq 1 400");
        key(app, r.window, "\r", 36, 0);
    }));
    steps.push_back(Box::new(|app, r| {
        let cols = app.views.get(&r.tab).map(|v| v.columns).unwrap_or(80);
        app.scroll_rows(r.tab, 150, true, cols);
        app.redraw_dirty();
        let v = app.views.get(&r.tab);
        let scrolled = v.is_some_and(|v| matches!(v.pos, ViewPos::At { .. }));
        let shows_history = v.is_some_and(|v| v.drawn.iter().any(|p| matches!(p, Place::History { .. })));
        r.check(scrolled && shows_history, "scrolling back fetches scrollback lines from the server");
        app.scroll_rows(r.tab, 10_000, false, cols);
        r.check(
            app.views.get(&r.tab).is_some_and(|v| v.pos == ViewPos::Live),
            "scrolling down returns to the live screen",
        );
        r.tabs_before = app.window_info(r.window).map(|w| w.tabs.len()).unwrap_or(0);
        key(app, r.window, "t", 17, crate::keys::COMMAND);
    }));
    steps.push_back(Box::new(|app, r| {
        let now = app.window_info(r.window).map(|w| w.tabs.len()).unwrap_or(0);
        r.check(now == r.tabs_before + 1, "⌘T opens a tab in this window");
        let new = app.focused_tab(r.window);
        r.check(new.is_some() && new != Some(r.tab), "the new tab is selected");
        if let Some(t) = new {
            app.tile(r.window, r.tab, Some(t), crate::layout::Edge::Left);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        let n = app.wins.get(&r.window).map(|w| w.tile_rects.len()).unwrap_or(0);
        r.check(n == 2, "two tabs side by side");
        let sizes: Vec<u16> = app
            .wins
            .get(&r.window)
            .map(|w| w.tile_rects.iter().filter_map(|(t, _)| app.tab_info(*t).map(|i| i.columns)).collect())
            .unwrap_or_default();
        r.check(sizes.len() == 2 && sizes.iter().all(|c| *c < 200), "each tile resized its tab");
        // Select the left tile and drag a selection across into the right one: it stays in one tab.
        if let Some(sel_tab) = app.wins.get(&r.window).and_then(|w| w.tile_rects.first().map(|x| x.0)) {
            r.tab = sel_tab;
        }
        key(app, r.window, "w", 13, crate::keys::COMMAND);
    }));
    steps.push_back(Box::new(|app, r| {
        let now = app.window_info(r.window).map(|w| w.tabs.len()).unwrap_or(0);
        r.check(now == r.tabs_before, "⌘W closes the focused tab");
        // `cat` echoes at once, so this times the terminal, not the shell's line editor.
        type_text(app, r.window, "cat");
        key(app, r.window, "\r", 36, 0);
    }));
    // Give `cat` a moment to start before timing keys.
    steps.push_back(Box::new(|_app, _r| {}));
    steps.push_back(Box::new(|app, r| {
        app.latencies.clear();
        let n = app.wins.get(&r.window).map(|w| w.tile_rects.len()).unwrap_or(0);
        r.check(n == 1, "closing a tiled tab gives its space to the neighbour");
    }));
    steps.push_back(Box::new(|app, r| {
        let t = app.focused_tab(r.window).unwrap_or(r.tab);
        r.tab = t;
        app.rename_tab(t, "Self test".into());
    }));
    steps.push_back(Box::new(|app, r| {
        let name = app.tab_info(r.tab).and_then(|i| i.name.clone());
        r.check(name.as_deref() == Some("Self test"), "renaming a tab");
        super::panels::history_open(app);
    }));
    steps.push_back(Box::new(|app, r| {
        let closed = app.panels.history.as_ref().map(|h| h.closed.len()).unwrap_or(0);
        r.check(closed >= 1, "History lists the tab closed with ⌘W");
        r.tabs_before = app.window_info(r.window).map(|w| w.tabs.len()).unwrap_or(0);
        if let Some(t) = app.panels.history.as_ref().and_then(|h| h.closed.first().map(|c| c.number)) {
            app.reopen(t);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        let total: usize = app.list.windows.iter().map(|w| w.tabs.len()).sum();
        r.check(total > r.tabs_before, "Reopen brings a closed tab back");
        if let Some(h) = app.panels.history.as_ref() {
            h.window.orderOut(None);
        }
        // Another client reads the tab: a badge.
        if let Some(ctx) = super::install::ctx_program() {
            let out = std::process::Command::new(&ctx)
                .args(["read", "last", "2", "lines", "in", "tab", &r.tab.to_string()])
                .output();
            if let Ok(o) = out
                && !o.status.success()
            {
                println!("note: {} said {}", ctx.display(), String::from_utf8_lossy(&o.stdout).trim());
            }
        }
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(app.touched.contains_key(&r.tab), "another client's read shows a badge on the tab");
        // ⌘+ acts on the focused tab of the key window.
        if let Some(t) = app.key_window().and_then(|w| app.focused_tab(w)) {
            r.tab = t;
        }
        let cols = app.views.get(&r.tab).and_then(|v| v.asked_size).map(|s| s.0).unwrap_or(0);
        app.perform(crate::shortcuts::Action::Bigger);
        app.perform(crate::shortcuts::Action::Bigger);
        r.saved_cols = cols;
    }));
    steps.push_back(Box::new(|app, r| {
        let cols = app.views.get(&r.tab).and_then(|v| v.asked_size).map(|s| s.0).unwrap_or(0);
        r.check(cols < r.saved_cols, "⌘+ makes the text bigger (fewer columns)");
        app.perform(crate::shortcuts::Action::ActualSize);
    }));
    steps.push_back(Box::new(|app, _r| {
        super::panels::activity_open(app);
        super::panels::find_all_open(app);
        super::panels::find_all_search_for_test(app, "selftest-ok");
    }));
    steps.push_back(Box::new(|app, r| {
        let text = |tv: &NSTextView| tv.string().to_string();
        let activity = app.panels.activity.as_ref().map(|p| text(&p.text)).unwrap_or_default();
        r.check(
            activity.contains("read") || activity.contains("ran"),
            "the Activity panel lists what another client did",
        );
        let found = app.panels.find.as_ref().map(|p| text(&p.results)).unwrap_or_default();
        r.check(found.contains("selftest-ok"), "Find in All Tabs finds text in saved and open tabs");
        if let Some(p) = &app.panels.activity {
            p.window.orderOut(None);
        }
        if let Some(p) = &app.panels.find {
            p.window.orderOut(None);
        }
    }));
    // Keystroke to pixels: type characters one at a time and time each echo being drawn.
    for i in 0..30 {
        steps.push_back(Box::new(move |app, r| {
            let c = (b'a' + (i % 26) as u8) as char;
            type_text(app, r.window, &c.to_string());
        }));
    }
    steps.push_back(Box::new(|app, r| {
        let mut l = app.latencies.clone();
        l.sort();
        if l.is_empty() {
            r.check(false, "keystroke latency was measured");
            return;
        }
        let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
        let median = ms(l[l.len() / 2]);
        let p95 = ms(l[(l.len() * 95 / 100).min(l.len() - 1)]);
        println!("keystroke to pixels: median {median:.2} ms, 95th percentile {p95:.2} ms ({} keys)", l.len());
        r.check(l.len() >= 20, "keystroke latency was measured");
        let w = r.window;
        if let Some(t) = app.focused_tab(w) {
            app.input(t, ctx_protocol::AppInput::Key { key: "ctrl-c".into(), text: None });
        }
    }));
    run(steps, Report::default());
}

impl App {
    fn fonts_peek(&mut self, size: f64, scale: f64) -> Option<std::rc::Rc<crate::render::Fonts>> {
        let family = self.loaded.app.font.clone();
        Some(self.fonts.get(&family, size, scale))
    }
}
