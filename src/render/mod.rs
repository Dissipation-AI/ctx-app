//! Drawing a window: terminal text and the app's own chrome (tab bar, buttons, bars, panels),
//! all into **one pixel buffer per window**. Measured in Phase 0 (see SPEC.md § Rendering): on
//! macOS that buffer is an IOSurface handed to the window's layer, drawn with Core Text and Core
//! Graphics on the CPU; Metal with two drawables held two buffers plus driver memory. On Linux it
//! is a plain buffer drawn by a small software canvas and handed to the window system.
//!
//! The drawing itself (rows, cursor, selection, box characters, chrome) is shared and goes
//! through [`Canvas`]; only fonts, glyphs and the buffer differ per platform. Only rows that
//! changed are redrawn.

use crate::text::Row;
use ctx_protocol::{CellStyle, CursorShape};
use std::rc::Rc;
use unicode_segmentation::UnicodeSegmentation;

#[cfg(target_os = "macos")]
mod cg;
#[cfg(target_os = "macos")]
pub use cg::{FontCache, Fonts, Painter, Surface, UiFont, png_difference};

#[cfg(not(target_os = "macos"))]
mod soft;
#[cfg(not(target_os = "macos"))]
pub use soft::{FontCache, Fonts, Painter, Surface, UiFont, png_difference};

pub type Rgb = [u8; 3];

/// Padding inside a tile, in points.
pub const PAD_X: f64 = 4.0;
pub const PAD_Y: f64 = 2.0;

/// A path for strokes, in pixels from the top-left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PathOp {
    Move(f64, f64),
    Line(f64, f64),
    /// A circular arc of radius `r` tangent to the line to `(x1, y1)` and the line from there to
    /// `(x2, y2)` (Core Graphics' `addArcToPoint`).
    ArcTo { x1: f64, y1: f64, x2: f64, y2: f64, r: f64 },
}

/// How text that doesn't fit is shortened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trunc {
    /// "…code/site": keep the end (folders).
    Head,
    /// "Dev ser…": keep the start (names).
    Tail,
}

/// What every platform's painter can do. Coordinates are pixels from the top-left of the buffer.
pub trait Canvas {
    fn fill(&mut self, c: Rgb, alpha: f64, x: f64, y: f64, w: f64, h: f64);
    /// A rectangle with rounded corners: radii top-left, top-right, bottom-right, bottom-left.
    fn fill_rounded(&mut self, c: Rgb, alpha: f64, x: f64, y: f64, w: f64, h: f64, radii: [f64; 4]);
    fn stroke(&mut self, path: &[PathOp], width: f64, c: Rgb, alpha: f64);
    /// Limit drawing to a rectangle (`None`: the whole buffer).
    fn clip(&mut self, r: Option<(f64, f64, f64, f64)>);
    /// One grapheme of terminal text in a cell, its baseline at `baseline`.
    fn glyph(&mut self, fonts: &Rc<Fonts>, text: &str, variant: u8, x: f64, baseline: f64, c: Rgb);
    /// A line of interface text with its top at `y`, at most `max_w` wide (shortened with an
    /// ellipsis); returns the width drawn.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, font: &UiFont, s: &str, x: f64, y: f64, max_w: f64, c: Rgb, trunc: Trunc) -> f64;
}

// ---------------------------------------------------------------------------------------------
// Frames
// ---------------------------------------------------------------------------------------------

/// The cursor as drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorFrame {
    pub column: usize,
    pub row: usize,
    pub shape: CursorShape,
    /// Unfocused tiles and windows draw it hollow.
    pub focused: bool,
    pub color: Rgb,
}

/// One tile: a tab's rows in a region of the window.
pub struct TileFrame<'a> {
    /// Region in pixels, from the top-left.
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub fonts: Rc<Fonts>,
    /// Rows top to bottom, and whether their widths are measured (scrollback rows).
    pub rows: Vec<(&'a Row, bool)>,
    pub foreground: Rgb,
    pub background: Rgb,
    pub selection_color: Rgb,
    pub cursor: Option<CursorFrame>,
    /// (row, from column, to column) — end exclusive.
    pub selection: Vec<(usize, usize, usize)>,
    /// Find matches: (row, from, to, is the current one).
    pub matches: Vec<(usize, usize, usize, bool)>,
    /// A link under the mouse with ⌘ held: (row, from, to).
    pub link: Option<(usize, usize, usize)>,
    /// Text being composed by an input method, drawn at the cursor.
    pub marked: Option<String>,
    /// Scrolled back: how far (0 oldest … 1 live).
    pub scrolled: Option<f64>,
}

