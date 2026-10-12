//! Selecting and copying text. A selection belongs to one tab (a view of one tab draws it), so it
//! can never span two tiles. Copy joins soft-wrapped rows, trims trailing spaces, and keeps wide
//! characters and emoji whole.

use crate::model::Place;
use crate::text::Row;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unit {
    Character,
    Word,
    Line,
}

/// A point between cells: row and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub place: Place,
    pub column: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    pub tab: u64,
    pub anchor: Point,
    pub head: Point,
    pub unit: Unit,
}

/// The rows a selection reads from.
pub trait Source {
    /// The row at a place (scrollback rows already wrapped to the view's width).
    fn row(&self, p: Place) -> Option<Row>;
    fn next(&self, p: Place) -> Option<Place>;
    fn prev(&self, p: Place) -> Option<Place>;
    /// Scrollback rows have no wide flags; their widths are measured.
    fn measured(&self, p: Place) -> bool {
        matches!(p, Place::History { .. })
    }
}

/// Characters that belong to a word for double-click (paths and URLs select whole).
pub fn is_word_char(s: &str) -> bool {
    s.chars().all(|c| c.is_alphanumeric() || "-_./~:@%+#=?&".contains(c))
}

impl Selection {
    pub fn new(tab: u64, at: Point, unit: Unit) -> Selection {
        Selection { tab, anchor: at, head: at, unit }
    }

    /// Start and end (end exclusive in columns), expanded to whole words or lines.
    pub fn range(&self, src: &dyn Source) -> (Point, Point) {
        let (mut a, mut b) = if self.anchor <= self.head { (self.anchor, self.head) } else { (self.head, self.anchor) };
        match self.unit {
            Unit::Character => {
                // The head's cell is included when dragging forwards.
                b.column += 1;
            }
            Unit::Word => {
                a.column = word_start(src, a);
                b.column = word_end(src, b);
            }
            Unit::Line => {
                // Whole logical lines: back to the row after one that doesn't wrap.
                while let Some(p) = src.prev(a.place) {
                    if src.row(p).is_some_and(|r| r.wraps) {
                        a.place = p;
                    } else {
                        break;
                    }
                }
                while src.row(b.place).is_some_and(|r| r.wraps) {
                    match src.next(b.place) {
                        Some(n) => b.place = n,
                        None => break,
                    }
                }
                a.column = 0;
                b.column = usize::MAX;
            }
        }
        (a, b)
    }

    pub fn contains(&self, src: &dyn Source, p: Place, column: usize) -> bool {
        let (a, b) = self.range(src);
        if p < a.place || p > b.place {
            return false;
        }
        (p != a.place || column >= a.column) && (p != b.place || column < b.column)
    }

    /// The selected text.
    pub fn text(&self, src: &dyn Source) -> String {
        let (a, b) = self.range(src);
        let mut out = String::new();
        let mut line = String::new();
        let mut p = a.place;
        while let Some(row) = src.row(p) {
            let measured = src.measured(p);
            let from = if p == a.place { a.column } else { 0 };
            let to = if p == b.place { b.column } else { usize::MAX };
            line.push_str(&row.text_between(from, to, measured));
            let last = p == b.place;
            if !row.wraps || last {
                out.push_str(line.trim_end_matches(' '));
                line.clear();
                if !last {
                    out.push('\n');
                }
            }
            if last {
                break;
            }
            match src.next(p) {
                Some(n) => p = n,
                None => break,
            }
        }
        if !line.is_empty() {
            out.push_str(line.trim_end_matches(' '));
        }
        out
    }
}

fn cell_texts(src: &dyn Source, p: Place) -> Vec<(usize, usize, String)> {
    let Some(row) = src.row(p) else { return Vec::new() };
    row.cells(src.measured(p)).iter().map(|c| (c.column, c.width, c.text.to_string())).collect()
}

fn word_start(src: &dyn Source, at: Point) -> usize {
    let cells = cell_texts(src, at.place);
    let Some(i) = cells.iter().position(|(c, w, _)| at.column >= *c && at.column < c + w) else { return at.column };
    if !is_word_char(&cells[i].2) {
        return cells[i].0;
    }
    let mut j = i;
    while j > 0 && is_word_char(&cells[j - 1].2) {
        j -= 1;
    }
    cells[j].0
}

