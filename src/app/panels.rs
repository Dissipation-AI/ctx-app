//! Panels drawn over a window's terminal area (no extra windows, so no extra pixel buffers):
//! History (closed tabs, their saved commands and output, with Reopen), Activity (what other
//! programs did), Find in All Tabs, and the Overview (every window, its tabs and what runs in
//! them, as a tree you can rearrange by dragging).
//!
//! A panel is laid out as a list of [`Item`]s (a rectangle, how to draw it, and what clicking it
//! does), so drawing and hit-testing can't disagree.

use super::chrome::{ChromeFonts, Status};
use super::ui::{self, Colors, Field};
use crate::layout::Rect;
use crate::render::{Canvas, Fonts, PathOp, Rgb, TileFrame, Trunc, mix};
use crate::text::Row;
use ctx_protocol::*;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    History,
    Activity,
    FindAll,
    Overview,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::History => "History",
            Kind::Activity => "Activity",
            Kind::FindAll => "Find in All Tabs",
            Kind::Overview => "Overview",
        }
    }
}

/// Who opened a closed tab, for History's filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Who {
    Anyone,
    You,
    Agents,
}

/// How long ago, for History's filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum When {
    Any,
    Today,
    Week,
    Month,
}

/// What clicking part of a panel does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanelHit {
    Close,
    Field,
    Who(Who),
    When(When),
    /// A closed tab in History's list.
    Closed(u64),
    Reopen(u64),
    /// An open tab (Activity, Find, Overview): show it.
    Tab(u64),
    /// One of Find's results.
    Found(usize),
    /// An Overview tab card: show it, or drag it.
    Card(u64),
    /// An Overview window card (a drop target).
    Window(u64),
    /// The Overview's "new window" drop target.
    NewWindow,
}

/// One panel's state.
pub struct Panel {
    pub kind: Kind,
    pub field: Field,
    /// Scroll offsets in points: the main list, and History's viewer.
    pub scroll: [f64; 2],
    pub hover: Option<PanelHit>,
    pub closed: Vec<ClosedTab>,
    pub selected: Option<u64>,
    /// The selected closed tab's saved output, as terminal rows.
    pub records: Option<(u64, Vec<Row>)>,
    pub who: Who,
    pub when: When,
    pub found: Option<FindResults>,
    pub searching: bool,
    /// A card being dragged in the Overview: tab, pointer (points).
    pub drag: Option<(u64, f64, f64, bool)>,
}

impl Panel {
    pub fn new(kind: Kind) -> Panel {
        let placeholder = match kind {
            Kind::History => "Filter by name, folder, who opened it, or date",
            Kind::FindAll => "Text to find in open and saved tabs, then Return",
            Kind::Activity => "Filter",
            Kind::Overview => "Filter tabs",
        };
        Panel {
            kind,
            field: Field::new(placeholder),
            scroll: [0.0; 2],
            hover: None,
            closed: Vec::new(),
            selected: None,
            records: None,
            who: Who::Anyone,
            when: When::Any,
            found: None,
            searching: false,
            drag: None,
        }
    }

    /// Closed tabs that pass History's filters, newest first.
    pub fn visible_closed(&self) -> Vec<&ClosedTab> {
        let words: Vec<String> = self.field.text.to_lowercase().split_whitespace().map(str::to_string).collect();
        let now = ctx_protocol::time::now_ms();
        self.closed
            .iter()
            .filter(|t| match self.who {
                Who::Anyone => true,
                Who::You => t.opened_by == "you",
                Who::Agents => t.opened_by != "you",
            })
            .filter(|t| {
                let age = ctx_protocol::time::parse_iso(&t.closed_at).map(|ms| now - ms).unwrap_or(0);
                let day = 86_400_000;
                match self.when {
                    When::Any => true,
                    When::Today => age < day,
                    When::Week => age < 7 * day,
                    When::Month => age < 30 * day,
                }
            })
            .filter(|t| history_matches(t, &words))
            .collect()
    }
}

/// Whether a closed tab matches every word of History's filter. A number matches the tab's
/// number or part of its date (`tab 6`, `2026-10`), not any digit in a timestamp.
fn history_matches(t: &ClosedTab, words: &[String]) -> bool {
    let text = format!("{} {} {} tab", t.name.clone().unwrap_or_default(), t.folder, t.opened_by).to_lowercase();
    let date = t.closed_at.get(..10).unwrap_or("");
    words.iter().all(|w| {
        if w.chars().all(|c| c.is_ascii_digit()) {
            w.parse::<u64>().is_ok_and(|n| n == t.number) || date.split('-').any(|p| p == w)
        } else if w.contains('-') && w.chars().all(|c| c.is_ascii_digit() || c == '-') {
            date.starts_with(w.as_str())
        } else {
            text.contains(w.as_str())
        }
    })
}

