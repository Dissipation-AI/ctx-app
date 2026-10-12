//! Rows of styled runs, split into cells. The server sends runs of `(text, style)`; each grapheme
//! is one cell, or two when the server flags it wide (scrollback lines have no flag, so their
//! width comes from Unicode tables, as the emulator's does).

use ctx_protocol::{CellStyle, StyledRun};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub const WIDE: u16 = 256;

/// One row (or one scrollback line) as received.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Row {
    pub runs: Vec<StyledRun>,
    /// Soft-wraps into the next row.
    pub wraps: bool,
    /// An event line drawn by ctx.
    pub event: bool,
}

/// One grapheme placed on the grid.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell<'a> {
    pub column: usize,
    pub text: &'a str,
    /// 1 or 2 columns.
    pub width: usize,
    pub style: CellStyle,
}

/// Width in columns of one grapheme when the server didn't say.
pub fn grapheme_width(g: &str) -> usize {
    if g.chars().any(|c| c == '\u{fe0f}') {
        return 2;
    }
    g.width().clamp(1, 2)
}

impl Row {
    pub fn plain(text: &str) -> Row {
        Row {
            runs: vec![StyledRun { text: text.to_string(), style: CellStyle::default() }],
            wraps: false,
            event: false,
        }
    }

    /// The row's cells, left to right. `measured`: compute widths (scrollback) instead of trusting
    /// the wide flag (screen rows).
    pub fn cells(&self, measured: bool) -> Vec<Cell<'_>> {
        let mut out = Vec::new();
        let mut col = 0usize;
        for run in &self.runs {
            let flagged_wide = run.style.flags & WIDE != 0;
            for g in run.text.graphemes(true) {
                let width = if measured {
                    grapheme_width(g)
                } else if flagged_wide {
                    2
                } else {
                    1
                };
                out.push(Cell { column: col, text: g, width, style: run.style });
                col += width;
            }
        }
        out
    }

    /// Columns used.
    pub fn width(&self, measured: bool) -> usize {
        self.cells(measured).last().map(|c| c.column + c.width).unwrap_or(0)
    }

    /// The text from column `a` up to (not including) column `b`.
    pub fn text_between(&self, a: usize, b: usize, measured: bool) -> String {
        let mut s = String::new();
        let mut next = a;
        for c in self.cells(measured) {
            if c.column + c.width <= a || c.column >= b {
                continue;
            }
            // Gaps (cells the server didn't send) are spaces.
            while next < c.column {
                s.push(' ');
                next += 1;
            }
            s.push_str(c.text);
            next = c.column + c.width;
        }
        s
    }

    pub fn text(&self) -> String {
        self.runs.iter().map(|r| r.text.as_str()).collect()
    }

    /// Split a logical line into rows of at most `columns` columns (wide graphemes never split).
    pub fn wrap(&self, columns: usize) -> Vec<Row> {
        let columns = columns.max(2);
        let mut rows: Vec<Row> = vec![Row { runs: Vec::new(), wraps: false, event: self.event }];
        let mut col = 0usize;
        for run in &self.runs {
            for g in run.text.graphemes(true) {
                let w = grapheme_width(g);
                if col + w > columns {
                    if let Some(last) = rows.last_mut() {
                        last.wraps = true;
                    }
                    rows.push(Row { runs: Vec::new(), wraps: false, event: self.event });
                    col = 0;
                }
                let row = rows.last_mut().unwrap();
                match row.runs.last_mut() {
                    Some(r) if r.style == run.style => r.text.push_str(g),
                    _ => row.runs.push(StyledRun { text: g.to_string(), style: run.style }),
                }
                col += w;
            }
        }
        rows
    }

    /// How many rows `wrap(columns)` makes, without building them.
    pub fn wrapped_rows(&self, columns: usize) -> usize {
        let columns = columns.max(2);
        let mut rows = 1usize;
        let mut col = 0usize;
        for run in &self.runs {
            for g in run.text.graphemes(true) {
                let w = grapheme_width(g);
                if col + w > columns {
                    rows += 1;
                    col = 0;
                }
                col += w;
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled(text: &str, flags: u16) -> StyledRun {
        StyledRun { text: text.into(), style: CellStyle { flags, ..Default::default() } }
    }

    #[test]
    fn wide_flag_places_cells() {
        let r = Row { runs: vec![styled("a", 0), styled("日本", WIDE), styled("b", 0)], wraps: false, event: false };
        let cols: Vec<usize> = r.cells(false).iter().map(|c| c.column).collect();
        assert_eq!(cols, [0, 1, 3, 5]);
        assert_eq!(r.width(false), 6);
        assert_eq!(r.text_between(1, 3, false), "日");
        assert_eq!(r.text_between(2, 6, false), "日本b");
    }

    #[test]
    fn emoji_and_combining_marks_are_one_cell_each() {
        let r = Row::plain("e\u{301}👍🏽x");
        let cells = r.cells(true);
        assert_eq!(cells.len(), 3);
        assert_eq!(cells[0].text, "e\u{301}");
        assert_eq!(cells[1].width, 2);
        assert_eq!(cells[2].column, 3);
    }

    #[test]
    fn wrapping_keeps_wide_graphemes_whole() {
        let r = Row::plain("abc日d");
        let rows = r.wrap(4);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text(), "abc");
        assert!(rows[0].wraps);
        assert_eq!(rows[1].text(), "日d");
        assert!(!rows[1].wraps);
        assert_eq!(r.wrapped_rows(4), 2);
        assert_eq!(Row::default().wrapped_rows(80), 1);
    }
}
