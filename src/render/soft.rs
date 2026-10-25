//! Linux drawing: a software canvas over one plain buffer per window (`0xFFRRGGBB` pixels),
//! glyphs shaped and rasterised by `swash` from system fonts found with `fontdb`, rounded
//! shapes and strokes by `tiny-skia`. Rasterised glyphs are cached (bounded) per font, size and
//! glyph, so a full repaint is mostly copying alpha masks.

use super::{Canvas, PathOp, Rgb, Trunc, emoji_presentation};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use swash::FontRef;
use swash::scale::image::{Content, Image};
use swash::scale::{Render, ScaleContext, Source, StrikeWith};
use swash::shape::ShapeContext;
use swash::zeno::{Format, Vector};

// ---------------------------------------------------------------------------------------------
// Font files
// ---------------------------------------------------------------------------------------------

type Data = Arc<dyn AsRef<[u8]> + Send + Sync>;

/// One face of a font file, mapped into memory.
#[derive(Clone)]
struct Face {
    id: fontdb::ID,
    data: Data,
    index: u32,
}

impl Face {
    fn font(&self) -> Option<FontRef<'_>> {
        FontRef::from_index((*self.data).as_ref(), self.index as usize)
    }

    fn has(&self, c: char) -> bool {
        self.font().is_some_and(|f| f.charmap().map(c) != 0)
    }
}

struct Library {
    db: fontdb::Database,
    loaded: HashMap<fontdb::ID, Face>,
    /// Character → the face that has it, when the set's own fonts don't.
    fallback: HashMap<(char, bool), Option<Face>>,
}

thread_local! {
    static LIBRARY: RefCell<Option<Library>> = const { RefCell::new(None) };
}

fn with_library<R>(f: impl FnOnce(&mut Library) -> R) -> R {
    LIBRARY.with(|l| {
        let mut l = l.borrow_mut();
        let lib = l.get_or_insert_with(|| {
            let mut db = fontdb::Database::new();
            db.load_system_fonts();
            // Fonts that ship with the app's tests, when present.
            if let Ok(dir) = std::env::var("CTX_FONT_DIR") {
                db.load_fonts_dir(dir);
            }
            Library { db, loaded: HashMap::new(), fallback: HashMap::new() }
        });
        f(lib)
    })
}

impl Library {
    fn load(&mut self, id: fontdb::ID) -> Option<Face> {
        if let Some(f) = self.loaded.get(&id) {
            return Some(f.clone());
        }
        // SAFETY: font files aren't changed while the app runs (memory-mapped, as fontdb does).
        let (data, index) = unsafe { self.db.make_shared_face_data(id) }?;
        let face = Face { id, data, index };
        face.font()?;
        self.loaded.insert(id, face.clone());
        Some(face)
    }

    fn query(&mut self, families: &[&str], generic: fontdb::Family, bold: bool, italic: bool) -> Option<Face> {
        let mut fams: Vec<fontdb::Family> = families.iter().map(|n| fontdb::Family::Name(n)).collect();
        fams.push(generic);
        let q = fontdb::Query {
            families: &fams,
            weight: if bold { fontdb::Weight::BOLD } else { fontdb::Weight::NORMAL },
            stretch: fontdb::Stretch::Normal,
            style: if italic { fontdb::Style::Italic } else { fontdb::Style::Normal },
        };
        let id = self.db.query(&q)?;
        self.load(id)
    }

    /// A face with `c`, preferring colour emoji for emoji-presentation characters.
    fn fallback_for(&mut self, c: char) -> Option<Face> {
        self.fallback_kind(c, emoji_presentation(c))
    }

