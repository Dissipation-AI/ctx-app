//! The pieces the app's own chrome is made of, drawn into the window's buffer like the terminal
//! text: colours taken from the theme, a one-line text field, pop-up menus and (on Linux) dialogs.
//! No native controls: each would cost a layer and its own pixel buffer.

use crate::keys::{self, Press};
use crate::layout::Rect;
use crate::render::{Canvas, Rgb, Trunc, UiFont, mix};
use crate::shortcuts::Action;
use crate::theme::Theme;

// ---------------------------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------------------------

/// Colours for the chrome, worked out from a theme so the bar suits the terminal under it.
#[derive(Debug, Clone, PartialEq)]
pub struct Colors {
    /// The strip the tabs sit on.
    pub bar: Rgb,
    /// An unselected tab.
    pub tab: Rgb,
    pub tab_hover: Rgb,
    pub text: Rgb,
    pub dim: Rgb,
    pub faint: Rgb,
    pub line: Rgb,
    pub accent: Rgb,
    /// Panels and menus.
    pub panel: Rgb,
    pub field: Rgb,
    pub hover: Rgb,
    pub selected: Rgb,
    pub typed: Rgb,
    pub read: Rgb,
    pub agent: Rgb,
    pub launcher: Rgb,
    pub busy: Rgb,
    pub waiting: Rgb,
    pub dark: bool,
}

pub fn colors(theme: &Theme) -> Colors {
    let bg = theme.background;
    let fg = theme.foreground;
    let dark = theme.dark;
    let (bar, tab, tab_hover, panel, field) = if dark {
        (
            mix(bg, [0, 0, 0], 0.38),
            mix(bg, [0, 0, 0], 0.22),
            mix(bg, [0, 0, 0], 0.08),
            mix(bg, [255, 255, 255], 0.05),
            mix(bg, [0, 0, 0], 0.25),
        )
    } else {
        (
            mix(bg, [0, 0, 0], 0.12),
            mix(bg, [0, 0, 0], 0.07),
            mix(bg, [0, 0, 0], 0.03),
            mix(bg, [0, 0, 0], 0.02),
            [255, 255, 255],
        )
    };
    Colors {
        bar,
        tab,
        tab_hover,
        text: mix(fg, bg, 0.05),
        dim: mix(fg, bg, 0.42),
        faint: mix(fg, bg, 0.65),
        line: mix(fg, bg, 0.82),
        accent: if dark { [122, 166, 218] } else { [10, 100, 220] },
        panel,
        field,
        hover: mix(fg, bg, 0.88),
        selected: theme.selection,
        typed: [240, 150, 40],
        read: [90, 150, 240],
        agent: [160, 130, 240],
        launcher: [60, 180, 130],
        busy: [120, 200, 120],
        waiting: [240, 190, 60],
        dark,
    }
}

// ---------------------------------------------------------------------------------------------
// Text fields
// ---------------------------------------------------------------------------------------------

/// A one-line text field: its text, the caret (a byte offset) and a selection anchor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Field {
    pub text: String,
    pub caret: usize,
    pub anchor: Option<usize>,
    pub placeholder: String,
}

/// Keys a field understands.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldKey {
    Text(String),
    Backspace,
    Delete,
    Left { select: bool, word: bool },
    Right { select: bool, word: bool },
    Home { select: bool },
    End { select: bool },
    SelectAll,
    Enter { shift: bool },
    Escape,
    Up,
    Down,
    Tab { shift: bool },
}

/// What a key did to a field.
#[derive(Debug, Clone, PartialEq)]
pub enum FieldEvent {
    None,
    Changed,
    Submit { shift: bool },
    Cancel,
    Up,
    Down,
    Tab { shift: bool },
}

impl Field {
    pub fn new(placeholder: &str) -> Field {
        Field { placeholder: placeholder.to_string(), ..Field::default() }
    }

    pub fn with_text(placeholder: &str, text: &str) -> Field {
        Field { text: text.to_string(), caret: text.len(), anchor: Some(0), placeholder: placeholder.to_string() }
    }

    pub fn set(&mut self, text: &str) {
        self.text = text.to_string();
        self.caret = self.text.len();
        self.anchor = None;
    }

    fn selection(&self) -> Option<(usize, usize)> {
        let a = self.anchor?;
        (a != self.caret).then(|| (a.min(self.caret), a.max(self.caret)))
    }