/// "12s ago", "3m ago", "2h ago", or a date.
pub fn ago(iso: &str) -> String {
    let Some(ms) = ctx_protocol::time::parse_iso(iso) else { return iso.to_string() };
    let s = ((ctx_protocol::time::now_ms() - ms) / 1000).max(0);
    match s {
        0..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => iso.get(..10).unwrap_or(iso).to_string(),
    }
}

/// A saved tab's commands and output as terminal rows, coloured as they were.
pub fn record_rows(rec: &CommandOutputs, palette: &[[u8; 3]; 256]) -> Vec<Row> {
    let mut rows = Vec::new();
    let bold = CellStyle { flags: 1, ..Default::default() };
    if rec.commands.is_empty() {
        rows.push(Row { runs: vec![StyledRun { text: "No commands were saved for this tab.".into(), style: CellStyle { flags: 4, ..Default::default() } }], wraps: false, event: false });
    }
    for c in &rec.commands {
        let by = if c.ran_by == "you" { String::new() } else { format!("  (ran by {})", c.ran_by) };
        let status = match c.exit_status {
            Some(0) | None => String::new(),
            Some(n) => format!("  [exit {n}]"),
        };
        rows.push(Row { runs: vec![StyledRun { text: format!("{} {}{by}{status}", c.location, c.command), style: bold }], wraps: false, event: false });
        let events: Vec<u64> = c.events.iter().map(|e| e.line).collect();
        for (i, line) in c.output.iter().enumerate() {
            rows.push(Row { runs: ctx_protocol::sgr::runs(line, palette), wraps: false, event: events.contains(&(i as u64)) });
        }
        rows.push(Row::default());
    }
    rows
}

// ---------------------------------------------------------------------------------------------
// Layout as items
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Font {
    Title,
    Label,
    Bold,
    Small,
}

pub enum Op {
    Fill { c: Rgb, a: f64, radius: f64 },
    Text { font: Font, s: String, c: Rgb, trunc: Trunc },
    Field,
    Stroke { path: Vec<PathOp>, w: f64, c: Rgb },
    /// Terminal rows (History's viewer), drawn as the terminal draws them.
    Mono { rows: Vec<Row>, fg: Rgb, bg: Rgb },
    Clip(Option<Rect>),
}

pub struct Item {
    pub rect: Rect,
    pub op: Op,
    pub hit: Option<PanelHit>,
}

/// What a panel needs from the app to lay itself out.
pub struct Ctx<'a> {
    pub list: &'a TabList,
    pub activity: &'a VecDeque<ActivityEntry>,
    pub statuses: &'a HashMap<u64, Status>,
    /// A tab's theme background.
    pub tab_bg: &'a dyn Fn(u64) -> Rgb,
    pub label: &'a dyn Fn(&TabInfo) -> (String, String),
    pub fonts: &'a ChromeFonts,
    pub mono: Rc<Fonts>,
    pub col: &'a Colors,
    pub scale: f64,
    pub focused_field: bool,
    pub agent_window: Option<u64>,
}

fn fill(items: &mut Vec<Item>, r: Rect, c: Rgb, radius: f64) {
    items.push(Item { rect: r, op: Op::Fill { c, a: 1.0, radius }, hit: None });
}

fn text(items: &mut Vec<Item>, r: Rect, font: Font, s: impl Into<String>, c: Rgb) {
    items.push(Item { rect: r, op: Op::Text { font, s: s.into(), c, trunc: Trunc::Tail }, hit: None });
}

fn hit(items: &mut Vec<Item>, r: Rect, h: PanelHit) {
    items.push(Item { rect: r, op: Op::Clip(None), hit: Some(h) });
}

fn chip(items: &mut Vec<Item>, ctx: &Ctx, x: f64, y: f64, label: &str, on: bool, hover: bool, h: PanelHit) -> f64 {
    let w = ctx.fonts.small.measure(label) / ctx.scale + 18.0;
    let r = Rect::new(x, y, w, 22.0);
    let bg = if on { mix(ctx.col.accent, ctx.col.panel, 0.25) } else if hover { ctx.col.hover } else { ctx.col.field };
    fill(items, r, bg, 11.0);
    text(items, Rect::new(x + 9.0, y + 3.0, w - 14.0, 16.0), Font::Small, label, if on { [255, 255, 255] } else { ctx.col.text });
    hit(items, r, h);
    w
}

