//! Colour themes: 16 ANSI colours plus foreground, background, cursor and selection. Built-in
//! themes, and theme files (`~/.config/ctx/themes/<name>.toml`) in the same shape.

use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct Theme {
    pub name: String,
    pub ansi: [[u8; 3]; 16],
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub cursor: [u8; 3],
    pub selection: [u8; 3],
    pub dark: bool,
}

impl Theme {
    pub fn look(&self, cursor_shape: ctx_protocol::CursorShape, cursor_blink: bool) -> ctx_protocol::Look {
        ctx_protocol::Look {
            palette: self.ansi.to_vec(),
            foreground: self.foreground,
            background: self.background,
            cursor: self.cursor,
            dark: self.dark,
            cursor_shape,
            cursor_blink,
        }
    }
}

/// Terminal.app's "Basic" profile.
pub fn basic_light() -> Theme {
    Theme {
        name: "basic".into(),
        ansi: ctx_protocol::sgr::DEFAULT_ANSI,
        foreground: [0, 0, 0],
        background: [255, 255, 255],
        cursor: [146, 146, 146],
        selection: [164, 205, 255],
        dark: false,
    }
}

/// "Basic" in dark mode, as Terminal.app shows it.
pub fn basic_dark() -> Theme {
    Theme {
        name: "basic-dark".into(),
        ansi: ctx_protocol::sgr::DEFAULT_ANSI,
        foreground: [255, 255, 255],
        background: [30, 30, 30],
        cursor: [146, 146, 146],
        selection: [63, 99, 139],
        dark: true,
    }
}

/// Ghostty's default look (its palette, foreground and background): CTX's default theme.
pub fn ghostty() -> Theme {
    Theme {
        name: "ghostty".into(),
        ansi: [
            [29, 31, 33],
            [204, 102, 102],
            [181, 189, 104],
            [240, 198, 116],
            [129, 162, 190],
            [178, 148, 187],
            [138, 190, 183],
            [197, 200, 198],
            [102, 102, 102],
            [213, 78, 83],
            [185, 202, 74],
            [231, 197, 71],
            [122, 166, 218],
            [195, 151, 216],
            [112, 192, 177],
            [234, 234, 234],
        ],
        foreground: [255, 255, 255],
        background: [40, 44, 52],
        cursor: [255, 255, 255],
        selection: [62, 68, 81],
        dark: true,
    }
}

/// Ghostty's palette on other backgrounds, distinct enough to tell tabs (and projects) apart at
/// a glance: name, background, selection.
const TINTS: &[(&str, [u8; 3], [u8; 3])] = &[
    ("midnight", [27, 35, 52], [52, 66, 96]),
    ("forest", [28, 40, 33], [52, 74, 60]),
    ("plum", [44, 31, 48], [80, 58, 88]),
    ("ember", [48, 33, 28], [88, 62, 52]),
    ("ocean", [20, 42, 50], [40, 76, 90]),
    ("graphite", [30, 30, 30], [70, 70, 74]),
];

fn tinted(name: &str) -> Option<Theme> {
    let (n, bg, sel) = TINTS.iter().find(|(n, ..)| *n == name)?;
    Some(Theme { name: n.to_string(), background: *bg, selection: *sel, ..ghostty() })
}

pub const BUILT_IN: &[&str] =
    &["system", "ghostty", "midnight", "forest", "plum", "ember", "ocean", "graphite", "basic", "basic-dark"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ThemeFile {
    palette: Vec<String>,
    foreground: String,
    background: String,
    cursor: Option<String>,
    selection: Option<String>,
    dark: Option<bool>,
}

/// `#rrggbb` or `rrggbb`.
pub fn parse_hex(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().trim_start_matches('#');
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

fn luminance(c: [u8; 3]) -> f64 {
    (0.299 * c[0] as f64 + 0.587 * c[1] as f64 + 0.114 * c[2] as f64) / 255.0
}

pub fn parse_file(name: &str, text: &str) -> Result<Theme, String> {
    let f: ThemeFile = toml::from_str(text).map_err(|e| e.to_string())?;
    if f.palette.len() != 16 {
        return Err(format!("palette needs 16 colours, found {}", f.palette.len()));
    }
    let mut ansi = [[0u8; 3]; 16];
    for (i, c) in f.palette.iter().enumerate() {
        ansi[i] = parse_hex(c).ok_or_else(|| format!("palette colour {} isn't #rrggbb: {c}", i + 1))?;
    }
    let colour = |s: &str, what: &str| parse_hex(s).ok_or_else(|| format!("{what} isn't #rrggbb: {s}"));
    let foreground = colour(&f.foreground, "foreground")?;
    let background = colour(&f.background, "background")?;
    let cursor = match &f.cursor {
        Some(c) => colour(c, "cursor")?,
        None => foreground,
    };
    let selection = match &f.selection {
        Some(c) => colour(c, "selection")?,
        None => [164, 205, 255],
    };
    Ok(Theme {
        name: name.to_string(),
        ansi,
        foreground,
        background,
        cursor,
        selection,
        dark: f.dark.unwrap_or(luminance(background) < 0.5),
    })
}

/// A theme by name: built in, or a file in `themes_dir`. `system` follows macOS appearance.
pub fn load(name: &str, themes_dir: &Path, system_dark: bool) -> Result<Theme, String> {
    match name {
        "system" | "" => Ok(if system_dark { basic_dark() } else { basic_light() }),
        "basic" => Ok(basic_light()),
        "basic-dark" => Ok(basic_dark()),
        "ghostty" => Ok(ghostty()),
        other if tinted(other).is_some() => Ok(tinted(other).unwrap()),
        other => {
            let path = themes_dir.join(format!("{other}.toml"));
            let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            parse_file(other, &text).map_err(|e| format!("{}: {e}", path.display()))
        }
    }
}

/// Every theme name available: built-ins, then files.
pub fn available(themes_dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = BUILT_IN.iter().map(|s| s.to_string()).collect();
    if let Ok(rd) = std::fs::read_dir(themes_dir) {
        let mut files: Vec<String> = rd
            .flatten()
            .filter_map(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".toml")).map(str::to_string))
            .collect();
        files.sort();
        v.extend(files);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colours() {
        assert_eq!(parse_hex("#ff8000"), Some([255, 128, 0]));
        assert_eq!(parse_hex("00ff00"), Some([0, 255, 0]));
        assert_eq!(parse_hex("#fff"), None);
    }

    #[test]
    fn theme_files() {
        let palette: Vec<String> = (0..16).map(|i| format!("\"#{:02x}{:02x}{:02x}\"", i * 10, i, i)).collect();
        let text = format!("palette = [{}]\nforeground = \"#eeeeee\"\nbackground = \"#101010\"\n", palette.join(", "));
        let t = parse_file("mine", &text).unwrap();
        assert!(t.dark);
        assert_eq!(t.ansi[15], [150, 15, 15]);
        assert_eq!(t.cursor, [0xee, 0xee, 0xee]);
        assert!(parse_file("bad", "palette = []\nforeground = \"#000000\"\nbackground = \"#ffffff\"").is_err());
        assert!(parse_file("bad", "nonsense = 1").is_err());
    }

    #[test]
    fn built_ins_load() {
        let dir = Path::new("/nonexistent");
        assert!(!load("system", dir, false).unwrap().dark);
        assert!(load("system", dir, true).unwrap().dark);
        assert_eq!(load("ghostty", dir, false).unwrap().name, "ghostty");
        assert!(load("missing", dir, false).is_err());
        assert_eq!(available(dir), BUILT_IN.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    }
}