    fn delete_selection(&mut self) -> bool {
        if let Some((a, b)) = self.selection() {
            self.text.replace_range(a..b, "");
            self.caret = a;
            self.anchor = None;
            return true;
        }
        self.anchor = None;
        false
    }

    pub fn insert(&mut self, s: &str) {
        self.delete_selection();
        let s: String = s.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.caret, &s);
        self.caret += s.len();
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].char_indices().next_back().map(|(j, _)| j).unwrap_or(0)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].chars().next().map(|c| i + c.len_utf8()).unwrap_or(i)
    }

    fn word_left(&self, i: usize) -> usize {
        let t = &self.text[..i];
        let trimmed = t.trim_end_matches(|c: char| !c.is_alphanumeric());
        trimmed.rfind(|c: char| !c.is_alphanumeric()).map(|j| j + 1).unwrap_or(0)
    }

    fn word_right(&self, i: usize) -> usize {
        let t = &self.text[i..];
        let skip = t.len() - t.trim_start_matches(|c: char| !c.is_alphanumeric()).len();
        let rest = &t[skip..];
        i + skip + rest.find(|c: char| !c.is_alphanumeric()).unwrap_or(rest.len())
    }

    fn move_to(&mut self, to: usize, select: bool) {
        if select {
            self.anchor.get_or_insert(self.caret);
        } else {
            self.anchor = None;
        }
        self.caret = to;
    }

    pub fn key(&mut self, k: FieldKey) -> FieldEvent {
        match k {
            FieldKey::Text(s) => {
                self.insert(&s);
                FieldEvent::Changed
            }
            FieldKey::Backspace => {
                if !self.delete_selection() {
                    if self.caret == 0 {
                        return FieldEvent::None;
                    }
                    let p = self.prev(self.caret);
                    self.text.replace_range(p..self.caret, "");
                    self.caret = p;
                }
                FieldEvent::Changed
            }
            FieldKey::Delete => {
                if !self.delete_selection() {
                    let n = self.next(self.caret);
                    if n == self.caret {
                        return FieldEvent::None;
                    }
                    self.text.replace_range(self.caret..n, "");
                }
                FieldEvent::Changed
            }
            FieldKey::Left { select, word } => {
                let to = match (self.selection(), select, word) {
                    (Some((a, _)), false, false) => a,
                    (_, _, true) => self.word_left(self.caret),
                    _ => self.prev(self.caret),
                };
                self.move_to(to, select);
                FieldEvent::None
            }
            FieldKey::Right { select, word } => {
                let to = match (self.selection(), select, word) {
                    (Some((_, b)), false, false) => b,
                    (_, _, true) => self.word_right(self.caret),
                    _ => self.next(self.caret),
                };
                self.move_to(to, select);
                FieldEvent::None
            }
            FieldKey::Home { select } => {
                self.move_to(0, select);
                FieldEvent::None
            }
            FieldKey::End { select } => {
                let n = self.text.len();
                self.move_to(n, select);
                FieldEvent::None
            }
            FieldKey::SelectAll => {
                self.anchor = Some(0);
                self.caret = self.text.len();
                FieldEvent::None
            }
            FieldKey::Enter { shift } => FieldEvent::Submit { shift },
            FieldKey::Escape => FieldEvent::Cancel,
            FieldKey::Up => FieldEvent::Up,
            FieldKey::Down => FieldEvent::Down,
            FieldKey::Tab { shift } => FieldEvent::Tab { shift },
        }
    }

    /// The selected text (for ⌘C).
    pub fn selected_text(&self) -> Option<String> {
        self.selection().map(|(a, b)| self.text[a..b].to_string())
    }

    /// Put the caret at `x` pixels from the text's left edge.
    pub fn click(&mut self, font: &UiFont, x: f64, select: bool) {
        let mut best = (0usize, f64::MAX);
        let mut idx: Vec<usize> = self.text.char_indices().map(|(i, _)| i).collect();
        idx.push(self.text.len());
        for i in idx {
            let d = (font.measure(&self.text[..i]) - x).abs();
            if d < best.1 {
                best = (i, d);
            }
        }
        self.move_to(best.0, select);
    }
}

