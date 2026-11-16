//! A small markdown editor for an agent's instructions (CLAUDE.md, AGENTS.md): plain text with a
//! caret, drawn with the terminal's renderer so it's monospaced and coloured like the terminal:
//! headings, emphasis, code, lists, quotes and links each get their own colour.
//!
//! Columns are characters: the file is edited by `char`, and wide characters are rare in
//! instructions (they still edit correctly; only the caret may sit half a cell off after one).

use super::ui::FieldKey;
use crate::text::Row;
use ctx_protocol::{CellStyle, StyledRun};

pub struct Editor {
    pub agent: String,
    pub path: String,
    pub lines: Vec<String>,
    /// Caret: line and character column.
    pub row: usize,
    pub col: usize,
    /// First line shown.
    pub top: usize,
    /// Changed since it was loaded or saved.
    pub changed: bool,
}

/// Colours for markdown, picked to read on the terminal background.
pub struct Palette {
    pub text: [u8; 3],
    pub heading: [u8; 3],
    pub emphasis: [u8; 3],
    pub code: [u8; 3],
    pub marker: [u8; 3],
    pub quote: [u8; 3],
    pub link: [u8; 3],
}

impl Editor {
    pub fn new(agent: &str, path: &str, text: &str) -> Editor {
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        Editor { agent: agent.into(), path: path.into(), lines, row: 0, col: 0, top: 0, changed: false }
    }

    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn len(&self, row: usize) -> usize {
        self.lines[row].chars().count()
    }

    fn byte(&self, row: usize, col: usize) -> usize {
        self.lines[row].char_indices().nth(col).map(|(i, _)| i).unwrap_or(self.lines[row].len())
    }

    fn insert(&mut self, s: &str) {
        for (i, part) in s.split('\n').enumerate() {
            if i > 0 {
                let at = self.byte(self.row, self.col);
                let rest = self.lines[self.row].split_off(at);
                self.lines.insert(self.row + 1, rest);
                self.row += 1;
                self.col = 0;
            }
            let at = self.byte(self.row, self.col);
            self.lines[self.row].insert_str(at, part);
            self.col += part.chars().count();
        }
        self.changed = true;
    }

    /// Handle a key; false if the editor doesn't use it (Escape).
    pub fn key(&mut self, k: FieldKey) -> bool {
        match k {
            FieldKey::Text(t) => self.insert(&t.replace('\r', "\n")),
            FieldKey::Enter { .. } => {
                // Keep a list going: "- " or "1. " carries on to the new line.
                let line = &self.lines[self.row];
                let indent: String = line.chars().take_while(|c| *c == ' ').collect();
                let rest = line.trim_start();
                let marker = if rest.starts_with("- ") || rest.starts_with("* ") {
                    rest[..2].to_string()
                } else {
                    String::new()
                };
                let empty_item = !marker.is_empty() && rest.len() == 2;
                if empty_item {
                    // Return on an empty item ends the list.
                    self.lines[self.row].clear();
                    self.col = 0;
                    self.insert("\n");
                } else {
                    self.insert(&format!("\n{indent}{marker}"));
                }
            }
            FieldKey::Tab { shift: false } => self.insert("  "),
            FieldKey::Tab { shift: true } => {}
            FieldKey::Backspace => {
                if self.col > 0 {
                    let (a, b) = (self.byte(self.row, self.col - 1), self.byte(self.row, self.col));
                    self.lines[self.row].replace_range(a..b, "");
                    self.col -= 1;
                    self.changed = true;
                } else if self.row > 0 {
                    let line = self.lines.remove(self.row);
                    self.row -= 1;
                    self.col = self.len(self.row);
                    self.lines[self.row].push_str(&line);
                    self.changed = true;
                }
            }
            FieldKey::Delete => {
                if self.col < self.len(self.row) {
                    let (a, b) = (self.byte(self.row, self.col), self.byte(self.row, self.col + 1));
                    self.lines[self.row].replace_range(a..b, "");
                    self.changed = true;
                } else if self.row + 1 < self.lines.len() {
                    let next = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&next);
                    self.changed = true;
                }
            }
            FieldKey::Left { word, .. } => {
                if self.col > 0 {
                    self.col = if word { self.word_left() } else { self.col - 1 };
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.len(self.row);
                }
            }
            FieldKey::Right { word, .. } => {
                if self.col < self.len(self.row) {
                    self.col = if word { self.word_right() } else { self.col + 1 };
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
            }
            FieldKey::Up => {
                if self.row > 0 {
                    self.row -= 1;
                    self.col = self.col.min(self.len(self.row));
                }
            }
            FieldKey::Down => {
                if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = self.col.min(self.len(self.row));
                }
            }
            FieldKey::Home { .. } => self.col = 0,
            FieldKey::End { .. } => self.col = self.len(self.row),
            FieldKey::SelectAll => {}
            FieldKey::Escape => return false,
        }
        true
    }

    fn word_left(&self) -> usize {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col;
        while i > 0 && !chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        while i > 0 && chars[i - 1].is_alphanumeric() {
            i -= 1;
        }
        i
    }

    fn word_right(&self) -> usize {
        let chars: Vec<char> = self.lines[self.row].chars().collect();
        let mut i = self.col;
        while i < chars.len() && !chars[i].is_alphanumeric() {
            i += 1;
        }
        while i < chars.len() && chars[i].is_alphanumeric() {
            i += 1;
        }
        i
    }

    /// Keep the caret within `rows` visible lines.
    pub fn scroll_to_caret(&mut self, rows: usize) {
        let rows = rows.max(1);
        if self.row < self.top {
            self.top = self.row;
        } else if self.row >= self.top + rows {
            self.top = self.row + 1 - rows;
        }
    }

    /// Put the caret at a cell (row from the top of what's shown, column).
    pub fn click(&mut self, row: usize, col: usize) {
        self.row = (self.top + row).min(self.lines.len() - 1);
        self.col = col.min(self.len(self.row));
    }

    /// The lines shown, coloured.
    pub fn rows(&self, count: usize, p: &Palette) -> Vec<Row> {
        let mut in_fence = self.lines[..self.top].iter().filter(|l| l.trim_start().starts_with("```")).count() % 2 == 1;
        self.lines
            .iter()
            .skip(self.top)
            .take(count)
            .map(|l| {
                let fence = l.trim_start().starts_with("```");
                let runs = if fence || in_fence { vec![run(l, p.code, 0)] } else { markdown(l, p) };
                if fence {
                    in_fence = !in_fence;
                }
                Row { runs, wraps: false, event: false, links: Vec::new() }
            })
            .collect()
    }
}

