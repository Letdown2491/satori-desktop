//! Stroke-drawn icons: glyphs painted as raw paths at render time.
//!
//! TODO(port): crates/app/assets/icons/scalable/actions/satori-*-symbolic.svg —
//! P0 (three of ~34 ported; add glyphs as views need them). The GTK symbolic
//! SVG pipeline dies with D13; in GPUI the 24-unit grid coordinates become
//! `PathBuilder` ops, stroked at paint time and crisp at every scale. Same
//! iconography, zero asset toolchain.

use gpui::{canvas, prelude::*, px, rgb, IntoElement, PathBuilder, Pixels, Point};

/// Maps 24-unit glyph coordinates onto the element's pixel bounds.
pub(crate) struct Grid {
    origin: Point<Pixels>,
    scale: f32,
}

impl Grid {
    fn p(&self, x: f32, y: f32) -> Point<Pixels> {
        gpui::point(
            self.origin.x + px(x * self.scale),
            self.origin.y + px(y * self.scale),
        )
    }
}

type Glyph = fn(&mut PathBuilder, &Grid);

// satori-like-symbolic.svg
fn heart(b: &mut PathBuilder, g: &Grid) {
    b.move_to(g.p(12.0, 20.3));
    b.curve_to(g.p(3.8, 12.8), g.p(8.0, 16.5));
    b.curve_to(g.p(2.6, 8.6), g.p(2.6, 10.8));
    b.curve_to(g.p(6.2, 4.0), g.p(2.6, 4.6));
    b.curve_to(g.p(12.0, 7.0), g.p(9.6, 3.6));
    b.curve_to(g.p(17.8, 4.0), g.p(14.4, 3.6));
    b.curve_to(g.p(21.4, 8.6), g.p(21.4, 4.6));
    b.curve_to(g.p(20.2, 12.8), g.p(21.4, 10.8));
    b.curve_to(g.p(12.0, 20.3), g.p(16.0, 16.5));
}

// satori-comment-symbolic.svg
fn bubble(b: &mut PathBuilder, g: &Grid) {
    b.move_to(g.p(20.5, 14.5));
    b.curve_to(g.p(18.5, 16.5), g.p(20.5, 15.6));
    b.line_to(g.p(8.0, 16.5));
    b.line_to(g.p(4.0, 20.0));
    b.line_to(g.p(4.0, 5.8));
    b.curve_to(g.p(5.9, 3.5), g.p(4.0, 4.4));
    b.line_to(g.p(18.1, 3.5));
    b.curve_to(g.p(20.5, 5.8), g.p(19.9, 3.5));
    b.close();
}

// satori-bookmark-symbolic.svg
fn bookmark(b: &mut PathBuilder, g: &Grid) {
    b.move_to(g.p(19.0, 21.0));
    b.line_to(g.p(12.0, 16.0));
    b.line_to(g.p(5.0, 21.0));
    b.line_to(g.p(5.0, 5.8));
    b.curve_to(g.p(6.9, 3.5), g.p(5.0, 4.4));
    b.line_to(g.p(17.1, 3.5));
    b.curve_to(g.p(19.0, 5.8), g.p(19.0, 4.4));
    b.close();
}

/// A stroke-drawn glyph on the 24-unit grid — the GPUI twin of a symbolic SVG.
/// All outline, no fill, per the icon rule.
pub fn icon(glyph: Glyph, color: u32, size_px: f32) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let scale = bounds.size.width.min(bounds.size.height) / 24.0;
            let g = Grid {
                origin: bounds.origin,
                scale: f32::from(scale),
            };
            let mut b = PathBuilder::stroke(px(1.9 * g.scale));
            glyph(&mut b, &g);
            if let Ok(path) = b.build() {
                window.paint_path(path, rgb(color));
            }
        },
    )
    .size(px(size_px))
}

pub fn heart_icon(color: u32) -> impl IntoElement {
    icon(heart, color, 17.0)
}

pub fn bubble_icon(color: u32) -> impl IntoElement {
    icon(bubble, color, 17.0)
}

pub fn bookmark_icon(color: u32) -> impl IntoElement {
    icon(bookmark, color, 17.0)
}

// The ensō — the brush circle with a gap, drawn as a slightly spiralling
// arc that tapers toward the tail (the brush lifting off). It opens every
// session: the waiting mark of the login flow.
fn enso_glyph(b: &mut PathBuilder, g: &Grid) {
    let start = 60.0_f32.to_radians();
    let sweep = 320.0_f32.to_radians();
    let n = 80;
    for i in 0..=n {
        let t = i as f32 / n as f32;
        let a = start + sweep * t;
        // Radius tapers inward toward the tail.
        let r = 9.2 - 1.1 * t;
        let pt = g.p(12.0 + r * a.cos(), 12.0 + r * a.sin());
        if i == 0 {
            b.move_to(pt);
        } else {
            b.line_to(pt);
        }
    }
}

/// The ensō at display size — a brush circle, so the stroke runs heavier
/// than the hairline glyphs.
pub fn enso_icon(color: u32, size_px: f32) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, _, window, _| {
            let scale = bounds.size.width.min(bounds.size.height) / 24.0;
            let g = Grid {
                origin: bounds.origin,
                scale: f32::from(scale),
            };
            let mut b = PathBuilder::stroke(px(2.4 * g.scale));
            enso_glyph(&mut b, &g);
            if let Ok(path) = b.build() {
                window.paint_path(path, rgb(color));
            }
        },
    )
    .size(px(size_px))
}