/// The field key for a key press, if it's one a field handles (text comes separately, through
/// text input).
pub fn field_key(p: &Press) -> Option<FieldKey> {
    let shift = p.flags & keys::SHIFT != 0;
    let word = p.flags & (keys::OPTION | keys::CONTROL) != 0;
    let cmd = p.flags & keys::COMMAND != 0;
    Some(match keys::named(p.keycode)? {
        "backspace" => FieldKey::Backspace,
        "delete" => FieldKey::Delete,
        "left" if cmd => FieldKey::Home { select: shift },
        "right" if cmd => FieldKey::End { select: shift },
        "left" => FieldKey::Left { select: shift, word },
        "right" => FieldKey::Right { select: shift, word },
        "home" => FieldKey::Home { select: shift },
        "end" => FieldKey::End { select: shift },
        "enter" => FieldKey::Enter { shift },
        "escape" => FieldKey::Escape,
        "up" => FieldKey::Up,
        "down" => FieldKey::Down,
        "tab" => FieldKey::Tab { shift },
        _ => return None,
    })
}

/// Draw a field in `r` (pixels). Returns the x where its text starts (for clicks).
#[allow(clippy::too_many_arguments)]
pub fn draw_field<C: Canvas>(c: &mut C, f: &Field, r: Rect, font: &UiFont, col: &Colors, focused: bool, s: f64, caret_on: bool) {
    c.fill_rounded(col.field, 1.0, r.x, r.y, r.w, r.h, [6.0 * s; 4]);
    if focused {
        // A focus ring.
        let ring = [r.x - 1.0 * s, r.y - 1.0 * s, r.w + 2.0 * s, r.h + 2.0 * s];
        c.fill_rounded(col.accent, 0.35, ring[0], ring[1], ring[2], ring[3], [7.0 * s; 4]);
        c.fill_rounded(col.field, 1.0, r.x, r.y, r.w, r.h, [6.0 * s; 4]);
    }
    let pad = 8.0 * s;
    let ty = r.y + ((r.h - font.height()) / 2.0).round();
    let avail = r.w - 2.0 * pad;
    c.clip(Some((r.x + pad, r.y, avail, r.h)));
    if f.text.is_empty() {
        c.text(font, &f.placeholder, r.x + pad, ty, avail, col.faint, Trunc::Tail);
    } else {
        // Scroll so the caret stays visible.
        let caret_x = font.measure(&f.text[..f.caret]);
        let shift = (caret_x - avail + 2.0 * s).max(0.0);
        let x0 = r.x + pad - shift;
        if let Some((a, b)) = f.selection() {
            let xa = x0 + font.measure(&f.text[..a]);
            let xb = x0 + font.measure(&f.text[..b]);
            c.fill(col.selected, 1.0, xa, ty, xb - xa, font.height());
        }
        c.text(font, &f.text, x0, ty, f64::MAX, col.text, Trunc::Tail);
        if focused && caret_on {
            c.fill(col.text, 1.0, (x0 + caret_x).round(), ty, (1.5 * s).round().max(1.0), font.height());
        }
    }
    if focused && caret_on && f.text.is_empty() {
        c.fill(col.text, 1.0, r.x + pad, ty, (1.5 * s).round().max(1.0), font.height());
    }
    c.clip(None);
}

// ---------------------------------------------------------------------------------------------
// Menus
// ---------------------------------------------------------------------------------------------

/// What choosing a menu item does.
#[derive(Debug, Clone, PartialEq)]
pub enum MenuAction {
    Perform(Action),
    NewTabIn(u64),
    /// Open a tab in `folder` and type `command` at its prompt (without pressing Enter).
    Launch { window: u64, folder: String, command: String },
    Rename(u64),
    Close(u64),
    Tile(u64),
    Untile(u64),
    MoveToNewWindow(u64),
    Theme { tab: u64, name: Option<String> },
    ShowTab(u64),
    Reopen(u64),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MenuItem {
    pub label: String,
    /// Smaller text after the label (a folder).
    pub detail: Option<String>,
    pub shortcut: Option<String>,
    pub action: Option<MenuAction>,
    pub checked: bool,
    /// A colour swatch before the label (themes).
    pub swatch: Option<Rgb>,
}

impl MenuItem {
    pub fn new(label: &str, action: MenuAction) -> MenuItem {
        MenuItem { label: label.to_string(), detail: None, shortcut: None, action: Some(action), checked: false, swatch: None }
    }

    pub fn separator() -> MenuItem {
        MenuItem { label: String::new(), detail: None, shortcut: None, action: None, checked: false, swatch: None }
    }

