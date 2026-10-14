//! What the app knows about a tab it is showing: the screen rows the server sent, the scrollback
//! lines fetched so far (a small cache, filled on demand), and where the view is scrolled to.
//! Nothing here touches AppKit, so it is all unit-tested.

use crate::text::Row;
use ctx_protocol::{CursorShape, HistoryLines, ScreenUpdate};
use std::collections::BTreeMap;

/// A row on screen, wherever it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Place {
    /// Row `sub` of scrollback line `line` (lines are wrapped to the view's width).
    History { line: u64, sub: usize },
    /// A row of the program's screen.
    Screen(u16),
}

impl PartialOrd for Place {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Place {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        use std::cmp::Ordering::*;
        match (self, other) {
            (Place::History { line: a, sub: s }, Place::History { line: b, sub: t }) => (a, s).cmp(&(b, t)),
            (Place::History { .. }, Place::Screen(_)) => Less,
            (Place::Screen(_), Place::History { .. }) => Greater,
            (Place::Screen(a), Place::Screen(b)) => a.cmp(b),
        }
    }
}

/// The screen of one tab, as last sent by the server.
#[derive(Debug, Clone)]
pub struct Screen {
    pub tab: u64,
    pub columns: u16,
    pub rows: u16,
    pub lines: Vec<Row>,
    pub cursor: Option<(u16, u16)>,
    pub cursor_visible: bool,
    pub cursor_shape: CursorShape,
    pub cursor_blink: bool,
    pub title: Option<String>,
    pub full_screen: bool,
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub mouse_tracking: bool,
    pub at_prompt: bool,
    pub history_first: u64,
    pub history_end: u64,
    /// At least one update arrived.
    pub received: bool,
}

/// What an update changed.
#[derive(Debug, Default, PartialEq)]
pub struct Changed {
    pub rows: Vec<u16>,
    pub resized: bool,
    pub history_moved: bool,
    pub title: bool,
}

impl Screen {
    pub fn new(tab: u64) -> Screen {
        Screen {
            tab,
            columns: 80,
            rows: 24,
            lines: vec![Row::default(); 24],
            cursor: None,
            cursor_visible: true,
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            title: None,
            full_screen: false,
            foreground: [0, 0, 0],
            background: [255, 255, 255],
            mouse_tracking: false,
            at_prompt: false,
            history_first: 0,
            history_end: 0,
            received: false,
        }
    }

    pub fn apply(&mut self, u: ScreenUpdate) -> Changed {
        let mut ch = Changed::default();
        if u.columns != self.columns || u.rows != self.rows {
            self.columns = u.columns;
            self.rows = u.rows;
            self.lines.resize(u.rows as usize, Row::default());
            ch.resized = true;
        }
        for r in u.changed {
            if let Some(slot) = self.lines.get_mut(r.row as usize) {
                *slot = Row { runs: r.runs, wraps: r.wraps, event: r.event, links: r.links };
                ch.rows.push(r.row);
            }
        }
        let old_cursor = self.cursor;
        self.cursor = u.cursor.map(|[x, y]| (x, y));
        if old_cursor != self.cursor {
            for (_, y) in old_cursor.iter().chain(self.cursor.iter()) {
                if !ch.rows.contains(y) {
                    ch.rows.push(*y);
                }
            }
        }
        if self.title != u.title {
            ch.title = true;
        }
        if self.history_first != u.history_first || self.history_end != u.history_end {
            ch.history_moved = true;
        }
        let look_changed = self.foreground != u.default_foreground || self.background != u.default_background;
        self.cursor_visible = u.cursor_visible;
        self.cursor_shape = u.cursor_shape;
        self.cursor_blink = u.cursor_blink;
        self.title = u.title;
        self.full_screen = u.full_screen;
        self.foreground = u.default_foreground;
        self.background = u.default_background;
        self.mouse_tracking = u.mouse_tracking;
        self.at_prompt = u.at_prompt;
        self.history_first = u.history_first;
        self.history_end = u.history_end;
        self.received = true;
        if look_changed {
            ch.resized = true;
        }
        ch
    }
}

/// Scrollback lines fetched from the server, by line number.
#[derive(Debug, Default)]
pub struct History {
    lines: BTreeMap<u64, Row>,
    pub first: u64,
    pub end: u64,
}

/// Most lines kept in the cache; the ones furthest from the view go first.
pub const HISTORY_CACHE_LINES: usize = 4000;

impl History {
    pub fn get(&self, n: u64) -> Option<&Row> {
        self.lines.get(&n)
    }

