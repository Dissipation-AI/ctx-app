//! Drawing terminal text. Measured in Phase 0 (see SPEC.md § Rendering): Core Text glyphs drawn
//! on the CPU into **one IOSurface per window**, handed to the window's layer as its contents.
//! That is exactly one pixel buffer per window, whatever the tab count, and it stays one buffer
//! while output streams. Metal with two drawables held two buffers plus driver memory.
//!
//! Only rows that changed are redrawn. Glyph lookups (including fallback fonts for emoji and
//! symbols, and shaped clusters) are cached per font set; Core Graphics caches the rasterised
//! glyphs themselves, shared by the whole app.

use crate::text::Row;
use ctx_protocol::{CellStyle, CursorShape};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CFDictionary, CFRange, CFRetained, CFString, CFType, CGFloat, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext};
use objc2_core_text::{CTFont, CTFontOrientation, CTFontSymbolicTraits, CTLine};
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use objc2_io_surface::{IOSurface, IOSurfaceLockOptions};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;
use unicode_segmentation::UnicodeSegmentation;

pub type Rgb = [u8; 3];

/// Padding inside a tile, in points.
pub const PAD_X: f64 = 4.0;
pub const PAD_Y: f64 = 2.0;

// ---------------------------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
enum Plan {
    /// A glyph from one of the set's fonts (index into `Fonts::fallbacks` when >= 4).
    Glyph {
        font: usize,
        glyph: u16,
    },
    /// A cluster that needs shaping (emoji sequences, combining marks): drawn as a line.
    Line(CFRetained<CTLine>),
    Blank,
}

/// One family at one size and scale: four styles, cell metrics in pixels, and a glyph cache.
pub struct Fonts {
    pub family: String,
    pub points: f64,
    pub scale: f64,
    styles: [CFRetained<CTFont>; 4],
    fallbacks: RefCell<Vec<CFRetained<CTFont>>>,
    plans: RefCell<HashMap<(String, u8), Plan>>,
    pub cell_w: f64,
    pub cell_h: f64,
    pub ascent: f64,
    pub descent: f64,
}

fn ct_font_named(name: &str, size: f64) -> Option<CFRetained<CTFont>> {
    if name.is_empty() || name.eq_ignore_ascii_case("SF Mono") || name.eq_ignore_ascii_case("system") {
        return None;
    }
    let cf = CFString::from_str(name);
    let f = unsafe { CTFont::with_name(&cf, size, std::ptr::null()) };
    // Core Text substitutes another font for unknown names; only accept what was asked for.
    let family = unsafe { f.family_name() }.to_string();
    let ps = unsafe { f.post_script_name() }.to_string();
    if family.eq_ignore_ascii_case(name) || ps.eq_ignore_ascii_case(name) { Some(f) } else { None }
}

/// The system monospaced font (SF Mono) at a weight.
fn system_mono(size: f64, bold: bool) -> CFRetained<CTFont> {
    let weight =
        if bold { unsafe { objc2_app_kit::NSFontWeightBold } } else { unsafe { objc2_app_kit::NSFontWeightRegular } };
    let ns = objc2_app_kit::NSFont::monospacedSystemFontOfSize_weight(size, weight);
    // NSFont and CTFont are toll-free bridged.
    let ptr = Retained::into_raw(ns) as *mut CTFont;
    unsafe { CFRetained::from_raw(NonNull::new(ptr).unwrap()) }
}

fn with_traits(f: &CTFont, size: f64, traits: u32) -> Option<CFRetained<CTFont>> {
    unsafe {
        f.copy_with_symbolic_traits(size, std::ptr::null(), CTFontSymbolicTraits(traits), CTFontSymbolicTraits(traits))
    }
}

const BOLD: u32 = CTFontSymbolicTraits::TraitBold.0;
const ITALIC: u32 = CTFontSymbolicTraits::TraitItalic.0;

impl Fonts {
    pub fn new(family: &str, points: f64, scale: f64) -> Fonts {
        let px = points * scale;
        let regular = ct_font_named(family, px).unwrap_or_else(|| system_mono(px, false));
        let bold = with_traits(&regular, px, BOLD).unwrap_or_else(|| {
            if ct_font_named(family, px).is_none() { system_mono(px, true) } else { regular.clone() }
        });
        let italic = with_traits(&regular, px, ITALIC).unwrap_or_else(|| regular.clone());
        let bold_italic = with_traits(&bold, px, ITALIC | BOLD).unwrap_or_else(|| bold.clone());
        let ascent = unsafe { regular.ascent() };
        let descent = unsafe { regular.descent() };
        let leading = unsafe { regular.leading() };
        let mut ch: u16 = 'M' as u16;
        let mut glyph: u16 = 0;
        let mut adv = CGSize::new(0.0, 0.0);
        unsafe {
            regular.glyphs_for_characters(NonNull::from(&mut ch), NonNull::from(&mut glyph), 1);
            regular.advances_for_glyphs(CTFontOrientation::Horizontal, NonNull::from(&mut glyph), &mut adv, 1);
        }
        let cell_w = adv.width.round().max(1.0);
        let cell_h = (ascent + descent + leading).ceil().max(1.0);
        Fonts {
            family: family.to_string(),
            points,
            scale,
            styles: [regular, bold, italic, bold_italic],
            fallbacks: RefCell::new(Vec::new()),
            plans: RefCell::new(HashMap::new()),
            cell_w,
            cell_h,
            ascent: ascent.round(),
            descent,
        }
    }