    /// A face with `c`: a colour emoji font when `emoji`.
    fn fallback_kind(&mut self, c: char, emoji: bool) -> Option<Face> {
        if let Some(f) = self.fallback.get(&(c, emoji)) {
            return f.clone();
        }
        let mut found = None;
        let preferred: &[&str] = if emoji {
            &["Noto Color Emoji", "Apple Color Emoji", "Twemoji", "Noto Emoji"]
        } else {
            &[
                "DejaVu Sans Mono",
                "DejaVu Sans",
                "Noto Sans Symbols",
                "Noto Sans Symbols 2",
                "Noto Sans CJK SC",
                "Noto Sans Mono CJK SC",
                "Symbola",
                "Noto Color Emoji",
            ]
        };
        for name in preferred {
            let ids: Vec<fontdb::ID> =
                self.db.faces().filter(|f| f.families.iter().any(|(n, _)| n == name)).map(|f| f.id).collect();
            for id in ids {
                if let Some(face) = self.load(id)
                    && face.has(c)
                {
                    found = Some(face);
                    break;
                }
            }
            if found.is_some() {
                break;
            }
        }
        if found.is_none() {
            let ids: Vec<fontdb::ID> = self.db.faces().map(|f| f.id).collect();
            for id in ids {
                if let Some(face) = self.load(id)
                    && face.has(c)
                {
                    found = Some(face);
                    break;
                }
            }
        }
        if self.fallback.len() > 4096 {
            self.fallback.clear();
        }
        self.fallback.insert((c, emoji), found.clone());
        found
    }
}

/// The monospaced families tried for the terminal, in order (Ghostty's default first).
const MONO: &[&str] = &["JetBrains Mono", "DejaVu Sans Mono", "Noto Sans Mono", "Liberation Mono", "Ubuntu Mono"];
const SANS: &[&str] = &["Inter", "Cantarell", "Noto Sans", "Ubuntu", "DejaVu Sans", "Liberation Sans"];

// ---------------------------------------------------------------------------------------------
// Glyph images
// ---------------------------------------------------------------------------------------------

struct Glyph {
    left: i32,
    top: i32,
    width: u32,
    height: u32,
    color: bool,
    data: Vec<u8>,
}

#[derive(Default)]
struct Raster {
    scale: ScaleContext,
    shape: ShapeContext,
    glyphs: HashMap<(fontdb::ID, u16, u32), Option<Rc<Glyph>>>,
}

thread_local! {
    static RASTER: RefCell<Raster> = RefCell::new(Raster::default());
}

/// Most glyph images kept (about 1 KB each at 13 pt on a 2× display).
const GLYPH_CACHE: usize = 4000;

fn rasterise(face: &Face, gid: u16, px: f64, fit: Option<(f64, f64)>) -> Option<Rc<Glyph>> {
    let key = (face.id, gid, (px * 64.0) as u32);
    RASTER.with(|r| {
        let mut r = r.borrow_mut();
        if let Some(g) = r.glyphs.get(&key) {
            return g.clone();
        }
        let font = face.font()?;
        let mut scaler = r.scale.builder(font).size(px as f32).hint(true).build();
        let img: Option<Image> =
            Render::new(&[Source::ColorOutline(0), Source::ColorBitmap(StrikeWith::BestFit), Source::Outline])
                .format(Format::Alpha)
                .offset(Vector::new(0.0, 0.0))
                .render(&mut scaler, gid);
        let g = img.map(|img| {
            let color = img.content == Content::Color;
            let mut g = Glyph {
                left: img.placement.left,
                top: img.placement.top,
                width: img.placement.width,
                height: img.placement.height,
                color,
                data: img.data,
            };
            // Bitmap emoji come at one big size: shrink to fit the cell.
            if let Some((fw, fh)) = fit
                && color
                && (g.width as f64 > fw * 1.05 || g.height as f64 > fh * 1.05)
            {
                g = shrink(&g, (fw / g.width as f64).min(fh / g.height as f64), fh);
            }
            Rc::new(g)
        });
        if r.glyphs.len() > GLYPH_CACHE {
            r.glyphs.clear();
        }
        r.glyphs.insert(key, g.clone());
        g
    })
}