    pub fn len(&self) -> usize {
        self.lines.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    /// The server's scrollback moved: lines below `first` are gone, and the newest line may have
    /// been still growing (a soft-wrapped line being joined), so it is fetched again.
    pub fn moved(&mut self, first: u64, end: u64) {
        if end < self.end {
            // Cleared (`ctx clear`, ⌘K).
            self.lines.clear();
        } else if self.end > 0 {
            self.lines.remove(&(self.end - 1));
        }
        self.lines.retain(|n, _| *n >= first && *n < end);
        self.first = first;
        self.end = end;
    }

    pub fn insert(&mut self, h: HistoryLines) {
        self.first = h.first;
        self.end = self.end.max(h.end);
        for (i, l) in h.lines.into_iter().enumerate() {
            self.lines.insert(h.from + i as u64, Row { runs: l.runs, wraps: false, event: l.event, links: Vec::new() });
        }
    }

    /// Drop every cached line (the tab is no longer shown; they are fetched again if needed).
    pub fn forget(&mut self) {
        self.lines = BTreeMap::new();
    }

    /// Keep the cache bounded, dropping lines far from `around`.
    pub fn trim(&mut self, around: u64) {
        while self.lines.len() > HISTORY_CACHE_LINES {
            let lo = *self.lines.keys().next().unwrap();
            let hi = *self.lines.keys().next_back().unwrap();
            if around.abs_diff(lo) > around.abs_diff(hi) {
                self.lines.remove(&lo);
            } else {
                self.lines.remove(&hi);
            }
        }
    }

    /// The first line in `a..b` that isn't cached.
    pub fn first_missing(&self, a: u64, b: u64) -> Option<u64> {
        (a.max(self.first)..b.min(self.end)).find(|n| !self.lines.contains_key(n))
    }
}

/// Where the top of the view is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViewPos {
    /// Showing the program's screen, following new output.
    #[default]
    Live,
    /// Scrolled back: the top row is row `sub` of scrollback line `line`.
    At { line: u64, sub: usize },
}

/// The rows to draw, top to bottom, or the scrollback lines that must be fetched first.
#[derive(Debug, PartialEq)]
pub enum Composed {
    Rows(Vec<Place>),
    Need(u64),
}

/// The places shown in a view `height` rows tall, `columns` wide.
pub fn compose(screen: &Screen, history: &History, pos: ViewPos, height: usize, columns: usize) -> Composed {
    let mut out = Vec::with_capacity(height);
    if let ViewPos::At { line, sub } = pos {
        let mut n = line;
        let mut s = sub;
        while out.len() < height && n < screen.history_end {
            let Some(row) = history.get(n) else { return Composed::Need(n) };
            let count = row.wrapped_rows(columns);
            while s < count && out.len() < height {
                out.push(Place::History { line: n, sub: s });
                s += 1;
            }
            n += 1;
            s = 0;
        }
    }
    let mut y = 0u16;
    while out.len() < height && y < screen.rows {
        out.push(Place::Screen(y));
        y += 1;
    }
    Composed::Rows(out)
}

/// Scroll towards older lines by `k` rows. Returns the new position, or a line to fetch first.
pub fn scroll_up(screen: &Screen, history: &History, pos: ViewPos, k: usize, columns: usize) -> Result<ViewPos, u64> {
    let (mut line, mut sub) = match pos {
        ViewPos::Live => (screen.history_end, 0),
        ViewPos::At { line, sub } => (line, sub),
    };
    for _ in 0..k {
        if sub > 0 {
            sub -= 1;
            continue;
        }
        if line <= screen.history_first {
            break;
        }
        let prev = line - 1;
        let Some(row) = history.get(prev) else { return Err(prev) };
        line = prev;
        sub = row.wrapped_rows(columns) - 1;
    }
    if line >= screen.history_end {
        return Ok(ViewPos::Live);
    }
    Ok(ViewPos::At { line, sub })
}

/// Scroll towards newer lines by `k` rows; reaching the screen goes back to following it.
pub fn scroll_down(screen: &Screen, history: &History, pos: ViewPos, k: usize, columns: usize) -> Result<ViewPos, u64> {
    let ViewPos::At { mut line, mut sub } = pos else { return Ok(ViewPos::Live) };
    for _ in 0..k {
        if line >= screen.history_end {
            break;
        }
        let Some(row) = history.get(line) else { return Err(line) };
        if sub + 1 < row.wrapped_rows(columns) {
            sub += 1;
        } else {
            line += 1;
            sub = 0;
        }
    }
    if line >= screen.history_end {
        return Ok(ViewPos::Live);
    }
    Ok(ViewPos::At { line, sub })
}

/// Keep a scrolled-back position valid after the scrollback moved (old lines dropped or cleared).
pub fn clamp(screen: &Screen, pos: ViewPos) -> ViewPos {
    match pos {
        ViewPos::At { line, .. } if line >= screen.history_end => ViewPos::Live,
        ViewPos::At { line, .. } if line < screen.history_first => ViewPos::At { line: screen.history_first, sub: 0 },
        p => p,
    }
}