/// What to redraw.
pub enum Damage {
    All,
    /// (tile index, row) pairs.
    Rows(Vec<(usize, usize)>),
}

pub fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [
        (a[0] as f64 * (1.0 - t) + b[0] as f64 * t).round() as u8,
        (a[1] as f64 * (1.0 - t) + b[1] as f64 * t).round() as u8,
        (a[2] as f64 * (1.0 - t) + b[2] as f64 * t).round() as u8,
    ]
}

/// Draw tiles into a surface. With `Damage::All`, `divider` fills the gaps between tiles and
/// everything in `area` (pixels) that no tile covers.
pub fn draw(surface: &mut Surface, area: (f64, f64, f64, f64), tiles: &[TileFrame], damage: &Damage, divider: Rgb) {
    surface.paint(|c| draw_tiles(c, area, tiles, damage, divider));
}

pub fn draw_tiles<C: Canvas>(
    c: &mut C,
    area: (f64, f64, f64, f64),
    tiles: &[TileFrame],
    damage: &Damage,
    divider: Rgb,
) {
    match damage {
        Damage::All => {
            c.fill(divider, 1.0, area.0, area.1, area.2, area.3);
            for t in tiles {
                c.fill(t.background, 1.0, t.x, t.y, t.w, t.h);
                for r in 0..t.rows.len() {
                    draw_row(c, t, r);
                }
                draw_overlays(c, t);
            }
        }
        Damage::Rows(rows) => {
            for (ti, r) in rows {
                if let Some(t) = tiles.get(*ti)
                    && *r < t.rows.len()
                {
                    let fonts = &t.fonts;
                    let top = t.y + PAD_Y * fonts.scale + *r as f64 * fonts.cell_h;
                    c.fill(t.background, 1.0, t.x, top, t.w, fonts.cell_h);
                    draw_row(c, t, *r);
                    if t.scrolled.is_some() {
                        draw_overlays(c, t);
                    }
                }
            }
        }
    }
}

fn draw_overlays<C: Canvas>(c: &mut C, t: &TileFrame) {
    if let Some(f) = t.scrolled {
        let s = t.fonts.scale;
        let track = t.h - 8.0 * s;
        let knob = (track * 0.12).max(20.0 * s);
        let y = t.y + 4.0 * s + (track - knob) * f;
        let col = mix(t.background, t.foreground, 0.35);
        c.fill_rounded(col, 1.0, t.x + t.w - 6.0 * s, y, 3.0 * s, knob, [1.5 * s; 4]);
    }
}

/// Bold and italic as a font variant: 0 regular, 1 bold, 2 italic, 3 both.
pub fn variant(style: &CellStyle) -> u8 {
    (style.flags & 1 != 0) as u8 | (((style.flags & 2 != 0) as u8) << 1)
}