    pub fn heading(label: &str) -> MenuItem {
        MenuItem { label: label.to_string(), detail: None, shortcut: None, action: None, checked: false, swatch: None }
    }

    fn is_separator(&self) -> bool {
        self.label.is_empty() && self.action.is_none()
    }
}

/// A pop-up menu in a window, at a point (in points).
#[derive(Debug, Clone, PartialEq)]
pub struct Menu {
    pub x: f64,
    pub y: f64,
    pub items: Vec<MenuItem>,
    pub hover: Option<usize>,
}

pub const MENU_ROW: f64 = 26.0;
pub const MENU_SEP: f64 = 9.0;
pub const MENU_PAD: f64 = 6.0;

impl Menu {
    pub fn new(x: f64, y: f64, items: Vec<MenuItem>) -> Menu {
        Menu { x, y, items, hover: None }
    }

    /// Each item's rectangle (points) and the menu's, kept inside a `w`×`h` window.
    pub fn layout(&self, label: &UiFont, small: &UiFont, scale: f64, w: f64, h: f64) -> (Rect, Vec<Rect>) {
        let mut width: f64 = 160.0;
        for it in &self.items {
            let mut iw = label.measure(&it.label) / scale + 44.0;
            if let Some(d) = &it.detail {
                iw += small.measure(d) / scale + 14.0;
            }
            if let Some(k) = &it.shortcut {
                iw += small.measure(k) / scale + 24.0;
            }
            width = width.max(iw);
        }
        let width = width.min(520.0).min(w - 8.0);
        let height: f64 =
            self.items.iter().map(|i| if i.is_separator() { MENU_SEP } else { MENU_ROW }).sum::<f64>() + 2.0 * MENU_PAD;
        let x = self.x.min(w - width - 4.0).max(4.0);
        let y = if self.y + height > h - 4.0 { (self.y - height).max(4.0) } else { self.y };
        let mut rows = Vec::new();
        let mut cy = y + MENU_PAD;
        for it in &self.items {
            let rh = if it.is_separator() { MENU_SEP } else { MENU_ROW };
            rows.push(Rect::new(x + 4.0, cy, width - 8.0, rh));
            cy += rh;
        }
        (Rect::new(x, y, width, height), rows)
    }

    pub fn item_at(&self, rows: &[Rect], x: f64, y: f64) -> Option<usize> {
        rows.iter().position(|r| r.contains(x, y)).filter(|i| self.items[*i].action.is_some())
    }