    fn variant(style: &CellStyle) -> u8 {
        (style.flags & 1 != 0) as u8 | (((style.flags & 2 != 0) as u8) << 1)
    }

    fn font(&self, i: usize) -> CFRetained<CTFont> {
        if i < 4 { self.styles[i].clone() } else { self.fallbacks.borrow()[i - 4].clone() }
    }

    fn plan(&self, g: &str, variant: u8) -> Plan {
        if let Some(p) = self.plans.borrow().get(&(g.to_string(), variant)) {
            return p.clone();
        }
        let p = self.make_plan(g, variant);
        let mut plans = self.plans.borrow_mut();
        // Bounded: a pathological stream of distinct clusters can't grow it forever.
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
        let base = &self.styles[variant as usize];
        let utf16: Vec<u16> = g.encode_utf16().collect();
        let single = g.chars().count() == 1;
        if single {
            let mut chars = utf16.clone();
            let mut glyphs = vec![0u16; chars.len()];
            let ok = unsafe {
                base.glyphs_for_characters(
                    NonNull::new(chars.as_mut_ptr()).unwrap(),
                    NonNull::new(glyphs.as_mut_ptr()).unwrap(),
                    chars.len() as isize,
                )
            };
            if ok && glyphs[0] != 0 {
                return Plan::Glyph { font: variant as usize, glyph: glyphs[0] };
            }
            // A fallback font that has it (emoji, symbols, CJK).
            let cf = CFString::from_str(g);
            let fb = unsafe { base.for_string(&cf, CFRange { location: 0, length: utf16.len() as isize }) };
            let ok = unsafe {
                fb.glyphs_for_characters(
                    NonNull::new(chars.as_mut_ptr()).unwrap(),
                    NonNull::new(glyphs.as_mut_ptr()).unwrap(),
                    chars.len() as isize,
                )
            };
            // Characters that are text by default (✔ ☐ ★ …) stay text-coloured: avoid the emoji font.
            let c = g.chars().next().unwrap_or(' ');
            let mut fb = fb;
            if unsafe { fb.post_script_name() }.to_string().contains("Emoji") && !emoji_presentation(c) {
                for name in ["Apple Symbols", "Menlo", "STIXTwoMath-Regular", "Arial Unicode MS"] {
                    if let Some(f) = ct_font_named(name, unsafe { base.size() }) {
                        let mut gl = [0u16; 2];
                        let mut ch = chars.clone();
                        let found = unsafe {
                            f.glyphs_for_characters(
                                NonNull::new(ch.as_mut_ptr()).unwrap(),
                                NonNull::new(gl.as_mut_ptr()).unwrap(),
                                ch.len().min(2) as isize,
                            )
                        };
                        if found && gl[0] != 0 {
                            glyphs[0] = gl[0];
                            fb = f;
                            break;
                        }
                    }
                }
            }
            if ok && glyphs[0] != 0 {
                let mut fbs = self.fallbacks.borrow_mut();
                let name = unsafe { fb.post_script_name() }.to_string();
                let idx = match fbs.iter().position(|f| unsafe { f.post_script_name() }.to_string() == name) {
                    Some(i) => i,
                    None => {
                        fbs.push(fb);
                        fbs.len() - 1
                    }
                };
                return Plan::Glyph { font: 4 + idx, glyph: glyphs[0] };
            }
        }
        // Shape it: Core Text picks fonts and ligatures for the whole cluster.
        Plan::Line(self.shaped_line(g, base))
    }

    fn shaped_line(&self, g: &str, font: &CTFont) -> CFRetained<CTLine> {
        let s = CFString::from_str(g);
        let yes = unsafe { objc2_core_foundation::kCFBooleanTrue }.unwrap();
        let keys: [&CFString; 2] = unsafe {
            [objc2_core_text::kCTFontAttributeName, objc2_core_text::kCTForegroundColorFromContextAttributeName]
        };
        let vals: [&CFType; 2] = [font.as_ref(), yes.as_ref()];
        let dict = CFDictionary::<CFString, CFType>::from_slices(&keys, &vals);
        let dict_ptr: *const CFDictionary = (&*dict as *const CFDictionary<CFString, CFType>).cast();
        let attr = unsafe { objc2_core_foundation::CFAttributedString::new(None, Some(&s), Some(&*dict_ptr)) }.unwrap();
        unsafe { CTLine::with_attributed_string(&attr) }
    }
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

/// Font sets shared by every window (one per family, size and scale).
#[derive(Default)]
pub struct FontCache {
    sets: HashMap<(String, u64, u64), Rc<Fonts>>,
}

impl FontCache {
    pub fn get(&mut self, family: &str, points: f64, scale: f64) -> Rc<Fonts> {
        let key = (family.to_string(), (points * 100.0) as u64, (scale * 100.0) as u64);
        self.sets.entry(key).or_insert_with(|| Rc::new(Fonts::new(family, points, scale))).clone()
    }