fn draw_row<C: Canvas>(c: &mut C, t: &TileFrame, r: usize) {
    let fonts = &t.fonts;
    let s = fonts.scale;
    let (row, measured) = t.rows[r];
    let left = t.x + PAD_X * s;
    let top = t.y + PAD_Y * s + r as f64 * fonts.cell_h;
    let cw = fonts.cell_w;
    let ch = fonts.cell_h;
    let max_cols = ((t.w - 2.0 * PAD_X * s) / cw).floor().max(0.0) as usize;
    c.clip(Some((t.x, top, t.w, ch)));
    let cells = row.cells(measured);
    let fg_of = |st: &CellStyle| {
        let mut fg = st.foreground.unwrap_or(t.foreground);
        let mut bg = st.background.unwrap_or(t.background);
        if st.flags & 16 != 0 {
            std::mem::swap(&mut fg, &mut bg);
        }
        if st.flags & 4 != 0 || row.event {
            fg = mix(fg, bg, 0.45);
        }
        (fg, bg)
    };
    // Backgrounds.
    for cell in &cells {
        if cell.column >= max_cols {
            break;
        }
        let (_, bg) = fg_of(&cell.style);
        if bg != t.background {
            c.fill(bg, 1.0, left + cell.column as f64 * cw, top, cw * cell.width as f64, ch);
        }
    }
    for &(sr, a, b) in &t.selection {
        if sr == r {
            let b = b.min(max_cols.max(1));
            if b > a {
                c.fill(t.selection_color, 1.0, left + a as f64 * cw, top, (b - a) as f64 * cw, ch);
            }
        }
    }
    for &(mr, a, b, current) in &t.matches {
        if mr == r {
            let col = if current { [255, 150, 50] } else { [255, 225, 90] };
            c.fill(col, 1.0, left + a as f64 * cw, top, (b - a) as f64 * cw, ch);
        }
    }
    // The cursor block goes under the text; the text on it is drawn in the background colour.
    let cursor_here = t.cursor.filter(|cur| cur.row == r);
    let width_at = |col: usize| cells.iter().find(|x| x.column == col).map(|x| x.width).unwrap_or(1);
    if let Some(cur) = cursor_here
        && cur.focused
        && cur.shape == CursorShape::Block
        && t.marked.is_none()
    {
        c.fill(cur.color, 1.0, left + cur.column as f64 * cw, top, cw * width_at(cur.column) as f64, ch);
    }
    let baseline = top + fonts.ascent;
    for cell in &cells {
        if cell.column >= max_cols {
            break;
        }
        if cell.style.flags & 64 != 0 {
            continue;
        }
        let (mut fg, bg) = fg_of(&cell.style);
        if let Some(cur) = cursor_here
            && cur.focused
            && cur.shape == CursorShape::Block
            && cur.column == cell.column
            && t.marked.is_none()
        {
            fg = bg;
            if fg == cur.color {
                fg = t.background;
            }
        }
        let x = left + cell.column as f64 * cw;
        if crate::boxes::is_shape(cell.text) {
            draw_shape(c, cell.text.chars().next().unwrap(), x, top, cw * cell.width as f64, ch, fg, s);
        } else {
            c.glyph(fonts, cell.text, variant(&cell.style), x, baseline, fg);
        }
        // Underline and strikethrough.
        let lw = s.max(1.0).round();
        if cell.style.flags & 8 != 0 {
            c.fill(fg, 1.0, x, top + fonts.ascent + (fonts.descent / 2.0).round(), cw * cell.width as f64, lw);
        }
        if cell.style.flags & 32 != 0 {
            c.fill(fg, 1.0, x, top + (ch / 2.0).round(), cw * cell.width as f64, lw);
        }
    }
    if let Some((lr, a, b)) = t.link
        && lr == r
    {
        c.fill(t.foreground, 1.0, left + a as f64 * cw, top + ch - s.round().max(1.0) * 2.0, (b - a) as f64 * cw, s.max(1.0));
    }
    // Other cursor shapes, over the text.
    if let Some(cur) = cursor_here
        && t.marked.is_none()
    {
        let x = left + cur.column as f64 * cw;
        let w = width_at(cur.column) as f64 * cw;
        let thick = (2.0 * s).round();
        match (cur.focused, cur.shape) {
            (true, CursorShape::Block) => {}
            (true, CursorShape::Bar) => c.fill(cur.color, 1.0, x, top, thick, ch),
            (true, CursorShape::Underline) => c.fill(cur.color, 1.0, x, top + ch - thick, w, thick),
            _ => {
                let lw = s.round().max(1.0);
                c.fill(cur.color, 1.0, x, top, w, lw);
                c.fill(cur.color, 1.0, x, top + ch - lw, w, lw);
                c.fill(cur.color, 1.0, x, top, lw, ch);
                c.fill(cur.color, 1.0, x + w - lw, top, lw, ch);
            }
        }
    }
    // Text being composed (accents, CJK input), with an underline.
    if let (Some(cur), Some(m)) = (cursor_here, &t.marked) {
        let x0 = left + cur.column as f64 * cw;
        let marked_row = Row::plain(m);
        let mcells = marked_row.cells(true);
        let width: usize = mcells.iter().map(|x| x.width).sum();
        c.fill(t.background, 1.0, x0, top, width as f64 * cw, ch);
        let mut x = x0;
        for mc in &mcells {
            c.glyph(fonts, mc.text, 0, x, baseline, t.foreground);
            x += mc.width as f64 * cw;
        }
        c.fill(t.foreground, 1.0, x0, top + ch - s.max(1.0), width as f64 * cw, s.max(1.0));
    }
    c.clip(None);
}