fn run(text: &str, fg: [u8; 3], flags: u16) -> StyledRun {
    StyledRun {
        text: text.to_string(),
        style: CellStyle { foreground: Some(fg), flags, ..Default::default() },
        cells: Vec::new(),
    }
}

/// One line of markdown as coloured runs. The text is kept exactly (markers included), so
/// columns match the file.
pub fn markdown(line: &str, p: &Palette) -> Vec<StyledRun> {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];
    if trimmed.starts_with('#') {
        return vec![run(line, p.heading, 1)];
    }
    if trimmed.starts_with('>') {
        return vec![run(line, p.quote, 2)];
    }
    let mut out = Vec::new();
    let mut rest = trimmed;
    if !indent.is_empty() {
        out.push(run(indent, p.text, 0));
    }
    // List markers.
    for m in ["- [ ] ", "- [x] ", "- ", "* ", "+ "] {
        if let Some(r) = rest.strip_prefix(m) {
            out.push(run(m, p.marker, 1));
            rest = r;
            break;
        }
    }
    if let Some(dot) = rest.find(". ")
        && dot > 0
        && rest[..dot].chars().all(|c| c.is_ascii_digit())
    {
        out.push(run(&rest[..dot + 2], p.marker, 1));
        rest = &rest[dot + 2..];
    }
    inline(rest, p, &mut out);
    out
}

/// `code`, **bold**, *italic*, [links](url) within a line.
fn inline(mut s: &str, p: &Palette, out: &mut Vec<StyledRun>) {
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut Vec<StyledRun>| {
        if !plain.is_empty() {
            out.push(run(plain, p.text, 0));
            plain.clear();
        }
    };
    while let Some(c) = s.chars().next() {
        let span = match c {
            '`' => s[1..].find('`').map(|e| (e + 2, p.code, 0)),
            '*' | '_' if s[1..].starts_with(c) => s[2..].find(&format!("{c}{c}")).map(|e| (e + 4, p.emphasis, 1)),
            '*' | '_' => s[1..].find(c).filter(|e| *e > 0).map(|e| (e + 2, p.emphasis, 2)),
            '[' => s.find("](").and_then(|m| s[m..].find(')').map(|e| (m + e + 1, p.link, 8))),
            _ => None,
        };
        match span {
            Some((len, fg, flags)) => {
                flush(&mut plain, out);
                out.push(run(&s[..len], fg, flags));
                s = &s[len..];
            }
            None => {
                plain.push(c);
                s = &s[c.len_utf8()..];
            }
        }
    }
    flush(&mut plain, out);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pal() -> Palette {
        Palette {
            text: [1, 1, 1],
            heading: [2, 2, 2],
            emphasis: [3, 3, 3],
            code: [4, 4, 4],
            marker: [5, 5, 5],
            quote: [6, 6, 6],
            link: [7, 7, 7],
        }
    }

    fn joined(runs: &[StyledRun]) -> String {
        runs.iter().map(|r| r.text.as_str()).collect()
    }

    #[test]
    fn colouring_keeps_every_character() {
        for l in [
            "# Title",
            "- item with `code` and **bold**",
            "  1. first [a link](https://x.y)",
            "plain *it* text",
            "> quote",
            "a * b",
        ] {
            assert_eq!(joined(&markdown(l, &pal())), l);
        }
        let r = markdown("- run `ctx help` now", &pal());
        assert_eq!(r[0].style.foreground, Some([5, 5, 5]));
        assert!(r.iter().any(|x| x.text == "`ctx help`" && x.style.foreground == Some([4, 4, 4])));
    }

    #[test]
    fn editing() {
        let mut e = Editor::new("a", "/x", "# A\n- one");
        e.key(FieldKey::Down);
        e.key(FieldKey::End { select: false });
        e.key(FieldKey::Enter { shift: false });
        assert_eq!(e.text(), "# A\n- one\n- ");
        e.key(FieldKey::Text("two".into()));
        e.key(FieldKey::Enter { shift: false });
        e.key(FieldKey::Enter { shift: false });
        assert_eq!(e.text(), "# A\n- one\n- two\n\n");
        e.key(FieldKey::Backspace);
        e.key(FieldKey::Backspace);
        assert_eq!(e.text(), "# A\n- one\n- two");
        e.key(FieldKey::Home { select: false });
        e.key(FieldKey::Delete);
        assert_eq!(e.text(), "# A\n- one\n two");
        assert!(e.changed);
        // Unicode edits by character.
        let mut e = Editor::new("a", "/x", "é");
        e.key(FieldKey::End { select: false });
        e.key(FieldKey::Backspace);
        assert_eq!(e.text(), "");
    }
}