    /// Drop sets nobody uses (after font size changes).
    pub fn prune(&mut self) {
        self.sets.retain(|_, f| Rc::strong_count(f) > 1);
    }
}

// ---------------------------------------------------------------------------------------------
// Surfaces
// ---------------------------------------------------------------------------------------------

enum Backing {
    Io(Retained<IOSurface>),
    Heap(Vec<u8>),
}

/// The pixels a window draws into.
pub struct Surface {
    backing: Backing,
    pub width: usize,
    pub height: usize,
}

const BITMAP_INFO: u32 = 2 | (2 << 12); // premultiplied first alpha, 32-bit little endian: BGRA

impl Surface {
    /// An IOSurface the window's layer shows directly.
    pub fn new_io(width: usize, height: usize) -> Option<Surface> {
        use objc2_io_surface::*;
        let (w, h) = (width.max(1), height.max(1));
        let keys: [&NSString; 4] = unsafe {
            [
                IOSurfacePropertyKeyWidth,
                IOSurfacePropertyKeyHeight,
                IOSurfacePropertyKeyBytesPerElement,
                IOSurfacePropertyKeyPixelFormat,
            ]
        };
        let vals = [
            NSNumber::new_usize(w),
            NSNumber::new_usize(h),
            NSNumber::new_usize(4),
            NSNumber::new_u32(u32::from_be_bytes(*b"BGRA")),
        ];
        let vals: Vec<&objc2::runtime::AnyObject> = vals.iter().map(|v| v.as_ref()).collect();
        let dict = NSDictionary::<NSString, objc2::runtime::AnyObject>::from_slices(&keys, &vals);
        let io = IOSurface::initWithProperties(IOSurface::alloc(), &dict)?;
        Some(Surface { backing: Backing::Io(io), width: w, height: h })
    }

    /// Plain memory (tests and `--render`).
    pub fn new_heap(width: usize, height: usize) -> Surface {
        Surface { backing: Backing::Heap(vec![0; width * height * 4]), width, height }
    }

    pub fn io_surface(&self) -> Option<&IOSurface> {
        match &self.backing {
            Backing::Io(s) => Some(s),
            Backing::Heap(_) => None,
        }
    }

    /// Run `f` with a bitmap context over the pixels.
    fn with_context(&mut self, f: impl FnOnce(&CGContext)) {
        let cs = CGColorSpace::new_device_rgb();
        let (base, bpr): (*mut c_void, usize) = match &mut self.backing {
            Backing::Io(s) => {
                s.lockWithOptions_seed(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
                (s.baseAddress().as_ptr(), s.bytesPerRow() as usize)
            }
            Backing::Heap(v) => (v.as_mut_ptr().cast(), self.width * 4),
        };
        if let Some(ctx) =
            unsafe { CGBitmapContextCreate(base, self.width, self.height, 8, bpr, cs.as_deref(), BITMAP_INFO) }
        {
            CGContext::set_allows_font_smoothing(Some(&ctx), true);
            CGContext::set_should_smooth_fonts(Some(&ctx), true);
            f(&ctx);
        }
        if let Backing::Io(s) = &self.backing {
            s.unlockWithOptions_seed(IOSurfaceLockOptions::empty(), std::ptr::null_mut());
        }
    }

    /// Make everything outside a rounded rectangle (inset by `inset`) transparent (the app icon).
    pub fn clear_outside_rounded(&mut self, inset: f64, radius: f64) {
        let (w, h) = (self.width as f64, self.height as f64);
        if let Backing::Heap(v) = &mut self.backing {
            for y in 0..self.height {
                for x in 0..self.width {
                    let (px, py) = (x as f64 + 0.5, y as f64 + 0.5);
                    let cx = px.clamp(inset + radius, w - inset - radius);
                    let cy = py.clamp(inset + radius, h - inset - radius);
                    let d = ((px - cx).powi(2) + (py - cy).powi(2)).sqrt();
                    let a = (radius + 0.5 - d).clamp(0.0, 1.0);
                    let inside = px >= inset && px <= w - inset && py >= inset && py <= h - inset;
                    let a = if inside { a } else { 0.0 };
                    let i = (y * self.width + x) * 4;
                    for c in &mut v[i..i + 4] {
                        *c = (*c as f64 * a) as u8;
                    }
                }
            }
        }
    }

    /// BGRA pixel at (x, y) from the top-left (tests).
    pub fn pixel(&self, x: usize, y: usize) -> [u8; 4] {
        match &self.backing {
            Backing::Heap(v) => {
                let i = (y * self.width + x) * 4;
                [v[i], v[i + 1], v[i + 2], v[i + 3]]
            }
            Backing::Io(_) => [0; 4],
        }
    }

    /// Write a PNG (heap surfaces).
    pub fn write_png(&mut self, path: &std::path::Path) -> Result<(), String> {
        let mut result = Err("couldn't make an image".to_string());
        self.with_context(|ctx| {
            result = png::write(ctx, path);
        });
        result
    }
}

mod png {
    use objc2_core_graphics::{CGBitmapContextCreateImage, CGContext, CGImage};
    use std::ffi::c_void;