/// A box-drawing or block character, filling its cell (`x`, `top`: pixels from the top-left).
#[allow(clippy::too_many_arguments)]
fn draw_shape<C: Canvas>(c: &mut C, ch: char, x: f64, top: f64, w: f64, h: f64, color: Rgb, scale: f64) {
    use crate::boxes;
    if let Some((rects, alpha)) = boxes::block(ch) {
        for (fx, fy, fw, fh) in rects {
            let x0 = (x + fx * w).round();
            let y0 = (top + fy * h).round();
            let x1 = (x + (fx + fw) * w).round();
            let y1 = (top + (fy + fh) * h).round();
            c.fill(color, alpha, x0, y0, x1 - x0, y1 - y0);
        }
        return;
    }
    let light = scale.round().max(1.0);
    let heavy = (light * 2.0).max(2.0);
    let cx = (x + w / 2.0).floor();
    let cy = (top + h / 2.0).floor();
    if boxes::is_diagonal(ch) {
        let mut path = Vec::new();
        if ch != '\u{2572}' {
            path.push(PathOp::Move(x, top + h));
            path.push(PathOp::Line(x + w, top));
        }
        if ch != '\u{2571}' {
            path.push(PathOp::Move(x, top));
            path.push(PathOp::Line(x + w, top + h));
        }
        c.stroke(&path, light, color, 1.0);
        return;
    }
    let Some([l, rt, u, d]) = boxes::arms(ch) else { return };
    if boxes::is_rounded(ch) {
        // An arc from one edge's midpoint, round the centre, to the other edge.
        let mx = cx + light / 2.0;
        let my = cy + light / 2.0;
        let hx = if l > 0 { x } else { x + w };
        let vy = if u > 0 { top } else { top + h };
        let radius = (w.min(h) / 2.0).floor();
        let path = [PathOp::Move(hx, my), PathOp::ArcTo { x1: mx, y1: my, x2: mx, y2: vy, r: radius }, PathOp::Line(mx, vy)];
        c.stroke(&path, light, color, 1.0);
        return;
    }
    let dashes = boxes::dashes(ch);
    let thick = |k: u8| if k == 2 { heavy } else { light };
    // The span a line of weight k covers across its centre line `c`.
    let span = |cl: f64, k: u8| -> (f64, f64) {
        match k {
            3 => (cl - (light / 2.0).floor() - light, cl - (light / 2.0).floor() + 2.0 * light),
            k => (cl - (thick(k) / 2.0).floor(), cl - (thick(k) / 2.0).floor() + thick(k)),
        }
    };
    // Where the horizontal and vertical lines sit, so arms meet them exactly.
    let across = |a: u8, b: u8, cl: f64| -> (f64, f64) {
        let ks: Vec<u8> = [a, b].into_iter().filter(|k| *k > 0).collect();
        if ks.is_empty() {
            return span(cl, 1);
        }
        let lo = ks.iter().map(|k| span(cl, *k).0).fold(f64::MAX, f64::min);
        let hi = ks.iter().map(|k| span(cl, *k).1).fold(f64::MIN, f64::max);
        (lo, hi)
    };
    let (h_top, h_bot) = across(l, rt, cy);
    let (v_left, v_right) = across(u, d, cx);
    let lines = |k: u8| -> Vec<(f64, f64)> {
        match k {
            3 => {
                let (a, _) = span(0.0, 3);
                vec![(a, light), (a + 2.0 * light, light)]
            }
            k => vec![(span(0.0, k).0, thick(k))],
        }
    };
    let mut rects: Vec<(f64, f64, f64, f64)> = Vec::new();
    let hline = |x0: f64, x1: f64, k: u8, rects: &mut Vec<(f64, f64, f64, f64)>| {
        for (off, t) in lines(k) {
            let y0 = cy + off;
            match dashes {
                Some(n) => {
                    let seg = w / n as f64;
                    for i in 0..n {
                        rects.push(((x + i as f64 * seg).round(), y0, (seg * 0.6).round().max(1.0), t));
                    }
                }
                None => rects.push((x0.round(), y0, (x1 - x0).round().max(1.0), t)),
            }
        }
    };
    let has_v = u > 0 || d > 0;
    let has_h = l > 0 || rt > 0;
    if l > 0 {
        hline(x, if has_v { v_right } else { cx + light }, l, &mut rects);
    }
    if rt > 0 {
        hline(if has_v { v_left } else { cx }, x + w, rt, &mut rects);
    }
    let vline = |y0: f64, y1: f64, k: u8, rects: &mut Vec<(f64, f64, f64, f64)>| {
        for (off, t) in lines(k) {
            let x0 = cx + off;
            match dashes {
                Some(n) => {
                    let seg = h / n as f64;
                    for i in 0..n {
                        rects.push((x0, (top + i as f64 * seg).round(), t, (seg * 0.6).round().max(1.0)));
                    }
                }
                None => rects.push((x0, y0.round(), t, (y1 - y0).round().max(1.0))),
            }
        }
    };
    if u > 0 {
        vline(top, if has_h { h_bot } else { cy + light }, u, &mut rects);
    }
    if d > 0 {
        vline(if has_h { h_top } else { cy }, top + h, d, &mut rects);
    }
    for (rx, ry, rw, rh) in rects {
        c.fill(color, 1.0, rx, ry, rw, rh);
    }
}