    /// Move the highlight with the arrow keys, skipping separators and headings.
    pub fn step(&mut self, down: bool) {
        let n = self.items.len();
        if n == 0 {
            return;
        }
        let mut i = self.hover.unwrap_or(if down { n - 1 } else { 0 });
        for _ in 0..n {
            i = if down { (i + 1) % n } else { (i + n - 1) % n };
            if self.items[i].action.is_some() {
                self.hover = Some(i);
                return;
            }
        }
    }
}

/// Draw a menu. `s` is the backing scale.
pub fn draw_menu<C: Canvas>(c: &mut C, m: &Menu, frame: Rect, rows: &[Rect], label: &UiFont, small: &UiFont, col: &Colors, s: f64) {
    let px = |r: Rect| Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
    let f = px(frame);
    // A soft shadow, then the body.
    for (i, a) in [(3.0, 0.10), (2.0, 0.12), (1.0, 0.16)] {
        c.fill_rounded([0, 0, 0], a, f.x - i * s, f.y - i * s + 2.0 * s, f.w + 2.0 * i * s, f.h + 2.0 * i * s, [(9.0 + i) * s; 4]);
    }
    c.fill_rounded(col.panel, 1.0, f.x, f.y, f.w, f.h, [9.0 * s; 4]);
    for (i, (it, r)) in m.items.iter().zip(rows).enumerate() {
        let r = px(*r);
        if it.is_separator() {
            c.fill(col.line, 1.0, r.x + 6.0 * s, r.y + (r.h / 2.0).round(), r.w - 12.0 * s, s.max(1.0));
            continue;
        }
        let heading = it.action.is_none();
        if m.hover == Some(i) && !heading {
            c.fill_rounded(col.accent, 0.9, r.x, r.y, r.w, r.h, [5.0 * s; 4]);
        }
        let fg = if m.hover == Some(i) && !heading { [255, 255, 255] } else if heading { col.dim } else { col.text };
        let ty = r.y + ((r.h - label.height()) / 2.0).round();
        let mut x = r.x + 22.0 * s;
        if it.checked {
            c.text(label, "✓", r.x + 6.0 * s, ty, 14.0 * s, fg, Trunc::Tail);
        }
        if let Some(sw) = it.swatch {
            c.fill_rounded(sw, 1.0, x, r.y + 6.0 * s, 14.0 * s, r.h - 12.0 * s, [3.0 * s; 4]);
            c.fill_rounded(col.line, 0.6, x, r.y + 6.0 * s, 14.0 * s, r.h - 12.0 * s, [3.0 * s; 4]);
            c.fill_rounded(sw, 1.0, x + s, r.y + 7.0 * s, 12.0 * s, r.h - 14.0 * s, [2.0 * s; 4]);
            x += 22.0 * s;
        }
        let kw = it.shortcut.as_ref().map(|k| small.measure(k)).unwrap_or(0.0);
        let right = r.x + r.w - 10.0 * s - if kw > 0.0 { kw + 12.0 * s } else { 0.0 };
        let lw = c.text(label, &it.label, x, ty, right - x, fg, Trunc::Tail);
        if let Some(d) = &it.detail {
            let dy = r.y + ((r.h - small.height()) / 2.0).round();
            let dfg = if m.hover == Some(i) { [230, 230, 240] } else { col.dim };
            c.text(small, d, x + lw + 10.0 * s, dy, right - (x + lw + 10.0 * s), dfg, Trunc::Head);
        }
        if let Some(k) = &it.shortcut {
            let dy = r.y + ((r.h - small.height()) / 2.0).round();
            let kfg = if m.hover == Some(i) { [230, 230, 240] } else { col.faint };
            c.text(small, k, r.x + r.w - 10.0 * s - kw, dy, kw + 2.0, kfg, Trunc::Tail);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Dialogs (Linux; macOS shows NSAlert)
// ---------------------------------------------------------------------------------------------

/// What a dialog answers: the button chosen (`None`: cancelled) and the field's text.
pub type DialogDone = Box<dyn FnOnce(&mut super::App, Option<usize>, String)>;

pub struct Dialog {
    pub title: String,
    pub text: String,
    /// The first is the default (Return); the last cancels (Escape) when there are two or more.
    pub buttons: Vec<String>,
    pub field: Option<Field>,
    pub done: Option<DialogDone>,
    pub hover: Option<usize>,
}

pub struct DialogLayout {
    pub frame: Rect,
    pub field: Option<Rect>,
    pub buttons: Vec<Rect>,
    pub lines: Vec<String>,
}

impl Dialog {
    pub fn layout(&self, title: &UiFont, body: &UiFont, scale: f64, w: f64, h: f64) -> DialogLayout {
        let width = 420.0f64.min(w - 24.0);
        let inner = (width - 40.0) * scale;
        let lines = wrap(&self.text, body, inner);
        let mut height = 20.0 + title.height() / scale + 10.0 + lines.len() as f64 * (body.height() / scale + 3.0) + 16.0;
        let field_y = height;
        if self.field.is_some() {
            height += 36.0;
        }
        let buttons_y = height;
        height += 30.0 + 18.0;
        let x = ((w - width) / 2.0).round();
        let y = ((h - height) / 3.0).round().max(8.0);
        let mut buttons = Vec::new();
        let mut bx = x + width - 20.0;
        for b in &self.buttons {
            let bw = (body.measure(b) / scale + 28.0).max(80.0);
            bx -= bw;
            buttons.push(Rect::new(bx, y + buttons_y, bw, 28.0));
            bx -= 10.0;
        }
        DialogLayout {
            frame: Rect::new(x, y, width, height),
            field: self.field.as_ref().map(|_| Rect::new(x + 20.0, y + field_y, width - 40.0, 28.0)),
            buttons,
            lines,
        }
    }
}

/// Words wrapped to `max_w` pixels, keeping the text's own line breaks.
pub fn wrap(text: &str, font: &UiFont, max_w: f64) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.lines() {
        let mut line = String::new();
        for word in para.split(' ') {
            let candidate = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
            if font.measure(&candidate) > max_w && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line = word.to_string();
            } else {
                line = candidate;
            }
        }
        out.push(line);
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub fn draw_dialog<C: Canvas>(
    c: &mut C,
    d: &Dialog,
    l: &DialogLayout,
    title: &UiFont,
    body: &UiFont,
    col: &Colors,
    s: f64,
    area: Rect,
    caret_on: bool,
) {
    // Dim what's behind.
    c.fill([0, 0, 0], 0.35, area.x * s, area.y * s, area.w * s, area.h * s);
    let f = Rect::new(l.frame.x * s, l.frame.y * s, l.frame.w * s, l.frame.h * s);
    c.fill_rounded([0, 0, 0], 0.25, f.x - 2.0 * s, f.y, f.w + 4.0 * s, f.h + 5.0 * s, [13.0 * s; 4]);
    c.fill_rounded(col.panel, 1.0, f.x, f.y, f.w, f.h, [12.0 * s; 4]);
    let mut y = f.y + 20.0 * s;
    c.text(title, &d.title, f.x + 20.0 * s, y, f.w - 40.0 * s, col.text, Trunc::Tail);
    y += title.height() + 10.0 * s;
    for line in &l.lines {
        c.text(body, line, f.x + 20.0 * s, y, f.w - 40.0 * s, col.dim, Trunc::Tail);
        y += body.height() + 3.0 * s;
    }
    if let (Some(field), Some(r)) = (&d.field, l.field) {
        draw_field(c, field, Rect::new(r.x * s, r.y * s, r.w * s, r.h * s), body, col, true, s, caret_on);
    }
    for (i, (b, r)) in d.buttons.iter().zip(&l.buttons).enumerate() {
        let r = Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
        let default = i == 0;
        let fill = if default { col.accent } else { col.field };
        c.fill_rounded(fill, if d.hover == Some(i) { 0.85 } else { 1.0 }, r.x, r.y, r.w, r.h, [7.0 * s; 4]);
        let tw = body.measure(b);
        let fg = if default { [255, 255, 255] } else { col.text };
        c.text(body, b, r.x + ((r.w - tw) / 2.0).round(), r.y + ((r.h - body.height()) / 2.0).round(), r.w, fg, Trunc::Tail);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_editing() {
        let mut f = Field::new("Find");
        f.key(FieldKey::Text("hello world".into()));
        assert_eq!(f.text, "hello world");
        f.key(FieldKey::Left { select: false, word: true });
        assert_eq!(f.caret, 6);
        f.key(FieldKey::Backspace);
        assert_eq!(f.text, "helloworld");
        f.key(FieldKey::SelectAll);
        assert_eq!(f.selected_text().as_deref(), Some("helloworld"));
        f.key(FieldKey::Text("é✓".into()));
        assert_eq!(f.text, "é✓");
        f.key(FieldKey::Left { select: true, word: false });
        assert_eq!(f.selected_text().as_deref(), Some("✓"));
        f.key(FieldKey::Delete);
        assert_eq!(f.text, "é");
        assert_eq!(f.key(FieldKey::Enter { shift: true }), FieldEvent::Submit { shift: true });
        assert_eq!(f.key(FieldKey::Home { select: false }), FieldEvent::None);
        assert_eq!(f.caret, 0);
        f.key(FieldKey::Text("a\tb\n".into()));
        assert_eq!(f.text, "abé", "control characters are dropped");
    }

    #[test]
    fn menu_steps_skip_headings_and_separators() {
        let mut m = Menu::new(
            0.0,
            0.0,
            vec![
                MenuItem::heading("Start"),
                MenuItem::new("A", MenuAction::Close(1)),
                MenuItem::separator(),
                MenuItem::new("B", MenuAction::Close(2)),
            ],
        );
        m.step(true);
        assert_eq!(m.hover, Some(1));
        m.step(true);
        assert_eq!(m.hover, Some(3));
        m.step(true);
        assert_eq!(m.hover, Some(1));
        m.step(false);
        assert_eq!(m.hover, Some(3));
    }

    #[test]
    fn chrome_colours_follow_the_theme() {
        let d = colors(&crate::theme::ghostty());
        assert!(d.dark);
        // The bar is darker than the terminal, and text contrasts with it.
        let lum = |c: Rgb| c[0] as u32 + c[1] as u32 + c[2] as u32;
        assert!(lum(d.bar) < lum(crate::theme::ghostty().background));
        assert!(lum(d.text) > lum(d.bar) + 300);
        let l = colors(&crate::theme::basic_light());
        assert!(!l.dark && lum(l.text) < lum(l.bar));
    }
}