    #[link(name = "ImageIO", kind = "framework")]
    unsafe extern "C" {
        fn CGImageSourceCreateWithURL(url: *const c_void, options: *const c_void) -> *mut c_void;
        fn CGImageSourceCreateImageAtIndex(src: *mut c_void, index: usize, options: *const c_void) -> *mut CGImage;
        fn CGImageDestinationCreateWithURL(
            url: *const c_void,
            ty: *const c_void,
            count: usize,
            options: *const c_void,
        ) -> *mut c_void;
        fn CGImageDestinationAddImage(dest: *mut c_void, image: *const CGImage, props: *const c_void);
        fn CGImageDestinationFinalize(dest: *mut c_void) -> bool;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFRelease(p: *const c_void);
    }

    /// Decode a PNG into BGRA pixels (width, height, pixels).
    pub fn read(path: &std::path::Path) -> Result<(usize, usize, Vec<u8>), String> {
        let url = objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str(
            &path.display().to_string(),
        ));
        unsafe {
            let src = CGImageSourceCreateWithURL(objc2::rc::Retained::as_ptr(&url).cast(), std::ptr::null());
            if src.is_null() {
                return Err(format!("can't read {}", path.display()));
            }
            let img = CGImageSourceCreateImageAtIndex(src, 0, std::ptr::null());
            CFRelease(src);
            if img.is_null() {
                return Err(format!("{} isn't an image", path.display()));
            }
            let w = objc2_core_graphics::CGImage::width(Some(&*img));
            let h = objc2_core_graphics::CGImage::height(Some(&*img));
            let mut px = vec![0u8; w * h * 4];
            let cs = objc2_core_graphics::CGColorSpace::new_device_rgb();
            let ctx = objc2_core_graphics::CGBitmapContextCreate(
                px.as_mut_ptr().cast(),
                w,
                h,
                8,
                w * 4,
                cs.as_deref(),
                super::BITMAP_INFO,
            );
            if let Some(ctx) = ctx {
                let r = objc2_core_foundation::CGRect::new(
                    objc2_core_foundation::CGPoint::new(0.0, 0.0),
                    objc2_core_foundation::CGSize::new(w as f64, h as f64),
                );
                CGContext::draw_image(Some(&ctx), r, Some(&*img));
            }
            CFRelease(img.cast());
            Ok((w, h, px))
        }
    }

