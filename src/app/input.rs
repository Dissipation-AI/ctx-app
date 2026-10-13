//! Keyboard and mouse. A key or click goes to whatever is on top: a dialog, then a menu, then
//! a panel or the find bar's field (when focused), then the tab bar, then the terminal. The
//! platform layer calls these with points in the window's content (top-left origin).

use super::chrome::{self, Hit};
use super::panels::{self, Kind, PanelHit};
use super::ui::{self, FieldEvent, FieldKey, MenuAction};
use super::{App, Dirty, Focus, TabPress, TabSource, plat};
use crate::keys::{self, MetaKeys, Press};
use crate::layout::{self, Edge, Rect};
use crate::model::{self, History, Place, ViewPos};
use crate::render;
use crate::selection::{self, Point, Selection, Unit};
use ctx_protocol::*;
use std::time::Instant;

/// Pointer shapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    Text,
    Pointer,
    Grab,
}

impl App {
    // -----------------------------------------------------------------------------------------
    // Keys
    // -----------------------------------------------------------------------------------------

    /// A key press in a window: what to do with it.
    pub fn key_down(&mut self, w: u64, p: &Press) -> keys::Input {
        let Some(win) = self.wins.get_mut(&w) else { return keys::Input::TextInput };
        // A dialog takes every key.
        if let Some(d) = win.dialog.as_mut() {
            let Some(k) = ui::field_key(p) else { return keys::Input::TextInput };
            let ev = match d.field.as_mut() {
                Some(f) => f.key(k.clone()),
                None => match k {
                    FieldKey::Enter { shift } => FieldEvent::Submit { shift },
                    FieldKey::Escape => FieldEvent::Cancel,
                    _ => FieldEvent::None,
                },
            };
            let n = d.buttons.len();
            win.dirty = Some(Dirty::All);
            match ev {
                FieldEvent::Submit { .. } => self.dialog_done(w, Some(0)),
                FieldEvent::Cancel => self.dialog_done(w, if n >= 2 { Some(n - 1) } else { None }),
                _ => self.redraw_dirty(),
            }
            return keys::Input::Handled;
        }
        if let Some(m) = win.menu.as_mut() {
            match ui::field_key(p) {
                Some(FieldKey::Up) => m.step(false),
                Some(FieldKey::Down) => m.step(true),
                Some(FieldKey::Escape) => win.menu = None,
                Some(FieldKey::Enter { .. }) => {
                    let action = m.hover.and_then(|i| m.items.get(i)).and_then(|it| it.action.clone());
                    win.menu = None;
                    win.dirty = Some(Dirty::All);
                    if let Some(a) = action {
                        self.menu_chose(w, a);
                    }
                    self.redraw_dirty();
                    return keys::Input::Handled;
                }
                _ => {}
            }
            win.dirty = Some(Dirty::All);
            self.redraw_dirty();
            return keys::Input::Handled;
        }
        match win.focus {
            Focus::Find | Focus::Panel => {
                if let Some(k) = ui::field_key(p) {
                    self.field_key(w, k);
                    return keys::Input::Handled;
                }
                return keys::Input::TextInput;
            }
            Focus::Term => {}
        }
        if win.paste_offer.is_some() {
            match ui::field_key(p) {
                Some(FieldKey::Enter { .. }) => self.paste_offer(w, true),
                Some(FieldKey::Escape) => self.paste_offer(w, false),
                _ => {}
            }
            return keys::Input::Handled;
        }
        if win.panel.is_some() {
            // The terminal is covered: keys reach a panel's field, or Escape closes it.
            if ui::field_key(p) == Some(FieldKey::Escape) {
                self.close_panel(w);
            }
            return keys::Input::Handled;
        }
        let Some(tab) = self.focused_tab(w) else { return keys::Input::TextInput };
        let meta = MetaKeys { left: self.loaded.app.left_option_is_meta, right: self.loaded.app.right_option_is_meta };
        let composing = self.views.get(&tab).is_some_and(|v| v.marked.is_some());
        let input = keys::map(p, meta, composing);
        if let keys::Input::Key { key, text } = &input {
            self.go_live(tab);
            self.key_sent = Some((tab, Instant::now()));
            self.input(tab, AppInput::Key { key: key.clone(), text: text.clone() });
        }
        input
    }