fn word_end(src: &dyn Source, at: Point) -> usize {
    let cells = cell_texts(src, at.place);
    let Some(i) = cells.iter().position(|(c, w, _)| at.column >= *c && at.column < c + w) else { return at.column + 1 };
    if !is_word_char(&cells[i].2) {
        return cells[i].0 + cells[i].1;
    }
    let mut j = i;
    while j + 1 < cells.len() && is_word_char(&cells[j + 1].2) {
        j += 1;
    }
    cells[j].0 + cells[j].1
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctx_protocol::{CellStyle, StyledRun};

    struct Rows(Vec<Row>);

    impl Source for Rows {
        fn row(&self, p: Place) -> Option<Row> {
            match p {
                Place::Screen(y) => self.0.get(y as usize).cloned(),
                _ => None,
            }
        }
        fn next(&self, p: Place) -> Option<Place> {
            match p {
                Place::Screen(y) if (y as usize + 1) < self.0.len() => Some(Place::Screen(y + 1)),
                _ => None,
            }
        }
        fn prev(&self, p: Place) -> Option<Place> {
            match p {
                Place::Screen(y) if y > 0 => Some(Place::Screen(y - 1)),
                _ => None,
            }
        }
    }

    fn pt(y: u16, x: usize) -> Point {
        Point { place: Place::Screen(y), column: x }
    }

    fn row(t: &str, wraps: bool) -> Row {
        Row { wraps, ..Row::plain(t) }
    }

    #[test]
    fn copies_across_rows_trimming_trailing_spaces() {
        let src = Rows(vec![row("hello   ", false), row("world", false)]);
        let mut s = Selection::new(1, pt(0, 0), Unit::Character);
        s.head = pt(1, 4);
        assert_eq!(s.text(&src), "hello\nworld");
    }

    #[test]
    fn wrapped_rows_join_without_newline() {
        let src = Rows(vec![row("a long line th", true), row("at wraps", false), row("next", false)]);
        let mut s = Selection::new(1, pt(0, 2), Unit::Character);
        s.head = pt(2, 3);
        assert_eq!(s.text(&src), "long line that wraps\nnext");
    }

    #[test]
    fn backwards_drag_is_the_same_text() {
        let src = Rows(vec![row("abcdef", false)]);
        let mut s = Selection::new(1, pt(0, 4), Unit::Character);
        s.head = pt(0, 1);
        assert_eq!(s.text(&src), "bcde");
    }

    #[test]
    fn word_and_line_units() {
        let src = Rows(vec![row("cd ~/code/site && ls", false), row("first part ", true), row("second", false)]);
        let s = Selection::new(1, pt(0, 6), Unit::Word);
        assert_eq!(s.text(&src), "~/code/site");
        let s = Selection::new(1, pt(2, 1), Unit::Line);
        assert_eq!(s.text(&src), "first part second");
    }

    #[test]
    fn wide_characters_and_emoji_stay_whole() {
        let wide = CellStyle { flags: crate::text::WIDE, ..Default::default() };
        let r = Row {
            runs: vec![
                StyledRun { text: "a".into(), style: CellStyle::default() },
                StyledRun { text: "日本".into(), style: wide },
                StyledRun { text: "b".into(), style: CellStyle::default() },
            ],
            wraps: false,
            event: false,
        };
        let src = Rows(vec![r]);
        // Columns: a=0, 日=1-2, 本=3-4, b=5. Starting on the second half of 日 still takes it.
        let mut s = Selection::new(1, pt(0, 2), Unit::Character);
        s.head = pt(0, 3);
        assert_eq!(s.text(&src), "日本");
    }

    #[test]
    fn contains_matches_text() {
        let src = Rows(vec![row("abc", false), row("def", false)]);
        let mut s = Selection::new(1, pt(0, 1), Unit::Character);
        s.head = pt(1, 0);
        assert!(!s.contains(&src, Place::Screen(0), 0));
        assert!(s.contains(&src, Place::Screen(0), 2));
        assert!(s.contains(&src, Place::Screen(1), 0));
        assert!(!s.contains(&src, Place::Screen(1), 1));
    }
}