/// Box-filter an RGBA glyph down by `k` (< 1), placing it on the baseline area.
fn shrink(g: &Glyph, k: f64, fit_h: f64) -> Glyph {
    let nw = ((g.width as f64 * k).round() as u32).max(1);
    let nh = ((g.height as f64 * k).round() as u32).max(1);
    let mut out = vec![0u8; (nw * nh * 4) as usize];
    for y in 0..nh {
        for x in 0..nw {
            let (x0, x1) = ((x as f64 / k) as u32, (((x + 1) as f64 / k) as u32).min(g.width).max(x + 1));
            let (y0, y1) = ((y as f64 / k) as u32, (((y + 1) as f64 / k) as u32).min(g.height).max(y + 1));
            let mut acc = [0u32; 4];
            let mut n = 0;
            for sy in y0..y1.min(g.height) {
                for sx in x0..x1.min(g.width) {
                    let i = ((sy * g.width + sx) * 4) as usize;
                    for (a, v) in acc.iter_mut().zip(&g.data[i..i + 4]) {
                        *a += *v as u32;
                    }
                    n += 1;
                }
            }
            let i = ((y * nw + x) * 4) as usize;
            for c in 0..4 {
                out[i + c] = (acc[c] / n.max(1)) as u8;
            }
        }
    }
    // Keep it on the line: its top sits a little under the cell's top.
    let top = ((fit_h * 0.8).round() as i32).max(nh as i32 - (fit_h * 0.25) as i32);
    Glyph { left: (g.left as f64 * k).round() as i32, top, width: nw, height: nh, color: true, data: out }
}

// ---------------------------------------------------------------------------------------------
// Terminal fonts
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Plan {
    Glyph {
        face: usize,
        gid: u16,
    },
    /// A cluster shaped as several glyphs: (face, glyph, x offset in pixels).
    Shaped(Rc<Vec<(usize, u16, f64)>>),
    Blank,
}

/// One family at one size and scale: four styles, cell metrics in pixels, and a plan cache.
pub struct Fonts {
    pub family: String,
    pub points: f64,
    pub scale: f64,
    px: f64,
    /// Regular, bold, italic, bold italic; then fallbacks.
    faces: RefCell<Vec<Face>>,
    plans: RefCell<HashMap<(String, u8), Plan>>,
    pub cell_w: f64,
    pub cell_h: f64,
    pub ascent: f64,
    pub descent: f64,
}

impl Fonts {
    pub fn new(family: &str, points: f64, scale: f64) -> Fonts {
        let px = points * scale;
        let named: Vec<&str> = match family {
            "" | "SF Mono" | "system" => MONO.to_vec(),
            f => std::iter::once(f).chain(MONO.iter().copied()).collect(),
        };
        let faces: Vec<Face> = with_library(|lib| {
            let regular = lib.query(&named, fontdb::Family::Monospace, false, false).expect("a monospaced font");
            let bold = lib.query(&named, fontdb::Family::Monospace, true, false).unwrap_or_else(|| regular.clone());
            let italic = lib.query(&named, fontdb::Family::Monospace, false, true).unwrap_or_else(|| regular.clone());
            let bi = lib.query(&named, fontdb::Family::Monospace, true, true).unwrap_or_else(|| bold.clone());
            vec![regular, bold, italic, bi]
        });
        let font = faces[0].font().expect("font parses");
        let m = font.metrics(&[]).scale(px as f32);
        let gid = font.charmap().map('M');
        let adv = font.glyph_metrics(&[]).scale(px as f32).advance_width(gid) as f64;
        let ascent = m.ascent as f64;
        let descent = m.descent as f64;
        let cell_w = adv.round().max(1.0);
        let cell_h = (ascent + descent + m.leading as f64).ceil().max(1.0);
        Fonts {
            family: family.to_string(),
            points,
            scale,
            px,
            faces: RefCell::new(faces),
            plans: RefCell::new(HashMap::new()),
            cell_w,
            cell_h,
            ascent: ascent.round(),
            descent,
        }
    }

    fn face(&self, i: usize) -> Face {
        self.faces.borrow()[i].clone()
    }

    fn face_index(&self, f: Face) -> usize {
        let mut faces = self.faces.borrow_mut();
        match faces.iter().position(|x| x.id == f.id) {
            Some(i) if i >= 4 => i,
            _ => {
                faces.push(f);
                faces.len() - 1
            }
        }
    }

