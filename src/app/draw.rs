//! Drawing a window: its tab bar, terminal tiles, find and reopen strips, and any panel, menu or
//! dialog, all into the window's one buffer. Only what changed is redrawn: rows of the terminal
//! that changed, or just the bar.

use super::chrome::{self, BarFrame, ChromeFonts, Hit, Status, TabItem};
use super::panels;
use super::ui::{self, Colors};
use super::{App, BADGE_SECONDS, Dirty, Focus, TabSource, plat};
use crate::layout::Rect;
use crate::model::{self, Composed, Place, ViewPos};
use crate::render::{self, Canvas, CursorFrame, Damage, TileFrame};
use crate::text::Row;
use ctx_protocol::*;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Instant;

impl App {
    pub fn redraw_dirty(&mut self) {
        // Focus, the active app and the tab in front all change before a redraw.
        self.update_secure_input();
        let ws: Vec<u64> = self
            .wins
            .iter()
            .filter(|(_, w)| (w.dirty.is_some() || w.chrome_dirty) && w.visible)
            .map(|(n, _)| *n)
            .collect();
        for w in ws {
            self.draw_window(w);
        }
    }

    /// Make sure scrollback lines `line` and around are cached.
    pub fn fetch_history(&mut self, tab: u64, line: u64) {
        let from = line.saturating_sub(300);
        let r = self.ask_server("", Request::History { tab, from: Some(from), count: 600 });
        if let Some(Response::Ok(Output::History(h))) = r
            && let Some(v) = self.views.get_mut(&tab)
        {
            v.history.insert(h);
            v.history.trim(line);
        }
    }

    fn compose_tab(&mut self, tab: u64, height: usize) -> Vec<Place> {
        for _ in 0..8 {
            let Some(v) = self.views.get(&tab) else { return Vec::new() };
            match model::compose(&v.screen, &v.history, v.pos, height, v.columns) {
                Composed::Rows(r) => return r,
                Composed::Need(n) => {
                    let before = v.history.len();
                    self.fetch_history(tab, n);
                    if self.views.get(&tab).is_none_or(|v| v.history.len() == before) {
                        // The server has no such line any more: go back to live.
                        if let Some(v) = self.views.get_mut(&tab) {
                            v.pos = ViewPos::Live;
                        }
                    }
                }
            }
        }
        Vec::new()
    }

    /// The chrome's fonts at a scale.
    pub fn chrome_fonts(&mut self, scale: f64) -> ChromeFonts {
        ChromeFonts {
            label: self.fonts.ui(12.5 * scale, false),
            label_bold: self.fonts.ui(12.5 * scale, true),
            small: self.fonts.ui(10.5 * scale, false),
            title: self.fonts.ui(16.0 * scale, true),
        }
    }

    /// Colours for window `w`'s chrome: from the theme of the tab it shows.
    pub fn chrome_colors(&mut self, w: u64) -> Colors {
        let tab = self.focused_tab(w);
        let theme = match tab {
            Some(t) => self.tab_theme(t),
            None => {
                let name = self.wins.get(&w).and_then(|x| x.theme_name.clone());
                self.theme(name.as_deref())
            }
        };
        ui::colors(&theme)
    }

    fn tab_items(&mut self, w: u64) -> Vec<TabItem> {
        let now = Instant::now();
        let Some(info) = self.window_info(w).cloned() else { return Vec::new() };
        let Some(win) = self.wins.get(&w) else { return Vec::new() };
        let tiled: Vec<u64> = win.tiles.as_ref().map(|n| n.tabs()).unwrap_or_default();
        let (focused, selected, has_tiles) = (win.focused, win.selected, win.tiles.is_some());
        info.tabs
            .iter()
            .map(|t| {
                let badge = self
                    .touched
                    .get(&t.number)
                    .and_then(|(at, typed)| (now.duration_since(*at).as_secs() < BADGE_SECONDS).then_some(*typed));
                let (label, path) = super::tab_lines(t);
                let background = self.tab_theme(t.number).background;
                let themed = self.state.tab_themes.contains_key(&t.number);
                TabItem {
                    tab: t.number,
                    label,
                    path,
                    title: t.title.clone(),
                    agent: (t.opened_by != "you").then(|| super::agent_name(&t.opened_by)).or_else(|| {
                        t.running
                            .as_deref()
                            .map(super::program_of)
                            .filter(|p| super::AGENTS.contains(p))
                            .map(str::to_string)
                    }),
                    launcher: t.permissions_of.clone(),
                    badge,
                    selected: focused == Some(t.number) || (!has_tiles && selected == Some(t.number)),
                    tiled: tiled.contains(&t.number),
                    status: self.statuses.get(&t.number).copied().unwrap_or(Status::Idle),
                    background,
                    themed,
                }
            })
            .collect()
    }