/// Lay out a panel over `area` (points).
pub fn layout(p: &Panel, area: Rect, ctx: &Ctx) -> Vec<Item> {
    let mut items = Vec::new();
    let col = ctx.col;
    fill(&mut items, area, col.panel, 0.0);
    // Header: title, field, close.
    let hy = area.y + 10.0;
    text(&mut items, Rect::new(area.x + 16.0, hy + 3.0, 200.0, 22.0), Font::Title, p.kind.title(), col.text);
    let close = Rect::new(area.x + area.w - 40.0, hy, 28.0, 28.0);
    if p.hover == Some(PanelHit::Close) {
        fill(&mut items, close, col.hover, 7.0);
    }
    let (cx, cy, a) = (close.x + 14.0, close.y + 14.0, 5.0);
    items.push(Item {
        rect: close,
        op: Op::Stroke { path: vec![PathOp::Move(cx - a, cy - a), PathOp::Line(cx + a, cy + a), PathOp::Move(cx + a, cy - a), PathOp::Line(cx - a, cy + a)], w: 1.5, c: col.dim },
        hit: Some(PanelHit::Close),
    });
    let title_w = ctx.fonts.title.measure(p.kind.title()) / ctx.scale;
    let fx = area.x + 16.0 + title_w + 20.0;
    let fw = (close.x - 12.0 - fx).min(460.0);
    if p.kind != Kind::Activity && fw > 80.0 {
        let r = Rect::new(fx, hy, fw, 28.0);
        items.push(Item { rect: r, op: Op::Field, hit: Some(PanelHit::Field) });
    }
    let top = hy + 40.0;
    fill(&mut items, Rect::new(area.x, top, area.w, 1.0), col.line, 0.0);
    let body = Rect::new(area.x, top + 1.0, area.w, (area.y + area.h - top - 1.0).max(0.0));
    match p.kind {
        Kind::History => history(&mut items, p, body, ctx),
        Kind::Activity => activity(&mut items, p, body, ctx),
        Kind::FindAll => find_all(&mut items, p, body, ctx),
        Kind::Overview => overview(&mut items, p, body, ctx),
    }
    items
}

/// The scroll region of a panel's main list (for wheel events): index into `scroll`.
pub fn scroll_region(p: &Panel, area: Rect, x: f64) -> usize {
    if p.kind == Kind::History && x > area.x + list_width(area) {
        1
    } else {
        0
    }
}

fn list_width(area: Rect) -> f64 {
    (area.w * 0.38).clamp(240.0, 380.0)
}

