//! `CTX_APP_SELFTEST=1`: the app drives itself through the same entry points the window system
//! calls (key presses, text input, clicks at points in the window, menu actions) against a
//! running server, checks what it sees, prints a report and quits with 0 when everything passed.
//! The same test runs on macOS and Linux; it needs no Accessibility or Screen Recording
//! permission because the events never leave the app.

use super::chrome::{self, Hit};
use super::panels::Kind;
use super::{App, later, plat};
use crate::keys::{self, Press};
use crate::model::{Place, ViewPos};
use crate::shortcuts::Action;
use std::collections::VecDeque;

type Step = Box<dyn FnOnce(&mut App, &mut Report)>;

#[derive(Default)]
pub struct Report {
    pub passed: Vec<String>,
    pub failed: Vec<String>,
    pub window: u64,
    pub tab: u64,
    pub saved_clipboard: Option<String>,
    pub count: usize,
    pub cols: u16,
    pub cursor: Option<(u16, u16)>,
    /// Another app had Secure Keyboard Entry on before the test (so "off" can't be checked).
    pub secure_elsewhere: bool,
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

/// Run the steps one after another, `gap` ms apart (time for the server to answer).
fn run(mut steps: VecDeque<Step>, report: Report) {
    let Some(step) = steps.pop_front() else {
        finish(report);
        return;
    };
    later(move |app| {
        let mut report = report;
        step(app, &mut report);
        plat::after(700, Box::new(move || run(steps, report)));
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
        plat::set_clipboard(c);
    }
    let code = if report.failed.is_empty() { 0 } else { 1 };
    // Leave the tabs as they are (the test server is thrown away).
    later(move |a| {
        a.quitting = true;
        a.loaded.keep_tabs_when_app_quits = true;
        std::process::exit(code);
    });
}

/// A named key (macOS key codes, which the key mapper speaks on every platform).
fn key(app: &mut App, w: u64, keycode: u16, flags: u64) {
    let p = Press { keycode, chars: String::new(), chars_ignoring: String::new(), flags };
    app.key_down(w, &p);
}

fn type_text(app: &mut App, w: u64, text: &str) {
    for c in text.chars() {
        let p = Press { keycode: 0xFFFF, chars: c.to_string(), chars_ignoring: c.to_string(), flags: 0 };
        if app.key_down(w, &p) == keys::Input::TextInput {
            app.insert_text(w, &c.to_string());
        }
    }
}

fn enter(app: &mut App, w: u64) {
    key(app, w, 36, 0);
}

/// Where a cell of a tile is, in points within the window.
fn cell_point(app: &mut App, w: u64, tab: u64, col: usize, row: usize) -> Option<(f64, f64)> {
    let (r, scale) =
        app.wins.get(&w).and_then(|win| win.tile_rects.iter().find(|(t, _)| *t == tab).map(|x| (x.1, win.scale)))?;
    let fonts = app.tab_fonts(tab, scale);
    Some((
        r.x + crate::render::PAD_X + (col as f64 + 0.5) * fonts.cell_w / scale,
        r.y + crate::render::PAD_Y + (row as f64 + 0.5) * fonts.cell_h / scale,
    ))
}

fn click_at(app: &mut App, w: u64, x: f64, y: f64, clicks: isize, button: u8) {
    app.mouse_down(w, x, y, clicks, 0, button);
    app.mouse_up(w, x, y, 0, button);
}

fn screen_text(app: &App, tab: u64) -> Vec<String> {
    app.views.get(&tab).map(|v| v.screen.lines.iter().map(|r| r.text()).collect()).unwrap_or_default()
}

fn find_row(app: &App, tab: u64, exact: &str) -> Option<usize> {
    let v = app.views.get(&tab)?;
    let y = v.screen.lines.iter().position(|r| r.text().trim_end() == exact)?;
    v.drawn.iter().position(|p| *p == Place::Screen(y as u16))
}

fn tab_count(app: &App, w: u64) -> usize {
    app.window_info(w).map(|i| i.tabs.len()).unwrap_or(0)
}

pub fn start() {
    let mut steps: VecDeque<Step> = VecDeque::new();
    steps.push_back(Box::new(|app, r| {
        r.saved_clipboard = plat::get_clipboard();
        r.window = app.wins.keys().next().copied().unwrap_or(0);
        r.tab = app.focused_tab(r.window).unwrap_or(0);
        r.check(r.window != 0 && r.tab != 0, "a window with a tab is shown");
        if let Some(win) = app.wins.get(&r.window) {
            plat::show_window(&win.views, true);
        }
        app.window_key(r.window, true);
    }));
    steps.push_back(Box::new(|app, r| {
        type_text(app, r.window, "echo ctx-self''test-ok");
        enter(app, r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        let lines = screen_text(app, r.tab);
        r.check(lines.iter().any(|l| l.trim_end() == "ctx-selftest-ok"), "typing goes through text input and runs");
        // Arrow keys at the prompt move the cursor, with nothing else changing on screen.
        type_text(app, r.window, "abcdef");
    }));
    steps.push_back(Box::new(|app, r| {
        r.cursor = app.views.get(&r.tab).and_then(|v| v.screen.cursor);
        for _ in 0..3 {
            key(app, r.window, 123, 0);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        let now = app.views.get(&r.tab).and_then(|v| v.screen.cursor);
        r.check(
            matches!((r.cursor, now), (Some((x0, y0)), Some((x1, y1))) if y0 == y1 && x1 + 3 == x0),
            "the cursor moves on arrow keys before anything is typed",
        );
        // Clear the line.
        app.input(r.tab, ctx_protocol::AppInput::Key { key: "ctrl-u".into(), text: None });
    }));
    steps.push_back(Box::new(|app, r| match find_row(app, r.tab, "ctx-selftest-ok") {
        Some(row) => {
            if let Some((x, y)) = cell_point(app, r.window, r.tab, 3, row) {
                click_at(app, r.window, x, y, 2, 1);
            }
        }
        None => r.check(false, "found the output row to click"),
    }));
    steps.push_back(Box::new(|app, r| {
        app.copy(r.window);
        r.check(
            plat::get_clipboard().as_deref() == Some("ctx-selftest-ok"),
            "double-click selects a word; Copy copies it",
        );
    }));
    steps.push_back(Box::new(|app, r| {
        app.open_find(r.window);
        type_text(app, r.window, "test-ok");
        let n = app.views.get(&r.tab).and_then(|v| v.find.as_ref()).map(|f| f.matches.len()).unwrap_or(0);
        r.check(n >= 2, "find in this tab finds the command and its output");
        key(app, r.window, 53, 0);
        r.check(app.wins.get(&r.window).is_some_and(|w| w.find.is_none()), "Escape closes the find bar");
    }));
    steps.push_back(Box::new(|app, r| {
        plat::set_clipboard("echo pasted-ok");
        app.paste(r.window);
        enter(app, r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(screen_text(app, r.tab).iter().any(|l| l.trim_end() == "pasted-ok"), "paste goes to the program");
        type_text(app, r.window, "seq 1 400");
        enter(app, r.window);
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
        r.count = tab_count(app, r.window);
        // The + menu, and New Tab from it.
        let plus = app.wins.get(&r.window).map(|w| w.bar.plus);
        if let Some(p) = plus {
            click_at(app, r.window, p.x + 4.0, p.y + 4.0, 1, 1);
        }
        r.check(app.wins.get(&r.window).is_some_and(|w| w.menu.is_some()), "the + button opens its menu");
        enter_menu_first(app, r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(tab_count(app, r.window) == r.count + 1, "New Tab from the + menu opens a tab in this window");
        let new = app.focused_tab(r.window);
        r.check(new.is_some() && new != Some(r.tab), "the new tab is selected");
        // Click the first tab in the bar: it's selected again.
        let first = app.wins.get(&r.window).and_then(|w| w.bar.tabs.first().copied());
        if let Some((t, rect)) = first {
            click_at(app, r.window, rect.x + rect.w / 2.0, rect.y + rect.h / 2.0, 1, 1);
            r.check(app.focused_tab(r.window) == Some(t), "clicking a tab in the bar selects it");
        }
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
        if let Some(t) = app.wins.get(&r.window).and_then(|w| w.tile_rects.first().map(|x| x.0)) {
            r.tab = t;
        }
        r.count = tab_count(app, r.window);
        app.perform(Action::CloseTab);
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(tab_count(app, r.window) + 1 == r.count, "Close Tab closes the focused tab");
        let n = app.wins.get(&r.window).map(|w| w.tile_rects.len()).unwrap_or(0);
        r.check(n == 1, "closing a tiled tab gives its space to the neighbour");
        let t = app.focused_tab(r.window).unwrap_or(r.tab);
        r.tab = t;
        app.rename_tab(t, "Self test".into());
    }));
    steps.push_back(Box::new(|app, r| {
        let name = app.tab_info(r.tab).and_then(|i| i.name.clone());
        r.check(name.as_deref() == Some("Self test"), "renaming a tab");
        // Its own theme: the server gets that tab's colours.
        app.set_tab_theme(r.tab, Some("forest".into()));
        let bg = app.views.get(&r.tab).and_then(|v| v.look.as_ref().map(|l| l.background));
        r.check(
            bg == Some(crate::theme::load("forest", std::path::Path::new("/"), true).unwrap().background),
            "a tab's own theme is sent to the server",
        );
        // The History button opens History over the terminal.
        let b = app
            .wins
            .get(&r.window)
            .and_then(|w| w.bar.buttons.iter().find(|(b, _)| *b == chrome::Button::History).map(|x| x.1));
        if let Some(b) = b {
            click_at(app, r.window, b.x + b.w / 2.0, b.y + b.h / 2.0, 1, 1);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        let p = app.wins.get(&r.window).and_then(|w| w.panel.as_ref());
        r.check(p.is_some_and(|p| p.kind == Kind::History), "the History button opens History");
        let closed = p.map(|p| p.closed.len()).unwrap_or(0);
        r.check(closed >= 1, "History lists the closed tab");
        r.count = app.list.windows.iter().map(|w| w.tabs.len()).sum();
        if let Some(t) = p.and_then(|p| p.closed.first().map(|c| c.number)) {
            app.reopen(t);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        let total: usize = app.list.windows.iter().map(|w| w.tabs.len()).sum();
        r.check(total > r.count, "Reopen brings a closed tab back");
        app.close_panel(r.window);
        // Another client reads the tab: a badge.
        if let Some(ctx) = plat::install::ctx_program() {
            let _ = std::process::Command::new(&ctx)
                .args(["read", "last", "2", "lines", "in", "tab", &r.tab.to_string()])
                .output();
        }
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(app.touched.contains_key(&r.tab), "another client's read shows a badge on the tab");
        if let Some(t) = app.key_window().and_then(|w| app.focused_tab(w)) {
            r.tab = t;
        }
        r.cols = app.views.get(&r.tab).and_then(|v| v.asked_size).map(|s| s.0).unwrap_or(0);
        app.perform(Action::Bigger);
        app.perform(Action::Bigger);
    }));
    steps.push_back(Box::new(|app, r| {
        let cols = app.views.get(&r.tab).and_then(|v| v.asked_size).map(|s| s.0).unwrap_or(0);
        r.check(cols < r.cols, "⌘+ makes the text bigger (fewer columns)");
        app.perform(Action::ActualSize);
        app.perform(Action::Activity);
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(app.activity.iter().any(|e| e.tab.is_some()), "the Activity panel has what another client did");
        app.open_panel(r.window, Kind::FindAll);
        type_text(app, r.window, "selftest-ok");
        enter(app, r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        let found = app
            .wins
            .get(&r.window)
            .and_then(|w| w.panel.as_ref())
            .and_then(|p| p.found.as_ref())
            .map(|f| f.hits.len())
            .unwrap_or(0);
        r.check(found >= 1, "Find in All Tabs finds text in open and saved tabs");
        app.toggle_panel(r.window, Kind::Overview);
        r.check(
            app.wins.get(&r.window).and_then(|w| w.panel.as_ref()).is_some_and(|p| p.kind == Kind::Overview),
            "the Overview opens",
        );
        app.close_panel(r.window);
        // Drag a tab out of the window: it gets a window of its own.
        r.count = app.list.windows.len();
        let first = app.wins.get(&r.window).and_then(|w| w.bar.tabs.first().copied());
        if let Some((_, rect)) = first {
            let (x, y) = (rect.x + 40.0, rect.y + rect.h / 2.0);
            app.mouse_down(r.window, x, y, 1, 0, 1);
            app.mouse_dragged(r.window, x + 20.0, y + 30.0, 0, 1);
            let (cw, ch) = app.wins.get(&r.window).map(|w| plat::content_size(&w.views)).unwrap_or((800.0, 600.0));
            app.mouse_dragged(r.window, cw + 400.0, ch + 300.0, 0, 1);
            app.mouse_up(r.window, cw + 400.0, ch + 300.0, 0, 1);
        }
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(app.list.windows.len() == r.count + 1, "dragging a tab out of the window opens a new window for it");
        let hit = app.chrome_hit(r.window, 2.0, 2.0);
        r.check(matches!(hit, Hit::Bar), "the empty bar is where the window is dragged from");
        // `cat` echoes at once, so this times the terminal, not the shell's line editor.
        type_text(app, r.window, "cat");
        enter(app, r.window);
    }));
    // A password prompt turns on Secure Keyboard Entry, only while CTX.app is active.
    steps.push_back(Box::new(|app, r| {
        r.secure_elsewhere = plat::secure_input_enabled();
        if let Some(win) = app.wins.get(&r.window) {
            plat::show_window(&win.views, true);
        }
        app.window_key(r.window, true);
        app.input(r.tab, ctx_protocol::AppInput::Key { key: "ctrl-c".into(), text: None });
    }));
    steps.push_back(Box::new(|app, r| {
        type_text(app, r.window, "read -s 'pw?Password: '");
        enter(app, r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        let on = |app: &App| app.secure_input && (cfg!(not(target_os = "macos")) || plat::secure_input_enabled());
        r.check(on(app), "Secure Keyboard Entry is on at a password prompt");
        app.app_active(false);
        r.check(
            !app.secure_input && (r.secure_elsewhere || !plat::secure_input_enabled()),
            "and off when CTX.app isn't active",
        );
        app.app_active(true);
        r.check(on(app), "and on again when it's active");
        type_text(app, r.window, "hunter2");
        enter(app, r.window);
    }));
    steps.push_back(Box::new(|app, r| {
        r.check(
            !app.secure_input && (r.secure_elsewhere || !plat::secure_input_enabled()),
            "Secure Keyboard Entry is off once the password has been entered",
        );
        r.check(!screen_text(app, r.tab).iter().any(|l| l.contains("hunter2")), "the password isn't shown");
    }));
    steps.push_back(Box::new(|app, _r| app.latencies.clear()));
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
        if let Some(t) = app.focused_tab(r.window) {
            app.input(t, ctx_protocol::AppInput::Key { key: "ctrl-c".into(), text: None });
        }
    }));
    run(steps, Report::default());
}

/// Choose the highlighted (or first) item of the open menu with Return.
fn enter_menu_first(app: &mut App, w: u64) {
    if let Some(m) = app.wins.get_mut(&w).and_then(|x| x.menu.as_mut())
        && m.hover.is_none()
    {
        m.step(true);
    }
    enter(app, w);
}