/// A translucent rectangle (where a dragged tab would land), in pixels from the top-left.
pub fn overlay(surface: &mut Surface, (x, y, w, h): (f64, f64, f64, f64), col: Rgb) {
    surface.paint(|c| c.fill(col, 0.45, x, y, w, h));
}

/// Columns and rows that fit in a tile of `w`×`h` pixels.
pub fn grid_size(fonts: &Fonts, w: f64, h: f64) -> (u16, u16) {
    let cols = ((w - 2.0 * PAD_X * fonts.scale) / fonts.cell_w).floor().max(1.0);
    let rows = ((h - 2.0 * PAD_Y * fonts.scale) / fonts.cell_h).floor().max(1.0);
    (cols.min(2000.0) as u16, rows.min(1000.0) as u16)
}

/// The cell under a point (pixels, from the tile's top-left). Columns may run past the row.
pub fn cell_at(fonts: &Fonts, px: f64, py: f64) -> (usize, usize) {
    let col = ((px - PAD_X * fonts.scale) / fonts.cell_w).floor().max(0.0) as usize;
    let row = ((py - PAD_Y * fonts.scale) / fonts.cell_h).floor().max(0.0) as usize;
    (col, row)
}

/// Clusters needing shaping, for tests.
pub fn needs_shaping(g: &str) -> bool {
    g.graphemes(true).count() == 1 && g.chars().count() > 1
}

