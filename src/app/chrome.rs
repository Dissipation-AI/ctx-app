//! The window's own chrome, drawn into its buffer: the tab bar (notebook tabs with the tab's
//! name over its folder, a + button, and buttons for Overview, History and Activity), the find
//! bar and the "reopen tabs" bar. Layout and hit-testing are plain arithmetic, so they're shared
//! by every platform and unit-tested.

use super::ui::{self, Colors, Field};
use crate::layout::Rect;
use crate::render::{Canvas, PathOp, Rgb, Trunc, UiFont, mix};

/// Height of the tab bar, in points. On macOS the tabs share it with the window's buttons.
pub const BAR_H: f64 = 40.0;
/// Height of the find bar and the reopen bar.
pub const STRIP_H: f64 = 34.0;
/// Where tabs start: room for the red/yellow/green buttons on macOS.
pub const TRAFFIC_LIGHTS: f64 = 78.0;
const TAB_TOP: f64 = 6.0;
const TAB_MIN: f64 = 116.0;
const TAB_MAX: f64 = 240.0;
const BUTTON_W: f64 = 32.0;
const PLUS_W: f64 = 30.0;
const CHEVRON_W: f64 = 18.0;

/// What a tab shows.
#[derive(Debug, Clone, PartialEq)]
pub struct TabItem {
    pub tab: u64,
    /// The first line: the tab's name, else what it runs, else its folder's last part.
    pub label: String,
    /// The second line: the folder (or remote host).
    pub path: String,
    pub title: Option<String>,
    /// Opened by an agent: its name.
    pub agent: Option<String>,
    /// Runs with a scoped launcher's permissions.
    pub launcher: Option<String>,
    /// Another client touched it recently: true if it typed, false if it only read.
    pub badge: Option<bool>,
    pub selected: bool,
    pub tiled: bool,
    pub status: Status,
    /// The tab's theme background (the selected tab is drawn in it).
    pub background: Rgb,
    /// The tab has a theme of its own: unselected, it's tinted with it so colours mark projects.
    pub themed: bool,
}

/// What an unselected tab is filled with (`None`: the bar shows through).
fn unselected_fill(item: &TabItem, hover: bool, col: &Colors) -> Option<Rgb> {
    match (item.themed, hover) {
        (true, false) => Some(mix(col.tab, item.background, 0.6)),
        (true, true) => Some(mix(col.tab_hover, item.background, 0.6)),
        (false, true) => Some(col.tab_hover),
        (false, false) => None,
    }
}

/// What's going on in a tab, for the tab bar and the overview.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    /// A command is running.
    Busy,
    /// An AI agent is working.
    AgentWorking,
    /// An AI agent is waiting for you.
    AgentWaiting,
}

/// The chrome's buttons.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Button {
    Overview,
    History,
    Activity,
}

pub const BUTTONS: [(Button, &str); 3] = [
    (Button::Overview, "Overview (all windows and tabs)"),
    (Button::History, "History of closed tabs (⌘Y)"),
    (Button::Activity, "Activity of other programs (⇧⌘A)"),
];

/// The window's red/yellow/green buttons, drawn by us (the window has no title bar).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Light {
    Close,
    Minimise,
    FullScreen,
}

const LIGHTS: [Light; 3] = [Light::Close, Light::Minimise, Light::FullScreen];
const LIGHT_D: f64 = 12.0;
const LIGHT_X: f64 = 14.0;
const LIGHT_STEP: f64 = 20.0;

/// Where the pointer is.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Hit {
    Light(Light),
    Tab {
        tab: u64,
        index: usize,
        close: bool,
    },
    Plus,
    Button(Button),
    ScrollLeft,
    ScrollRight,
    /// Empty bar: drags the window.
    Bar,
    Term,
    FindField,
    FindPrev,
    FindNext,
    FindDone,
    ReopenYes,
    ReopenNo,
    PasteYes,
    PasteNo,
}

/// Where a tab's activity badge is (a click there opens Activity), in points.
pub fn badge_rect(tab: Rect) -> Rect {
    Rect::new(tab.x + tab.w - 26.0, tab.y + 2.0, 24.0, tab.h / 2.0 + 2.0)
}