    /// A key for the focused field (find bar or panel).
    fn field_key(&mut self, w: u64, k: FieldKey) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        match win.focus {
            Focus::Find => {
                let Some(f) = win.find.as_mut() else { return };
                let ev = f.field.key(k);
                let text = f.field.text.clone();
                win.chrome_dirty = true;
                match ev {
                    FieldEvent::Changed => self.find_changed(w, text),
                    FieldEvent::Submit { shift } | FieldEvent::Tab { shift } => self.find_step(w, !shift),
                    FieldEvent::Down => self.find_step(w, true),
                    FieldEvent::Up => self.find_step(w, false),
                    FieldEvent::Cancel => self.close_find(w),
                    FieldEvent::None => {}
                }
            }
            Focus::Panel => {
                let Some(p) = win.panel.as_mut() else { return };
                let ev = p.field.key(k);
                win.dirty = Some(Dirty::All);
                match ev {
                    FieldEvent::Changed => {
                        p.scroll = [0.0; 2];
                        p.found = None;
                    }
                    FieldEvent::Submit { .. } => self.panel_submit(w),
                    FieldEvent::Cancel => self.close_panel(w),
                    FieldEvent::Down | FieldEvent::Up => {
                        let down = ev == FieldEvent::Down;
                        self.panel_step(w, down);
                    }
                    _ => {}
                }
            }
            Focus::Term => {}
        }
        self.ensure_ticker();
        self.redraw_dirty();
    }

    pub fn go_live(&mut self, tab: u64) {
        if let Some(v) = self.views.get_mut(&tab) {
            let mut changed = false;
            if v.pos != ViewPos::Live {
                v.pos = ViewPos::Live;
                changed = true;
            }
            if v.selection.is_some() {
                v.selection = None;
                changed = true;
            }
            if changed {
                self.dirty_tab(tab);
            }
        }
    }

    /// Text from the keyboard or an input method.
    pub fn insert_text(&mut self, w: u64, text: &str) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        if let Some(d) = win.dialog.as_mut() {
            if let Some(f) = d.field.as_mut() {
                f.insert(text);
                win.dirty = Some(Dirty::All);
                self.redraw_dirty();
            }
            return;
        }
        if win.menu.is_some() {
            return;
        }
        match win.focus {
            Focus::Find | Focus::Panel => {
                self.field_key(w, FieldKey::Text(text.to_string()));
                return;
            }
            Focus::Term if win.panel.is_some() => return,
            Focus::Term => {}
        }
        let Some(tab) = self.focused_tab(w) else { return };
        if let Some(v) = self.views.get_mut(&tab) {
            v.marked = None;
        }
        self.go_live(tab);
        // Enter from an input method arrives as text.
        let t = text.replace('\n', "\r");
        self.key_sent = Some((tab, Instant::now()));
        self.input(tab, AppInput::Text(t));
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn set_marked(&mut self, w: u64, text: Option<String>) {
        let Some(tab) = self.focused_tab(w) else { return };
        if let Some(v) = self.views.get_mut(&tab) {
            v.marked = text.filter(|t| !t.is_empty());
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn has_marked(&self, w: u64) -> bool {
        self.focused_tab(w).and_then(|t| self.views.get(&t)).is_some_and(|v| v.marked.is_some())
    }

    /// The cursor's rectangle (points, in the window's content), for input method windows.
    pub fn cursor_rect(&mut self, w: u64) -> Option<Rect> {
        let tab = self.focused_tab(w)?;
        let win = self.wins.get(&w)?;
        let scale = win.scale;
        let (_, r) = *win.tile_rects.iter().find(|(t, _)| *t == tab)?;
        let fonts = self.tab_fonts(tab, scale);
        let v = self.views.get(&tab)?;
        let (x, y) = v.screen.cursor?;
        let row = v.drawn.iter().position(|p| *p == Place::Screen(y)).unwrap_or(y as usize);
        Some(Rect::new(
            r.x + render::PAD_X + x as f64 * fonts.cell_w / scale,
            r.y + render::PAD_Y + row as f64 * fonts.cell_h / scale,
            fonts.cell_w / scale,
            fonts.cell_h / scale,
        ))
    }

    // -----------------------------------------------------------------------------------------
    // The terminal under the pointer
    // -----------------------------------------------------------------------------------------

    /// The tab, place and column under a point (points, in the window's content).
    pub fn hit(&mut self, w: u64, x: f64, y: f64) -> Option<(u64, Place, usize, Rect)> {
        let win = self.wins.get(&w)?;
        let scale = win.scale;
        let (tab, r) = *win.tile_rects.iter().find(|(_, r)| r.contains(x, y))?;
        let fonts = self.tab_fonts(tab, scale);
        let (col, row) = render::cell_at(&fonts, (x - r.x) * scale, (y - r.y) * scale);
        let v = self.views.get(&tab)?;
        let place = v.drawn.get(row).copied().or_else(|| v.drawn.last().copied())?;
        Some((tab, place, col, r))
    }

    // -----------------------------------------------------------------------------------------
    // Mouse
    // -----------------------------------------------------------------------------------------

    pub fn mouse_down(&mut self, w: u64, x: f64, y: f64, clicks: isize, flags: u64, button: u8) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        // A dialog is modal.
        if win.dialog.is_some() {
            if button == 1 {
                self.dialog_click(w, x, y);
            }
            return;
        }
        // A click anywhere closes a menu; on an item it chooses it.
        if let Some(m) = win.menu.take() {
            win.dirty = Some(Dirty::All);
            let scale = win.scale;
            let (cw, ch) = plat::content_size(&win.views);
            let fonts = self.chrome_fonts(scale);
            let (_, rows) = m.layout(&fonts.label, &fonts.small, scale, cw, ch);
            if let Some(i) = m.item_at(&rows, x, y)
                && let Some(a) = m.items[i].action.clone()
            {
                self.menu_chose(w, a);
            }
            self.redraw_dirty();
            return;
        }
        let hit = self.chrome_hit(w, x, y);
        // A panel covers the terminal.
        if hit == Hit::Term && self.wins.get(&w).is_some_and(|w| w.panel.is_some()) {
            if button == 1 {
                self.panel_down(w, x, y, clicks);
            }
            return;
        }
        if hit != Hit::Term && hit != Hit::FindField {
            self.set_focus(w, Focus::Term);
        }
        match hit {
            Hit::Tab { tab, index, close } => {
                if button == 2 {
                    self.tab_menu(w, tab, x, y);
                    return;
                }
                if button == 3 {
                    // Middle click closes, as in browsers.
                    self.close_tab(tab, false);
                    return;
                }
                if close && self.wins.get(&w).is_some_and(|win| matches!(win.hover, Some(Hit::Tab { index: i, .. }) if i == index)) {
                    self.close_tab(tab, false);
                    return;
                }
                // A click on the activity badge shows what touched the tab.
                let badge = self.touched.get(&tab).is_some_and(|(at, _)| at.elapsed().as_secs() < super::BADGE_SECONDS);
                let on_badge = self.wins.get(&w).and_then(|win| win.bar.tabs.get(index)).is_some_and(|(_, r)| chrome::badge_rect(*r).contains(x, y));
                if badge && on_badge {
                    self.open_panel(w, Kind::Activity);
                    return;
                }
                if clicks == 2 {
                    self.rename_dialog(tab);
                    return;
                }
                let previous = self.focused_tab(w).filter(|t| *t != tab);
                if let Some(win) = self.wins.get_mut(&w) {
                    win.tab_press = Some(TabPress { tab, index, x, y, dragging: false, previous });
                }
                if self.focused_tab(w) != Some(tab) {
                    self.select_tab(w, tab);
                }
                return;
            }
            Hit::Plus => {
                self.plus_menu(w, x, y);
                return;
            }
            Hit::Button(b) => {
                let kind = match b {
                    chrome::Button::Overview => Kind::Overview,
                    chrome::Button::History => Kind::History,
                    chrome::Button::Activity => Kind::Activity,
                };
                self.toggle_panel(w, kind);
                return;
            }
            Hit::ScrollLeft | Hit::ScrollRight => {
                let d = if hit == Hit::ScrollLeft { -160.0 } else { 160.0 };
                self.scroll_bar(w, d);
                return;
            }
            Hit::Bar => {
                if let Some(win) = self.wins.get(&w) {
                    if clicks == 2 {
                        plat::title_double_click(&win.views);
                    } else if button == 1 {
                        plat::drag_window(&win.views);
                    }
                }
                return;
            }
            Hit::FindField => {
                self.set_focus(w, Focus::Find);
                if let Some(win) = self.wins.get_mut(&w)
                    && let Some(f) = win.find.as_mut()
                {
                    let fonts = super::draw::label_font(&mut self.fonts, win.scale);
                    let l = chrome::find_layout(plat::content_size(&win.views).0, 0.0);
                    f.field.click(&fonts, (x - l.field.x - 8.0) * win.scale, flags & keys::SHIFT != 0);
                    win.chrome_dirty = true;
                }
                self.redraw_dirty();
                return;
            }
            Hit::FindPrev | Hit::FindNext => {
                self.find_step(w, hit == Hit::FindNext);
                return;
            }
            Hit::FindDone => {
                self.close_find(w);
                return;
            }
            Hit::ReopenYes | Hit::ReopenNo => {
                self.reopen_offer(w, hit == Hit::ReopenYes);
                return;
            }
            Hit::PasteYes | Hit::PasteNo => {
                self.paste_offer(w, hit == Hit::PasteYes);
                return;
            }
            Hit::Term => {}
        }
        self.term_mouse_down(w, x, y, clicks, flags, button);
    }

    fn term_mouse_down(&mut self, w: u64, x: f64, y: f64, clicks: isize, flags: u64, button: u8) {
        // Dividers between tiles.
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(t) = &win.tiles
            && let Some(path) = t.divider_at(win.term_rect, x, y, 3.0)
        {
            win.dragging_divider = Some(path);
            return;
        }
        let Some((tab, place, col, _)) = self.hit(w, x, y) else { return };
        // Focus that tile.
        if self.wins.get(&w).is_some_and(|win| win.focused != Some(tab)) {
            let win = self.wins.get_mut(&w).unwrap();
            win.focused = Some(tab);
            win.dirty = Some(Dirty::All);
            win.chrome_dirty = true;
            self.update_window_titles();
        }
        let tracking = self.views.get(&tab).is_some_and(|v| v.screen.mouse_tracking);
        if flags & plat::LINK_MODIFIER != 0 && button == 1 && self.link_at(tab, place, col).is_some() {
            if let Some(url) = self.link_at(tab, place, col) {
                plat::open_url(&url);
            }
            return;
        }
        if tracking && flags & keys::OPTION == 0 {
            if let Place::Screen(row) = place {
                self.mouse_report(tab, MouseAction::Press, button, col, row, flags);
            }
            return;
        }
        if button == 2 {
            self.term_menu(w, x, y);
            return;
        }
        if button != 1 {
            return;
        }
        let unit = match clicks {
            2 => Unit::Word,
            n if n >= 3 => Unit::Line,
            _ => Unit::Character,
        };
        let pt = Point { place, column: col };
        if let Some(v) = self.views.get_mut(&tab) {
            if flags & keys::SHIFT != 0
                && let Some(sel) = v.selection.as_mut()
            {
                sel.head = pt;
            } else if unit == Unit::Character {
                // A plain click clears; dragging makes a selection.
                v.selection = None;
                self.wins.get_mut(&w).unwrap().selecting = Some(tab);
                self.pending_anchor = Some((tab, pt));
            } else {
                v.selection = Some(Selection::new(tab, pt, unit));
                self.wins.get_mut(&w).unwrap().selecting = Some(tab);
            }
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn mouse_dragged(&mut self, w: u64, x: f64, y: f64, flags: u64, button: u8) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        // A tab being dragged along the bar, or out of it.
        if let Some(mut press) = win.tab_press {
            if !press.dragging && ((x - press.x).abs() > 4.0 || (y - press.y).abs() > 4.0) {
                press.dragging = true;
                plat::set_cursor(Cursor::Grab);
            }
            if press.dragging {
                // Along the bar the tab follows the pointer; away from it, it stays put.
                press.x = if y < chrome::BAR_H + 20.0 { x } else { win.bar.tabs.get(press.index).map(|(_, r)| r.x + r.w / 2.0).unwrap_or(x) };
                win.tab_press = Some(press);
                win.chrome_dirty = true;
                let (sx, sy) = plat::to_screen(&win.views, x, y);
                self.drag_hint(sx, sy);
                self.redraw_dirty();
            } else {
                win.tab_press = Some(press);
            }
            return;
        }
        if let Some(p) = win.panel.as_mut() {
            if let Some((tab, _, _, moved)) = p.drag {
                let moved = moved || true;
                p.drag = Some((tab, x, y, moved));
                win.dirty = Some(Dirty::All);
                self.panel_hover(w, x, y);
                self.redraw_dirty();
            }
            return;
        }
        if let Some(path) = win.dragging_divider.clone() {
            let area = win.term_rect;
            if let Some(t) = win.tiles.as_mut() {
                t.drag_divider(&path, area, x, y);
            }
            self.layout(w);
            self.redraw_dirty();
            return;
        }
        let Some(tab) = win.selecting.or(win.focused) else { return };
        let Some(r) = win.tile_rects.iter().find(|(t, _)| *t == tab).map(|x| x.1) else { return };
        let tracking = self.views.get(&tab).is_some_and(|v| v.screen.mouse_tracking);
        if tracking && flags & keys::OPTION == 0 && self.wins[&w].selecting.is_none() {
            if let Some((t, Place::Screen(row), col, _)) = self.hit(w, x, y)
                && t == tab
            {
                self.mouse_report(tab, MouseAction::Motion, button, col, row, flags);
            }
            return;
        }
        if self.wins[&w].selecting != Some(tab) {
            return;
        }
        // Dragging above or below the tile scrolls.
        let cols = self.views.get(&tab).map(|v| v.columns).unwrap_or(80);
        if y < r.y {
            self.scroll_rows(tab, 1, true, cols);
        } else if y > r.y + r.h {
            self.scroll_rows(tab, 1, false, cols);
        }
        self.redraw_dirty();
        let cx = x.clamp(r.x, r.x + r.w - 1.0);
        let cy = y.clamp(r.y, r.y + r.h - 1.0);
        let Some((t, place, col, _)) = self.hit(w, cx, cy) else { return };
        if t != tab {
            return;
        }
        let pt = Point { place, column: col };
        if let Some(v) = self.views.get_mut(&tab) {
            match v.selection.as_mut() {
                Some(sel) => sel.head = pt,
                None => {
                    if let Some((pt_tab, anchor)) = self.pending_anchor
                        && pt_tab == tab
                        && anchor != pt
                    {
                        let mut s = Selection::new(tab, anchor, Unit::Character);
                        s.head = pt;
                        v.selection = Some(s);
                    }
                }
            }
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn mouse_up(&mut self, w: u64, x: f64, y: f64, flags: u64, button: u8) {
        self.pending_anchor = None;
        let Some(win) = self.wins.get_mut(&w) else { return };
        if let Some(press) = win.tab_press.take() {
            win.chrome_dirty = true;
            if press.dragging {
                plat::set_cursor(Cursor::Arrow);
                let (sx, sy) = plat::to_screen(&win.views, x, y);
                self.drop_tab(press.tab, w, press.index, press.previous, sx, sy);
            }
            self.redraw_dirty();
            return;
        }
        if let Some(p) = win.panel.as_mut() {
            if let Some((tab, _, _, moved)) = p.drag.take() {
                win.dirty = Some(Dirty::All);
                let target = self.panel_hit(w, x, y);
                if !moved {
                    self.show_tab(tab, true);
                    self.close_panel_of_tab_window(w);
                } else {
                    match target {
                        Some(PanelHit::Window(to)) if self.window_of(tab) != Some(to) => {
                            self.move_tab(tab, WindowRef::Number(to), None);
                        }
                        Some(PanelHit::Card(other)) if other != tab => {
                            // Onto another tab: next to it, in its window.
                            if let Some(to) = self.window_of(other) {
                                let pos = self
                                    .window_info(to)
                                    .and_then(|i| i.tabs.iter().position(|t| t.number == other))
                                    .map(|i| i as u32 + 1);
                                self.move_tab(tab, WindowRef::Number(to), pos);
                            }
                        }
                        Some(PanelHit::NewWindow) => self.move_tab(tab, WindowRef::New, None),
                        _ => {}
                    }
                }
            }
            self.redraw_dirty();
            return;
        }
        if win.dragging_divider.take().is_some() {
            self.save_state();
            return;
        }
        win.selecting = None;
        if let Some((tab, Place::Screen(row), col, _)) = self.hit(w, x, y)
            && self.views.get(&tab).is_some_and(|v| v.screen.mouse_tracking)
            && flags & keys::OPTION == 0
        {
            self.mouse_report(tab, MouseAction::Release, button, col, row, flags);
        }
    }

    fn mouse_report(&mut self, tab: u64, action: MouseAction, button: u8, col: usize, row: u16, flags: u64) {
        self.input(
            tab,
            AppInput::Mouse(MouseInput {
                action,
                button,
                column: col.min(u16::MAX as usize) as u16,
                row,
                shift: flags & keys::SHIFT != 0,
                alt: flags & keys::OPTION != 0,
                ctrl: flags & keys::CONTROL != 0,
            }),
        );
    }

    pub fn mouse_moved(&mut self, w: u64, x: f64, y: f64, flags: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        // Hover in menus and dialogs.
        if let Some(m) = win.menu.as_mut() {
            let scale = win.scale;
            let (cw, ch) = plat::content_size(&win.views);
            let fonts = super::draw::menu_fonts(&mut self.fonts, scale);
            let (_, rows) = m.layout(&fonts.0, &fonts.1, scale, cw, ch);
            let h = m.item_at(&rows, x, y);
            if h != m.hover && h.is_some() {
                m.hover = h;
                win.dirty = Some(Dirty::All);
                self.redraw_dirty();
            }
            plat::set_cursor(Cursor::Arrow);
            return;
        }
        if win.dialog.is_some() {
            let b = self.dialog_button_at(w, x, y);
            if let Some(win) = self.wins.get_mut(&w)
                && let Some(d) = win.dialog.as_mut()
                && d.hover != b
            {
                d.hover = b;
                win.dirty = Some(Dirty::All);
                self.redraw_dirty();
            }
            plat::set_cursor(Cursor::Arrow);
            return;
        }
        let hit = self.chrome_hit(w, x, y);
        let in_panel = hit == Hit::Term && self.wins.get(&w).is_some_and(|w| w.panel.is_some());
        let win = self.wins.get_mut(&w).unwrap();
        let hover = (hit != Hit::Term && hit != Hit::Bar).then_some(hit);
        if win.hover != hover {
            win.hover = hover;
            win.chrome_dirty = true;
        }
        if in_panel {
            self.panel_hover(w, x, y);
            let field = self.panel_hit(w, x, y) == Some(PanelHit::Field);
            plat::set_cursor(if field { Cursor::Text } else { Cursor::Arrow });
            self.redraw_dirty();
            return;
        }
        if hit != Hit::Term {
            plat::set_cursor(if hit == Hit::FindField { Cursor::Text } else { Cursor::Arrow });
            if let Some(win) = self.wins.get_mut(&w)
                && win.hover_link.take().is_some()
            {
                win.dirty = Some(Dirty::All);
            }
            self.redraw_dirty();
            return;
        }
        // ⌘ (Ctrl on Linux) over a link underlines it.
        let mut link = None;
        if flags & plat::LINK_MODIFIER != 0
            && let Some((tab, place, col, _)) = self.hit(w, x, y)
            && let Some(row) = self.row_at(tab, place)
        {
            let measured = matches!(place, Place::History { .. });
            if let Some(l) = crate::links::at(&row, measured, col) {
                let drawn_row = self.views[&tab].drawn.iter().position(|p| *p == place).unwrap_or(0);
                link = Some((tab, drawn_row, l.start, l.end));
            }
        }
        // Dividers between tiles get a resize pointer; text an I-beam.
        let win = self.wins.get_mut(&w).unwrap();
        if win.hover_link != link {
            win.hover_link = link;
            win.dirty = Some(Dirty::All);
        }
        plat::set_cursor(if link.is_some() { Cursor::Pointer } else { Cursor::Text });
        self.redraw_dirty();
    }

    /// The pointer left the window.
    pub fn mouse_exited(&mut self, w: u64) {
        if let Some(win) = self.wins.get_mut(&w)
            && win.tab_press.is_none()
        {
            if win.hover.take().is_some() {
                win.chrome_dirty = true;
            }
            if let Some(p) = win.panel.as_mut()
                && p.hover.take().is_some()
            {
                win.dirty = Some(Dirty::All);
            }
            self.redraw_dirty();
        }
    }

    fn row_at(&self, tab: u64, place: Place) -> Option<crate::text::Row> {
        let v = self.views.get(&tab)?;
        selection::Source::row(&TabSource { view: v, history: &v.history }, place)
    }

    fn link_at(&self, tab: u64, place: Place, col: usize) -> Option<String> {
        let row = self.row_at(tab, place)?;
        crate::links::at(&row, matches!(place, Place::History { .. }), col).map(|l| l.url)
    }

    /// Scroll a tab's view by whole rows (`up`: towards older lines).
    pub fn scroll_rows(&mut self, tab: u64, k: usize, up: bool, columns: usize) {
        for _ in 0..4 {
            let Some(v) = self.views.get(&tab) else { return };
            let r = if up {
                model::scroll_up(&v.screen, &v.history, v.pos, k, columns)
            } else {
                model::scroll_down(&v.screen, &v.history, v.pos, k, columns)
            };
            match r {
                Ok(p) => {
                    let v = self.views.get_mut(&tab).unwrap();
                    if v.pos != p {
                        v.pos = p;
                        self.dirty_tab(tab);
                    }
                    return;
                }
                Err(line) => self.fetch_history(tab, line),
            }
        }
    }

    /// A scroll event: trackpads give points (`precise`), wheels give lines. Positive `dy`
    /// scrolls up (towards older lines).
    pub fn scroll_event(&mut self, w: u64, x: f64, y: f64, dx: f64, dy: f64, precise: bool, flags: u64) {
        let hit = self.chrome_hit(w, x, y);
        if y < chrome::BAR_H {
            // Over the tab bar: scroll the tabs sideways.
            let d = if dx.abs() > dy.abs() { -dx } else { -dy };
            self.scroll_bar(w, d * if precise { 1.0 } else { 40.0 });
            return;
        }
        if hit == Hit::Term && self.wins.get(&w).is_some_and(|w| w.panel.is_some()) {
            self.panel_scroll(w, x, if precise { dy } else { dy * 40.0 });
            return;
        }
        let rows = if precise {
            let Some((tab, ..)) = self.hit(w, x, y) else { return };
            let scale = self.wins.get(&w).map(|win| win.scale).unwrap_or(2.0);
            let fonts = self.tab_fonts(tab, scale);
            dy / (fonts.cell_h / scale)
        } else {
            dy
        };
        self.scroll_wheel(w, x, y, rows, flags);
    }

    /// A scroll wheel or trackpad: `dy` in rows (fractional for trackpads), positive = up.
    pub fn scroll_wheel(&mut self, w: u64, x: f64, y: f64, dy: f64, flags: u64) {
        let Some((tab, place, col, _)) = self.hit(w, x, y) else { return };
        let Some(v) = self.views.get_mut(&tab) else { return };
        v.scroll_rest += dy;
        let whole = v.scroll_rest.trunc();
        v.scroll_rest -= whole;
        let n = whole.abs() as usize;
        if n == 0 {
            return;
        }
        let up = whole > 0.0;
        let tracking = v.screen.mouse_tracking;
        let full = v.screen.full_screen;
        let columns = v.columns;
        if tracking && flags & keys::OPTION == 0 {
            if let Place::Screen(row) = place {
                for _ in 0..n.min(10) {
                    self.mouse_report(tab, MouseAction::Press, if up { 4 } else { 5 }, col, row, flags);
                }
            }
            return;
        }
        if full {
            // Full-screen programs without mouse reports get arrow keys (like other terminals).
            for _ in 0..n.min(10) {
                self.input(tab, AppInput::Key { key: if up { "up".into() } else { "down".into() }, text: None });
            }
            return;
        }
        self.scroll_rows(tab, n, up, columns);
        self.redraw_dirty();
    }

    fn scroll_bar(&mut self, w: u64, d: f64) {
        if let Some(win) = self.wins.get_mut(&w) {
            win.bar_scroll = (win.bar_scroll + d).max(0.0);
        }
        self.layout(w);
        if let Some(win) = self.wins.get_mut(&w) {
            // The layout clamps the scroll; keep what it settled on.
            win.bar_scroll = win.bar.tabs.first().map(|(_, r)| (win.bar.strip.x - r.x).max(0.0)).unwrap_or(0.0);
            win.chrome_dirty = true;
        }
        self.redraw_dirty();
    }

    // -----------------------------------------------------------------------------------------
    // Clipboard, drops
    // -----------------------------------------------------------------------------------------

    pub fn paste(&mut self, w: u64) {
        let Some(text) = plat::get_clipboard() else { return };
        if self.wins.get(&w).is_some_and(|win| win.dialog.is_some() || win.focus != Focus::Term) {
            let one_line = text.lines().next().unwrap_or("").to_string();
            self.insert_text(w, &one_line);
            return;
        }
        let Some(tab) = self.focused_tab(w) else { return };
        let v = self.views.get(&tab);
        let at_prompt = v.is_some_and(|v| v.screen.at_prompt && !v.screen.full_screen);
        let lines = text.trim_end_matches('\n').lines().count();
        if self.loaded.app.confirm_multiline_paste && at_prompt && lines > 1 {
            // A one-line preview over the terminal: Return (or Paste) pastes, Escape cancels.
            if let Some(win) = self.wins.get_mut(&w) {
                win.paste_offer = Some((tab, text));
                win.chrome_dirty = true;
            }
            self.layout(w);
            self.dirty_win(w);
            self.redraw_dirty();
            return;
        }
        self.go_live(tab);
        self.input(tab, AppInput::Paste(text));
    }

    /// Answer the paste preview.
    pub fn paste_offer(&mut self, w: u64, accept: bool) {
        let Some((tab, text)) = self.wins.get_mut(&w).and_then(|x| x.paste_offer.take()) else { return };
        if accept {
            self.go_live(tab);
            self.input(tab, AppInput::Paste(text));
        }
        self.layout(w);
        self.dirty_win(w);
        self.redraw_dirty();
    }

    pub fn drop_paths(&mut self, w: u64, paths: Vec<String>) {
        let Some(tab) = self.focused_tab(w) else { return };
        let quoted: Vec<String> = paths.iter().map(|p| super::shell_quote(p)).collect();
        self.go_live(tab);
        self.input(tab, AppInput::Text(quoted.join(" ") + " "));
    }

    /// The selected text, fetching whatever scrollback it spans.
    pub fn selected_text(&mut self, tab: u64) -> Option<String> {
        let sel = self.views.get(&tab)?.selection?;
        let first_line = match sel.anchor.place.min(sel.head.place) {
            Place::History { line, .. } => Some(line),
            Place::Screen(_) => None,
        };
        // A complete copy of the scrollback the selection covers.
        let mut full = History::default();
        if let Some(a) = first_line {
            let end = self.views[&tab].screen.history_end;
            let mut from = a;
            while from < end {
                let count = (end - from).min(10_000) as u32;
                match self.ask_server("", Request::History { tab, from: Some(from), count }) {
                    Some(Response::Ok(Output::History(h))) if !h.lines.is_empty() => {
                        from = h.from + h.lines.len() as u64;
                        full.insert(h);
                    }
                    _ => break,
                }
            }
        }
        let v = self.views.get(&tab)?;
        let src = TabSource { view: v, history: if first_line.is_some() { &full } else { &v.history } };
        Some(sel.text(&src))
    }

    pub fn copy(&mut self, w: u64) {
        if let Some(win) = self.wins.get(&w) {
            let field = match win.focus {
                Focus::Find => win.find.as_ref().map(|f| &f.field),
                Focus::Panel => win.panel.as_ref().map(|p| &p.field),
                Focus::Term => None,
            };
            if let Some(f) = field {
                if let Some(t) = f.selected_text() {
                    plat::set_clipboard(&t);
                }
                return;
            }
        }
        let Some(tab) = self.focused_tab(w) else { return };
        if let Some(t) = self.selected_text(tab).filter(|t| !t.is_empty()) {
            plat::set_clipboard(&t);
        }
    }

    pub fn select_all(&mut self, w: u64) {
        if self.wins.get(&w).is_some_and(|win| win.focus != Focus::Term) {
            self.field_key(w, FieldKey::SelectAll);
            return;
        }
        let Some(tab) = self.focused_tab(w) else { return };
        let Some(v) = self.views.get_mut(&tab) else { return };
        let start = if v.screen.history_end > v.screen.history_first {
            Place::History { line: v.screen.history_first, sub: 0 }
        } else {
            Place::Screen(0)
        };
        let end = Place::Screen(v.screen.rows.saturating_sub(1));
        let mut s = Selection::new(tab, Point { place: start, column: 0 }, Unit::Character);
        s.head = Point { place: end, column: usize::MAX / 4 };
        v.selection = Some(s);
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    // -----------------------------------------------------------------------------------------
    // Find in a tab
    // -----------------------------------------------------------------------------------------

    pub fn open_find(&mut self, w: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        if win.panel.is_some() {
            return;
        }
        if win.find.is_none() {
            win.find = Some(super::FindBar { field: ui::Field::new("Find in this tab"), status: String::new() });
        } else if let Some(f) = win.find.as_mut() {
            f.field.key(FieldKey::SelectAll);
        }
        win.focus = Focus::Find;
        win.chrome_dirty = true;
        self.layout(w);
        self.ensure_ticker();
        self.redraw_dirty();
    }

    pub fn close_find(&mut self, w: u64) {
        let Some(win) = self.wins.get_mut(&w) else { return };
        win.find = None;
        win.focus = Focus::Term;
        let tab = self.focused_tab(w);
        if let Some(v) = tab.and_then(|t| self.views.get_mut(&t)) {
            v.find = None;
        }
        self.layout(w);
        self.dirty_win(w);
        self.redraw_dirty();
    }

    pub fn find_changed(&mut self, w: u64, text: String) {
        let Some(tab) = self.focused_tab(w) else { return };
        if text.is_empty() {
            if let Some(v) = self.views.get_mut(&tab) {
                v.find = None;
            }
            self.dirty_tab(tab);
            self.set_find_status(w, "");
            self.redraw_dirty();
            return;
        }
        let r = self.ask_server("", Request::SearchTab { tab, text: text.clone(), limit: 2000 });
        let Some(Response::Ok(Output::TabMatches(m))) = r else { return };
        let Some(v) = self.views.get_mut(&tab) else { return };
        // Start at the last match (nearest the bottom), as Terminal.app does.
        let current = if m.matches.is_empty() { None } else { Some(m.matches.len() - 1) };
        v.find = Some(super::Find { text, matches: m.matches, current, more: m.more });
        self.reveal_match(tab);
        self.update_find_status(w, tab);
    }

    pub fn find_step(&mut self, w: u64, forward: bool) {
        let Some(tab) = self.focused_tab(w) else { return };
        let Some(f) = self.views.get_mut(&tab).and_then(|v| v.find.as_mut()) else { return };
        let n = f.matches.len();
        if n == 0 {
            return;
        }
        f.current = Some(match f.current {
            Some(i) if forward => (i + 1) % n,
            Some(i) => (i + n - 1) % n,
            None => 0,
        });
        self.reveal_match(tab);
        self.update_find_status(w, tab);
    }

    fn set_find_status(&mut self, w: u64, s: &str) {
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(f) = win.find.as_mut()
        {
            f.status = s.to_string();
            win.chrome_dirty = true;
        }
    }

    fn update_find_status(&mut self, w: u64, tab: u64) {
        let s = match self.views.get(&tab).and_then(|v| v.find.as_ref()) {
            Some(f) if f.matches.is_empty() => "Not found".to_string(),
            Some(f) => format!("{} of {}{}", f.current.map(|c| c + 1).unwrap_or(0), f.matches.len(), if f.more { "+" } else { "" }),
            None => String::new(),
        };
        self.set_find_status(w, &s);
        self.redraw_dirty();
    }

    /// Scroll so the current match is on screen.
    fn reveal_match(&mut self, tab: u64) {
        let Some(v) = self.views.get(&tab) else { return };
        let Some(f) = &v.find else { return };
        let Some(m) = f.current.and_then(|c| f.matches.get(c)) else { return };
        match m.place {
            MatchPlace::Screen(_) => {
                if let Some(v) = self.views.get_mut(&tab) {
                    v.pos = ViewPos::Live;
                }
            }
            MatchPlace::History(line) => {
                if !v.drawn.iter().any(|p| matches!(p, Place::History { line: l, .. } if *l == line)) {
                    if v.history.get(line).is_none() {
                        self.fetch_history(tab, line);
                    }
                    if let Some(v) = self.views.get_mut(&tab) {
                        v.pos = ViewPos::At { line, sub: 0 };
                        // A few lines of context above.
                        let cols = v.columns;
                        self.scroll_rows(tab, 3, true, cols);
                    }
                }
            }
        }
        self.dirty_tab(tab);
        self.redraw_dirty();
    }

    pub fn set_focus(&mut self, w: u64, f: Focus) {
        if let Some(win) = self.wins.get_mut(&w)
            && win.focus != f
        {
            win.focus = f;
            win.dirty = Some(Dirty::All);
        }
    }

    // -----------------------------------------------------------------------------------------
    // Tabs dragged out of the bar
    // -----------------------------------------------------------------------------------------

    /// A tab dragged from window `from`'s bar and released at a screen point.
    pub fn drop_tab(&mut self, tab: u64, from: u64, index: usize, previous: Option<u64>, sx: f64, sy: f64) {
        for win in self.wins.values_mut() {
            if win.drop_hint.take().is_some() {
                win.dirty = Some(Dirty::All);
            }
        }
        let target = plat::window_at(self, sx, sy);
        if let Some((w, lx, ly)) = target {
            // On a tab bar: reorder, or move to that window at that place.
            if ly < chrome::BAR_H {
                let Some(win) = self.wins.get(&w) else { return };
                let mut at = win.bar.insertion(lx);
                if w == from && at > index {
                    at -= 1;
                }
                if w == from && at == index {
                    return;
                }
                self.move_tab(tab, WindowRef::Number(w), Some(at as u32 + 1));
                if w != from {
                    self.want_select_after_move = Some((w, tab));
                }
                return;
            }
            // On the terminal: side by side with the tab under it (moving it there first).
            let target_tile = self.wins.get(&w).and_then(|win| win.tile_rects.iter().find(|(_, r)| r.contains(lx, ly)).copied());
            if let Some((mut target_tab, r)) = target_tile
                && let Some(edge) = layout::drop_edge(r, lx, ly)
            {
                if target_tab == tab {
                    // The press selected the dragged tab: tile it next to what showed before.
                    let Some(prev) = previous.filter(|_| w == from) else { return };
                    if let Some(win) = self.wins.get_mut(&w)
                        && win.tiles.is_none()
                    {
                        win.selected = Some(prev);
                        win.focused = Some(prev);
                    }
                    target_tab = prev;
                }
                if w != from {
                    self.move_tab(tab, WindowRef::Number(w), None);
                    self.pending_tile = Some((w, tab, target_tab, edge));
                } else {
                    self.tile(w, tab, Some(target_tab), edge);
                }
                return;
            }
            if w != from {
                self.move_tab(tab, WindowRef::Number(w), None);
                self.want_select_after_move = Some((w, tab));
            }
            return;
        }
        // Outside every window: a new window for it, where it was dropped. The only tab of a
        // window already has one.
        let only = self.window_info(from).is_some_and(|i| i.tabs.len() == 1);
        if only {
            if let Some(win) = self.wins.get(&from) {
                plat::move_window_top_left(&win.views, sx, sy);
            }
        } else {
            self.drop_at = Some((tab, sx, sy));
            self.move_tab(tab, WindowRef::New, None);
        }
    }

    /// While dragging a tab: show where it would land.
    pub fn drag_hint(&mut self, sx: f64, sy: f64) {
        let target = plat::window_at(self, sx, sy);
        for win in self.wins.values_mut() {
            let mut hint = None;
            if let Some((w, lx, ly)) = target
                && w == win.number
                && ly >= chrome::BAR_H
                && let Some((_, r)) = win.tile_rects.iter().find(|(_, r)| r.contains(lx, ly))
                && let Some(edge) = layout::drop_edge(*r, lx, ly)
            {
                hint = Some(match edge {
                    Edge::Left => Rect::new(r.x, r.y, r.w / 2.0, r.h),
                    Edge::Right => Rect::new(r.x + r.w / 2.0, r.y, r.w / 2.0, r.h),
                    Edge::Top => Rect::new(r.x, r.y, r.w, r.h / 2.0),
                    Edge::Bottom => Rect::new(r.x, r.y + r.h / 2.0, r.w, r.h / 2.0),
                });
            }
            if win.drop_hint != hint {
                win.drop_hint = hint;
                win.dirty = Some(Dirty::All);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Dialogs
    // -----------------------------------------------------------------------------------------

    fn dialog_button_at(&mut self, w: u64, x: f64, y: f64) -> Option<usize> {
        let win = self.wins.get(&w)?;
        win.dialog.as_ref()?;
        let scale = win.scale;
        let (cw, ch) = plat::content_size(&win.views);
        let fonts = self.chrome_fonts(scale);
        let win = self.wins.get(&w)?;
        let l = win.dialog.as_ref()?.layout(&fonts.title, &fonts.label, scale, cw, ch);
        l.buttons.iter().position(|r| r.contains(x, y))
    }

    fn dialog_click(&mut self, w: u64, x: f64, y: f64) {
        if let Some(b) = self.dialog_button_at(w, x, y) {
            self.dialog_done(w, Some(b));
        }
    }

    // -----------------------------------------------------------------------------------------
    // Panels: pointer
    // -----------------------------------------------------------------------------------------

    pub fn panel_hit(&mut self, w: u64, x: f64, y: f64) -> Option<PanelHit> {
        let area = self.panel_area(w)?;
        self.with_panel_ctx(w, |p, ctx| panels::hit_test(&panels::layout(p, area, ctx), x, y)).flatten()
    }

    fn panel_hover(&mut self, w: u64, x: f64, y: f64) {
        let h = self.panel_hit(w, x, y);
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(p) = win.panel.as_mut()
            && p.hover != h
        {
            p.hover = h;
            win.dirty = Some(Dirty::All);
        }
    }

    fn panel_down(&mut self, w: u64, x: f64, y: f64, clicks: isize) {
        let h = self.panel_hit(w, x, y);
        let focus = if h == Some(PanelHit::Field) { Focus::Panel } else { Focus::Term };
        self.set_focus(w, focus);
        if let Some(win) = self.wins.get_mut(&w) {
            win.dirty = Some(Dirty::All);
        }
        match h {
            Some(PanelHit::Close) => self.close_panel(w),
            Some(PanelHit::Field) => {
                let scale = self.wins.get(&w).map(|x| x.scale).unwrap_or(1.0);
                let font = super::draw::label_font(&mut self.fonts, scale);
                let area = self.panel_area(w);
                let fx = self.with_panel_ctx(w, |p, ctx| {
                    panels::layout(p, area.unwrap(), ctx).iter().find(|i| i.hit == Some(PanelHit::Field)).map(|i| i.rect.x)
                });
                if let Some(Some(fx)) = fx
                    && let Some(win) = self.wins.get_mut(&w)
                    && let Some(p) = win.panel.as_mut()
                {
                    p.field.click(&font, (x - fx - 8.0) * scale, false);
                }
                self.ensure_ticker();
            }
            Some(PanelHit::Who(who)) => {
                if let Some(p) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) {
                    p.who = who;
                    p.scroll[0] = 0.0;
                }
            }
            Some(PanelHit::When(when)) => {
                if let Some(p) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) {
                    p.when = when;
                    p.scroll[0] = 0.0;
                }
            }
            Some(PanelHit::Closed(t)) => {
                if clicks == 2 {
                    self.reopen(t);
                } else {
                    self.history_select(w, t);
                }
            }
            Some(PanelHit::Reopen(t)) => self.reopen(t),
            Some(PanelHit::Tab(t)) => {
                self.close_panel(w);
                self.show_tab(t, true);
            }
            Some(PanelHit::Found(i)) => self.found_chosen(w, i),
            Some(PanelHit::Card(t)) => {
                if let Some(p) = self.wins.get_mut(&w).and_then(|x| x.panel.as_mut()) {
                    p.drag = Some((t, x, y, false));
                }
            }
            Some(PanelHit::Window(_)) | Some(PanelHit::NewWindow) | None => {}
        }
        self.redraw_dirty();
    }

    fn panel_scroll(&mut self, w: u64, x: f64, dy: f64) {
        let Some(area) = self.panel_area(w) else { return };
        let heights = self.with_panel_ctx(w, |p, ctx| panels::content_height(p, ctx)).unwrap_or([0.0; 2]);
        if let Some(win) = self.wins.get_mut(&w)
            && let Some(p) = win.panel.as_mut()
        {
            let i = panels::scroll_region(p, area, x);
            let max = (heights[i] - area.h + 80.0).max(0.0);
            p.scroll[i] = (p.scroll[i] - dy).clamp(0.0, max);
            win.dirty = Some(Dirty::All);
        }
        self.redraw_dirty();
    }

    fn close_panel_of_tab_window(&mut self, w: u64) {
        if self.wins.get(&w).is_some_and(|x| x.panel.is_some()) {
            self.close_panel(w);
        }
    }

    /// A menu item was chosen in window `w`.
    pub fn menu_chose(&mut self, w: u64, a: MenuAction) {
        match a {
            MenuAction::Perform(action) => self.perform(action),
            MenuAction::NewTabIn(win) => self.new_tab(win),
            MenuAction::Launch { window, folder, command } => self.new_tab_with(window, Some(folder), Some(command)),
            MenuAction::Rename(t) => self.rename_dialog(t),
            MenuAction::Close(t) => self.close_tab(t, false),
            MenuAction::Tile(t) => {
                if let Some(win) = self.window_of(t) {
                    self.tile(win, t, None, Edge::Right);
                }
            }
            MenuAction::Untile(t) => {
                if let Some(win) = self.window_of(t) {
                    self.untile(win, t);
                }
            }
            MenuAction::MoveToNewWindow(t) => self.move_tab(t, WindowRef::New, None),
            MenuAction::Theme { tab, name } => self.set_tab_theme(tab, name),
            MenuAction::ShowTab(t) => self.show_tab(t, true),
            MenuAction::Reopen(t) => self.reopen(t),
        }
        let _ = w;
    }
}