    pub fn write(ctx: &CGContext, path: &std::path::Path) -> Result<(), String> {
        let image = CGBitmapContextCreateImage(Some(ctx)).ok_or("no image")?;
        let url = objc2_foundation::NSURL::fileURLWithPath(&objc2_foundation::NSString::from_str(
            &path.display().to_string(),
        ));
        let ty = objc2_foundation::NSString::from_str("public.png");
        unsafe {
            let dest = CGImageDestinationCreateWithURL(
                objc2::rc::Retained::as_ptr(&url).cast(),
                objc2::rc::Retained::as_ptr(&ty).cast(),
                1,
                std::ptr::null(),
            );
            if dest.is_null() {
                return Err(format!("can't write {}", path.display()));
            }
            CGImageDestinationAddImage(dest, &*image, std::ptr::null());
            let ok = CGImageDestinationFinalize(dest);
            CFRelease(dest);
            if ok { Ok(()) } else { Err(format!("can't write {}", path.display())) }
        }
    }
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

fn rgb(c: Rgb) -> (f64, f64, f64) {
    (c[0] as f64 / 255.0, c[1] as f64 / 255.0, c[2] as f64 / 255.0)
}

fn mix(a: Rgb, b: Rgb, t: f64) -> Rgb {
    [
        (a[0] as f64 * (1.0 - t) + b[0] as f64 * t) as u8,
        (a[1] as f64 * (1.0 - t) + b[1] as f64 * t) as u8,
        (a[2] as f64 * (1.0 - t) + b[2] as f64 * t) as u8,
    ]
}

fn fill(ctx: &CGContext, surface_h: f64, c: Rgb, x: f64, y: f64, w: f64, h: f64) {
    let (r, g, b) = rgb(c);
    CGContext::set_rgb_fill_color(Some(ctx), r, g, b, 1.0);
    CGContext::fill_rect(Some(ctx), CGRect::new(CGPoint::new(x, surface_h - y - h), CGSize::new(w, h)));
}

/// Draw tiles into a surface. `divider` fills the gaps between tiles.
pub fn draw(surface: &mut Surface, tiles: &[TileFrame], damage: &Damage, divider: Rgb) {
    let sh = surface.height as f64;
    let sw = surface.width as f64;
    surface.with_context(|ctx| match damage {
        Damage::All => {
            fill(ctx, sh, divider, 0.0, 0.0, sw, sh);
            for t in tiles {
                fill(ctx, sh, t.background, t.x, t.y, t.w, t.h);
                for r in 0..t.rows.len() {
                    draw_row(ctx, sh, t, r);
                }
                draw_overlays(ctx, sh, t);
            }
        }
        Damage::Rows(rows) => {
            for (ti, r) in rows {
                if let Some(t) = tiles.get(*ti)
                    && *r < t.rows.len()
                {
                    let fonts = &t.fonts;
                    let top = t.y + PAD_Y * fonts.scale + *r as f64 * fonts.cell_h;
                    fill(ctx, sh, t.background, t.x, top, t.w, fonts.cell_h);
                    draw_row(ctx, sh, t, *r);
                    if t.scrolled.is_some() {
                        draw_overlays(ctx, sh, t);
                    }
                }
            }
        }
    });
}

fn draw_overlays(ctx: &CGContext, sh: f64, t: &TileFrame) {
    if let Some(f) = t.scrolled {
        let s = t.fonts.scale;
        let track = t.h - 8.0 * s;
        let knob = (track * 0.12).max(20.0 * s);
        let y = t.y + 4.0 * s + (track - knob) * f;
        let c = mix(t.background, t.foreground, 0.35);
        fill(ctx, sh, c, t.x + t.w - 6.0 * s, y, 3.0 * s, knob);
    }
}

fn draw_row(ctx: &CGContext, sh: f64, t: &TileFrame, r: usize) {
    let fonts = &t.fonts;
    let s = fonts.scale;
    let (row, measured) = t.rows[r];
    let left = t.x + PAD_X * s;
    let top = t.y + PAD_Y * s + r as f64 * fonts.cell_h;
    let cw = fonts.cell_w;
    let ch = fonts.cell_h;
    let max_cols = ((t.w - 2.0 * PAD_X * s) / cw).floor().max(0.0) as usize;
    CGContext::save_g_state(Some(ctx));
    CGContext::clip_to_rect(Some(ctx), CGRect::new(CGPoint::new(t.x, sh - top - ch), CGSize::new(t.w, ch)));
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
    for c in &cells {
        if c.column >= max_cols {
            break;
        }
        let (_, bg) = fg_of(&c.style);
        if bg != t.background {
            fill(ctx, sh, bg, left + c.column as f64 * cw, top, cw * c.width as f64, ch);
        }
    }
    for &(sr, a, b) in &t.selection {
        if sr == r {
            let b = b.min(max_cols.max(1));
            if b > a {
                fill(ctx, sh, t.selection_color, left + a as f64 * cw, top, (b - a) as f64 * cw, ch);
            }
        }
    }
    for &(mr, a, b, current) in &t.matches {
        if mr == r {
            let c = if current { [255, 150, 50] } else { [255, 225, 90] };
            fill(ctx, sh, c, left + a as f64 * cw, top, (b - a) as f64 * cw, ch);
        }
    }
    // The cursor block goes under the text; the text on it is drawn in the background colour.
    let cursor_here = t.cursor.filter(|c| c.row == r);
    if let Some(c) = cursor_here
        && c.focused
        && c.shape == CursorShape::Block
        && t.marked.is_none()
    {
        let w = cells.iter().find(|x| x.column == c.column).map(|x| x.width).unwrap_or(1);
        fill(ctx, sh, c.color, left + c.column as f64 * cw, top, cw * w as f64, ch);
    }
    // Glyphs, batched by font and colour.
    let baseline = sh - top - fonts.ascent;
    let mut batch_font: Option<usize> = None;
    let mut batch_color: Rgb = [0, 0, 0];
    let mut glyphs: Vec<u16> = Vec::new();
    let mut positions: Vec<CGPoint> = Vec::new();
    let flush = |font: Option<usize>, color: Rgb, glyphs: &mut Vec<u16>, positions: &mut Vec<CGPoint>| {
        if let Some(fi) = font
            && !glyphs.is_empty()
        {
            let (cr, cg, cb) = rgb(color);
            CGContext::set_rgb_fill_color(Some(ctx), cr, cg, cb, 1.0);
            let f = fonts.font(fi);
            unsafe {
                f.draw_glyphs(
                    NonNull::new(glyphs.as_mut_ptr()).unwrap(),
                    NonNull::new(positions.as_mut_ptr()).unwrap(),
                    glyphs.len(),
                    ctx,
                )
            };
        }
        glyphs.clear();
        positions.clear();
    };
    for c in &cells {
        if c.column >= max_cols {
            break;
        }
        if c.style.flags & 64 != 0 {
            continue;
        }
        let (mut fg, bg) = fg_of(&c.style);
        if let Some(cur) = cursor_here
            && cur.focused
            && cur.shape == CursorShape::Block
            && cur.column == c.column
            && t.marked.is_none()
        {
            fg = bg;
            if fg == cur.color {
                fg = t.background;
            }
        }
        let x = left + c.column as f64 * cw;
        if crate::boxes::is_shape(c.text) {
            flush(batch_font, batch_color, &mut glyphs, &mut positions);
            batch_font = None;
            draw_shape(ctx, sh, c.text.chars().next().unwrap(), x, top, cw * c.width as f64, ch, fg, s);
            continue;
        }
        match fonts.plan(c.text, Fonts::variant(&c.style)) {
            Plan::Blank => {}
            Plan::Glyph { font, glyph } => {
                if batch_font != Some(font) || batch_color != fg {
                    flush(batch_font, batch_color, &mut glyphs, &mut positions);
                    batch_font = Some(font);
                    batch_color = fg;
                }
                glyphs.push(glyph);
                positions.push(CGPoint::new(x, baseline));
            }
            Plan::Line(line) => {
                flush(batch_font, batch_color, &mut glyphs, &mut positions);
                batch_font = None;
                let (cr, cg, cb) = rgb(fg);
                CGContext::set_rgb_fill_color(Some(ctx), cr, cg, cb, 1.0);
                CGContext::set_text_position(Some(ctx), x, baseline);
                unsafe { line.draw(ctx) };
                reset_text(ctx);
            }
        }
        // Underline and strikethrough.
        let lw = (s).max(1.0).round();
        if c.style.flags & 8 != 0 {
            fill(ctx, sh, fg, x, top + fonts.ascent + (fonts.descent / 2.0).round(), cw * c.width as f64, lw);
        }
        if c.style.flags & 32 != 0 {
            fill(ctx, sh, fg, x, top + (ch / 2.0).round(), cw * c.width as f64, lw);
        }
    }
    flush(batch_font, batch_color, &mut glyphs, &mut positions);
    if let Some((lr, a, b)) = t.link
        && lr == r
    {
        fill(
            ctx,
            sh,
            t.foreground,
            left + a as f64 * cw,
            top + ch - s.round().max(1.0) * 2.0,
            (b - a) as f64 * cw,
            s.max(1.0),
        );
    }
    // Other cursor shapes, over the text.
    if let Some(c) = cursor_here
        && t.marked.is_none()
    {
        let x = left + c.column as f64 * cw;
        let w = cells.iter().find(|x| x.column == c.column).map(|x| x.width).unwrap_or(1) as f64 * cw;
        let thick = (2.0 * s).round();
        match (c.focused, c.shape) {
            (true, CursorShape::Block) => {}
            (true, CursorShape::Bar) => fill(ctx, sh, c.color, x, top, thick, ch),
            (true, CursorShape::Underline) => fill(ctx, sh, c.color, x, top + ch - thick, w, thick),
            _ => {
                let lw = s.round().max(1.0);
                fill(ctx, sh, c.color, x, top, w, lw);
                fill(ctx, sh, c.color, x, top + ch - lw, w, lw);
                fill(ctx, sh, c.color, x, top, lw, ch);
                fill(ctx, sh, c.color, x + w - lw, top, lw, ch);
            }
        }
    }
    // Text being composed (accents, CJK input), with an underline.
    if let (Some(c), Some(m)) = (cursor_here, &t.marked) {
        let x0 = left + c.column as f64 * cw;
        let mut x = x0;
        let marked_row = Row::plain(m);
        let mcells = marked_row.cells(true);
        let width: usize = mcells.iter().map(|c| c.width).sum();
        fill(ctx, sh, t.background, x0, top, width as f64 * cw, ch);
        for mc in &mcells {
            match fonts.plan(mc.text, 0) {
                Plan::Glyph { font, glyph } => {
                    let (cr, cg, cb) = rgb(t.foreground);
                    CGContext::set_rgb_fill_color(Some(ctx), cr, cg, cb, 1.0);
                    let mut g = glyph;
                    let mut p = CGPoint::new(x, baseline);
                    unsafe { fonts.font(font).draw_glyphs(NonNull::from(&mut g), NonNull::from(&mut p), 1, ctx) };
                }
                Plan::Line(line) => {
                    let (cr, cg, cb) = rgb(t.foreground);
                    CGContext::set_rgb_fill_color(Some(ctx), cr, cg, cb, 1.0);
                    CGContext::set_text_position(Some(ctx), x, baseline);
                    unsafe { line.draw(ctx) };
                    reset_text(ctx);
                }
                Plan::Blank => {}
            }
            x += mc.width as f64 * cw;
        }
        fill(ctx, sh, t.foreground, x0, top + ch - s.max(1.0), width as f64 * cw, s.max(1.0));
    }
    CGContext::restore_g_state(Some(ctx));
}

/// Drawing a shaped cluster moves the text position; glyph positions are relative to it.
fn reset_text(ctx: &CGContext) {
    use objc2_core_foundation::CGAffineTransform;
    CGContext::set_text_matrix(Some(ctx), CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 });
    CGContext::set_text_position(Some(ctx), 0.0, 0.0);
}

#[allow(clippy::too_many_arguments)]
fn fill_alpha(ctx: &CGContext, sh: f64, c: Rgb, a: f64, x: f64, y: f64, w: f64, h: f64) {
    let (r, g, b) = rgb(c);
    CGContext::set_rgb_fill_color(Some(ctx), r, g, b, a);
    CGContext::fill_rect(Some(ctx), CGRect::new(CGPoint::new(x, sh - y - h), CGSize::new(w, h)));
}

/// A box-drawing or block character, filling its cell (`x`, `top`: pixels from the top-left).
#[allow(clippy::too_many_arguments)]
fn draw_shape(ctx: &CGContext, sh: f64, c: char, x: f64, top: f64, w: f64, h: f64, color: Rgb, scale: f64) {
    use crate::boxes;
    if let Some((rects, alpha)) = boxes::block(c) {
        for (fx, fy, fw, fh) in rects {
            let x0 = (x + fx * w).round();
            let y0 = (top + fy * h).round();
            let x1 = (x + (fx + fw) * w).round();
            let y1 = (top + (fy + fh) * h).round();
            fill_alpha(ctx, sh, color, alpha, x0, y0, x1 - x0, y1 - y0);
        }
        return;
    }
    let light = scale.round().max(1.0);
    let heavy = (light * 2.0).max(2.0);
    let cx = (x + w / 2.0).floor();
    let cy = (top + h / 2.0).floor();
    let (r, g, b) = rgb(color);
    if boxes::is_diagonal(c) {
        CGContext::set_rgb_stroke_color(Some(ctx), r, g, b, 1.0);
        CGContext::set_line_width(Some(ctx), light);
        CGContext::begin_path(Some(ctx));
        if c != '\u{2572}' {
            CGContext::move_to_point(Some(ctx), x, sh - (top + h));
            CGContext::add_line_to_point(Some(ctx), x + w, sh - top);
        }
        if c != '\u{2571}' {
            CGContext::move_to_point(Some(ctx), x, sh - top);
            CGContext::add_line_to_point(Some(ctx), x + w, sh - (top + h));
        }
        CGContext::stroke_path(Some(ctx));
        return;
    }
    let Some([l, rt, u, d]) = boxes::arms(c) else { return };
    if boxes::is_rounded(c) {
        // An arc from one edge's midpoint, round the centre, to the other edge.
        CGContext::set_rgb_stroke_color(Some(ctx), r, g, b, 1.0);
        CGContext::set_line_width(Some(ctx), light);
        let mx = cx + light / 2.0;
        let my = sh - (cy + light / 2.0);
        let hx = if l > 0 { x } else { x + w };
        let vy = if u > 0 { sh - top } else { sh - (top + h) };
        let radius = (w.min(h) / 2.0).floor();
        CGContext::begin_path(Some(ctx));
        CGContext::move_to_point(Some(ctx), hx, my);
        CGContext::add_arc_to_point(Some(ctx), mx, my, mx, vy, radius);
        CGContext::add_line_to_point(Some(ctx), mx, vy);
        CGContext::stroke_path(Some(ctx));
        return;
    }
    let dashes = boxes::dashes(c);
    let thick = |k: u8| if k == 2 { heavy } else { light };
    // The span a line of weight k covers across its centre line `c`.
    let span = |c: f64, k: u8| -> (f64, f64) {
        match k {
            3 => (c - (light / 2.0).floor() - light, c - (light / 2.0).floor() + 2.0 * light),
            k => (c - (thick(k) / 2.0).floor(), c - (thick(k) / 2.0).floor() + thick(k)),
        }
    };
    // Where the horizontal and vertical lines sit, so arms meet them exactly.
    let across = |a: u8, b: u8, c: f64| -> (f64, f64) {
        let ks: Vec<u8> = [a, b].into_iter().filter(|k| *k > 0).collect();
        if ks.is_empty() {
            return span(c, 1);
        }
        let lo = ks.iter().map(|k| span(c, *k).0).fold(f64::MAX, f64::min);
        let hi = ks.iter().map(|k| span(c, *k).1).fold(f64::MIN, f64::max);
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
    let hline = |x0: f64, x1: f64, k: u8| {
        for (off, t) in lines(k) {
            let y0 = cy + off;
            match dashes {
                Some(n) => {
                    let seg = w / n as f64;
                    for i in 0..n {
                        fill(ctx, sh, color, (x + i as f64 * seg).round(), y0, (seg * 0.6).round().max(1.0), t);
                    }
                }
                None => fill(ctx, sh, color, x0.round(), y0, (x1 - x0).round().max(1.0), t),
            }
        }
    };
    let vline = |y0: f64, y1: f64, k: u8| {
        for (off, t) in lines(k) {
            let x0 = cx + off;
            match dashes {
                Some(n) => {
                    let seg = h / n as f64;
                    for i in 0..n {
                        fill(ctx, sh, color, x0, (top + i as f64 * seg).round(), t, (seg * 0.6).round().max(1.0));
                    }
                }
                None => fill(ctx, sh, color, x0, y0.round(), t, (y1 - y0).round().max(1.0)),
            }
        }
    };
    let has_v = u > 0 || d > 0;
    let has_h = l > 0 || rt > 0;
    if l > 0 {
        hline(x, if has_v { v_right } else { cx + light }, l);
    }
    if rt > 0 {
        hline(if has_v { v_left } else { cx }, x + w, rt);
    }
    if u > 0 {
        vline(top, if has_h { h_bot } else { cy + light }, u);
    }
    if d > 0 {
        vline(if has_h { h_top } else { cy }, top + h, d);
    }
}

/// Compare two PNGs: the share of pixels that differ by more than `threshold` in any channel.
/// Different sizes count as entirely different.
pub fn png_difference(a: &std::path::Path, b: &std::path::Path, threshold: u8) -> Result<f64, String> {
    let (aw, ah, ap) = png::read(a)?;
    let (bw, bh, bp) = png::read(b)?;
    if (aw, ah) != (bw, bh) {
        return Ok(1.0);
    }
    let differing = ap
        .as_chunks::<4>()
        .0
        .iter()
        .zip(bp.as_chunks::<4>().0.iter())
        .filter(|(x, y)| x.iter().zip(y.iter()).take(3).any(|(p, q)| p.abs_diff(*q) > threshold))
        .count();
    Ok(differing as f64 / (aw * ah).max(1) as f64)
}

/// A translucent rectangle (where a dragged tab would land), in pixels from the top-left.
pub fn overlay(surface: &mut Surface, (x, y, w, h): (f64, f64, f64, f64), c: Rgb) {
    let sh = surface.height as f64;
    surface.with_context(|ctx| {
        let (r, g, b) = rgb(c);
        CGContext::set_rgb_fill_color(Some(ctx), r, g, b, 0.45);
        CGContext::fill_rect(Some(ctx), CGRect::new(CGPoint::new(x, sh - y - h), CGSize::new(w, h)));
    });
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

#[allow(dead_code)]
fn _cgfloat(_: CGFloat) {}

#[cfg(test)]
mod tests {
    use super::*;
    use ctx_protocol::StyledRun;

    fn tile<'a>(fonts: Rc<Fonts>, rows: Vec<(&'a Row, bool)>, w: f64, h: f64) -> TileFrame<'a> {
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
        let f = Fonts::new("SF Mono", 13.0, 2.0);
        assert_eq!(f.cell_w, f.cell_w.round());
        assert!(f.cell_w >= 14.0 && f.cell_w <= 20.0, "{}", f.cell_w);
        assert!(f.cell_h > f.cell_w);
        let (c, r) = grid_size(&f, 80.0 * f.cell_w + 2.0 * PAD_X * 2.0, 24.0 * f.cell_h + 2.0 * PAD_Y * 2.0);
        assert_eq!((c, r), (80, 24));
        assert_eq!(cell_at(&f, PAD_X * 2.0 + f.cell_w * 3.5, PAD_Y * 2.0 + f.cell_h * 1.2), (3, 1));
    }