/// The bar's geometry, in points.
#[derive(Debug, Clone, PartialEq)]
pub struct BarLayout {
    pub tabs: Vec<(u64, Rect)>,
    pub plus: Rect,
    pub buttons: Vec<(Button, Rect)>,
    pub left: Option<Rect>,
    pub right: Option<Rect>,
    /// The visible span of the tab strip.
    pub strip: Rect,
    /// The window's buttons, when we draw them (the window has no title bar).
    pub lights: Vec<(Light, Rect)>,
}

/// Lay out `n` tabs in a bar `w` points wide, scrolled by `scroll` points, starting at `inset`.
pub fn bar_layout(ids: &[u64], w: f64, inset: f64, scroll: f64) -> BarLayout {
    let mut buttons = Vec::new();
    let mut bx = w - 6.0;
    for (b, _) in BUTTONS.iter().rev() {
        bx -= BUTTON_W;
        buttons.push((*b, Rect::new(bx, 4.0, BUTTON_W, BAR_H - 8.0)));
    }
    buttons.reverse();
    let strip_x = inset;
    let strip_end = bx - 8.0;
    let n = ids.len().max(1) as f64;
    let room = (strip_end - strip_x - PLUS_W).max(TAB_MIN);
    let tw = (room / n).clamp(TAB_MIN, TAB_MAX).floor();
    let overflow = tw * ids.len() as f64 > room + 0.5;
    let (sx, sw) = if overflow {
        (strip_x + CHEVRON_W, strip_end - strip_x - PLUS_W - 2.0 * CHEVRON_W)
    } else {
        (strip_x, strip_end - strip_x - PLUS_W)
    };
    let max_scroll = (tw * ids.len() as f64 - sw).max(0.0);
    let scroll = scroll.clamp(0.0, max_scroll);
    let tabs: Vec<(u64, Rect)> = ids
        .iter()
        .enumerate()
        .map(|(i, t)| (*t, Rect::new(sx + i as f64 * tw - scroll, TAB_TOP, tw, BAR_H - TAB_TOP)))
        .collect();
    let plus_x = if overflow { sx + sw + CHEVRON_W } else { sx + tw * ids.len() as f64 + 2.0 };
    BarLayout {
        tabs,
        plus: Rect::new(plus_x, TAB_TOP + 3.0, PLUS_W - 4.0, BAR_H - TAB_TOP - 8.0),
        buttons,
        left: overflow.then(|| Rect::new(strip_x, TAB_TOP, CHEVRON_W, BAR_H - TAB_TOP)),
        right: overflow.then(|| Rect::new(sx + sw, TAB_TOP, CHEVRON_W, BAR_H - TAB_TOP)),
        strip: Rect::new(sx, 0.0, sw, BAR_H),
        lights: Vec::new(),
    }
}

impl BarLayout {
    /// Add the window's red/yellow/green buttons in the room left of the tabs.
    pub fn with_lights(mut self) -> BarLayout {
        let y = ((BAR_H - LIGHT_D) / 2.0).round();
        self.lights =
            LIGHTS.iter().enumerate().map(|(i, l)| (*l, Rect::new(LIGHT_X + i as f64 * LIGHT_STEP, y, LIGHT_D, LIGHT_D))).collect();
        self
    }

    /// Scroll that keeps `tab` in view.
    pub fn scroll_to(&self, tab: u64, scroll: f64) -> f64 {
        let Some((_, r)) = self.tabs.iter().find(|(t, _)| *t == tab) else { return scroll };
        if r.x < self.strip.x {
            scroll - (self.strip.x - r.x)
        } else if r.x + r.w > self.strip.x + self.strip.w {
            scroll + (r.x + r.w - self.strip.x - self.strip.w)
        } else {
            scroll
        }
        .max(0.0)
    }

    pub fn hit(&self, x: f64, y: f64) -> Hit {
        // A little slack around each button, as with the system's.
        for (l, r) in &self.lights {
            if Rect::new(r.x - 3.0, r.y - 3.0, r.w + 6.0, r.h + 6.0).contains(x, y) {
                return Hit::Light(*l);
            }
        }
        for (b, r) in &self.buttons {
            if r.contains(x, y) {
                return Hit::Button(*b);
            }
        }
        if let Some(r) = self.left
            && r.contains(x, y)
        {
            return Hit::ScrollLeft;
        }
        if let Some(r) = self.right
            && r.contains(x, y)
        {
            return Hit::ScrollRight;
        }
        if self.plus.contains(x, y) {
            return Hit::Plus;
        }
        if self.strip.contains(x, y) {
            for (i, (t, r)) in self.tabs.iter().enumerate() {
                if r.contains(x, y) {
                    let close = x < r.x + 24.0;
                    return Hit::Tab { tab: *t, index: i, close };
                }
            }
        }
        Hit::Bar
    }