fn history(items: &mut Vec<Item>, p: &Panel, body: Rect, ctx: &Ctx) {
    let col = ctx.col;
    let lw = list_width(body);
    // Filter chips.
    let mut x = body.x + 12.0;
    let y = body.y + 8.0;
    for (w, label) in [(Who::Anyone, "Anyone"), (Who::You, "You"), (Who::Agents, "Agents")] {
        x += chip(items, ctx, x, y, label, p.who == w, p.hover == Some(PanelHit::Who(w)), PanelHit::Who(w)) + 4.0;
    }
    x += 10.0;
    for (w, label) in [(When::Any, "Any time"), (When::Today, "Today"), (When::Week, "7 days"), (When::Month, "30 days")] {
        x += chip(items, ctx, x, y, label, p.when == w, p.hover == Some(PanelHit::When(w)), PanelHit::When(w)) + 4.0;
    }
    let list_top = y + 30.0;
    fill(items, Rect::new(body.x + lw, list_top, 1.0, body.y + body.h - list_top), col.line, 0.0);
    // The list.
    let list = Rect::new(body.x, list_top, lw, body.y + body.h - list_top);
    items.push(Item { rect: list, op: Op::Clip(Some(list)), hit: None });
    let shown = p.visible_closed();
    let row_h = 44.0;
    let mut ry = list.y + 4.0 - p.scroll[0];
    if shown.is_empty() {
        let msg = if p.closed.is_empty() { "No closed tabs are saved." } else { "Nothing matches." };
        text(items, Rect::new(list.x + 16.0, ry + 8.0, lw - 24.0, 18.0), Font::Label, msg, col.dim);
    }
    for t in shown {
        if ry + row_h >= list.y && ry <= list.y + list.h {
            let r = Rect::new(list.x + 6.0, ry, lw - 12.0, row_h - 4.0);
            let sel = p.selected == Some(t.number);
            if sel {
                fill(items, r, mix(col.accent, col.panel, 0.55), 7.0);
            } else if p.hover == Some(PanelHit::Closed(t.number)) {
                fill(items, r, col.hover, 7.0);
            }
            let title = match &t.name {
                Some(n) => n.clone(),
                None => super::short_folder(&t.folder),
            };
            text(items, Rect::new(r.x + 10.0, r.y + 4.0, r.w - 20.0, 18.0), Font::Bold, title, col.text);
            let by = if t.opened_by == "you" { String::new() } else { format!(" · {}", super::agent_name(&t.opened_by)) };
            let ended = if t.ended == "interrupted" { " · interrupted" } else { "" };
            let commands = if t.commands == 1 { "1 command".to_string() } else { format!("{} commands", t.commands) };
            let detail = format!("tab {} · {} · {commands}{by}{ended} · {}", t.number, ago(&t.closed_at), super::short_folder(&t.folder));
            text(items, Rect::new(r.x + 10.0, r.y + 22.0, r.w - 20.0, 16.0), Font::Small, detail, col.dim);
            hit(items, r, PanelHit::Closed(t.number));
        }
        ry += row_h;
    }
    items.push(Item { rect: list, op: Op::Clip(None), hit: None });
    // The viewer.
    let vx = body.x + lw + 1.0;
    let viewer = Rect::new(vx, list_top, body.x + body.w - vx, body.y + body.h - list_top);
    let Some(sel) = p.selected else {
        text(items, Rect::new(vx + 16.0, list_top + 12.0, viewer.w - 32.0, 18.0), Font::Label, "Choose a closed tab to see its saved commands and output.", col.dim);
        return;
    };
    let bg = (ctx.tab_bg)(0);
    let reopen = Rect::new(viewer.x + viewer.w - 104.0, viewer.y + 8.0, 92.0, 26.0);
    fill(items, reopen, if p.hover == Some(PanelHit::Reopen(sel)) { mix(col.accent, [255, 255, 255], 0.15) } else { col.accent }, 7.0);
    text(items, Rect::new(reopen.x + 18.0, reopen.y + 5.0, 70.0, 16.0), Font::Label, "Reopen", [255, 255, 255]);
    hit(items, reopen, PanelHit::Reopen(sel));
    if let Some(t) = p.closed.iter().find(|t| t.number == sel) {
        let title = match &t.name {
            Some(n) => format!("tab {} — {n}", t.number),
            None => format!("tab {}", t.number),
        };
        text(items, Rect::new(vx + 14.0, viewer.y + 12.0, reopen.x - vx - 24.0, 18.0), Font::Bold, title, col.text);
    }
    let out = Rect::new(viewer.x + 8.0, viewer.y + 44.0, viewer.w - 16.0, (viewer.h - 52.0).max(0.0));
    fill(items, out, bg, 8.0);
    if let Some((t, rows)) = &p.records
        && *t == sel
    {
        let ch = ctx.mono.cell_h / ctx.scale;
        let skip = (p.scroll[1] / ch).floor().max(0.0) as usize;
        let fit = (out.h / ch).ceil() as usize;
        let rows: Vec<Row> = rows.iter().skip(skip).take(fit).cloned().collect();
        items.push(Item { rect: out, op: Op::Mono { rows, fg: [230, 230, 230], bg }, hit: None });
    } else {
        text(items, Rect::new(out.x + 12.0, out.y + 10.0, out.w - 24.0, 18.0), Font::Label, "Loading…", col.dim);
    }
}

