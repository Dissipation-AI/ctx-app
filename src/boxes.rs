//! Box-drawing (U+2500–257F) and block elements (U+2580–259F) drawn as shapes filling the cell,
//! so lines join across rows and columns whatever the font's line height (as Ghostty and
//! Terminal.app do). TUIs like htop, lazygit and Claude Code's input box rely on this.

/// Arms of a box character: left, right, up, down. 0 none, 1 light, 2 heavy, 3 double.
pub fn arms(c: char) -> Option<[u8; 4]> {
    // One entry per code point from U+2500, as "LRUD" digits; dashed lines are marked by the
    // caller.
    const TABLE: [&str; 128] = [
        "1100", "2200", "0011", "0022", "1100", "2200", "0011", "0022", "1100", "2200", "0011",
        "0022", // 2500-250B
        "0101", "0201", "0102", "0202", "1001", "2001", "1002", "2002", "0110", "0210", "0120",
        "0220", // 250C-2517
        "1010", "2010", "1020", "2020", "0111", "0211", "0121", "0112", "0122", "0221", "0212",
        "0222", // 2518-2523
        "1011", "2011", "1021", "1012", "1022", "2021", "2012", "2022", "1101", "2101", "1201",
        "2201", // 2524-252F
        "1102", "2102", "1202", "2202", "1110", "2110", "1210", "2210", "1120", "2120", "1220",
        "2220", // 2530-253B
        "1111", "2111", "1211", "2211", "1121", "1112", "1122", "2121", "1221", "2112", "1212",
        "2221", // 253C-2547
        "2212", "2122", "1222", "2222", "1100", "2200", "0011", "0022", "3300", "0033", "0301",
        "0103", // 2548-2553
        "0303", "3001", "1003", "3003", "0310", "0130", "0330", "3010", "1030", "3030", "0311",
        "0133", // 2554-255F
        "0333", "3011", "1033", "3033", "3301", "1103", "3303", "3310", "1130", "3330", "3311",
        "1133", // 2560-256B
        "3333", "0101", "1001", "1010", "0110", "0000", "0000", "0000", "1000", "0010", "0100",
        "0001", // 256C-2577
        "2000", "0020", "0200", "0002", "1200", "0012", "2100", "0021", // 2578-257F
    ];
    let i = (c as u32).checked_sub(0x2500)? as usize;
    let s = TABLE.get(i)?.as_bytes();
    let a = [s[0] - b'0', s[1] - b'0', s[2] - b'0', s[3] - b'0'];
    if a == [0, 0, 0, 0] { None } else { Some(a) }
}

/// Dashed box characters: how many dashes per cell.
pub fn dashes(c: char) -> Option<usize> {
    match c as u32 {
        0x2504..=0x2507 => Some(3),
        0x2508..=0x250B => Some(4),
        0x254C..=0x254F => Some(2),
        _ => None,
    }
}

pub fn is_rounded(c: char) -> bool {
    ('\u{256D}'..='\u{2570}').contains(&c)
}

pub fn is_diagonal(c: char) -> bool {
    ('\u{2571}'..='\u{2573}').contains(&c)
}

/// A block element as rectangles in cell fractions (x, y from the top, w, h) and an alpha.
pub fn block(c: char) -> Option<(Vec<(f64, f64, f64, f64)>, f64)> {
    let full = 1.0;
    let r = match c as u32 {
        0x2580 => vec![(0.0, 0.0, full, 0.5)],
        0x2581..=0x2587 => {
            let n = (c as u32 - 0x2580) as f64 / 8.0;
            vec![(0.0, 1.0 - n, full, n)]
        }
        0x2588 => vec![(0.0, 0.0, full, full)],
        0x2589..=0x258F => {
            let n = (0x2590 - c as u32) as f64 / 8.0;
            vec![(0.0, 0.0, n, full)]
        }
        0x2590 => vec![(0.5, 0.0, 0.5, full)],
        0x2591 => return Some((vec![(0.0, 0.0, full, full)], 0.25)),
        0x2592 => return Some((vec![(0.0, 0.0, full, full)], 0.5)),
        0x2593 => return Some((vec![(0.0, 0.0, full, full)], 0.75)),
        0x2594 => vec![(0.0, 0.0, full, 0.125)],
        0x2595 => vec![(0.875, 0.0, 0.125, full)],
        0x2596..=0x259F => {
            // Quadrants: upper-left, upper-right, lower-left, lower-right bits.
            let q: [u8; 10] = [0b0010, 0b0001, 0b1000, 0b1011, 0b1001, 0b1110, 0b1101, 0b0100, 0b0110, 0b0111];
            let bits = q[(c as u32 - 0x2596) as usize];
            let mut v = Vec::new();
            if bits & 0b1000 != 0 {
                v.push((0.0, 0.0, 0.5, 0.5));
            }
            if bits & 0b0100 != 0 {
                v.push((0.5, 0.0, 0.5, 0.5));
            }
            if bits & 0b0010 != 0 {
                v.push((0.0, 0.5, 0.5, 0.5));
            }
            if bits & 0b0001 != 0 {
                v.push((0.5, 0.5, 0.5, 0.5));
            }
            v
        }
        _ => return None,
    };
    Some((r, 1.0))
}

/// Whether a grapheme is drawn as a shape.
pub fn is_shape(g: &str) -> bool {
    let mut it = g.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => arms(c).is_some() || block(c).is_some() || is_diagonal(c),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arms_follow_the_unicode_chart() {
        assert_eq!(arms('─'), Some([1, 1, 0, 0]));
        assert_eq!(arms('│'), Some([0, 0, 1, 1]));
        assert_eq!(arms('┌'), Some([0, 1, 0, 1]));
        assert_eq!(arms('┘'), Some([1, 0, 1, 0]));
        assert_eq!(arms('┼'), Some([1, 1, 1, 1]));
        assert_eq!(arms('╋'), Some([2, 2, 2, 2]));
        assert_eq!(arms('═'), Some([3, 3, 0, 0]));
        assert_eq!(arms('╔'), Some([0, 3, 0, 3]));
        assert_eq!(arms('╬'), Some([3, 3, 3, 3]));
        assert_eq!(arms('╭'), Some([0, 1, 0, 1]));
        assert_eq!(arms('╯'), Some([1, 0, 1, 0]));
        assert_eq!(arms('╿'), Some([0, 0, 2, 1]));
        assert_eq!(arms('a'), None);
        assert!(is_rounded('╰') && !is_rounded('└'));
        assert_eq!(dashes('┄'), Some(3));
    }

    #[test]
    fn blocks() {
        assert_eq!(block('█').unwrap().0, vec![(0.0, 0.0, 1.0, 1.0)]);
        assert_eq!(block('▄').unwrap().0, vec![(0.0, 0.5, 1.0, 0.5)]);
        assert_eq!(block('▌').unwrap().0, vec![(0.0, 0.0, 0.5, 1.0)]);
        assert_eq!(block('▒').unwrap().1, 0.5);
        assert_eq!(block('▚').unwrap().0.len(), 2);
        assert!(is_shape("█") && is_shape("┼") && !is_shape("a") && !is_shape("──"));
    }
}