    fn plan(&self, g: &str, variant: u8) -> Plan {
        if let Some(p) = self.plans.borrow().get(&(g.to_string(), variant)) {
            return p.clone();
        }
        let p = self.make_plan(g, variant);
        let mut plans = self.plans.borrow_mut();
        if plans.len() > 20_000 {
            plans.clear();
        }
        plans.insert((g.to_string(), variant), p.clone());
        p
    }

    fn make_plan(&self, g: &str, variant: u8) -> Plan {
        if g.trim().is_empty() {
            return Plan::Blank;
        }
        let mut chars = g.chars();
        let first = chars.next().unwrap_or(' ');
        let single = chars.next().is_none();
        let base = self.face(variant as usize);
        // An emoji variation selector asks for the emoji form.
        let emoji = emoji_presentation(first) || g.contains('\u{fe0f}');
        if single && !emoji {
            let gid = base.font().map(|f| f.charmap().map(first)).unwrap_or(0);
            if gid != 0 {
                return Plan::Glyph { face: variant as usize, gid };
            }
        }
        // Another font: the emoji font, or one with this symbol.
        let Some(fb) = with_library(|lib| lib.fallback_kind(first, emoji)) else { return Plan::Blank };
        let fi = self.face_index(fb.clone());
        if single {
            let gid = fb.font().map(|f| f.charmap().map(first)).unwrap_or(0);
            return if gid == 0 { Plan::Blank } else { Plan::Glyph { face: fi, gid } };
        }
        // A sequence (emoji with modifiers, combining marks): shape it with that font.
        let mut glyphs = Vec::new();
        RASTER.with(|r| {
            let mut r = r.borrow_mut();
            if let Some(font) = fb.font() {
                let mut shaper = r.shape.builder(font).size(self.px as f32).build();
                shaper.add_str(g);
                let mut x = 0.0;
                shaper.shape_with(|cluster| {
                    for gl in cluster.glyphs {
                        glyphs.push((fi, gl.id, x + gl.x as f64));
                        x += gl.advance as f64;
                    }
                });
            }
        });
        if glyphs.is_empty() { Plan::Blank } else { Plan::Shaped(Rc::new(glyphs)) }
    }
}

/// Font sets shared by every window (one per family, size and scale).
#[derive(Default)]
pub struct FontCache {
    sets: HashMap<(String, u64, u64), Rc<Fonts>>,
    ui: HashMap<(u64, bool), Rc<UiFont>>,
}

impl FontCache {
    pub fn get(&mut self, family: &str, points: f64, scale: f64) -> Rc<Fonts> {
        let key = (family.to_string(), (points * 100.0) as u64, (scale * 100.0) as u64);
        self.sets.entry(key).or_insert_with(|| Rc::new(Fonts::new(family, points, scale))).clone()
    }

    /// The interface font at `px` pixels.
    pub fn ui(&mut self, px: f64, bold: bool) -> Rc<UiFont> {
        self.ui.entry(((px * 100.0) as u64, bold)).or_insert_with(|| Rc::new(UiFont::new(px, bold))).clone()
    }

    /// Drop sets nobody uses (after font size changes).
    pub fn prune(&mut self) {
        self.sets.retain(|_, f| Rc::strong_count(f) > 1);
        self.ui.retain(|_, f| Rc::strong_count(f) > 1);
    }
}

// ---------------------------------------------------------------------------------------------
// Interface font
// ---------------------------------------------------------------------------------------------

/// A proportional font for the app's own chrome (tab labels, buttons, panels).
pub struct UiFont {
    face: Face,
    px: f64,
    ascent: f64,
    descent: f64,
}

impl UiFont {
    fn new(px: f64, bold: bool) -> UiFont {
        let face = with_library(|lib| {
            lib.query(SANS, fontdb::Family::SansSerif, bold, false)
                .or_else(|| lib.query(MONO, fontdb::Family::Monospace, bold, false))
                .expect("a font")
        });
        let m = face.font().expect("font parses").metrics(&[]).scale(px as f32);
        UiFont { face, px, ascent: m.ascent as f64, descent: m.descent as f64 }
    }

