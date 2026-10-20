//! OpenType text pipeline: unicode-bidi + rustybuzz shaping + ab_glyph
//! rasterization into a glyph-cached atlas, drawn as epaint meshes.
//!
//! egui 0.31 has no shaping/bidi, so any complex script (Arabic, Hebrew,
//! Syriac, …) goes through this pipeline; pure-Latin strings keep using
//! egui's native text (fast path) via [`needs`].
//!
//! Design notes:
//! - Layout (shaping + placement) is cached per `(text, size, bold)`.
//! - Rasterization is cached per `(face, glyph_id, size)`, so a string that
//!   changes every frame (speed/ETA) only touches the atlas when it contains
//!   a glyph that has never been drawn before — no more full-atlas uploads.
//! - Layout runs in two passes: first the max ascent/descent over *all*
//!   bidi runs, then glyph placement. A single-pass version positions the
//!   first run before the later run's larger ascent is known, which splits
//!   mixed Latin/Arabic text across two baselines.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::{LazyLock, Mutex};

use ab_glyph::{point, Font as AbFont, FontRef, Glyph as AbGlyph, GlyphId, PxScale};
use eframe::egui::{
    Align, Align2, Color32, ColorImage, FontId, Mesh, Pos2, Rect, Shape, TextureHandle,
    TextureOptions, Vec2,
};
use rustybuzz::{Direction, Face as RbFace, UnicodeBuffer};
use unicode_bidi::{Level, ParagraphBidiInfo};

const ATLAS_W: usize = 1024;
const ATLAS_H: usize = 1024;
/// Shaped-layout cache cap; beyond this the map is dropped wholesale.
const MAX_LAYOUT_CACHE: usize = 512;

const F_LATIN: u8 = 0;
const F_LATIN_BOLD: u8 = 1;
const F_ARABIC: u8 = 2;
const F_ARABIC_BOLD: u8 = 3;

/// Does this string need the shaping pipeline (any RTL/complex char)?
pub fn needs(s: &str) -> bool {
    s.chars().any(is_complex_char)
}

fn is_complex_char(c: char) -> bool {
    let u = c as u32;
    matches!(u,
        0x0590..=0x05FF   // Hebrew
        | 0x0600..=0x06FF // Arabic
        | 0x0700..=0x077F // Syriac
        | 0x0780..=0x07BF // Thaana
        | 0x08A0..=0x08FF // Arabic Extended-A
        | 0xFB1D..=0xFB4F // Hebrew presentation forms
        | 0xFB50..=0xFDFF // Arabic presentation forms-A
        | 0xFE70..=0xFEFF // Arabic presentation forms-B
    )
}

/// Base paragraph direction: first strong char decides; Arabic/Hebrew → RTL.
fn first_strong_level(s: &str) -> Level {
    for c in s.chars() {
        if is_complex_char(c) {
            return Level::rtl();
        }
        if c.is_alphabetic() {
            return Level::ltr();
        }
    }
    Level::ltr()
}

// ---------------------------------------------------------------------------
// Text style
// ---------------------------------------------------------------------------

/// Size + weight, cheap to copy. Shared by the shaping pipeline and the
/// Latin fast path so callers can pass one value around.
#[derive(Clone, Copy, PartialEq)]
pub struct TextStyle {
    pub size: f32,
    pub bold: bool,
}

impl TextStyle {
    pub const fn new(size: f32, bold: bool) -> Self {
        Self { size, bold }
    }
}

// ---------------------------------------------------------------------------
// Font bytes — read from %WINDIR% once, leak once, reuse everywhere
// ---------------------------------------------------------------------------

/// Font file bytes keyed by file name; each hit is leaked exactly once so
/// every face (and every pipeline instance) shares the same allocation.
type FontCache = HashMap<&'static str, Option<&'static [u8]>>;