    /// What a panel needs to lay itself out, for window `w`.
    pub fn with_panel_ctx<R>(&mut self, w: u64, f: impl FnOnce(&panels::Panel, &panels::Ctx) -> R) -> Option<R> {
        let scale = self.wins.get(&w)?.scale;
        let fonts = self.chrome_fonts(scale);
        let col = self.chrome_colors(w);
        let mono = self.tab_fonts(0, scale);
        let bgs: HashMap<u64, render::Rgb> = self
            .list
            .windows
            .iter()
            .flat_map(|x| x.tabs.iter().map(|t| t.number))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| (t, self.tab_theme(t).background))
            .collect();
        let default_bg = self.theme(None).background;
        let tab_bg = move |t: u64| bgs.get(&t).copied().unwrap_or(default_bg);
        let label = |t: &TabInfo| super::tab_lines(t);
        let agent_window = self.agent_window();
        let win = self.wins.get(&w)?;
        let p = win.panel.as_ref()?;
        let ctx = panels::Ctx {
            list: &self.list,
            activity: &self.activity,
            statuses: &self.statuses,
            tab_bg: &tab_bg,
            label: &label,
            fonts: &fonts,
            mono,
            col: &col,
            scale,
            focused_field: win.focus == Focus::Panel && win.key,
            agent_window,
            processes: &self.processes,
        };
        Some(f(p, &ctx))
    }

    /// The panel's area in window `w` (points): everything under the bar.
    pub fn panel_area(&self, w: u64) -> Option<Rect> {
        let win = self.wins.get(&w)?;
        let (cw, ch) = plat::content_size(&win.views);
        Some(Rect::new(0.0, win.bar.height, cw, (ch - win.bar.height).max(0.0)))
    }

    pub fn draw_window(&mut self, w: u64) {
        let Some(win) = self.wins.get(&w) else { return };
        if !win.visible {
            return;
        }
        let scale = win.scale;
        let (cw, ch) = plat::content_size(&win.views);
        let (pw, ph) = ((cw * scale).round().max(1.0) as usize, (ch * scale).round().max(1.0) as usize);
        let rects = win.tile_rects.clone();
        let term_rect = win.term_rect;
        let focused = win.focused;
        let key = win.key && self.active;
        let hint = win.drop_hint;
        let hover = win.hover_link;
        let overlay = win.menu.is_some() || win.dialog.is_some() || hint.is_some();
        let has_panel = win.panel.is_some();
        let mut dirty = win.dirty.clone();
        let mut chrome_dirty = win.chrome_dirty;
        // A buffer of the right size.
        let need_new = win.surface.as_ref().is_none_or(|s| s.width != pw || s.height != ph);
        if need_new {
            let Some(s) = plat::new_surface(&win.views, pw, ph) else { return };
            self.wins.get_mut(&w).unwrap().surface = Some(s);
            dirty = Some(Dirty::All);
        }
        // Menus, dialogs and drop hints sit over the terminal: redraw it all under them.
        if overlay && dirty.is_some() {
            dirty = Some(Dirty::All);
        }
        if dirty == Some(Dirty::All) {
            chrome_dirty = true;
        }
        let caret_on = self.blink_on;
        // Chrome.
        let fonts = self.chrome_fonts(scale);
        let col = self.chrome_colors(w);
        let items = if chrome_dirty { self.tab_items(w) } else { Vec::new() };
        // Terminal tiles.
        let mut prepared: Vec<(u64, Rect, Rc<render::Fonts>, Vec<(Row, bool)>, Vec<Place>)> = Vec::new();
        if dirty.is_some() && !has_panel {
            for (t, r) in &rects {
                let fonts = self.tab_fonts(*t, scale);
                let height = render::grid_size(&fonts, r.w * scale, r.h * scale).1 as usize;
                let places = self.compose_tab(*t, height);
                let Some(v) = self.views.get(t) else { continue };
                let mut rows = Vec::with_capacity(places.len());
                let mut wrapped: HashMap<u64, Vec<Row>> = HashMap::new();
                for p in &places {
                    match p {
                        Place::Screen(y) => {
                            rows.push((v.screen.lines.get(*y as usize).cloned().unwrap_or_default(), false))
                        }
                        Place::History { line, sub } => {
                            let rs = wrapped
                                .entry(*line)
                                .or_insert_with(|| v.history.get(*line).map(|r| r.wrap(v.columns)).unwrap_or_default());
                            rows.push((rs.get(*sub).cloned().unwrap_or_default(), true));
                        }
                    }
                }
                prepared.push((*t, *r, fonts, rows, places));
            }
        }
        let mut themes = HashMap::new();
        for (t, ..) in &prepared {
            themes.insert(*t, self.tab_theme(*t));
        }
        let mut frames: Vec<TileFrame> = Vec::new();
        for (t, r, fonts, rows, places) in &prepared {
            let v = &self.views[t];
            let theme = &themes[t];
            let src = TabSource { view: v, history: &v.history };
            let mut selection = Vec::new();
            if let Some(sel) = v.selection {
                let (a, b) = sel.range(&src);
                for (i, place) in places.iter().enumerate() {
                    if *place < a.place || *place > b.place {
                        continue;
                    }
                    let from = if *place == a.place { a.column } else { 0 };
                    let to = if *place == b.place { b.column } else { usize::MAX / 2 };
                    selection.push((i, from, to.min(4000)));
                }
            }
            let mut matches = Vec::new();
            if let Some(f) = &v.find {
                for (mi, m) in f.matches.iter().enumerate() {
                    for (place, col_a, col_b) in locate_match(v, m) {
                        if let Some(row) = places.iter().position(|x| *x == place) {
                            matches.push((row, col_a, col_b, f.current == Some(mi)));
                        }
                    }
                }
            }
            let live = v.pos == ViewPos::Live;
            let cursor = if live && v.screen.cursor_visible && (self.blink_on || !v.screen.cursor_blink || !key) {
                v.screen.cursor.map(|(x, y)| CursorFrame {
                    column: x as usize,
                    // The cursor's row in what's drawn (scrollback rows may be above the screen).
                    row: places.iter().position(|p| *p == Place::Screen(y)).unwrap_or(usize::MAX),
                    shape: v.screen.cursor_shape,
                    focused: key && focused == Some(*t),
                    color: theme.cursor,
                })
            } else {
                None
            };
            let link = hover.filter(|h| h.0 == *t).map(|h| (h.1, h.2, h.3));
            frames.push(TileFrame {
                x: (r.x * scale).round(),
                y: (r.y * scale).round(),
                w: (r.w * scale).round(),
                h: (r.h * scale).round(),
                fonts: fonts.clone(),
                rows: rows.iter().map(|(r, m)| (r, *m)).collect(),
                foreground: if v.screen.received { v.screen.foreground } else { theme.foreground },
                background: if v.screen.received { v.screen.background } else { theme.background },
                selection_color: theme.selection,
                cursor,
                selection,
                matches,
                link,
                marked: if focused == Some(*t) { v.marked.clone() } else { None },
                scrolled: (!live).then(|| model::scroll_fraction(&v.screen, v.pos)),
            });
        }
        let damage = match &dirty {
            Some(Dirty::All) | None => Damage::All,
            Some(Dirty::Rows(set)) => Damage::Rows(
                set.iter()
                    .filter_map(|(tab, row)| prepared.iter().position(|p| p.0 == *tab).map(|i| (i, *row)))
                    .collect(),
            ),
        };
        let divider = col.line;
        // Panel layout (before borrowing the window mutably).
        let panel_items = if has_panel && chrome_dirty || has_panel && dirty.is_some() {
            let area = self.panel_area(w).unwrap_or(term_rect);
            self.with_panel_ctx(w, |p, ctx| panels::layout(p, area, ctx))
        } else {
            None
        };
        let panel_ctx_bits = if panel_items.is_some() {
            Some((self.chrome_fonts(scale), self.chrome_colors(w), self.tab_fonts(0, scale)))
        } else {
            None
        };
        let statuses = self.statuses.clone();
        let list = self.list.clone();
        let activity = self.activity.clone();
        let agent_window = self.agent_window();
        let processes = self.processes.clone();
        let bgs: HashMap<u64, render::Rgb> = list
            .windows
            .iter()
            .flat_map(|x| x.tabs.iter().map(|t| t.number))
            .collect::<Vec<_>>()
            .into_iter()
            .map(|t| (t, self.tab_theme(t).background))
            .collect();
        let default_bg = self.theme(None).background;
        let win = self.wins.get_mut(&w).unwrap();
        let find = win.find.clone();
        let find_focused = win.focus == Focus::Find && key;
        let reopen = win.reopen_offer.as_ref().map(|t| t.len());
        let paste = win.paste_offer.as_ref().map(|(_, t)| t.clone());
        let hover_hit = win.hover;
        let drag = win.tab_press.filter(|p| p.dragging).map(|p| (p.index, p.x));
        let full = matches!(dirty, Some(Dirty::All));
        let surface = win.surface.as_mut().unwrap();
        surface.paint(|c| {
            // Terminal.
            if dirty.is_some() && !has_panel {
                let area = (term_rect.x * scale, term_rect.y * scale, term_rect.w * scale, term_rect.h * scale);
                render::draw_tiles(c, area, &frames, &damage, divider);
            }
            if chrome_dirty {
                let frame = BarFrame {
                    layout: &win.bar,
                    items: &items,
                    hover: hover_hit,
                    dragging: drag,
                    active: key,
                    width: cw,
                };
                chrome::draw_bar(c, &frame, &fonts, &col, scale);
                let mut strip_y = frame.layout.height;
                if let Some(n) = reopen {
                    let l = chrome::reopen_layout(cw, strip_y);
                    let text = format!("Reopen {n} {} from last time?", if n == 1 { "tab" } else { "tabs" });
                    chrome::draw_reopen(c, &l, &text, hover_hit, &fonts, &col, scale);
                    strip_y += chrome::STRIP_H;
                }
                if let Some(t) = &paste {
                    let l = chrome::paste_layout(cw, strip_y);
                    chrome::draw_paste(c, &l, t, hover_hit, &fonts, &col, scale);
                }
                if let Some(f) = &find
                    && !has_panel
                {
                    let l = chrome::find_layout(cw, ch - chrome::STRIP_H);
                    chrome::draw_find(
                        c,
                        &l,
                        &f.field,
                        &f.status,
                        find_focused,
                        hover_hit,
                        &fonts,
                        &col,
                        scale,
                        caret_on,
                    );
                }
            }
            // A panel over the terminal.
            if let (Some(items), Some((pf, pc, mono)), Some(p)) = (&panel_items, &panel_ctx_bits, &win.panel) {
                let tab_bg = |t: u64| bgs.get(&t).copied().unwrap_or(default_bg);
                let label = |t: &TabInfo| super::tab_lines(t);
                let ctx = panels::Ctx {
                    list: &list,
                    activity: &activity,
                    statuses: &statuses,
                    tab_bg: &tab_bg,
                    label: &label,
                    fonts: pf,
                    mono: mono.clone(),
                    col: pc,
                    scale,
                    focused_field: win.focus == Focus::Panel && key,
                    agent_window,
                    processes: &processes,
                };
                panels::draw(c, p, items, &ctx, caret_on);
            }
            if let Some(h) = hint {
                c.fill(col.accent, 0.35, h.x * scale, h.y * scale, h.w * scale, h.h * scale);
            }
            if let Some(m) = &win.menu {
                let (frame, rows) = m.layout(&fonts.label, &fonts.small, scale, cw, ch);
                ui::draw_menu(c, m, frame, &rows, &fonts.label, &fonts.small, &col, scale);
            }
            if let Some(d) = &win.dialog {
                let l = d.layout(&fonts.title, &fonts.label, scale, cw, ch);
                ui::draw_dialog(
                    c,
                    d,
                    &l,
                    &fonts.title,
                    &fonts.label,
                    &col,
                    scale,
                    Rect::new(0.0, 0.0, cw, ch),
                    caret_on,
                );
            }
        });
        let _ = full;
        win.dirty = None;
        win.chrome_dirty = false;
        plat::present(&win.views, win.surface.as_ref().unwrap(), scale);
        // CTX_APP_DUMP=<folder>: save each window's pixels after drawing (debugging, tests).
        if let Some(dir) = std::env::var_os("CTX_APP_DUMP")
            && let Some(s) = win.surface.as_mut()
        {
            let _ = s.write_png(&std::path::Path::new(&dir).join(format!("window-{w}.png")));
        }
        // CTX_APP_INSPECT=<file>: what the app thinks is on screen, as JSON, after each draw.
        if let Some(path) = std::env::var_os("CTX_APP_INSPECT") {
            let _ = std::fs::write(path, serde_json::to_vec_pretty(&self.inspect()).unwrap_or_default());
        }
        if let Some((t, at)) = self.key_echo
            && rects.iter().any(|(x, _)| *x == t)
        {
            self.key_echo = None;
            // Bounded: the newest measurements are enough.
            if self.latencies.len() >= 1000 {
                self.latencies.drain(..500);
            }
            self.latencies.push(at.elapsed());
        }
        // Remember what's drawn where, for the next partial update.
        for (t, _, _, _, places) in prepared {
            if let Some(v) = self.views.get_mut(&t) {
                v.drawn = places;
            }
        }
    }

    /// The app's state as JSON, for tests and debugging (`CTX_APP_INSPECT`).
    pub fn inspect(&self) -> serde_json::Value {
        use serde_json::json;
        let rect = |r: &Rect| json!([r.x, r.y, r.w, r.h]);
        let wins: Vec<serde_json::Value> = self
            .wins
            .values()
            .map(|w| {
                json!({
                    "window": w.number,
                    "key": w.key,
                    "visible": w.visible,
                    "size": plat::content_size(&w.views),
                    "scale": w.scale,
                    "focus": format!("{:?}", w.focus),
                    "selected": w.selected,
                    "focused": w.focused,
                    "tiles": w.tiles,
                    "term": rect(&w.term_rect),
                    "tile_rects": w.tile_rects.iter().map(|(t, r)| json!({"tab": t, "rect": rect(r)})).collect::<Vec<_>>(),
                    "bar": {
                        "tabs": w.bar.tabs.iter().map(|(t, r)| json!({"tab": t, "rect": rect(r)})).collect::<Vec<_>>(),
                        "plus": rect(&w.bar.plus),
                        "buttons": w.bar.buttons.iter().map(|(b, r)| json!({"button": format!("{b:?}"), "rect": rect(r)})).collect::<Vec<_>>(),
                        "scroll": w.bar_scroll,
                    },
                    "hover": w.hover.map(|h| format!("{h:?}")),
                    "panel": w.panel.as_ref().map(|p| json!({"kind": format!("{:?}", p.kind), "field": p.field.text, "selected": p.selected})),
                    "menu": w.menu.as_ref().map(|m| m.items.iter().map(|i| i.label.clone()).collect::<Vec<_>>()),
                    "dialog": w.dialog.as_ref().map(|d| d.title.clone()),
                    "find": w.find.as_ref().map(|f| json!({"text": f.field.text, "status": f.status})),
                    "reopen_offer": w.reopen_offer,
                })
            })
            .collect();
        let tabs: Vec<serde_json::Value> = self
            .views
            .iter()
            .map(|(t, v)| {
                json!({
                    "tab": t,
                    "grid": [v.screen.columns, v.screen.rows],
                    "cursor": v.screen.cursor,
                    "scrolled": v.pos != ViewPos::Live,
                    "selection": v.selection.is_some(),
                    "status": self.statuses.get(t).map(|s| format!("{s:?}")),
                    "theme": self.state.tab_themes.get(t),
                    "screen": v.screen.lines.iter().map(|r| r.text()).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "windows": wins,
            "tabs": tabs,
            "starters": self.starters.iter().map(|s| json!({"folder": s.folder, "command": s.command, "count": s.count})).collect::<Vec<_>>(),
            "connected": self.client.is_some(),
        })
    }

    /// Hit-test window `w`'s chrome and strips (points in its content).
    pub fn chrome_hit(&self, w: u64, x: f64, y: f64) -> Hit {
        let Some(win) = self.wins.get(&w) else { return Hit::Term };
        let (cw, ch) = plat::content_size(&win.views);
        if y < win.bar.height {
            return win.bar.hit(x, y);
        }
        let mut strip_y = win.bar.height;
        if win.reopen_offer.is_some() {
            if let Some(h) = chrome::reopen_layout(cw, strip_y).hit(x, y) {
                return h;
            }
            strip_y += chrome::STRIP_H;
        }
        if win.paste_offer.is_some()
            && let Some(h) = chrome::paste_layout(cw, strip_y).hit(x, y)
        {
            return match h {
                Hit::ReopenYes => Hit::PasteYes,
                Hit::ReopenNo => Hit::PasteNo,
                other => other,
            };
        }
        if win.find.is_some()
            && win.panel.is_none()
            && let Some(h) = chrome::find_layout(cw, ch - chrome::STRIP_H).hit(x, y)
        {
            return h;
        }
        Hit::Term
    }
}

/// Where a find match is drawn: (place, from column, to column) on each row it covers.
pub fn locate_match(v: &super::TabView, m: &TabMatch) -> Vec<(Place, usize, usize)> {
    let rows: Vec<(Place, Row)> = match m.place {
        MatchPlace::Screen(y) => {
            let Some(r) = v.screen.lines.get(y as usize) else { return Vec::new() };
            vec![(Place::Screen(y), r.clone())]
        }
        MatchPlace::History(line) => {
            let Some(r) = v.history.get(line) else { return Vec::new() };
            r.wrap(v.columns).into_iter().enumerate().map(|(i, w)| (Place::History { line, sub: i }, w)).collect()
        }
    };
    let measured = matches!(m.place, MatchPlace::History(_));
    // Walk the rows' cells, counting characters.
    let mut out = Vec::new();
    let mut chars = 0usize;
    for (place, r) in rows {
        let mut from: Option<usize> = None;
        let mut to = 0usize;
        for c in r.cells(measured) {
            let n = c.text.chars().count();
            let (a, b) = (chars, chars + n);
            if b > m.start as usize && a < m.end as usize {
                from.get_or_insert(c.column);
                to = c.column + c.width;
            }
            chars = b;
        }
        if let Some(f) = from {
            out.push((place, f, to));
        }
    }
    out
}

/// The chrome's label font (fields measure with it).
pub fn label_font(fc: &mut render::FontCache, scale: f64) -> Rc<render::UiFont> {
    fc.ui(12.5 * scale, false)
}

/// The fonts menus are drawn with: label, small.
pub fn menu_fonts(fc: &mut render::FontCache, scale: f64) -> (Rc<render::UiFont>, Rc<render::UiFont>) {
    (fc.ui(12.5 * scale, false), fc.ui(10.5 * scale, false))
}
