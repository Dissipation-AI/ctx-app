//! Finding links in text so ⌘-click can open them: URLs with a scheme, and `www.` addresses.

use crate::text::Row;

/// A link on a row: columns `start..end` and what it opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub start: usize,
    pub end: usize,
    pub url: String,
}

const SCHEMES: &[&str] = &["https://", "http://", "file://", "ftp://", "ssh://", "mailto:"];

fn url_char(c: char) -> bool {
    !c.is_whitespace() && !"<>\"'`{}|\\^".contains(c) && !c.is_control()
}

/// Links in a string, as character ranges.
pub fn find(text: &str) -> Vec<(usize, usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    let lower: String = text.to_lowercase();
    let lower: Vec<char> = lower.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let starts_word = i == 0 || !chars[i - 1].is_alphanumeric();
        let scheme = SCHEMES.iter().find(|s| {
            let s: Vec<char> = s.chars().collect();
            lower.len() >= i + s.len() && lower[i..i + s.len()] == s[..]
        });
        let www = starts_word && lower.len() >= i + 4 && lower[i..i + 4] == ['w', 'w', 'w', '.'];
        if (scheme.is_some() && starts_word) || www {
            let start = i;
            let mut j = i;
            while j < chars.len() && url_char(chars[j]) {
                j += 1;
            }
            // Trailing punctuation belongs to the sentence, and a closing bracket only to the URL
            // when the URL opened one.
            while j > start {
                let c = chars[j - 1];
                let opens = |o: char| chars[start..j].iter().filter(|x| **x == o).count();
                let closes = |cl: char| chars[start..j].iter().filter(|x| **x == cl).count();
                let unbalanced = match c {
                    ')' => closes(')') > opens('('),
                    ']' => closes(']') > opens('['),
                    _ => false,
                };
                if ".,;:!?".contains(c) || unbalanced {
                    j -= 1;
                } else {
                    break;
                }
            }
            let min = scheme.map(|s| s.chars().count()).unwrap_or(4) + 1;
            if j - start >= min {
                let s: String = chars[start..j].iter().collect();
                let url = if scheme.is_none() { format!("https://{s}") } else { s };
                out.push((start, j, url));
                i = j;
                continue;
            }
        }
        i += 1;
    }
    out
}

/// Links on a row, by column: the program's own (OSC 8) first, then URLs found in the text.
pub fn on_row(row: &Row, measured: bool) -> Vec<Link> {
    let mut out: Vec<Link> =
        row.links.iter().map(|l| Link { start: l.start as usize, end: l.end as usize, url: l.uri.clone() }).collect();
    for l in found_on_row(row, measured) {
        if !out.iter().any(|o| l.start < o.end && o.start < l.end) {
            out.push(l);
        }
    }
    out
}

fn found_on_row(row: &Row, measured: bool) -> Vec<Link> {
    let cells = row.cells(measured);
    let text: String = cells.iter().map(|c| c.text).collect::<Vec<_>>().join("");
    // Character index → cell index (a cell may hold several characters).
    let mut char_cell = Vec::new();
    for (i, c) in cells.iter().enumerate() {
        for _ in c.text.chars() {
            char_cell.push(i);
        }
    }
    find(&text)
        .into_iter()
        .filter_map(|(a, b, url)| {
            let first = cells.get(*char_cell.get(a)?)?;
            let last = cells.get(*char_cell.get(b - 1)?)?;
            Some(Link { start: first.column, end: last.column + last.width, url })
        })
        .collect()
}

pub fn at(row: &Row, measured: bool, column: usize) -> Option<Link> {
    on_row(row, measured).into_iter().find(|l| column >= l.start && column < l.end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_programs_own_links_come_first() {
        let mut r = Row::plain("see docs at https://a.example now");
        r.links = vec![ctx_protocol::LinkSpan { start: 4, end: 8, uri: "https://docs.example/x".into() }];
        assert_eq!(at(&r, false, 5).map(|l| l.url).as_deref(), Some("https://docs.example/x"));
        assert_eq!(at(&r, false, 14).map(|l| l.url).as_deref(), Some("https://a.example"));
        assert!(at(&r, false, 1).is_none());
    }

    #[test]
    fn finds_urls_and_trims_punctuation() {
        let f = find("see https://example.com/a?b=1. and (http://x.org/y) or www.rust-lang.org!");
        let urls: Vec<&str> = f.iter().map(|x| x.2.as_str()).collect();
        assert_eq!(urls, ["https://example.com/a?b=1", "http://x.org/y", "https://www.rust-lang.org"]);
    }

    #[test]
    fn keeps_balanced_brackets() {
        let f = find("https://en.wikipedia.org/wiki/Rust_(programming_language) done");
        assert_eq!(f[0].2, "https://en.wikipedia.org/wiki/Rust_(programming_language)");
    }

    #[test]
    fn ignores_schemes_inside_words() {
        assert!(find("xhttps://nope").is_empty());
        assert!(find("http://").is_empty());
    }

    #[test]
    fn maps_to_columns_after_wide_text() {
        let r = Row::plain("日本 https://a.io x");
        let links = on_row(&r, true);
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].start, 5);
        assert_eq!(links[0].end, 5 + "https://a.io".len());
        assert_eq!(at(&r, true, 7).unwrap().url, "https://a.io");
        assert!(at(&r, true, 1).is_none());
    }
}