fn font_bytes(name: &'static str) -> Option<&'static [u8]> {
    static CACHE: LazyLock<Mutex<FontCache>> = LazyLock::new(|| Mutex::new(HashMap::new()));
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(hit) = cache.get(name) {
        return *hit;
    }
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let path = std::path::Path::new(&windir).join("Fonts").join(name);
    let loaded = std::fs::read(path)
        .ok()
        .map(|b| -> &'static [u8] { Box::leak(b.into_boxed_slice()) });
    cache.insert(name, loaded);
    loaded
}

// ---------------------------------------------------------------------------
// Font faces (rustybuzz shapes, ab_glyph rasterizes — same leaked bytes)
// ---------------------------------------------------------------------------

struct Face {
    rb: RbFace<'static>,
    ab: FontRef<'static>,
}

impl Face {
    fn load(names: &[&'static str]) -> Option<Self> {
        for name in names {
            let Some(bytes) = font_bytes(name) else {
                continue;
            };
            let Some(rb) = RbFace::from_slice(bytes, 0) else {
                continue;
            };
            let Ok(ab) = FontRef::try_from_slice(bytes) else {
                continue;
            };
            return Some(Self { rb, ab });
        }
        None
    }

    fn covers(&self, c: char) -> bool {
        self.rb.glyph_index(c).is_some()
    }

    fn ascender_px(&self, size: f32) -> f32 {
        self.rb.ascender() as f32 * size / self.rb.units_per_em() as f32
    }

    fn descender_px(&self, size: f32) -> f32 {
        self.rb.descender() as f32 * size / self.rb.units_per_em() as f32
    }

    /// Shape one run; glyph advances/offsets in px.
    fn shape(&self, text: &str, rtl: bool, size: f32) -> Vec<ShapedGlyph> {
        if text.is_empty() {
            return Vec::new();
        }
        let mut buf = UnicodeBuffer::new();
        buf.push_str(text);
        buf.set_direction(if rtl {
            Direction::RightToLeft
        } else {
            Direction::LeftToRight
        });
        let out = rustybuzz::shape(&self.rb, &[], buf);
        let scale = size / self.rb.units_per_em() as f32;
        out.glyph_infos()
            .iter()
            .zip(out.glyph_positions())
            .map(|(info, pos)| ShapedGlyph {
                id: info.glyph_id as u16,
                x_advance: pos.x_advance as f32 * scale,
                x_offset: pos.x_offset as f32 * scale,
                y_offset: pos.y_offset as f32 * scale,
            })
            .collect()
    }
}

#[derive(Clone, Copy)]
struct ShapedGlyph {
    id: u16,
    x_advance: f32,
    x_offset: f32,
    y_offset: f32,
}

/// One glyph placed in a text block: `x` is pen x, `y` is the baseline y
/// measured from the block's top.
#[derive(Clone, Copy)]
struct Placed {
    face: u8,
    id: u16,
    x: f32,
    y: f32,
}

#[derive(Clone)]
struct Layout {
    width: f32,
    height: f32,
    glyphs: Vec<Placed>,
}

// ---------------------------------------------------------------------------
// Atlas — shelf packer + per-glyph raster cache, coverage in alpha
// ---------------------------------------------------------------------------

/// Cached raster metrics/UVs for one `(face, glyph, size)`.
/// `ox`/`oy` are the ink bounds offset from the baseline origin.
#[derive(Clone, Copy)]
struct Raster {
    ox: f32,
    oy: f32,
    w: f32,
    h: f32,
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
}

type RasterKey = (u8, u16, u16);

fn size_q(size: f32) -> u16 {
    (size * 4.0).round().clamp(0.0, u16::MAX as f32) as u16
}

struct Atlas {
    x: u32,
    y: u32,
    row_h: u32,
    image: ColorImage,
    rasters: HashMap<RasterKey, Raster>,
    dirty: bool,
}

impl Atlas {
    fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            row_h: 0,
            image: ColorImage::new([ATLAS_W, ATLAS_H], Color32::TRANSPARENT),
            rasters: HashMap::new(),
            dirty: false,
        }
    }

    fn reset(&mut self) {
        self.x = 0;
        self.y = 0;
        self.row_h = 0;
        self.rasters.clear();
        self.image.pixels.fill(Color32::TRANSPARENT);
        self.dirty = true;
    }

    fn alloc(&mut self, w: u32, h: u32) -> Option<(u32, u32)> {
        let w = w + 1;
        let h = h + 1;
        if w as usize > ATLAS_W || h as usize > ATLAS_H {
            return None;
        }
        if self.x + w > ATLAS_W as u32 {
            self.y += self.row_h;
            self.x = 0;
            self.row_h = 0;
        }
        if self.y + h > ATLAS_H as u32 {
            return None;
        }
        let pos = (self.x, self.y);
        self.x += w;
        self.row_h = self.row_h.max(h);
        Some(pos)
    }

    /// Rasterize `glyph` at `size` into the atlas (cached per face/id/size).
    fn raster(&mut self, face: &Face, face_id: u8, glyph: u16, size: f32) -> Raster {
        let key = (face_id, glyph, size_q(size));
        if let Some(hit) = self.rasters.get(&key) {
            return *hit;
        }
        let outlined = face.ab.outline_glyph(AbGlyph {
            id: GlyphId(glyph),
            scale: PxScale::from(size),
            position: point(0.0, 0.0),
        });
        let raster = match outlined {
            Some(outlined) => {
                let bounds = outlined.px_bounds();
                let bw = bounds.width().ceil() as u32;
                let bh = bounds.height().ceil() as u32;
                if bw == 0 || bh == 0 {
                    Raster {
                        ox: 0.0,
                        oy: 0.0,
                        w: 0.0,
                        h: 0.0,
                        u0: 0.0,
                        v0: 0.0,
                        u1: 0.0,
                        v1: 0.0,
                    }
                } else {
                    let mut packed = self.alloc(bw, bh);
                    if packed.is_none() {
                        self.reset();
                        packed = self.alloc(bw, bh);
                    }
                    match packed {
                        Some((ax, ay)) => {
                            let (w, h) = (ATLAS_W as u32, ATLAS_H as u32);
                            outlined.draw(|px, py, cov| {
                                let (x, y) = (ax + px, ay + py);
                                if x < w && y < h {
                                    let idx = (y * w + x) as usize;
                                    if let Some(p) = self.image.pixels.get_mut(idx) {
                                        *p = Color32::from_rgba_unmultiplied(
                                            255,
                                            255,
                                            255,
                                            (cov * 255.0) as u8,
                                        );
                                    }
                                }
                            });
                            self.dirty = true;
                            Raster {
                                ox: bounds.min.x,
                                oy: bounds.min.y,
                                w: bounds.width(),
                                h: bounds.height(),
                                u0: ax as f32 / ATLAS_W as f32,
                                v0: ay as f32 / ATLAS_H as f32,
                                u1: (ax + bw) as f32 / ATLAS_W as f32,
                                v1: (ay + bh) as f32 / ATLAS_H as f32,
                            }
                        }
                        // glyph larger than the whole atlas — nothing to draw
                        None => Raster {
                            ox: 0.0,
                            oy: 0.0,
                            w: 0.0,
                            h: 0.0,
                            u0: 0.0,
                            v0: 0.0,
                            u1: 0.0,
                            v1: 0.0,
                        },
                    }
                }
            }
            // no outline (e.g. space) — zero-size, but remember it
            None => Raster {
                ox: 0.0,
                oy: 0.0,
                w: 0.0,
                h: 0.0,
                u0: 0.0,
                v0: 0.0,
                u1: 0.0,
                v1: 0.0,
            },
        };
        self.rasters.insert(key, raster);
        raster
    }
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

pub struct TextPipeline {
    faces: [Option<Face>; 4],
    atlas: Atlas,
    tex: Option<TextureHandle>,
    /// `RefCell` so `measure`/`fit` (shared refs) can warm the cache too;
    /// the pipeline is only ever touched from the UI thread.
    layouts: RefCell<HashMap<(String, u32, bool), Layout>>,
}

impl Default for TextPipeline {
    fn default() -> Self {
        Self::new()
    }
}

impl TextPipeline {
    pub fn new() -> Self {
        let latin = Face::load(&["segoeui.ttf", "tahoma.ttf", "arial.ttf"]);
        let latin_bold = Face::load(&["seguisb.ttf", "arialbd.ttf", "tahomabd.ttf"]);
        let samples = ['\u{0628}', '\u{0644}', '\u{064A}'];
        let arabic = Face::load(&["segoeuiar.ttf", "arial.ttf", "tahoma.ttf"])
            .filter(|f| samples.iter().all(|&c| f.covers(c)));
        let arabic_bold = Face::load(&["arialbd.ttf", "tahomabd.ttf"])
            .filter(|f| samples.iter().all(|&c| f.covers(c)));
        if arabic.is_none() {
            eprintln!("cutie: no Arabic-capable font found; Arabic will use the Latin face");
        }
        Self {
            faces: [latin, latin_bold, arabic, arabic_bold],
            atlas: Atlas::new(),
            tex: None,
            layouts: RefCell::new(HashMap::new()),
        }
    }

    pub fn init_textures(&mut self, ctx: &eframe::egui::Context) {
        self.tex = Some(ctx.load_texture(
            "cutie_text_atlas",
            self.atlas.image.clone(),
            TextureOptions::LINEAR,
        ));
    }

    /// Width in px, no rasterization (for fit/measure).
    pub fn measure(&self, text: &str, style: TextStyle) -> f32 {
        self.layout_for(text, style).width
    }

    /// Truncate to max_w; returns (display_string, was_truncated).
    pub fn fit(&self, text: &str, style: TextStyle, max_w: f32) -> (String, bool) {
        if max_w <= 4.0 {
            return ("…".into(), true);
        }
        if self.measure(text, style) <= max_w {
            return (text.to_string(), false);
        }
        let chars: Vec<char> = text.chars().collect();
        let mut lo = 0usize;
        let mut hi = chars.len();
        let mut best = 0usize;
        while lo <= hi {
            let mid = (lo + hi) / 2;
            let head: String = chars[..mid].iter().collect();
            let cand = format!("{head}…");
            if self.measure(&cand, style) <= max_w {
                best = mid;
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }
        let head: String = chars[..best].iter().collect();
        (format!("{head}…"), true)
    }

    /// Draw text; `align` positions the text block box (width x height) at `pos`.
    pub fn paint(
        &mut self,
        painter: &eframe::egui::Painter,
        text: &str,
        pos: Pos2,
        align: Align2,
        style: TextStyle,
        color: Color32,
    ) {
        if text.is_empty() {
            return;
        }
        let Some(tex_id) = self.tex.as_ref().map(|t| t.id()) else {
            return;
        };
        let layout = self.layout_for(text, style);
        if layout.glyphs.is_empty() {
            return;
        }
        let ox = match align.0[0] {
            Align::Min => pos.x,
            Align::Center => pos.x - layout.width * 0.5,
            Align::Max => pos.x - layout.width,
        };
        let oy = match align.0[1] {
            Align::Min => pos.y,
            Align::Center => pos.y - layout.height * 0.5,
            Align::Max => pos.y - layout.height,
        };

        self.ensure_rasters(&layout, style.size);
        let mut mesh = Mesh::with_texture(tex_id);
        for g in &layout.glyphs {
            let Some(r) = self.atlas.rasters.get(&(g.face, g.id, size_q(style.size))) else {
                continue;
            };
            if r.w <= 0.0 || r.h <= 0.0 {
                continue;
            }
            mesh.add_rect_with_uv(
                Rect::from_min_size(
                    Pos2::new(ox + g.x + r.ox, oy + g.y + r.oy),
                    Vec2::new(r.w, r.h),
                ),
                Rect::from_min_max(Pos2::new(r.u0, r.v0), Pos2::new(r.u1, r.v1)),
                color,
            );
        }
        if !mesh.indices.is_empty() {
            painter.add(Shape::Mesh(mesh.into()));
        }
        self.upload();
    }

    fn ensure_rasters(&mut self, layout: &Layout, size: f32) {
        for g in &layout.glyphs {
            let Some(face) = self.faces[g.face as usize].as_ref() else {
                continue;
            };
            self.atlas.raster(face, g.face, g.id, size);
        }
    }

    fn upload(&mut self) {
        if !self.atlas.dirty {
            return;
        }
        if let Some(tex) = self.tex.as_mut() {
            tex.set(self.atlas.image.clone(), TextureOptions::LINEAR);
        }
        self.atlas.dirty = false;
    }

    fn layout_for(&self, text: &str, style: TextStyle) -> Layout {
        let key = (text.to_string(), style.size.to_bits(), style.bold);
        let mut layouts = self.layouts.borrow_mut();
        if let Some(hit) = layouts.get(&key) {
            return hit.clone();
        }
        if layouts.len() > MAX_LAYOUT_CACHE {
            layouts.clear();
        }
        let layout = Self::layout(text, style.size, style.bold, &self.faces);
        layouts.insert(key, layout.clone());
        layout
    }

    /// Layout + cache. Two-pass: metrics for every bidi run first, then place.
    fn layout(text: &str, size: f32, bold: bool, faces: &[Option<Face>; 4]) -> Layout {
        let mut out = Layout {
            width: 0.0,
            height: size.max(1.0),
            glyphs: Vec::new(),
        };
        if text.is_empty() {
            return out;
        }
        let base = first_strong_level(text);
        let info = ParagraphBidiInfo::new(text, Some(base));
        let (levels, runs) = info.visual_runs(Range {
            start: 0,
            end: text.len(),
        });

        // pass 1 — pick a face per run, take the max ascent/min descent
        let mut picked: Vec<(Level, Range<usize>, u8, &Face)> = Vec::new();
        let mut ascent = 0.0f32;
        let mut descent = 0.0f32;
        for (level, range) in levels.into_iter().zip(runs) {
            if range.is_empty() {
                continue;
            }
            let rtl = text[range.clone()].chars().any(is_complex_char);
            let Some((face_id, face)) = pick_face(rtl, bold, faces) else {
                continue;
            };
            ascent = ascent.max(face.ascender_px(size));
            descent = descent.min(face.descender_px(size));
            picked.push((level, range, face_id, face));
        }

        // pass 2 — shape and place every glyph on that shared baseline
        let mut pen_x = 0.0f32;
        for (level, range, face_id, face) in picked {
            let run = &text[range];
            let rtl = level.is_rtl();
            for g in face.shape(run, rtl, size) {
                out.glyphs.push(Placed {
                    face: face_id,
                    id: g.id,
                    x: pen_x + g.x_offset,
                    y: ascent + g.y_offset,
                });
                pen_x += g.x_advance;
            }
        }

        out.width = pen_x;
        out.height = (ascent - descent).max(size);
        out
    }

    #[cfg(test)]
    pub(crate) fn debug_baseline_spread(&self, text: &str, size: f32, bold: bool) -> f32 {
        let l = Self::layout(text, size, bold, &self.faces);
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for g in &l.glyphs {
            lo = lo.min(g.y);
            hi = hi.max(g.y);
        }
        hi - lo
    }

    #[cfg(test)]
    pub(crate) fn debug_raster_count(&self) -> usize {
        self.atlas.rasters.len()
    }
}

/// RTL runs prefer the Arabic face, LTR runs the Latin one; every chain ends
/// in the Latin face so text is never dropped when a font is missing.
fn pick_face(rtl: bool, bold: bool, faces: &[Option<Face>; 4]) -> Option<(u8, &Face)> {
    let chain: &[u8] = if rtl {
        if bold {
            &[F_ARABIC_BOLD, F_ARABIC, F_LATIN_BOLD, F_LATIN]
        } else {
            &[F_ARABIC, F_LATIN]
        }
    } else if bold {
        &[F_LATIN_BOLD, F_LATIN]
    } else {
        &[F_LATIN]
    };
    chain
        .iter()
        .find_map(|&id| faces[id as usize].as_ref().map(|f| (id, f)))
}

// ---------------------------------------------------------------------------
// Latin fast-path helpers (egui text, no shaping)
// ---------------------------------------------------------------------------

/// Binary-search truncation of a Latin string to `max_w` px.
pub fn fit_egui(
    p: &eframe::egui::Painter,
    text: &str,
    font: &FontId,
    max_w: f32,
) -> (String, bool) {
    if max_w <= 4.0 {
        return ("…".into(), true);
    }
    let measure = |s: &str| -> f32 {
        p.fonts(|f| f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE))
            .size()
            .x
    };
    if measure(text) <= max_w {
        return (text.to_string(), false);
    }
    let chars: Vec<char> = text.chars().collect();
    let mut lo = 0usize;
    let mut hi = chars.len();
    let mut best = 0usize;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let head: String = chars[..mid].iter().collect();
        let cand = format!("{head}…");
        if measure(&cand) <= max_w {
            best = mid;
            lo = mid + 1;
        } else if mid == 0 {
            break;
        } else {
            hi = mid - 1;
        }
    }
    let head: String = chars[..best].iter().collect();
    (format!("{head}…"), true)
}

/// Character-count truncation with an ellipsis.
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: TextStyle = TextStyle::new(14.0, false);
    const SB: TextStyle = TextStyle::new(14.0, true);

    #[test]
    fn latin_does_not_need_shaping() {
        assert!(!needs("Hello world 123"));
    }

    #[test]
    fn arabic_needs_shaping() {
        assert!(needs("مرحبا"));
    }

    #[test]
    fn shapes_and_measures_arabic() {
        let p = TextPipeline::new();
        let w = p.measure("مرحبا بالعالم", S);
        assert!(w > 10.0, "arabic must measure to a real width, got {w}");
    }

    #[test]
    fn fit_truncates_arabic() {
        let p = TextPipeline::new();
        let long = "مرحبا بالعالم هذا نص طويل جدا للاختبار";
        let (s, truncated) = p.fit(long, S, 80.0);
        assert!(truncated);
        assert!(s.ends_with('…'));
        assert!(p.measure(&s, S) <= 81.0);
    }

    #[test]
    fn mixed_ltr_rtl_measures() {
        let p = TextPipeline::new();
        let w = p.measure("Title مرحبا AMV", SB);
        assert!(w > 20.0);
    }

    #[test]
    fn mixed_script_shares_one_baseline() {
        let p = TextPipeline::new();
        // regression: a single-pass layout put "Title " on a shorter ascent
        // than the Arabic run, splitting the line across two baselines
        let spread = p.debug_baseline_spread("Title مرحبا AMV", 14.0, true);
        assert!(
            spread < 2.0,
            "mixed-script baseline spread too large: {spread}"
        );
    }

    #[test]
    fn height_covers_ascent() {
        let p = TextPipeline::new();
        let mixed = TextPipeline::layout("Title مرحبا", 14.0, true, &p.faces).height;
        assert!(mixed >= 14.0, "height must cover the em box, got {mixed}");
    }

    #[test]
    fn rasters_are_cached_per_glyph() {
        let mut p = TextPipeline::new();
        let layout = TextPipeline::layout("مرحبا مرحبا", 14.0, false, &p.faces);
        p.ensure_rasters(&layout, 14.0);
        let first = p.debug_raster_count();
        // laying out and rasterizing the same glyphs again must not grow the atlas
        let again = TextPipeline::layout("مرحبا", 14.0, false, &p.faces);
        p.ensure_rasters(&again, 14.0);
        assert_eq!(p.debug_raster_count(), first, "glyphs must be reused");
        assert!(first > 0, "expected some rasters");
    }

    #[test]
    fn truncate_helpers() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello world", 6), "hello…");
        assert_eq!(truncate("مرحبا بالعالم", 5), "مرحب…");
    }

    #[test]
    fn empty_and_missing_glyphs_are_safe() {
        let p = TextPipeline::new();
        assert_eq!(p.measure("", S), 0.0);
        assert_eq!(p.measure("", SB), 0.0);
        let _ = p.debug_baseline_spread("", 14.0, false);
    }
}