fn activity(items: &mut Vec<Item>, p: &Panel, body: Rect, ctx: &Ctx) {
    let col = ctx.col;
    let clip = Rect::new(body.x, body.y, body.w, body.h);
    items.push(Item { rect: clip, op: Op::Clip(Some(clip)), hit: None });
    let mut y = body.y + 8.0 - p.scroll[0];
    if ctx.activity.is_empty() {
        text(items, Rect::new(body.x + 16.0, y + 6.0, body.w - 32.0, 18.0), Font::Label, "Nothing other than you has touched a tab yet.", col.dim);
    }
    let row_h = 42.0;
    for e in ctx.activity.iter().rev() {
        if y + row_h >= body.y && y <= body.y + body.h {
            let r = Rect::new(body.x + 8.0, y, body.w - 16.0, row_h - 4.0);
            if let Some(t) = e.tab
                && p.hover == Some(PanelHit::Tab(t))
            {
                fill(items, r, col.hover, 7.0);
            }
            let who = match &e.declared_name {
                Some(d) => format!("{} (says it's {d})", e.client),
                None => e.client.clone(),
            };
            let what = match (&e.typed, e.tab) {
                (Some(t), Some(tab)) => format!("typed `{t}` in tab {tab}"),
                (None, Some(tab)) => format!("ran `{}` on tab {tab}", e.sentence),
                (_, None) => format!("ran `{}`", e.sentence),
            };
            let typed = e.typed.is_some();
            let dot = Rect::new(r.x + 10.0, r.y + 9.0, 8.0, 8.0);
            fill(items, dot, if typed { col.typed } else { col.read }, 4.0);
            text(items, Rect::new(r.x + 26.0, r.y + 3.0, r.w - 130.0, 18.0), Font::Bold, who, col.text);
            text(items, Rect::new(r.x + 26.0, r.y + 20.0, r.w - 40.0, 16.0), Font::Small, what, col.dim);
            text(items, Rect::new(r.x + r.w - 100.0, r.y + 4.0, 92.0, 16.0), Font::Small, ago(&e.at), col.faint);
            if let Some(t) = e.tab {
                hit(items, r, PanelHit::Tab(t));
            }
        }
        y += row_h;
    }
    items.push(Item { rect: clip, op: Op::Clip(None), hit: None });
}

fn find_all(items: &mut Vec<Item>, p: &Panel, body: Rect, ctx: &Ctx) {
    let col = ctx.col;
    items.push(Item { rect: body, op: Op::Clip(Some(body)), hit: None });
    let mut y = body.y + 8.0 - p.scroll[0];
    match &p.found {
        None => {
            let msg = if p.searching { "Searching…" } else { "Type what to find above and press Return. Every tab is searched, open and closed." };
            text(items, Rect::new(body.x + 16.0, y + 6.0, body.w - 32.0, 18.0), Font::Label, msg, col.dim);
        }
        Some(f) if f.hits.is_empty() => {
            text(items, Rect::new(body.x + 16.0, y + 6.0, body.w - 32.0, 18.0), Font::Label, "Nothing found.", col.dim);
        }
        Some(f) => {
            for (i, h) in f.hits.iter().enumerate() {
                let lines = h.before.len() + 1 + h.after.len();
                let rh = 24.0 + lines as f64 * 17.0 + 8.0;
                if y + rh >= body.y && y <= body.y + body.h {
                    let r = Rect::new(body.x + 8.0, y, body.w - 16.0, rh - 4.0);
                    if p.hover == Some(PanelHit::Found(i)) {
                        fill(items, r, col.hover, 7.0);
                    }
                    let name = h.tab_name.as_ref().map(|n| format!(" “{n}”")).unwrap_or_default();
                    let what = match (&h.command_text, h.found_in.as_str()) {
                        (Some(c), "command") => format!("command `{c}`"),
                        (Some(c), _) => format!("output of `{c}`"),
                        (None, w) => w.to_string(),
                    };
                    let head = format!("tab {}{name}", h.tab);
                    let hw = ctx.fonts.label_bold.measure(&head) / ctx.scale;
                    text(items, Rect::new(r.x + 10.0, r.y + 4.0, r.w - 20.0, 18.0), Font::Bold, head, col.accent);
                    text(items, Rect::new(r.x + 18.0 + hw, r.y + 5.0, r.w - hw - 30.0, 16.0), Font::Small, format!("{what} · {}", ago(&h.at)), col.dim);
                    let mut ly = r.y + 24.0;
                    for (k, l) in h.before.iter().chain(std::iter::once(&h.text)).chain(h.after.iter()).enumerate() {
                        let main = k == h.before.len();
                        text(items, Rect::new(r.x + 18.0, ly, r.w - 30.0, 16.0), if main { Font::Label } else { Font::Small }, l.clone(), if main { col.text } else { col.faint });
                        ly += 17.0;
                    }
                    hit(items, r, PanelHit::Found(i));
                }
                y += rh;
            }
        }
    }
    items.push(Item { rect: body, op: Op::Clip(None), hit: None });
}