    /// Where a tab dropped at `x` would go: its index among the tabs.
    pub fn insertion(&self, x: f64) -> usize {
        self.tabs.iter().position(|(_, r)| x < r.x + r.w / 2.0).unwrap_or(self.tabs.len())
    }
}

/// Fonts the chrome uses, at the window's scale.
pub struct ChromeFonts {
    pub label: std::rc::Rc<UiFont>,
    pub label_bold: std::rc::Rc<UiFont>,
    pub small: std::rc::Rc<UiFont>,
    pub title: std::rc::Rc<UiFont>,
}

/// Everything needed to draw the bar.
pub struct BarFrame<'a> {
    pub layout: &'a BarLayout,
    pub items: &'a [TabItem],
    pub hover: Option<Hit>,
    /// A tab being dragged along the bar: its index and where its middle is (points).
    pub dragging: Option<(usize, f64)>,
    pub active: bool,
    pub width: f64,
}

fn px(r: Rect, s: f64) -> Rect {
    Rect::new(r.x * s, r.y * s, r.w * s, r.h * s)
}

/// Draw the tab bar across the top of the buffer.
pub fn draw_bar<C: Canvas>(c: &mut C, f: &BarFrame, fonts: &ChromeFonts, col: &Colors, s: f64) {
    let bar = Rect::new(0.0, 0.0, f.width * s, BAR_H * s);
    c.fill(col.bar, 1.0, bar.x, bar.y, bar.w, bar.h);
    draw_lights(c, f, col, s);
    let strip = px(f.layout.strip, s);
    c.clip(Some((strip.x, 0.0, strip.w, bar.h)));
    // Unselected tabs first, so the selected one's curves sit on top.
    let order: Vec<usize> = (0..f.items.len())
        .filter(|i| !f.items[*i].selected)
        .chain((0..f.items.len()).filter(|i| f.items[*i].selected))
        .collect();
    for i in order {
        let item = &f.items[i];
        let Some((_, mut r)) = f.layout.tabs.get(i).copied() else { continue };
        if let Some((di, mid)) = f.dragging
            && di == i
        {
            r.x = mid - r.w / 2.0;
        }
        let hover = matches!(f.hover, Some(Hit::Tab { index, .. }) if index == i);
        let hovered = |j: usize| matches!(f.hover, Some(Hit::Tab { index, .. }) if index == j);
        let beside = |j: Option<usize>| {
            j.and_then(|j| f.items.get(j).map(|n| (j, n)))
                .and_then(|(j, n)| unselected_fill(n, hovered(j), col))
                .unwrap_or(col.bar)
        };
        let feet = (beside(i.checked_sub(1)), beside(Some(i + 1)));
        draw_tab(
            c,
            item,
            px(r, s),
            hover,
            matches!(f.hover, Some(Hit::Tab { index, close: true, .. }) if index == i),
            feet,
            fonts,
            col,
            s,
            f.active,
        );
        // A thin separator between plain unselected neighbours.
        let next = f.items.get(i + 1);
        let plain = |n: &TabItem, j: usize| !n.selected && unselected_fill(n, hovered(j), col).is_none();
        if plain(item, i) && next.is_some_and(|n| plain(n, i + 1)) {
            let x = (r.x + r.w) * s;
            c.fill(col.line, 0.7, x - 0.5 * s, (TAB_TOP + 9.0) * s, s.max(1.0), (BAR_H - TAB_TOP - 18.0) * s);
        }
    }
    c.clip(None);
    // Overflow chevrons.
    for (r, glyph, h) in [(f.layout.left, "‹", Hit::ScrollLeft), (f.layout.right, "›", Hit::ScrollRight)] {
        if let Some(r) = r {
            let r = px(r, s);
            if f.hover == Some(h) {
                c.fill_rounded(
                    col.hover,
                    1.0,
                    r.x + 1.0 * s,
                    r.y + 4.0 * s,
                    r.w - 2.0 * s,
                    r.h - 10.0 * s,
                    [5.0 * s; 4],
                );
            }
            let tw = fonts.label_bold.measure(glyph);
            c.text(
                &fonts.label_bold,
                glyph,
                r.x + (r.w - tw) / 2.0,
                r.y + (r.h - fonts.label_bold.height()) / 2.0 - 2.0 * s,
                r.w,
                col.dim,
                Trunc::Tail,
            );
        }
    }
    // The + button.
    let p = px(f.layout.plus, s);
    if f.hover == Some(Hit::Plus) {
        c.fill_rounded(col.hover, 1.0, p.x, p.y, p.w, p.h, [7.0 * s; 4]);
    }
    let (cx, cy) = ((p.x + p.w / 2.0).round(), (p.y + p.h / 2.0).round());
    let arm = 6.0 * s;
    let t = (1.5 * s).round().max(1.0);
    c.fill(col.dim, 1.0, cx - arm, cy - (t / 2.0).floor(), 2.0 * arm, t);
    c.fill(col.dim, 1.0, cx - (t / 2.0).floor(), cy - arm, t, 2.0 * arm);
    // Overview, History, Activity.
    for (b, r) in &f.layout.buttons {
        let r = px(*r, s);
        if f.hover == Some(Hit::Button(*b)) {
            c.fill_rounded(col.hover, 1.0, r.x + 2.0 * s, r.y, r.w - 4.0 * s, r.h, [7.0 * s; 4]);
        }
        draw_icon(c, *b, r, col.dim, s);
    }
}