    #[test]
    fn glyph_plans_are_cached_and_fall_back() {
        let f = Fonts::new("Menlo", 12.0, 1.0);
        assert!(matches!(f.plan("a", 0), Plan::Glyph { font: 0, .. }));
        assert!(matches!(f.plan("a", 1), Plan::Glyph { font: 1, .. }));
        // An emoji isn't in Menlo: a fallback font (Apple Color Emoji).
        assert!(matches!(f.plan("😀", 0), Plan::Glyph { font: 4, .. }));
        // The same fallback is reused, not added twice.
        let _ = f.plan("😁", 0);
        assert_eq!(f.fallbacks.borrow().len(), 1);
        // Sequences are shaped.
        assert!(matches!(f.plan("👍🏽", 0), Plan::Line(_)));
        assert!(matches!(f.plan(" ", 0), Plan::Blank));
        let n = f.plans.borrow().len();
        let _ = f.plan("a", 0);
        assert_eq!(f.plans.borrow().len(), n, "no duplicate lookups");
        assert!(needs_shaping("👍🏽") && !needs_shaping("a"));
        // A text-style symbol isn't drawn with the emoji font (it takes the text colour).
        if let Plan::Glyph { font, .. } = f.plan("✔", 0) {
            let name = unsafe { f.font(font).post_script_name() }.to_string();
            assert!(!name.contains("Emoji"), "{name}");
        }
        assert!(emoji_presentation('✅') && !emoji_presentation('✔') && emoji_presentation('😀'));
    }

