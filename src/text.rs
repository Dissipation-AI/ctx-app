//! Rows of styled runs, split into cells. The server sends runs of `(text, style)` with the
//! terminal's own grid: where each cell's text starts and how wide it is (plain ASCII runs leave
//! that out: one character per cell). Text is placed by that grid, never by guessing widths, so
//! it lines up exactly as the emulator laid it out. Only text the app makes itself (input-method
//! text, panels) is measured with Unicode tables.

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
            runs: vec![StyledRun { text: text.to_string(), ..Default::default() }],
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
            // The terminal's grid, as sent.
            if !run.cells.is_empty() {
                for (i, c) in run.cells.iter().enumerate() {
                    let a = c[0] as usize;
                    let b = run.cells.get(i + 1).map(|n| n[0] as usize).unwrap_or(run.text.len());
                    let text = run.text.get(a..b).unwrap_or("");
                    let width = (c[1] as usize).max(1);
                    out.push(Cell { column: col, text, width, style: run.style });
                    col += width;
                }
                continue;
            }
            // Plain ASCII: one character per cell.
            if run.text.is_ascii() {
                let width = if flagged_wide { 2 } else { 1 };
                for i in 0..run.text.len() {
                    out.push(Cell { column: col, text: &run.text[i..i + 1], width, style: run.style });
                    col += width;
                }
                continue;
            }
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

    /// Split a logical line into rows of at most `columns` columns (wide cells never split). The
    /// rows keep the line's cells.
    pub fn wrap(&self, columns: usize) -> Vec<Row> {
        let columns = columns.max(2);
        let mut rows: Vec<Row> = vec![Row { runs: Vec::new(), wraps: false, event: self.event }];
        let mut col = 0usize;
        for c in self.cells(true) {
            if col + c.width > columns {
                if let Some(last) = rows.last_mut() {
                    last.wraps = true;
                }
                rows.push(Row { runs: Vec::new(), wraps: false, event: self.event });
                col = 0;
            }
            let row = rows.last_mut().unwrap();
            match row.runs.last_mut() {
                Some(r) if r.style == c.style => {
                    let at = r.text.len() as u16;
                    r.text.push_str(c.text);
                    r.cells.push([at, c.width as u16]);
                }
                _ => row.runs.push(StyledRun { text: c.text.to_string(), style: c.style, cells: vec![[0, c.width as u16]] }),
            }
            col += c.width;
        }
        for row in &mut rows {
            for r in &mut row.runs {
                let w = if r.style.flags & WIDE != 0 { 2 } else { 1 };
                if r.text.is_ascii() && r.cells.len() == r.text.len() && r.cells.iter().all(|c| c[1] == w) {
                    r.cells.clear();
                }
            }
        }
        rows
    }

    /// How many rows `wrap(columns)` makes, without building them.
    pub fn wrapped_rows(&self, columns: usize) -> usize {
        let columns = columns.max(2);
        let mut rows = 1usize;
        let mut col = 0usize;
        for c in self.cells(true) {
            if col + c.width > columns {
                rows += 1;
                col = 0;
            }
            col += c.width;
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_terminals_grid_wins_over_unicode_tables() {
        // The server says "✔️" is one cell of width 1 here (say, mode 2027 off): trust it.
        let r = Row { runs: vec![StyledRun { text: "✔️x".into(), style: CellStyle::default(), cells: vec![[0, 1], [6, 1]] }], wraps: false, event: false };
        let cells = r.cells(true);
        assert_eq!(cells.iter().map(|c| (c.column, c.text, c.width)).collect::<Vec<_>>(), vec![(0, "✔️", 1), (1, "x", 1)]);
        // Without the grid the same text would be measured as two columns for the emoji.
        let guessed = Row::plain("✔️x");
        assert_eq!(guessed.cells(true)[1].column, 2);
        // Wrapping keeps the grid.
        let long = Row { runs: vec![StyledRun { text: "中中中".into(), style: CellStyle::default(), cells: vec![[0, 2], [3, 2], [6, 2]] }], wraps: false, event: false };
        let rows = long.wrap(5);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].runs[0].cells, vec![[0, 2], [3, 2]]);
        assert_eq!(rows[1].cells(true)[0].width, 2);
    }

    fn styled(text: &str, flags: u16) -> StyledRun {
        StyledRun { text: text.into(), style: CellStyle { flags, ..Default::default() }, cells: Vec::new() }
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