/// How far through the scrollback the view is, 0 (oldest) to 1 (live), for the scroll indicator.
pub fn scroll_fraction(screen: &Screen, pos: ViewPos) -> f64 {
    match pos {
        ViewPos::Live => 1.0,
        ViewPos::At { line, .. } => {
            let span = (screen.history_end - screen.history_first).max(1) as f64;
            (line - screen.history_first) as f64 / span
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctx_protocol::{HistoryLine, RowUpdate, StyledRun};

    fn update(rows: u16, changed: &[(u16, &str)], first: u64, end: u64) -> ScreenUpdate {
        ScreenUpdate {
            tab: 1,
            columns: 10,
            rows,
            changed: changed
                .iter()
                .map(|(r, t)| RowUpdate {
                    row: *r,
                    runs: vec![StyledRun { text: t.to_string(), ..Default::default() }],
                    event: false,
                    wraps: false,
                    links: Vec::new(),
                })
                .collect(),
            cursor: Some([0, 0]),
            cursor_visible: true,
            title: None,
            full_screen: false,
            default_foreground: [0, 0, 0],
            default_background: [255, 255, 255],
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            mouse_tracking: false,
            at_prompt: true,
            history_first: first,
            history_end: end,
        }
    }

    fn history(first: u64, lines: &[&str]) -> HistoryLines {
        HistoryLines {
            tab: 1,
            first,
            end: first + lines.len() as u64,
            from: first,
            lines: lines
                .iter()
                .map(|t| HistoryLine {
                    runs: vec![StyledRun { text: t.to_string(), ..Default::default() }],
                    event: false,
                })
                .collect(),
        }
    }

    #[test]
    fn updates_replace_changed_rows() {
        let mut s = Screen::new(1);
        let ch = s.apply(update(3, &[(0, "a"), (1, "b"), (2, "c")], 0, 0));
        assert!(ch.resized);
        assert_eq!(s.lines[1].text(), "b");
        let ch = s.apply(update(3, &[(1, "B")], 0, 2));
        assert_eq!(ch.rows, vec![1]);
        assert!(ch.history_moved);
        assert_eq!(s.lines[1].text(), "B");
        assert_eq!(s.lines[2].text(), "c");
    }

    #[test]
    fn compose_live_and_scrolled() {
        let mut s = Screen::new(1);
        s.apply(update(3, &[(0, "x"), (1, "y"), (2, "z")], 0, 3));
        let mut h = History::default();
        assert_eq!(
            compose(&s, &h, ViewPos::Live, 3, 10),
            Composed::Rows(vec![Place::Screen(0), Place::Screen(1), Place::Screen(2)])
        );
        // Scrolling up needs line 2 first.
        assert_eq!(scroll_up(&s, &h, ViewPos::Live, 1, 10), Err(2));
        // Line 1 is 25 columns: three rows at width 10.
        h.insert(history(0, &["zero", "0123456789012345678901234", "two"]));
        let p = scroll_up(&s, &h, ViewPos::Live, 2, 10).unwrap();
        assert_eq!(p, ViewPos::At { line: 1, sub: 2 });
        assert_eq!(
            compose(&s, &h, p, 3, 10),
            Composed::Rows(vec![
                Place::History { line: 1, sub: 2 },
                Place::History { line: 2, sub: 0 },
                Place::Screen(0)
            ])
        );
        // Up past the oldest line stops there.
        let top = scroll_up(&s, &h, p, 100, 10).unwrap();
        assert_eq!(top, ViewPos::At { line: 0, sub: 0 });
        // Down again returns to live.
        assert_eq!(scroll_down(&s, &h, top, 100, 10).unwrap(), ViewPos::Live);
        assert_eq!(scroll_down(&s, &h, top, 1, 10).unwrap(), ViewPos::At { line: 1, sub: 0 });
    }

    #[test]
    fn missing_lines_are_requested() {
        let mut s = Screen::new(1);
        s.apply(update(2, &[], 10, 20));
        let h = History::default();
        assert_eq!(compose(&s, &h, ViewPos::At { line: 15, sub: 0 }, 2, 10), Composed::Need(15));
        // An empty cache knows of no lines yet.
        assert_eq!(h.first_missing(0, 30), None);
    }

    #[test]
    fn cache_trims_and_follows_moves() {
        let mut h = History::default();
        let lines: Vec<String> = (0..5000).map(|i| format!("l{i}")).collect();
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        h.insert(history(0, &refs));
        h.trim(4900);
        assert_eq!(h.len(), HISTORY_CACHE_LINES);
        assert!(h.get(4999).is_some());
        assert!(h.get(0).is_none());
        // The newest line is fetched again after more output (it may have grown).
        h.moved(100, 5001);
        assert!(h.get(4999).is_none());
        assert_eq!(h.first_missing(4990, 5001), Some(4999));
        // Cleared.
        h.moved(0, 0);
        assert!(h.is_empty());
    }

    #[test]
    fn clamping_after_drops() {
        let mut s = Screen::new(1);
        s.apply(update(2, &[], 50, 60));
        assert_eq!(clamp(&s, ViewPos::At { line: 10, sub: 3 }), ViewPos::At { line: 50, sub: 0 });
        assert_eq!(clamp(&s, ViewPos::At { line: 70, sub: 0 }), ViewPos::Live);
        assert!((scroll_fraction(&s, ViewPos::At { line: 55, sub: 0 }) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn places_order_history_before_screen() {
        assert!(Place::History { line: 9, sub: 9 } < Place::Screen(0));
        assert!(Place::History { line: 1, sub: 2 } < Place::History { line: 2, sub: 0 });
        assert!(Place::Screen(1) < Place::Screen(2));
    }
}