    #[test]
    fn draws_backgrounds_text_and_cursor() {
        let mut cache = FontCache::default();
        let fonts = cache.get("Menlo", 12.0, 1.0);
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
        draw(&mut s, &[t], &Damage::All, [128, 128, 128]);
        let y = (PAD_Y + fonts.cell_h / 2.0) as usize;
        // Red background behind "AB" (BGRA).
        let px = s.pixel((PAD_X + 1.0) as usize, (PAD_Y + 1.0) as usize);
        assert_eq!(&px[..3], &[0, 0, 255]);
        // The cursor block at column 3 is blue.
        let px = s.pixel((PAD_X + fonts.cell_w * 3.0 + 1.0) as usize, (PAD_Y + 1.0) as usize);
        assert_eq!(&px[..3], &[255, 0, 0]);
        // Background elsewhere stays white, and "x" put dark pixels in its cell.
        let px = s.pixel((PAD_X + fonts.cell_w * 2.5) as usize, y);
        assert_eq!(&px[..3], &[255, 255, 255]);
        let dark = (0..fonts.cell_w as usize)
            .flat_map(|dx| (0..fonts.cell_h as usize).map(move |dy| (dx, dy)))
            .any(|(dx, dy)| s.pixel((PAD_X + fonts.cell_w * 4.0) as usize + dx, PAD_Y as usize + dy)[0] < 100);
        assert!(dark, "the glyph drew");
    }
}