    /// Each character's face, glyph and advance.
    fn layout(&self, s: &str) -> Vec<(Face, u16, f64)> {
        let mut out = Vec::new();
        for c in s.chars() {
            let own = self.face.font().map(|f| f.charmap().map(c)).unwrap_or(0);
            let (face, gid) = if own != 0 || c.is_control() {
                (self.face.clone(), own)
            } else {
                match with_library(|lib| lib.fallback_for(c)) {
                    Some(f) => {
                        let g = f.font().map(|x| x.charmap().map(c)).unwrap_or(0);
                        (f, g)
                    }
                    None => (self.face.clone(), 0),
                }
            };
            let adv = face.font().map(|f| f.glyph_metrics(&[]).scale(self.px as f32).advance_width(gid)).unwrap_or(0.0);
            out.push((face, gid, adv as f64));
        }
        out
    }

    pub fn measure(&self, s: &str) -> f64 {
        self.layout(s).iter().map(|g| g.2).sum()
    }

    /// Line height in pixels.
    pub fn height(&self) -> f64 {
        (self.ascent + self.descent).ceil()
    }
}

// ---------------------------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------------------------

/// The pixels a window draws into: `0xFFRRGGBB`, rows top to bottom.
pub struct Surface {
    pub pixels: Vec<u32>,
    pub width: usize,
    pub height: usize,
}

impl Surface {
    pub fn copy_from(&mut self, other: &Surface) {
        if self.width == other.width && self.height == other.height {
            self.pixels.copy_from_slice(&other.pixels);
        }
    }

    /// Never: these pixels are copied to the screen, not shown in place.
    pub fn in_use(&self) -> bool {
        false
    }

    pub fn new_heap(width: usize, height: usize) -> Surface {
        let (w, h) = (width.max(1), height.max(1));
        Surface { pixels: vec![0xFF00_0000; w * h], width: w, height: h }
    }

    pub fn rgb(&self, x: usize, y: usize) -> Rgb {
        let p = self.pixels[y * self.width + x];
        [(p >> 16) as u8, (p >> 8) as u8, p as u8]
    }

    pub fn paint<R>(&mut self, f: impl FnOnce(&mut Painter) -> R) -> R {
        let (w, h) = (self.width, self.height);
        let mut p = Painter { px: &mut self.pixels, w, h, clip: (0, 0, w as i32, h as i32) };
        f(&mut p)
    }

    /// Make everything outside a rounded rectangle (inset by `inset`) transparent (the app icon).
    pub fn clear_outside_rounded(&mut self, inset: f64, radius: f64) {
        let (w, h) = (self.width as f64, self.height as f64);
        for y in 0..self.height {
            for x in 0..self.width {
                let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                let cx = px.clamp(inset + radius, w - inset - radius);
                let cy = py.clamp(inset + radius, h - inset - radius);
                let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                let inside = px >= inset && px <= w - inset && py >= inset && py <= h - inset;
                let a = if inside { (radius + 0.5 - d).clamp(0.0, 1.0) } else { 0.0 };
                let p = &mut self.pixels[y * self.width + x];
                *p = (*p & 0x00FF_FFFF) | (((a * 255.0) as u32) << 24);
            }
        }
    }

    pub fn write_png(&mut self, path: &std::path::Path) -> Result<(), String> {
        let file = std::fs::File::create(path).map_err(|e| format!("can't write {}: {e}", path.display()))?;
        let mut enc = png::Encoder::new(std::io::BufWriter::new(file), self.width as u32, self.height as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().map_err(|e| e.to_string())?;
        let mut data = Vec::with_capacity(self.pixels.len() * 4);
        for p in &self.pixels {
            data.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8, (p >> 24) as u8]);
        }
        w.write_image_data(&data).map_err(|e| e.to_string())
    }
}

fn read_png(path: &std::path::Path) -> Result<(usize, usize, Vec<u8>), String> {
    let f = std::fs::File::open(path).map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let mut dec = png::Decoder::new(std::io::BufReader::new(f));
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut r = dec.read_info().map_err(|e| e.to_string())?;
    let mut buf = vec![0; r.output_buffer_size().ok_or("png too big")?];
    let info = r.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let (w, h) = (info.width as usize, info.height as usize);
    let ch = info.color_type.samples();
    let mut out = Vec::with_capacity(w * h * 4);
    for px in buf[..w * h * ch].chunks(ch) {
        let (r, g, b) = if ch >= 3 { (px[0], px[1], px[2]) } else { (px[0], px[0], px[0]) };
        out.extend_from_slice(&[r, g, b, 255]);
    }
    Ok((w, h, out))
}

