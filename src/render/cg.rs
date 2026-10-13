//! macOS drawing: Core Text glyphs and Core Graphics fills into an IOSurface (the window's one
//! pixel buffer), tagged sRGB so colours look the same on every display (as Ghostty's default).
//! Glyph lookups (including fallback fonts for emoji and symbols, and shaped clusters) are cached
//! per font set; Core Graphics caches the rasterised glyphs themselves, shared by the whole app.

use super::{Canvas, PathOp, Rgb, Trunc, emoji_presentation};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_core_foundation::{CFDictionary, CFRange, CFRetained, CFString, CFType, CGPoint, CGRect, CGSize};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext};
use objc2_core_text::{CTFont, CTFontOrientation, CTFontSymbolicTraits, CTLine, CTLineTruncationType};
use objc2_foundation::{NSDictionary, NSNumber, NSString};
use objc2_io_surface::{IOSurface, IOSurfaceLockOptions};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::rc::Rc;

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
        Plan::Line(shaped_line(g, base))
    }
}

fn shaped_line(g: &str, font: &CTFont) -> CFRetained<CTLine> {
    {
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

    /// The interface font (the system font) at `px` pixels.
    pub fn ui(&mut self, px: f64, bold: bool) -> Rc<UiFont> {
        self.ui.entry(((px * 100.0) as u64, bold)).or_insert_with(|| Rc::new(UiFont::new(px, bold))).clone()
    }

    /// Drop sets nobody uses (after font size changes).
    pub fn prune(&mut self) {
        self.sets.retain(|_, f| Rc::strong_count(f) > 1);
        self.ui.retain(|_, f| Rc::strong_count(f) > 1);
    }
}

/// The system font for the app's own chrome (tab labels, buttons, panels).
pub struct UiFont {
    font: CFRetained<CTFont>,
    ascent: f64,
    descent: f64,
}

impl UiFont {
    fn new(px: f64, bold: bool) -> UiFont {
        let weight = if bold {
            unsafe { objc2_app_kit::NSFontWeightSemibold }
        } else {
            unsafe { objc2_app_kit::NSFontWeightRegular }
        };
        let ns = objc2_app_kit::NSFont::systemFontOfSize_weight(px, weight);
        // NSFont and CTFont are toll-free bridged.
        let ptr = Retained::into_raw(ns) as *mut CTFont;
        let font = unsafe { CFRetained::from_raw(NonNull::new(ptr).unwrap()) };
        let ascent = unsafe { font.ascent() };
        let descent = unsafe { font.descent() };
        UiFont { font, ascent, descent }
    }

    fn line(&self, s: &str) -> CFRetained<CTLine> {
        shaped_line(s, &self.font)
    }

    pub fn measure(&self, s: &str) -> f64 {
        let l = self.line(s);
        unsafe { l.typographic_bounds(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) }
    }

    /// Line height in pixels.
    pub fn height(&self) -> f64 {
        (self.ascent + self.descent).ceil()
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
        // Tell the compositor the pixels are sRGB (what we draw in), so colours aren't
        // stretched to the display's wider gamut.
        unsafe {
            let srgb: &NSString =
                &*(objc2_core_graphics::kCGColorSpaceSRGB as *const objc2_core_foundation::CFString).cast::<NSString>();
            io.setAttachment_forKey(srgb, &NSString::from_str("IOSurfaceColorSpace"));
        }
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
        let cs = CGColorSpace::with_name(Some(unsafe { objc2_core_graphics::kCGColorSpaceSRGB }));
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

    /// RGB of the pixel at (x, y) from the top-left (tests; heap surfaces).
    pub fn rgb(&self, x: usize, y: usize) -> Rgb {
        match &self.backing {
            Backing::Heap(v) => {
                let i = (y * self.width + x) * 4;
                [v[i + 2], v[i + 1], v[i]]
            }
            Backing::Io(_) => [0; 3],
        }
    }

    /// Draw with a painter.
    pub fn paint<R>(&mut self, f: impl FnOnce(&mut Painter) -> R) -> R {
        let sh = self.height as f64;
        let mut out = None;
        self.with_context(|ctx| {
            let mut p = Painter { ctx, sh, clipped: false, batch: None, glyphs: Vec::new(), positions: Vec::new() };
            let r = f(&mut p);
            p.flush();
            if p.clipped {
                CGContext::restore_g_state(Some(ctx));
            }
            out = Some(r);
        });
        out.expect("a bitmap context")
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
            let cs = objc2_core_graphics::CGColorSpace::with_name(Some(objc2_core_graphics::kCGColorSpaceSRGB));
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

// ---------------------------------------------------------------------------------------------
// The painter
// ---------------------------------------------------------------------------------------------

fn rgb(c: Rgb) -> (f64, f64, f64) {
    (c[0] as f64 / 255.0, c[1] as f64 / 255.0, c[2] as f64 / 255.0)
}

/// Core Graphics behind [`Canvas`]. Glyphs of one font and colour are batched into one
/// `CTFontDrawGlyphs` call, flushed before anything else is drawn.
pub struct Painter<'a> {
    ctx: &'a CGContext,
    /// Surface height: Core Graphics counts y from the bottom.
    sh: f64,
    clipped: bool,
    batch: Option<(Rc<Fonts>, usize, Rgb)>,
    glyphs: Vec<u16>,
    positions: Vec<CGPoint>,
}

impl Painter<'_> {
    fn flush(&mut self) {
        if let Some((fonts, fi, color)) = &self.batch
            && !self.glyphs.is_empty()
        {
            let (r, g, b) = rgb(*color);
            CGContext::set_rgb_fill_color(Some(self.ctx), r, g, b, 1.0);
            let f = fonts.font(*fi);
            unsafe {
                f.draw_glyphs(
                    NonNull::new(self.glyphs.as_mut_ptr()).unwrap(),
                    NonNull::new(self.positions.as_mut_ptr()).unwrap(),
                    self.glyphs.len(),
                    self.ctx,
                )
            };
        }
        self.glyphs.clear();
        self.positions.clear();
        self.batch = None;
    }

    fn rect(&self, x: f64, y: f64, w: f64, h: f64) -> CGRect {
        CGRect::new(CGPoint::new(x, self.sh - y - h), CGSize::new(w, h))
    }

    fn draw_line(&mut self, line: &CTLine, x: f64, baseline: f64, c: Rgb) {
        let (r, g, b) = rgb(c);
        CGContext::set_rgb_fill_color(Some(self.ctx), r, g, b, 1.0);
        CGContext::set_text_position(Some(self.ctx), x, self.sh - baseline);
        unsafe { line.draw(self.ctx) };
        // Drawing a line moves the text position; glyph positions are relative to it.
        use objc2_core_foundation::CGAffineTransform;
        CGContext::set_text_matrix(
            Some(self.ctx),
            CGAffineTransform { a: 1.0, b: 0.0, c: 0.0, d: 1.0, tx: 0.0, ty: 0.0 },
        );
        CGContext::set_text_position(Some(self.ctx), 0.0, 0.0);
    }
}

impl Canvas for Painter<'_> {
    fn fill(&mut self, c: Rgb, alpha: f64, x: f64, y: f64, w: f64, h: f64) {
        self.flush();
        let (r, g, b) = rgb(c);
        CGContext::set_rgb_fill_color(Some(self.ctx), r, g, b, alpha);
        CGContext::fill_rect(Some(self.ctx), self.rect(x, y, w, h));
    }

    fn fill_rounded(&mut self, c: Rgb, alpha: f64, x: f64, y: f64, w: f64, h: f64, radii: [f64; 4]) {
        self.flush();
        let ctx = Some(self.ctx);
        let (r, g, b) = rgb(c);
        CGContext::set_rgb_fill_color(ctx, r, g, b, alpha);
        let lim = (w.min(h) / 2.0).max(0.0);
        let [tl, tr, br, bl] = radii.map(|v| v.clamp(0.0, lim));
        // Flipped: the top edge is at sh - y.
        let (top, bottom) = (self.sh - y, self.sh - y - h);
        CGContext::begin_path(ctx);
        CGContext::move_to_point(ctx, x + tl, top);
        CGContext::add_arc_to_point(ctx, x + w, top, x + w, bottom, tr);
        CGContext::add_arc_to_point(ctx, x + w, bottom, x, bottom, br);
        CGContext::add_arc_to_point(ctx, x, bottom, x, top, bl);
        CGContext::add_arc_to_point(ctx, x, top, x + w, top, tl);
        CGContext::close_path(ctx);
        CGContext::fill_path(ctx);
    }

    fn stroke(&mut self, path: &[PathOp], width: f64, c: Rgb, alpha: f64) {
        self.flush();
        let ctx = Some(self.ctx);
        let (r, g, b) = rgb(c);
        CGContext::set_rgb_stroke_color(ctx, r, g, b, alpha);
        CGContext::set_line_width(ctx, width);
        CGContext::begin_path(ctx);
        let sh = self.sh;
        for op in path {
            match *op {
                PathOp::Move(x, y) => CGContext::move_to_point(ctx, x, sh - y),
                PathOp::Line(x, y) => CGContext::add_line_to_point(ctx, x, sh - y),
                PathOp::ArcTo { x1, y1, x2, y2, r } => CGContext::add_arc_to_point(ctx, x1, sh - y1, x2, sh - y2, r),
            }
        }
        CGContext::stroke_path(ctx);
    }

    fn clip(&mut self, r: Option<(f64, f64, f64, f64)>) {
        self.flush();
        if self.clipped {
            CGContext::restore_g_state(Some(self.ctx));
            self.clipped = false;
        }
        if let Some((x, y, w, h)) = r {
            CGContext::save_g_state(Some(self.ctx));
            CGContext::clip_to_rect(Some(self.ctx), self.rect(x, y, w, h));
            self.clipped = true;
        }
    }

    fn glyph(&mut self, fonts: &Rc<Fonts>, text: &str, variant: u8, x: f64, baseline: f64, c: Rgb) {
        match fonts.plan(text, variant) {
            Plan::Blank => {}
            Plan::Glyph { font, glyph } => {
                let same =
                    self.batch.as_ref().is_some_and(|(f, fi, col)| Rc::ptr_eq(f, fonts) && *fi == font && *col == c);
                if !same {
                    self.flush();
                    self.batch = Some((fonts.clone(), font, c));
                }
                self.glyphs.push(glyph);
                self.positions.push(CGPoint::new(x, self.sh - baseline));
            }
            Plan::Line(line) => {
                self.flush();
                self.draw_line(&line, x, baseline, c);
            }
        }
    }

    fn text(&mut self, font: &UiFont, s: &str, x: f64, y: f64, max_w: f64, c: Rgb, trunc: Trunc) -> f64 {
        self.flush();
        if s.is_empty() || max_w <= 1.0 {
            return 0.0;
        }
        let mut line = font.line(s);
        let mut w =
            unsafe { line.typographic_bounds(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut()) };
        if w > max_w {
            let token = font.line("…");
            let kind = match trunc {
                Trunc::Head => CTLineTruncationType::Start,
                Trunc::Tail => CTLineTruncationType::End,
            };
            if let Some(t) = unsafe { line.truncated_line(max_w, kind, Some(&token)) } {
                line = t;
                w = unsafe {
                    line.typographic_bounds(std::ptr::null_mut(), std::ptr::null_mut(), std::ptr::null_mut())
                };
            }
        }
        self.draw_line(&line, x, y + font.ascent.round(), c);
        w
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // A text-style symbol isn't drawn with the emoji font (it takes the text colour).
        if let Plan::Glyph { font, .. } = f.plan("✔", 0) {
            let name = unsafe { f.font(font).post_script_name() }.to_string();
            assert!(!name.contains("Emoji"), "{name}");
        }
    }
}