/// Where each Overview card goes: window cards in the first column, their tabs in the second,
/// what runs in each tab in the third, joined by lines.
fn overview(items: &mut Vec<Item>, p: &Panel, body: Rect, ctx: &Ctx) {
    let col = ctx.col;
    items.push(Item { rect: body, op: Op::Clip(Some(body)), hit: None });
    let words: Vec<String> = p.field.text.to_lowercase().split_whitespace().map(str::to_string).collect();
    let (wx, tx, px_) = (body.x + 20.0, body.x + 230.0, body.x + 230.0 + 300.0 + 40.0);
    let (ww, tw, pw) = (180.0, 300.0, (body.x + body.w - px_ - 20.0).clamp(160.0, 360.0));
    let card_h = 46.0;
    let gap = 10.0;
    let mut y = body.y + 16.0 - p.scroll[0];
    let line = mix(col.dim, col.panel, 0.3);
    let dragging = p.drag.filter(|d| d.3).map(|d| d.0);
    for w in &ctx.list.windows {
        let tabs: Vec<&TabInfo> = w
            .tabs
            .iter()
            .filter(|t| {
                let (l, f) = (ctx.label)(t);
                let hay = format!("{l} {f} {} {}", t.running.clone().unwrap_or_default(), t.opened_by).to_lowercase();
                words.iter().all(|x| hay.contains(x))
            })
            .collect();
        if tabs.is_empty() && !words.is_empty() {
            continue;
        }
        let rows = tabs.len().max(1) as f64;
        let group_h = rows * (card_h + gap) - gap;
        // Window card, centred on its tabs.
        let wy = y + (group_h - card_h) / 2.0;
        let wr = Rect::new(wx, wy, ww, card_h);
        let drop = dragging.is_some() && p.hover == Some(PanelHit::Window(w.number));
        fill(items, wr, if drop { mix(col.accent, col.panel, 0.5) } else { col.field }, 9.0);
        let wname = match &w.name {
            Some(n) => n.clone(),
            None => format!("Window {}", w.number),
        };
        let agents = Some(w.number) == ctx.agent_window;
        text(items, Rect::new(wr.x + 12.0, wr.y + 6.0, ww - 24.0, 18.0), Font::Bold, wname, col.text);
        let n = w.tabs.len();
        let sub = format!("{n} {}{}", if n == 1 { "tab" } else { "tabs" }, if agents { " · agents" } else { "" });
        text(items, Rect::new(wr.x + 12.0, wr.y + 25.0, ww - 24.0, 16.0), Font::Small, sub, col.dim);
        hit(items, wr, PanelHit::Window(w.number));
        let mut ty = y;
        for t in &tabs {
            // Elbow from the window card to the tab card.
            let (x0, y0) = (wr.x + ww, wr.y + card_h / 2.0);
            let (x1, y1) = (tx, ty + card_h / 2.0);
            let mx = (x0 + x1) / 2.0;
            items.push(Item { rect: body, op: Op::Stroke { path: vec![PathOp::Move(x0, y0), PathOp::Line(mx, y0), PathOp::Line(mx, y1), PathOp::Line(x1, y1)], w: 1.2, c: line }, hit: None });
            let tr = Rect::new(tx, ty, tw, card_h);
            let ghost = dragging == Some(t.number);
            let bg = (ctx.tab_bg)(t.number);
            let hover = p.hover == Some(PanelHit::Card(t.number));
            fill(items, tr, if hover { mix(bg, [255, 255, 255], 0.08) } else { bg }, 9.0);
            if ghost {
                items.push(Item { rect: tr, op: Op::Fill { c: col.panel, a: 0.6, radius: 9.0 }, hit: None });
            }
            let (label, folder) = (ctx.label)(t);
            let status = ctx.statuses.get(&t.number).copied().unwrap_or(Status::Idle);
            let (dot, state) = match status {
                Status::Idle => (col.faint, "idle"),
                Status::Busy => (col.busy, "running"),
                Status::AgentWorking => (col.agent, "agent working"),
                Status::AgentWaiting => (col.waiting, "waiting for you"),
            };
            fill(items, Rect::new(tr.x + 12.0, tr.y + 10.0, 8.0, 8.0), dot, 4.0);
            text(items, Rect::new(tr.x + 28.0, tr.y + 5.0, tw - 120.0, 18.0), Font::Bold, label, [235, 235, 235]);
            text(items, Rect::new(tr.x + tw - 108.0, tr.y + 6.0, 98.0, 16.0), Font::Small, format!("tab {} · {state}", t.number), [190, 190, 190]);
            text(items, Rect::new(tr.x + 28.0, tr.y + 25.0, tw - 40.0, 16.0), Font::Small, folder, [170, 170, 170]);
            hit(items, tr, PanelHit::Card(t.number));
            // What runs in it.
            if let Some(run) = &t.running {
                let (x0, y0) = (tr.x + tw, tr.y + card_h / 2.0);
                items.push(Item { rect: body, op: Op::Stroke { path: vec![PathOp::Move(x0, y0), PathOp::Line(px_, y0)], w: 1.2, c: line }, hit: None });
                let pr = Rect::new(px_, ty + 4.0, pw, card_h - 8.0);
                fill(items, pr, col.field, 8.0);
                let pid = t.pid.map(|p| format!("pid {p}")).unwrap_or_default();
                let since = t.started.as_deref().map(ago).map(|a| format!(" · since {}", a.trim_end_matches(" ago"))).unwrap_or_default();
                text(items, Rect::new(pr.x + 10.0, pr.y + 3.0, pw - 20.0, 18.0), Font::Label, run.clone(), col.text);
                text(items, Rect::new(pr.x + 10.0, pr.y + 20.0, pw - 20.0, 16.0), Font::Small, format!("{pid}{since}"), col.dim);
                hit(items, pr, PanelHit::Card(t.number));
            }
            ty += card_h + gap;
        }
        y += group_h + 28.0;
    }
    // Drop here for a new window.
    if dragging.is_some() {
        let r = Rect::new(wx, y, ww, card_h);
        let on = p.hover == Some(PanelHit::NewWindow);
        fill(items, r, if on { mix(col.accent, col.panel, 0.5) } else { col.hover }, 9.0);
        text(items, Rect::new(r.x + 12.0, r.y + 14.0, ww - 24.0, 18.0), Font::Label, "+ New window", col.text);
        hit(items, r, PanelHit::NewWindow);
    }
    // The dragged card follows the pointer.
    if let Some((tab, x, py, true)) = p.drag
        && let Some(t) = ctx.list.windows.iter().flat_map(|w| w.tabs.iter()).find(|t| t.number == tab)
    {
        let r = Rect::new(x - tw / 2.0, py - card_h / 2.0, tw, card_h);
        items.push(Item { rect: r, op: Op::Fill { c: [0, 0, 0], a: 0.3, radius: 10.0 }, hit: None });
        fill(items, Rect::new(r.x, r.y - 2.0, r.w, r.h), (ctx.tab_bg)(t.number), 9.0);
        let (label, folder) = (ctx.label)(t);
        text(items, Rect::new(r.x + 14.0, r.y + 3.0, tw - 28.0, 18.0), Font::Bold, label, [240, 240, 240]);
        text(items, Rect::new(r.x + 14.0, r.y + 22.0, tw - 28.0, 16.0), Font::Small, folder, [180, 180, 180]);
    }
    items.push(Item { rect: body, op: Op::Clip(None), hit: None });
}