/// Compare two PNGs: the share of pixels that differ by more than `threshold` in any channel.
/// Different sizes count as entirely different.
pub fn png_difference(a: &std::path::Path, b: &std::path::Path, threshold: u8) -> Result<f64, String> {
    let (aw, ah, ap) = read_png(a)?;
    let (bw, bh, bp) = read_png(b)?;
    if (aw, ah) != (bw, bh) {
        return Ok(1.0);
    }
    let differing = ap
        .chunks(4)
        .zip(bp.chunks(4))
        .filter(|(x, y)| x.iter().zip(y.iter()).take(3).any(|(p, q)| p.abs_diff(*q) > threshold))
        .count();
    Ok(differing as f64 / (aw * ah).max(1) as f64)
}

// ---------------------------------------------------------------------------------------------
// The painter
// ---------------------------------------------------------------------------------------------

pub struct Painter<'a> {
    px: &'a mut [u32],
    w: usize,
    h: usize,
    /// x0, y0, x1, y1 in whole pixels.
    clip: (i32, i32, i32, i32),
}

#[inline]
fn blend(dst: u32, c: Rgb, a: u32) -> u32 {
    if a >= 255 {
        return 0xFF00_0000 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
    }
    let inv = 255 - a;
    let ch = |shift: u32, v: u8| ((((dst >> shift) & 0xFF) * inv + v as u32 * a + 127) / 255) << shift;
    0xFF00_0000 | ch(16, c[0]) | ch(8, c[1]) | ch(0, c[2])
}

impl Painter<'_> {
    fn span(&self, x: f64, y: f64, w: f64, h: f64) -> Option<(usize, usize, usize, usize)> {
        let x0 = (x.round() as i32).max(self.clip.0);
        let y0 = (y.round() as i32).max(self.clip.1);
        let x1 = ((x + w).round() as i32).min(self.clip.2);
        let y1 = ((y + h).round() as i32).min(self.clip.3);
        (x1 > x0 && y1 > y0).then_some((x0 as usize, y0 as usize, x1 as usize, y1 as usize))
    }

    fn put(&mut self, x: i32, y: i32, c: Rgb, a: u32) {
        if x < self.clip.0 || y < self.clip.1 || x >= self.clip.2 || y >= self.clip.3 || a == 0 {
            return;
        }
        let i = y as usize * self.w + x as usize;
        self.px[i] = blend(self.px[i], c, a);
    }

    fn draw_image(&mut self, g: &Glyph, x: f64, baseline: f64, c: Rgb) {
        let ox = x.round() as i32 + g.left;
        let oy = baseline.round() as i32 - g.top;
        for gy in 0..g.height as i32 {
            for gx in 0..g.width as i32 {
                let i = (gy as u32 * g.width + gx as u32) as usize;
                if g.color {
                    let p = &g.data[i * 4..i * 4 + 4];
                    self.put(ox + gx, oy + gy, [p[0], p[1], p[2]], p[3] as u32);
                } else {
                    let a = g.data[i] as u32;
                    if a > 0 {
                        // A touch of contrast, as Core Text's font smoothing gives.
                        let a = ((a as f64 / 255.0).powf(0.85) * 255.0) as u32;
                        self.put(ox + gx, oy + gy, c, a);
                    }
                }
            }
        }
    }

    /// Run a tiny-skia drawing over the buffer (its colours have red and blue swapped, since
    /// it writes RGBA bytes and the buffer holds BGRA).
    fn skia(&mut self, f: impl FnOnce(&mut tiny_skia::PixmapMut)) {
        let bytes: &mut [u8] =
            // SAFETY: a u32 slice reinterpreted as its bytes; same length ×4, alignment 1.
            unsafe { std::slice::from_raw_parts_mut(self.px.as_mut_ptr().cast(), self.px.len() * 4) };
        if let Some(mut pm) = tiny_skia::PixmapMut::from_bytes(bytes, self.w as u32, self.h as u32) {
            f(&mut pm);
        }
    }

    fn paint_of(&self, c: Rgb, alpha: f64) -> tiny_skia::Paint<'static> {
        let mut p = tiny_skia::Paint::default();
        p.set_color_rgba8(c[2], c[1], c[0], (alpha.clamp(0.0, 1.0) * 255.0) as u8);
        p.anti_alias = true;
        p
    }

    fn clip_mask(&self) -> Option<tiny_skia::Mask> {
        if self.clip == (0, 0, self.w as i32, self.h as i32) {
            return None;
        }
        let mut m = tiny_skia::Mask::new(self.w as u32, self.h as u32)?;
        let r =
            tiny_skia::Rect::from_ltrb(self.clip.0 as f32, self.clip.1 as f32, self.clip.2 as f32, self.clip.3 as f32)?;
        m.fill_path(
            &tiny_skia::PathBuilder::from_rect(r),
            tiny_skia::FillRule::Winding,
            false,
            tiny_skia::Transform::identity(),
        );
        Some(m)
    }
}