/// The red/yellow/green buttons: plain circles, grey while the window isn't focused.
fn draw_lights<C: Canvas>(c: &mut C, f: &BarFrame, col: &Colors, s: f64) {
    for (l, r) in &f.layout.lights {
        let fill: Rgb = if !f.active {
            mix(col.bar, col.text, 0.2)
        } else {
            match l {
                Light::Close => [0xff, 0x5f, 0x57],
                Light::Minimise => [0xfe, 0xbc, 0x2e],
                Light::FullScreen => [0x28, 0xc8, 0x40],
            }
        };
        let r = px(*r, s);
        // A faint darker rim, as the system draws.
        c.fill_rounded(mix(fill, [0, 0, 0], 0.12), 1.0, r.x, r.y, r.w, r.h, [r.w / 2.0; 4]);
        let i = 0.5 * s;
        c.fill_rounded(fill, 1.0, r.x + i, r.y + i, r.w - 2.0 * i, r.h - 2.0 * i, [r.w / 2.0 - i; 4]);
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_tab<C: Canvas>(
    c: &mut C,
    item: &TabItem,
    r: Rect,
    hover: bool,
    close_hover: bool,
    feet: (Rgb, Rgb),
    fonts: &ChromeFonts,
    col: &Colors,
    s: f64,
    active: bool,
) {
    let rad = 8.0 * s;
    let (x, w) = if item.selected { (r.x, r.w) } else { (r.x + 1.0 * s, r.w - 2.0 * s) };
    let top = r.y + if item.selected { 0.0 } else { 2.0 * s };
    let h = r.y + r.h - top;
    let fill = if item.selected { Some(item.background) } else { unselected_fill(item, hover, col) };
    if let Some(f) = fill {
        c.fill_rounded(f, 1.0, x, top, w, h, [rad, rad, 0.0, 0.0]);
    }
    let under = fill.unwrap_or(col.bar);
    if item.selected {
        // The notebook look: the tab flows into the terminal, with outward curves at its feet
        // cut from whatever is beside it.
        let foot = 6.0 * s;
        let bottom = r.y + r.h;
        c.fill(under, 1.0, x - foot, bottom - foot, foot, foot);
        c.fill(under, 1.0, x + w, bottom - foot, foot, foot);
        c.fill_rounded(feet.0, 1.0, x - 2.0 * foot, bottom - 2.0 * foot, 2.0 * foot, 2.0 * foot, [foot; 4]);
        c.fill_rounded(feet.1, 1.0, x + w, bottom - 2.0 * foot, 2.0 * foot, 2.0 * foot, [foot; 4]);
    } else if item.tiled {
        // Shown side by side: a line along the bottom.
        c.fill(col.accent, 0.8, x + 10.0 * s, r.y + r.h - 3.0 * s, w - 20.0 * s, 2.0 * s);
    }
    let mut left = x + 12.0 * s;
    let mut right = x + w - 10.0 * s;
    // Close button on hover, at the left as in Terminal.app.
    if hover {
        let cr = Rect::new(x + 6.0 * s, top + (h - 16.0 * s) / 2.0, 16.0 * s, 16.0 * s);
        if close_hover {
            c.fill_rounded(col.hover, 1.0, cr.x, cr.y, cr.w, cr.h, [4.0 * s; 4]);
        }
        let (cx, cy, a) = (cr.x + cr.w / 2.0, cr.y + cr.h / 2.0, 3.5 * s);
        let path = [
            PathOp::Move(cx - a, cy - a),
            PathOp::Line(cx + a, cy + a),
            PathOp::Move(cx + a, cy - a),
            PathOp::Line(cx - a, cy + a),
        ];
        c.stroke(&path, 1.4 * s, col.dim, 1.0);
        left = cr.x + cr.w + 6.0 * s;
    }
    let lf = if item.selected { &fonts.label_bold } else { &fonts.label };
    let lh = lf.height();
    let ph = fonts.small.height();
    let ty = (top + (h - lh - ph - 1.0 * s) / 2.0).round();
    let line2 = ty + lh + 1.0 * s;
    // Activity badge at the right of the first line: filled for "typed", a ring for "read".
    if let Some(typed) = item.badge {
        let d = 8.0 * s;
        let (bx, by) = (right - d, ty + (lh - d) / 2.0);
        if typed {
            c.fill_rounded(col.typed, 1.0, bx, by, d, d, [d / 2.0; 4]);
        } else {
            c.fill_rounded(col.read, 1.0, bx, by, d, d, [d / 2.0; 4]);
            c.fill_rounded(under, 1.0, bx + 1.5 * s, by + 1.5 * s, d - 3.0 * s, d - 3.0 * s, [d / 2.0; 4]);
        }
        right -= d + 6.0 * s;
    }
    // Status dot before the label: green while a command runs, purple while an agent works,
    // amber while it waits for you.
    let dot = match item.status {
        Status::Idle => None,
        Status::Busy => Some(col.busy),
        Status::AgentWorking => Some(col.agent),
        Status::AgentWaiting => Some(col.waiting),
    };
    if let Some(dc) = dot {
        let d = 7.0 * s;
        c.fill_rounded(dc, 1.0, left, ty + (lh - d) / 2.0, d, d, [d / 2.0; 4]);
        left += d + 6.0 * s;
    }
    // First line: the label, what the eye reads first.
    let avail = (right - left).max(10.0 * s);
    let strong = if item.selected && active { col.text } else { mix(col.text, under, 0.2) };
    c.text(lf, &item.label, left, ty, avail, strong, Trunc::Tail);
    // Second line, smaller and dimmer: who opened it (an agent), a launcher, then the folder.
    let mut x2 = left;
    let end2 = x + w - 10.0 * s;
    if let Some(a) = &item.agent {
        let text = match item.status {
            Status::AgentWaiting => format!("{a} · waiting"),
            Status::AgentWorking => format!("{a} · working"),
            _ => a.clone(),
        };
        let tint = if item.status == Status::AgentWaiting { col.waiting } else { col.agent };
        let tw = c.text(&fonts.small, &text, x2, line2, (end2 - x2) * 0.6, tint, Trunc::Tail);
        x2 += tw + 6.0 * s;
    }
    if let Some(l) = &item.launcher {
        let tw = c.text(&fonts.small, &format!("⚿ {l}"), x2, line2, (end2 - x2) * 0.5, col.launcher, Trunc::Tail);
        x2 += tw + 6.0 * s;
    }
    if end2 - x2 > 12.0 * s {
        c.text(&fonts.small, &item.path, x2, line2, end2 - x2, col.dim, Trunc::Head);
    }
}

/// Small line icons for the bar's buttons.
fn draw_icon<C: Canvas>(c: &mut C, b: Button, r: Rect, col: Rgb, s: f64) {
    let (cx, cy) = ((r.x + r.w / 2.0).round(), (r.y + r.h / 2.0).round());
    let lw = 1.4 * s;
    match b {
        Button::Overview => {
            // A small tree, left to right: one node, branching to three.
            let n = 3.5 * s;
            c.fill_rounded(col, 1.0, cx - 7.5 * s, cy - n / 2.0, n, n, [1.0 * s; 4]);
            for dy in [-5.5, 0.0, 5.5] {
                c.fill_rounded(col, 1.0, cx + 3.5 * s, cy + dy * s - n / 2.0, n + 1.5 * s, n, [1.0 * s; 4]);
            }
            let path = [
                PathOp::Move(cx - 4.0 * s, cy),
                PathOp::Line(cx - 1.0 * s, cy),
                PathOp::Move(cx - 1.0 * s, cy - 5.5 * s),
                PathOp::Line(cx - 1.0 * s, cy + 5.5 * s),
                PathOp::Move(cx - 1.0 * s, cy - 5.5 * s),
                PathOp::Line(cx + 3.5 * s, cy - 5.5 * s),
                PathOp::Move(cx - 1.0 * s, cy),
                PathOp::Line(cx + 3.5 * s, cy),
                PathOp::Move(cx - 1.0 * s, cy + 5.5 * s),
                PathOp::Line(cx + 3.5 * s, cy + 5.5 * s),
            ];
            c.stroke(&path, lw * 0.85, col, 1.0);
        }
        Button::History => {
            // A clock with an arrow round it.
            let rad = 7.0 * s;
            let mut path = Vec::new();
            let steps = 28;
            for i in 0..=steps {
                let a = -2.4 + (i as f64 / steps as f64) * 5.6;
                let p = (cx + rad * a.cos(), cy + rad * a.sin());
                path.push(if i == 0 { PathOp::Move(p.0, p.1) } else { PathOp::Line(p.0, p.1) });
            }
            c.stroke(&path, lw, col, 1.0);
            let hands =
                [PathOp::Move(cx, cy - 4.0 * s), PathOp::Line(cx, cy), PathOp::Line(cx + 3.0 * s, cy + 2.0 * s)];
            c.stroke(&hands, lw, col, 1.0);
            let a0 = -2.4f64;
            let tip = (cx + rad * a0.cos(), cy + rad * a0.sin());
            let head = [
                PathOp::Move(tip.0 - 3.0 * s, tip.1 - 0.5 * s),
                PathOp::Line(tip.0, tip.1),
                PathOp::Line(tip.0 + 0.5 * s, tip.1 - 3.2 * s),
            ];
            c.stroke(&head, lw, col, 1.0);
        }
        Button::Activity => {
            // A pulse line.
            let path = [
                PathOp::Move(cx - 8.0 * s, cy),
                PathOp::Line(cx - 4.0 * s, cy),
                PathOp::Line(cx - 2.0 * s, cy - 6.0 * s),
                PathOp::Line(cx + 1.0 * s, cy + 6.0 * s),
                PathOp::Line(cx + 3.5 * s, cy - 2.0 * s),
                PathOp::Line(cx + 5.0 * s, cy),
                PathOp::Line(cx + 8.0 * s, cy),
            ];
            c.stroke(&path, lw, col, 1.0);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Find bar and reopen bar
// ---------------------------------------------------------------------------------------------

/// Rects of the find bar's parts (points), for a bar at `y` in a window `w` wide.
pub struct FindLayout {
    pub bar: Rect,
    pub field: Rect,
    pub status: Rect,
    pub prev: Rect,
    pub next: Rect,
    pub done: Rect,
}

pub fn find_layout(w: f64, y: f64) -> FindLayout {
    let fw = (w - 260.0).clamp(120.0, 380.0);
    FindLayout {
        bar: Rect::new(0.0, y, w, STRIP_H),
        field: Rect::new(10.0, y + 5.0, fw, STRIP_H - 10.0),
        status: Rect::new(10.0 + fw + 10.0, y, 110.0, STRIP_H),
        prev: Rect::new(w - 138.0, y + 5.0, 28.0, STRIP_H - 10.0),
        next: Rect::new(w - 108.0, y + 5.0, 28.0, STRIP_H - 10.0),
        done: Rect::new(w - 72.0, y + 5.0, 62.0, STRIP_H - 10.0),
    }
}

impl FindLayout {
    pub fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        if !self.bar.contains(x, y) {
            return None;
        }
        Some(if self.field.contains(x, y) {
            Hit::FindField
        } else if self.prev.contains(x, y) {
            Hit::FindPrev
        } else if self.next.contains(x, y) {
            Hit::FindNext
        } else if self.done.contains(x, y) {
            Hit::FindDone
        } else {
            Hit::Term
        })
    }
}

/// A rounded push button with centred text.
#[allow(clippy::too_many_arguments)]
pub fn draw_button<C: Canvas>(
    c: &mut C,
    r: Rect,
    label: &str,
    font: &UiFont,
    col: &Colors,
    s: f64,
    hover: bool,
    primary: bool,
) {
    let r = px(r, s);
    let fill = if primary { col.accent } else { col.field };
    c.fill_rounded(fill, if hover { 0.8 } else { 1.0 }, r.x, r.y, r.w, r.h, [6.0 * s; 4]);
    let tw = font.measure(label);
    let fg = if primary { [255, 255, 255] } else { col.text };
    c.text(
        font,
        label,
        r.x + ((r.w - tw) / 2.0).round(),
        r.y + ((r.h - font.height()) / 2.0).round(),
        r.w,
        fg,
        Trunc::Tail,
    );
}

#[allow(clippy::too_many_arguments)]
pub fn draw_find<C: Canvas>(
    c: &mut C,
    l: &FindLayout,
    field: &Field,
    status: &str,
    focused: bool,
    hover: Option<Hit>,
    fonts: &ChromeFonts,
    col: &Colors,
    s: f64,
    caret_on: bool,
) {
    let b = px(l.bar, s);
    c.fill(col.bar, 1.0, b.x, b.y, b.w, b.h);
    c.fill(col.line, 1.0, b.x, b.y, b.w, s.max(1.0));
    ui::draw_field(c, field, px(l.field, s), &fonts.label, col, focused, s, caret_on);
    let st = px(l.status, s);
    c.text(&fonts.small, status, st.x, st.y + (st.h - fonts.small.height()) / 2.0, st.w, col.dim, Trunc::Tail);
    draw_button(c, l.prev, "‹", &fonts.label_bold, col, s, hover == Some(Hit::FindPrev), false);
    draw_button(c, l.next, "›", &fonts.label_bold, col, s, hover == Some(Hit::FindNext), false);
    draw_button(c, l.done, "Done", &fonts.label, col, s, hover == Some(Hit::FindDone), false);
}

pub struct ReopenLayout {
    pub bar: Rect,
    pub yes: Rect,
    pub no: Rect,
}

pub fn reopen_layout(w: f64, y: f64) -> ReopenLayout {
    ReopenLayout {
        bar: Rect::new(0.0, y, w, STRIP_H),
        yes: Rect::new(w - 196.0, y + 5.0, 86.0, STRIP_H - 10.0),
        no: Rect::new(w - 102.0, y + 5.0, 92.0, STRIP_H - 10.0),
    }
}

impl ReopenLayout {
    pub fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        if !self.bar.contains(x, y) {
            return None;
        }
        Some(if self.yes.contains(x, y) {
            Hit::ReopenYes
        } else if self.no.contains(x, y) {
            Hit::ReopenNo
        } else {
            Hit::Term
        })
    }
}

/// The strip asking whether to paste several lines at a prompt.
pub fn paste_layout(w: f64, y: f64) -> ReopenLayout {
    reopen_layout(w, y)
}

/// One line showing what would be pasted: its lines joined with ⏎.
pub fn paste_preview(text: &str) -> String {
    let lines: Vec<&str> = text.trim_end_matches('\n').lines().collect();
    let n = lines.len();
    let joined: String = lines.join("  ⏎  ").chars().take(240).collect();
    format!("Paste {n} lines?   {joined}")
}

#[allow(clippy::too_many_arguments)]
pub fn draw_paste<C: Canvas>(
    c: &mut C,
    l: &ReopenLayout,
    text: &str,
    hover: Option<Hit>,
    fonts: &ChromeFonts,
    col: &Colors,
    s: f64,
) {
    let b = px(l.bar, s);
    c.fill(mix(col.bar, col.waiting, 0.14), 1.0, b.x, b.y, b.w, b.h);
    c.fill(col.line, 1.0, b.x, b.y + b.h - s.max(1.0), b.w, s.max(1.0));
    c.text(
        &fonts.label,
        &paste_preview(text),
        b.x + 12.0 * s,
        b.y + (b.h - fonts.label.height()) / 2.0,
        (l.yes.x - 20.0) * s,
        col.text,
        Trunc::Tail,
    );
    draw_button(c, l.yes, "Paste", &fonts.label, col, s, hover == Some(Hit::PasteYes), true);
    draw_button(c, l.no, "Cancel", &fonts.label, col, s, hover == Some(Hit::PasteNo), false);
}

#[allow(clippy::too_many_arguments)]
pub fn draw_reopen<C: Canvas>(
    c: &mut C,
    l: &ReopenLayout,
    text: &str,
    hover: Option<Hit>,
    fonts: &ChromeFonts,
    col: &Colors,
    s: f64,
) {
    let b = px(l.bar, s);
    c.fill(mix(col.bar, col.accent, 0.12), 1.0, b.x, b.y, b.w, b.h);
    c.fill(col.line, 1.0, b.x, b.y + b.h - s.max(1.0), b.w, s.max(1.0));
    c.text(
        &fonts.label,
        text,
        b.x + 12.0 * s,
        b.y + (b.h - fonts.label.height()) / 2.0,
        (l.yes.x - 20.0) * s,
        col.text,
        Trunc::Tail,
    );
    draw_button(c, l.yes, "Reopen", &fonts.label, col, s, hover == Some(Hit::ReopenYes), true);
    draw_button(c, l.no, "Not Now", &fonts.label, col, s, hover == Some(Hit::ReopenNo), false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tabs_share_the_bar_and_overflow_scrolls() {
        let l = bar_layout(&[1, 2, 3], 900.0, 8.0, 0.0);
        assert_eq!(l.tabs.len(), 3);
        assert!(l.left.is_none());
        assert_eq!(l.tabs[0].1.w, TAB_MAX, "few tabs get the most width");
        // The + sits just after the last tab, the buttons at the right.
        assert!(l.plus.x >= l.tabs[2].1.x + l.tabs[2].1.w);
        assert!(l.buttons.iter().all(|(_, r)| r.x > l.plus.x));
        assert_eq!(l.hit(l.tabs[1].1.x + 60.0, 20.0), Hit::Tab { tab: 2, index: 1, close: false });
        assert_eq!(l.hit(l.tabs[1].1.x + 5.0, 20.0), Hit::Tab { tab: 2, index: 1, close: true });
        assert_eq!(l.hit(l.plus.x + 5.0, 20.0), Hit::Plus);
        assert!(matches!(l.hit(l.buttons[1].1.x + 5.0, 20.0), Hit::Button(Button::History)));
        assert_eq!(l.insertion(l.tabs[1].1.x + 10.0), 1);
        assert_eq!(l.insertion(10_000.0), 3);
        // Thirty tabs don't fit: chevrons, and scrolling brings the last into view.
        let ids: Vec<u64> = (1..=30).collect();
        let l = bar_layout(&ids, 900.0, 8.0, 0.0);
        assert!(l.left.is_some() && l.right.is_some());
        assert_eq!(l.tabs[0].1.w, TAB_MIN);
        let sc = l.scroll_to(30, 0.0);
        let l2 = bar_layout(&ids, 900.0, 8.0, sc);
        let r = l2.tabs[29].1;
        assert!(r.x + r.w <= l2.strip.x + l2.strip.w + 0.5 && r.x >= l2.strip.x - 0.5);
        // Clicks on hidden tabs (outside the strip) don't count.
        assert_eq!(l2.hit(l2.tabs[0].1.x + 50.0, 20.0), Hit::Bar);
    }

    #[test]
    fn traffic_lights_push_tabs_right() {
        let l = bar_layout(&[1], 900.0, TRAFFIC_LIGHTS, 0.0);
        assert!(l.tabs[0].1.x >= TRAFFIC_LIGHTS);
        assert_eq!(l.hit(20.0, 20.0), Hit::Bar);
        // Drawn buttons sit in that room, left of the first tab; the bar around them still drags.
        let l = l.with_lights();
        assert_eq!(l.hit(20.0, 20.0), Hit::Light(Light::Close));
        assert_eq!(l.hit(40.0, 20.0), Hit::Light(Light::Minimise));
        assert_eq!(l.hit(60.0, 20.0), Hit::Light(Light::FullScreen));
        assert!(l.lights.iter().all(|(_, r)| r.x + r.w < l.tabs[0].1.x));
        assert_eq!(l.hit(20.0, 37.0), Hit::Bar);
    }

    #[test]
    fn find_and_reopen_bars_hit() {
        let f = find_layout(800.0, 500.0);
        assert_eq!(f.hit(f.field.x + 3.0, f.field.y + 3.0), Some(Hit::FindField));
        assert_eq!(f.hit(f.done.x + 3.0, f.done.y + 3.0), Some(Hit::FindDone));
        assert_eq!(f.hit(3.0, 10.0), None);
        let r = reopen_layout(800.0, 40.0);
        assert_eq!(r.hit(r.yes.x + 1.0, r.yes.y + 1.0), Some(Hit::ReopenYes));
        assert_eq!(paste_preview("echo a\necho b\n"), "Paste 2 lines?   echo a  ⏎  echo b");
    }
}