/// How tall a panel's main list is, for clamping its scroll.
pub fn content_height(p: &Panel, ctx: &Ctx) -> [f64; 2] {
    let main = match p.kind {
        Kind::History => p.visible_closed().len() as f64 * 44.0 + 40.0,
        Kind::Activity => ctx.activity.len() as f64 * 42.0 + 16.0,
        Kind::FindAll => p.found.as_ref().map(|f| f.hits.iter().map(|h| 32.0 + (h.before.len() + 1 + h.after.len()) as f64 * 17.0).sum()).unwrap_or(0.0) + 16.0,
        Kind::Overview => ctx.list.windows.iter().map(|w| w.tabs.len().max(1) as f64 * 56.0 + 28.0).sum::<f64>() + 80.0,
    };
    let viewer = p.records.as_ref().map(|(_, r)| r.len() as f64 * ctx.mono.cell_h / ctx.scale + 40.0).unwrap_or(0.0);
    [main, viewer]
}

/// Draw laid-out items (points → pixels at scale `s`).
pub fn draw<C: Canvas>(c: &mut C, p: &Panel, items: &[Item], ctx: &Ctx, caret_on: bool) {
    let s = ctx.scale;
    let px = |r: Rect| Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
    for it in items {
        let r = px(it.rect);
        match &it.op {
            Op::Fill { c: col, a, radius } => {
                if *radius > 0.0 {
                    c.fill_rounded(*col, *a, r.x, r.y, r.w, r.h, [radius * s; 4]);
                } else {
                    c.fill(*col, *a, r.x, r.y, r.w, r.h);
                }
            }
            Op::Text { font, s: t, c: col, trunc } => {
                let f = match font {
                    Font::Title => &ctx.fonts.title,
                    Font::Label => &ctx.fonts.label,
                    Font::Bold => &ctx.fonts.label_bold,
                    Font::Small => &ctx.fonts.small,
                };
                c.text(f, t, r.x, r.y, r.w, *col, *trunc);
            }
            Op::Field => ui::draw_field(c, &p.field, r, &ctx.fonts.label, ctx.col, ctx.focused_field, s, caret_on),
            Op::Stroke { path, w, c: col } => {
                let path: Vec<PathOp> = path
                    .iter()
                    .map(|op| match *op {
                        PathOp::Move(x, y) => PathOp::Move(x * s, y * s),
                        PathOp::Line(x, y) => PathOp::Line(x * s, y * s),
                        PathOp::ArcTo { x1, y1, x2, y2, r } => PathOp::ArcTo { x1: x1 * s, y1: y1 * s, x2: x2 * s, y2: y2 * s, r: r * s },
                    })
                    .collect();
                c.stroke(&path, w * s, *col, 1.0);
            }
            Op::Mono { rows, fg, bg } => {
                let tile = TileFrame {
                    x: r.x,
                    y: r.y,
                    w: r.w,
                    h: r.h,
                    fonts: ctx.mono.clone(),
                    rows: rows.iter().map(|row| (row, true)).collect(),
                    foreground: *fg,
                    background: *bg,
                    selection_color: ctx.col.selected,
                    cursor: None,
                    selection: Vec::new(),
                    matches: Vec::new(),
                    link: None,
                    marked: None,
                    scrolled: None,
                };
                crate::render::draw_tiles(c, (r.x, r.y, r.w, r.h), std::slice::from_ref(&tile), &crate::render::Damage::All, *bg);
            }
            Op::Clip(rect) => {
                if it.hit.is_none() {
                    c.clip(rect.map(|r| {
                        let r = px(r);
                        (r.x, r.y, r.w, r.h)
                    }));
                }
            }
        }
    }
    c.clip(None);
}

