//! Shared colors, animation curves and small drawing helpers.

use eframe::egui::{Color32, CornerRadius, Painter, Pos2, Rect};

pub const BOLD_FONT: &str = "cutie_bold";
pub const POP_DUR: f32 = 0.28;
pub const OUT_DUR: f32 = 0.20;

pub fn accent_a() -> Color32 {
    Color32::from_rgb(255, 113, 176)
}

pub fn accent_b() -> Color32 {
    Color32::from_rgb(140, 92, 240)
}

pub fn bg() -> Color32 {
    Color32::from_rgb(28, 28, 34)
}

pub fn border() -> Color32 {
    Color32::from_rgb(55, 55, 66)
}

pub fn text_c() -> Color32 {
    Color32::from_rgb(240, 240, 246)
}

pub fn dim_c() -> Color32 {
    Color32::from_rgb(140, 138, 155)
}

pub fn track() -> Color32 {
    Color32::from_rgb(45, 45, 54)
}

pub fn ok_c() -> Color32 {
    Color32::from_rgb(94, 234, 168)
}

pub fn err_c() -> Color32 {
    Color32::from_rgb(255, 110, 110)
}

pub fn hover_bg() -> Color32 {
    Color32::from_rgb(42, 42, 50)
}

pub fn alpha_c(c: Color32, a8: u8) -> Color32 {
    Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a8)
}

/// Color32 → the `[f32; 4]` egui wants for `clear_color`.
pub fn to_clear_color(c: Color32) -> [f32; 4] {
    [
        f32::from(c.r()) / 255.0,
        f32::from(c.g()) / 255.0,
        f32::from(c.b()) / 255.0,
        1.0,
    ]
}

// ---------------- animation ----------------

pub fn ease_out_cubic(x: f32) -> f32 {
    1.0 - (1.0 - x).powi(3)
}

pub fn ease_in_quad(x: f32) -> f32 {
    x * x
}

fn ease_out_back(x: f32) -> f32 {
    const C1: f32 = 1.2;
    const C3: f32 = C1 + 1.0;
    1.0 + C3 * (x - 1.0).powi(3) + C1 * (x - 1.0).powi(2)
}

/// pop-in: (alpha, scale, slide_px)
pub fn pop_in(p: f32) -> (f32, f32, f32) {
    let p = p.clamp(0.0, 1.0);
    let alpha = 1.0 - (1.0 - p).powi(2);
    let scale = 0.94 + 0.06 * ease_out_back(p);
    let slide = 14.0 * (1.0 - ease_out_cubic(p));
    (alpha, scale, slide)
}

/// fade-out: (alpha, scale, slide_px)
pub fn pop_out(q: f32) -> (f32, f32, f32) {
    let q = q.clamp(0.0, 1.0);
    let e = ease_in_quad(q);
    (1.0 - e, 1.0 - 0.05 * e, 8.0 * e)
}

/// Alpha channel of an f32 alpha in 0..=1, as u8.
pub fn alpha8(alpha: f32) -> u8 {
    (alpha.clamp(0.0, 1.0) * 255.0).round() as u8
}

// ---------------- drawing ----------------

/// Gradient (accent_a → accent_b) fill, drawn as `n` overlapping slices.
pub fn gradient_bar(p: &Painter, fill: Rect, rounding: CornerRadius, a8: u8) {
    if fill.width() <= 0.5 {
        return;
    }
    const N: u32 = 14;
    let w = fill.width();
    for i in 0..N {
        let t0 = i as f32 / N as f32;
        let t1 = (i + 1) as f32 / N as f32;
        let c0 = lerp_color(accent_a(), accent_b(), t0);
        let x0 = fill.left() + w * t0;
        let x1 = (fill.left() + w * t1 + 0.6).min(fill.right());
        if x1 <= x0 {
            continue;
        }
        p.rect_filled(
            Rect::from_min_max(Pos2::new(x0, fill.top()), Pos2::new(x1, fill.bottom())),
            rounding,
            alpha_c(c0, a8),
        );
    }
}

pub fn lerp_color(a: Color32, b: Color32, t: f32) -> Color32 {
    let t = t.clamp(0.0, 1.0);
    let mix = |x: u8, y: u8| (f32::from(x) + (f32::from(y) - f32::from(x)) * t) as u8;
    Color32::from_rgb(mix(a.r(), b.r()), mix(a.g(), b.g()), mix(a.b(), b.b()))
}

/// Dim a u8 alpha (used to soften already-faded text).
pub fn dim_alpha(a8: u8) -> u8 {
    ((u16::from(a8) * 3) / 5) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha8_clamps() {
        assert_eq!(alpha8(0.0), 0);
        assert_eq!(alpha8(1.0), 255);
        assert_eq!(alpha8(2.0), 255);
        assert_eq!(alpha8(-1.0), 0);
    }

    #[test]
    fn pop_in_out_are_monotone_enough() {
        let (a0, s0, d0) = pop_in(0.0);
        let (a1, s1, _) = pop_in(1.0);
        assert!(a0 < a1);
        assert!(s0 < s1);
        assert!(d0 > 0.0);
        let (oa, _, od) = pop_out(1.0);
        assert!(oa <= 0.0 && od >= 8.0);
    }

    #[test]
    fn dim_alpha_shrinks() {
        assert!(dim_alpha(200) < 200);
        assert_eq!(dim_alpha(0), 0);
    }
}
