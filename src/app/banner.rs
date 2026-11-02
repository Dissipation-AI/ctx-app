//! Banners: what CTX tells or asks you, in a strip under the tab bar instead of a dialog. One
//! shows at a time (in every window, so it's seen whichever is in front); the rest wait their turn.
//! A button answers it, the × closes it; either way the next one shows.

use super::App;
use super::chrome::{self, ChromeFonts, Hit, STRIP_H};
use super::ui::Colors;
use crate::layout::Rect;
use crate::render::{Canvas, PathOp, Trunc, mix};

/// What happens when a banner is answered: the button's index, or None for the ×.
pub type Then = Box<dyn FnOnce(&mut App, Option<usize>)>;

pub struct Banner {
    pub text: String,
    /// The first is drawn as the main one.
    pub buttons: Vec<String>,
    pub then: Option<Then>,
    /// A problem (amber) rather than news (the accent colour).
    pub warning: bool,
}

pub struct Layout {
    pub bar: Rect,
    pub buttons: Vec<Rect>,
    pub close: Rect,
}

fn button_width(label: &str) -> f64 {
    (label.chars().count() as f64 * 7.5 + 28.0).max(72.0)
}

/// The banner across a window `w` wide at `y`: its buttons, then the ×, at the right.
pub fn layout(w: f64, y: f64, labels: &[String]) -> Layout {
    let close = Rect::new(w - 34.0, y + (STRIP_H - 22.0) / 2.0, 22.0, 22.0);
    let mut x = close.x - 8.0;
    let mut buttons = Vec::new();
    for l in labels.iter().rev() {
        let bw = button_width(l);
        x -= bw;
        buttons.push(Rect::new(x, y + 5.0, bw, STRIP_H - 10.0));
        x -= 8.0;
    }
    buttons.reverse();
    Layout { bar: Rect::new(0.0, y, w, STRIP_H), buttons, close }
}

impl Layout {
    pub fn hit(&self, x: f64, y: f64) -> Option<Hit> {
        if !self.bar.contains(x, y) {
            return None;
        }
        if self.close.contains(x, y) {
            return Some(Hit::BannerClose);
        }
        Some(self.buttons.iter().position(|b| b.contains(x, y)).map(Hit::BannerButton).unwrap_or(Hit::Term))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn draw<C: Canvas>(
    c: &mut C,
    l: &Layout,
    b: &Banner,
    hover: Option<Hit>,
    fonts: &ChromeFonts,
    col: &Colors,
    s: f64,
) {
    let px = |r: Rect| Rect::new(r.x * s, r.y * s, r.w * s, r.h * s);
    let bar = px(l.bar);
    c.fill(mix(col.bar, if b.warning { col.waiting } else { col.accent }, 0.14), 1.0, bar.x, bar.y, bar.w, bar.h);
    c.fill(col.line, 1.0, bar.x, bar.y + bar.h - s.max(1.0), bar.w, s.max(1.0));
    let end = l.buttons.first().map(|r| r.x).unwrap_or(l.close.x) - 16.0;
    c.text(
        &fonts.label,
        &b.text,
        bar.x + 12.0 * s,
        bar.y + (bar.h - fonts.label.height()) / 2.0,
        (end - 12.0).max(0.0) * s,
        col.text,
        Trunc::Tail,
    );
    for (i, (r, label)) in l.buttons.iter().zip(&b.buttons).enumerate() {
        chrome::draw_button(c, *r, label, &fonts.label, col, s, hover == Some(Hit::BannerButton(i)), i == 0);
    }
    let cr = px(l.close);
    if hover == Some(Hit::BannerClose) {
        c.fill_rounded(col.hover, 1.0, cr.x, cr.y, cr.w, cr.h, [4.0 * s; 4]);
    }
    let (cx, cy, a) = (cr.x + cr.w / 2.0, cr.y + cr.h / 2.0, 4.0 * s);
    let path = [
        PathOp::Move(cx - a, cy - a),
        PathOp::Line(cx + a, cy + a),
        PathOp::Move(cx + a, cy - a),
        PathOp::Line(cx - a, cy + a),
    ];
    c.stroke(&path, 1.4 * s, col.dim, 1.0);
}

impl App {
    /// Tell or ask something in a banner. `then` hears which button was clicked (None: the ×).
    /// The same text already waiting isn't added twice.
    pub fn banner(
        &mut self,
        text: impl Into<String>,
        buttons: &[&str],
        warning: bool,
        then: impl FnOnce(&mut App, Option<usize>) + 'static,
    ) {
        let text = text.into().replace('\n', "  ");
        if self.banners.iter().any(|b| b.text == text) {
            return;
        }
        self.banners.push_back(Banner {
            text,
            buttons: buttons.iter().map(|b| b.to_string()).collect(),
            then: Some(Box::new(then)),
            warning,
        });
        if self.banners.len() == 1 {
            self.relayout_all();
        }
    }

    /// The banner showing was answered: run what it asked for and show the next.
    pub fn answer_banner(&mut self, choice: Option<usize>) {
        let Some(mut b) = self.banners.pop_front() else { return };
        self.relayout_all();
        if let Some(then) = b.then.take() {
            then(self, choice);
        }
    }

    fn relayout_all(&mut self) {
        let wins: Vec<u64> = self.wins.keys().copied().collect();
        for w in wins {
            self.layout(w);
            self.dirty_win(w);
        }
        self.redraw_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_then_the_close_at_the_right() {
        let l = layout(800.0, 40.0, &["Restart Server".into(), "Later".into()]);
        assert!(l.buttons[0].x + l.buttons[0].w < l.buttons[1].x);
        assert!(l.buttons[1].x + l.buttons[1].w < l.close.x && l.close.x + l.close.w <= 800.0);
        assert_eq!(l.hit(l.buttons[1].x + 2.0, l.buttons[1].y + 2.0), Some(Hit::BannerButton(1)));
        assert_eq!(l.hit(l.close.x + 11.0, l.close.y + 11.0), Some(Hit::BannerClose));
        assert_eq!(l.hit(10.0, 50.0), Some(Hit::Term));
        assert_eq!(l.hit(10.0, 80.0), None);
    }
}