/// What's under a point (points), topmost first.
pub fn hit_test(items: &[Item], x: f64, y: f64) -> Option<PanelHit> {
    items.iter().rev().find(|it| it.hit.is_some() && it.rect.contains(x, y)).and_then(|it| it.hit)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn closed(n: u64, name: &str, by: &str, closed_at: &str) -> ClosedTab {
        ClosedTab {
            number: n,
            name: Some(name.into()),
            folder: "/code/site".into(),
            opened_by: by.into(),
            opened_at: closed_at.into(),
            closed_at: closed_at.into(),
            ended: "closed".into(),
            commands: 3,
        }
    }

    #[test]
    fn history_filters() {
        let mut p = Panel::new(Kind::History);
        let now = ctx_protocol::time::now_ms();
        let iso = |ms: i64| ctx_protocol::time::iso_utc(ms);
        p.closed = vec![
            closed(1, "deploy", "you", &iso(now - 3_600_000)),
            closed(2, "auth refactor", "claude (pid 9)", &iso(now - 3 * 86_400_000)),
            closed(3, "old", "you", &iso(now - 40 * 86_400_000)),
        ];
        assert_eq!(p.visible_closed().len(), 3);
        p.who = Who::Agents;
        assert_eq!(p.visible_closed().iter().map(|t| t.number).collect::<Vec<_>>(), vec![2]);
        p.who = Who::Anyone;
        p.when = When::Week;
        assert_eq!(p.visible_closed().len(), 2);
        p.when = When::Today;
        assert_eq!(p.visible_closed().iter().map(|t| t.number).collect::<Vec<_>>(), vec![1]);
        p.when = When::Any;
        p.field.set("site dep");
        assert_eq!(p.visible_closed().iter().map(|t| t.number).collect::<Vec<_>>(), vec![1]);
        // A number is a tab number, not any digit in a date.
        p.field.set("tab 2");
        assert_eq!(p.visible_closed().iter().map(|t| t.number).collect::<Vec<_>>(), vec![2]);
    }
}