/// Whether a character is shown as an emoji by default (Unicode's Emoji_Presentation), rather
/// than as text that takes the foreground colour.
pub fn emoji_presentation(c: char) -> bool {
    let u = c as u32;
    if (0x1F000..=0x1FAFF).contains(&u) {
        return !matches!(u, 0x1F100..=0x1F1E5);
    }
    matches!(
        u,
        0x231A | 0x231B | 0x23E9..=0x23EC | 0x23F0 | 0x23F3 | 0x25FD | 0x25FE | 0x2614 | 0x2615 | 0x2648..=0x2653
            | 0x267F | 0x2693 | 0x26A1 | 0x26AA | 0x26AB | 0x26BD | 0x26BE | 0x26C4 | 0x26C5 | 0x26CE | 0x26D4
            | 0x26EA | 0x26F2 | 0x26F3 | 0x26F5 | 0x26FA | 0x26FD | 0x2705 | 0x270A | 0x270B | 0x2728 | 0x274C
            | 0x274E | 0x2753..=0x2755 | 0x2757 | 0x2795..=0x2797 | 0x27B0 | 0x27BF | 0x2B1B | 0x2B1C | 0x2B50 | 0x2B55
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ctx_protocol::StyledRun;

    pub fn tile<'a>(fonts: Rc<Fonts>, rows: Vec<(&'a Row, bool)>, w: f64, h: f64) -> TileFrame<'a> {
        TileFrame {
            x: 0.0,
            y: 0.0,
            w,
            h,
            fonts,
            rows,
            foreground: [0, 0, 0],
            background: [255, 255, 255],
            selection_color: [164, 205, 255],
            cursor: None,
            selection: Vec::new(),
            matches: Vec::new(),
            link: None,
            marked: None,
            scrolled: None,
        }
    }

    #[test]
    fn metrics_are_whole_pixels() {
        let f = Fonts::new("", 13.0, 2.0);
        assert_eq!(f.cell_w, f.cell_w.round());
        assert!(f.cell_w >= 14.0 && f.cell_w <= 20.0, "{}", f.cell_w);
        assert!(f.cell_h > f.cell_w);
        let (c, r) = grid_size(&f, 80.0 * f.cell_w + 2.0 * PAD_X * 2.0, 24.0 * f.cell_h + 2.0 * PAD_Y * 2.0);
        assert_eq!((c, r), (80, 24));
        assert_eq!(cell_at(&f, PAD_X * 2.0 + f.cell_w * 3.5, PAD_Y * 2.0 + f.cell_h * 1.2), (3, 1));
    }

    #[test]
    fn draws_backgrounds_text_and_cursor() {
        let mut cache = FontCache::default();
        let fonts = cache.get("", 12.0, 1.0);
        let red = CellStyle { background: Some([255, 0, 0]), ..Default::default() };
        let row = Row {
            runs: vec![
                StyledRun { text: "AB".into(), style: red },
                StyledRun { text: "  x".into(), style: CellStyle::default() },
            ],
            wraps: false,
            event: false,
        };
        let w = (fonts.cell_w * 10.0 + 2.0 * PAD_X).ceil();
        let h = (fonts.cell_h * 2.0 + 2.0 * PAD_Y).ceil();
        let mut s = Surface::new_heap(w as usize, h as usize);
        let mut t = tile(fonts.clone(), vec![(&row, false)], w, h);
        t.cursor =
            Some(CursorFrame { column: 3, row: 0, shape: CursorShape::Block, focused: true, color: [0, 0, 255] });
        draw(&mut s, (0.0, 0.0, w, h), &[t], &Damage::All, [128, 128, 128]);
        let y = (PAD_Y + fonts.cell_h / 2.0) as usize;
        // Red background behind "AB".
        assert_eq!(s.rgb((PAD_X + 1.0) as usize, (PAD_Y + 1.0) as usize), [255, 0, 0]);
        // The cursor block at column 3 is blue.
        assert_eq!(s.rgb((PAD_X + fonts.cell_w * 3.0 + 1.0) as usize, (PAD_Y + 1.0) as usize), [0, 0, 255]);
        // Background elsewhere stays white, and "x" put dark pixels in its cell.
        assert_eq!(s.rgb((PAD_X + fonts.cell_w * 2.5) as usize, y), [255, 255, 255]);
        let dark = (0..fonts.cell_w as usize)
            .flat_map(|dx| (0..fonts.cell_h as usize).map(move |dy| (dx, dy)))
            .any(|(dx, dy)| s.rgb((PAD_X + fonts.cell_w * 4.0) as usize + dx, PAD_Y as usize + dy)[0] < 100);
        assert!(dark, "the glyph drew");
    }

    #[test]
    fn box_lines_join_and_text_measures() {
        let mut cache = FontCache::default();
        let fonts = cache.get("", 12.0, 1.0);
        let row = Row::plain("─┼─");
        let w = (fonts.cell_w * 4.0 + 2.0 * PAD_X).ceil();
        let h = (fonts.cell_h + 2.0 * PAD_Y).ceil();
        let mut s = Surface::new_heap(w as usize, h as usize);
        draw(&mut s, (0.0, 0.0, w, h), &[tile(fonts.clone(), vec![(&row, false)], w, h)], &Damage::All, [0, 0, 0]);
        // The horizontal line runs through all three cells without a gap.
        let y = (PAD_Y + (fonts.cell_h / 2.0).floor()) as usize;
        for x in (PAD_X as usize)..(PAD_X + fonts.cell_w * 3.0) as usize {
            assert!(s.rgb(x, y)[0] < 60, "gap at x={x}");
        }
        let ui = cache.ui(13.0, false);
        assert!(ui.measure("Dev server") > ui.measure("Dev"));
        assert!(ui.height() > 10.0);
    }
}