impl Canvas for Painter<'_> {
    fn fill(&mut self, c: Rgb, alpha: f64, x: f64, y: f64, w: f64, h: f64) {
        let Some((x0, y0, x1, y1)) = self.span(x, y, w, h) else { return };
        let a = (alpha.clamp(0.0, 1.0) * 255.0).round() as u32;
        if a >= 255 {
            let v = 0xFF00_0000 | (c[0] as u32) << 16 | (c[1] as u32) << 8 | c[2] as u32;
            for row in y0..y1 {
                self.px[row * self.w + x0..row * self.w + x1].fill(v);
            }
        } else {
            for row in y0..y1 {
                for p in &mut self.px[row * self.w + x0..row * self.w + x1] {
                    *p = blend(*p, c, a);
                }
            }
        }
    }

    fn fill_rounded(&mut self, c: Rgb, alpha: f64, x: f64, y: f64, w: f64, h: f64, radii: [f64; 4]) {
        if radii.iter().all(|r| *r <= 0.0) {
            return self.fill(c, alpha, x, y, w, h);
        }
        let lim = (w.min(h) / 2.0).max(0.0);
        let [tl, tr, br, bl] = radii.map(|v| v.clamp(0.0, lim) as f32);
        let (x, y, w, h) = (x as f32, y as f32, w as f32, h as f32);
        let k = 0.447_715_f32; // 1 - 0.5523: cubic approximation of a quarter circle.
        let mut pb = tiny_skia::PathBuilder::new();
        pb.move_to(x + tl, y);
        pb.line_to(x + w - tr, y);
        pb.cubic_to(x + w - tr * k, y, x + w, y + tr * k, x + w, y + tr);
        pb.line_to(x + w, y + h - br);
        pb.cubic_to(x + w, y + h - br * k, x + w - br * k, y + h, x + w - br, y + h);
        pb.line_to(x + bl, y + h);
        pb.cubic_to(x + bl * k, y + h, x, y + h - bl * k, x, y + h - bl);
        pb.line_to(x, y + tl);
        pb.cubic_to(x, y + tl * k, x + tl * k, y, x + tl, y);
        pb.close();
        let Some(path) = pb.finish() else { return };
        let paint = self.paint_of(c, alpha);
        let mask = self.clip_mask();
        self.skia(|pm| {
            pm.fill_path(&path, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), mask.as_ref())
        });
    }

    fn stroke(&mut self, path: &[PathOp], width: f64, c: Rgb, alpha: f64) {
        let mut pb = tiny_skia::PathBuilder::new();
        let mut cur = (0.0f64, 0.0f64);
        for op in path {
            match *op {
                PathOp::Move(x, y) => {
                    pb.move_to(x as f32, y as f32);
                    cur = (x, y);
                }
                PathOp::Line(x, y) => {
                    pb.line_to(x as f32, y as f32);
                    cur = (x, y);
                }
                PathOp::ArcTo { x1, y1, x2, y2, r } => {
                    // Tangent points r away from the corner along each line, then a cubic.
                    let d1 = ((cur.0 - x1).powi(2) + (cur.1 - y1).powi(2)).sqrt().max(1e-9);
                    let d2 = ((x2 - x1).powi(2) + (y2 - y1).powi(2)).sqrt().max(1e-9);
                    let p0 = (x1 + (cur.0 - x1) / d1 * r, y1 + (cur.1 - y1) / d1 * r);
                    let p1 = (x1 + (x2 - x1) / d2 * r, y1 + (y2 - y1) / d2 * r);
                    let k = 0.5523;
                    pb.line_to(p0.0 as f32, p0.1 as f32);
                    pb.cubic_to(
                        (p0.0 + (x1 - p0.0) * k) as f32,
                        (p0.1 + (y1 - p0.1) * k) as f32,
                        (p1.0 + (x1 - p1.0) * k) as f32,
                        (p1.1 + (y1 - p1.1) * k) as f32,
                        p1.0 as f32,
                        p1.1 as f32,
                    );
                    cur = p1;
                }
            }
        }
        let Some(path) = pb.finish() else { return };
        let paint = self.paint_of(c, alpha);
        let stroke = tiny_skia::Stroke { width: width as f32, ..Default::default() };
        let mask = self.clip_mask();
        self.skia(|pm| pm.stroke_path(&path, &paint, &stroke, tiny_skia::Transform::identity(), mask.as_ref()));
    }

    fn clip(&mut self, r: Option<(f64, f64, f64, f64)>) {
        self.clip = match r {
            Some((x, y, w, h)) => (
                (x.round() as i32).max(0),
                (y.round() as i32).max(0),
                ((x + w).round() as i32).min(self.w as i32),
                ((y + h).round() as i32).min(self.h as i32),
            ),
            None => (0, 0, self.w as i32, self.h as i32),
        };
    }

    fn glyph(&mut self, fonts: &Rc<Fonts>, text: &str, variant: u8, x: f64, baseline: f64, c: Rgb) {
        let fit = Some((fonts.cell_w * 2.0, fonts.cell_h));
        match fonts.plan(text, variant) {
            Plan::Blank => {}
            Plan::Glyph { face, gid } => {
                if let Some(g) = rasterise(&fonts.face(face), gid, fonts.px, fit) {
                    self.draw_image(&g, x, baseline, c);
                }
            }
            Plan::Shaped(glyphs) => {
                for (face, gid, dx) in glyphs.iter() {
                    if let Some(g) = rasterise(&fonts.face(*face), *gid, fonts.px, fit) {
                        self.draw_image(&g, x + dx, baseline, c);
                    }
                }
            }
        }
    }

    fn text(&mut self, font: &UiFont, s: &str, x: f64, y: f64, max_w: f64, c: Rgb, trunc: Trunc) -> f64 {
        if s.is_empty() || max_w <= 1.0 {
            return 0.0;
        }
        let mut glyphs = font.layout(s);
        let total: f64 = glyphs.iter().map(|g| g.2).sum();
        if total > max_w {
            let ell = font.layout("…");
            let room = max_w - ell.iter().map(|g| g.2).sum::<f64>();
            match trunc {
                Trunc::Tail => {
                    let mut w = 0.0;
                    let n = glyphs.iter().take_while(|g| {
                        w += g.2;
                        w <= room
                    });
                    let n = n.count();
                    glyphs.truncate(n);
                    glyphs.extend(ell);
                }
                Trunc::Head => {
                    let mut w = 0.0;
                    let n = glyphs.iter().rev().take_while(|g| {
                        w += g.2;
                        w <= room
                    });
                    let n = n.count();
                    let keep = glyphs.split_off(glyphs.len() - n);
                    glyphs = ell;
                    glyphs.extend(keep);
                }
            }
        }
        let baseline = y + font.ascent.round();
        let mut pen = x;
        for (face, gid, adv) in &glyphs {
            if *gid != 0
                && let Some(g) = rasterise(face, *gid, font.px, None)
            {
                self.draw_image(&g, pen, baseline, c);
            }
            pen += adv;
        }
        pen - x
    }
}
