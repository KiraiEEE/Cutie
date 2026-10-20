use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Color32, ColorImage, CornerRadius, CursorIcon, FontFamily, FontId, Frame, Key, Pos2,
    Rect, Sense, Stroke, TextureHandle, TextureOptions, Vec2,
};

use crate::dl::{fmt_bytes, fmt_speed, short_folder, DlHandle, Ev};
use crate::text::{self, TextPipeline, TextStyle};
use crate::theme::{
    accent_a, accent_b, alpha8, alpha_c, bg, border, dim_alpha, dim_c, err_c, gradient_bar,
    hover_bg, ok_c, pop_in, pop_out, text_c, to_clear_color, track, BOLD_FONT, OUT_DUR, POP_DUR,
};

const W: f32 = 420.0;
const H: f32 = 132.0;
const PAD: f32 = 16.0;
const THUMB_W: f32 = 96.0;
const THUMB_H: f32 = 54.0;
const TITLE: TextStyle = TextStyle::new(14.0, true);
const META: TextStyle = TextStyle::new(11.0, false);
const TOOLTIP: TextStyle = TextStyle::new(11.5, false);
/// Speed/ETA text is rebuilt at most this often (it feeds the glyph atlas).
const META_THROTTLE: Duration = Duration::from_millis(250);

pub enum Phase {
    Fetching,
    Downloading,
    Done,
    Error(String),
}

/// Per-frame animation state of the whole card.
struct Anim {
    alpha: f32,
    scale: f32,
    slide: f32,
    closing: bool,
}

/// Geometry + paint state shared by every `draw_*` helper.
#[derive(Clone, Copy)]
struct Card {
    rect: Rect,
    a8: u8,
    left: f32,
    right: f32,
    text_x: f32,
    time: f32,
    closing: bool,
}

impl Card {
    fn corner() -> CornerRadius {
        CornerRadius::same(0)
    }
}

pub struct CutieApp {
    rx: Receiver<Ev>,
    handle: DlHandle,
    dir: PathBuf,
    phase: Phase,
    title: String,
    channel: String,
    duration: String,
    size_hint: Option<u64>,
    downloaded: u64,
    total: u64,
    speed_ema: f64,
    eta: String,
    progress: f32,
    shown: f32,
    done_path: String,
    born: Instant,
    close_at: Option<Instant>,
    closing_at: Option<Instant>,
    instant_close: bool,
    dragging: bool,
    win_pos: Pos2,
    thumb: Option<TextureHandle>,
    pipe: TextPipeline,
    /// cached fit result: (source, max_w) → (shown, truncated)
    fit_cache: Option<(String, f32, (String, bool))>,
    meta_cache: String,
    meta_built_at: Instant,
    first_frame_logged: bool,
}

impl CutieApp {
    pub fn new(
        rx: Receiver<Ev>,
        handle: DlHandle,
        dir: PathBuf,
        start_pos: Pos2,
        pipe: TextPipeline,
    ) -> Self {
        Self {
            rx,
            handle,
            dir,
            phase: Phase::Fetching,
            title: String::from("Starting download…"),
            channel: String::new(),
            duration: String::new(),
            size_hint: None,
            downloaded: 0,
            total: 0,
            speed_ema: 0.0,
            eta: String::new(),
            progress: 0.0,
            shown: 0.0,
            done_path: String::new(),
            born: Instant::now(),
            close_at: None,
            closing_at: None,
            instant_close: false,
            dragging: false,
            win_pos: start_pos,
            thumb: None,
            pipe,
            fit_cache: None,
            meta_cache: String::new(),
            meta_built_at: Instant::now() - META_THROTTLE,
            first_frame_logged: false,
        }
    }

    // ---------------- state ----------------

    fn drain(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Ev::Meta {
                    title,
                    channel,
                    duration,
                    size_hint,
                } => {
                    if !matches!(self.phase, Phase::Error(_)) {
                        self.title = title;
                        self.channel = channel;
                        self.duration = duration;
                        self.size_hint = size_hint;
                        self.fit_cache = None;
                        if matches!(self.phase, Phase::Fetching) {
                            self.phase = Phase::Downloading;
                        }
                    }
                }
                Ev::Thumbnail { rgba, w, h } => {
                    let img = ColorImage::from_rgba_unmultiplied([w as usize, h as usize], &rgba);
                    self.thumb = Some(ctx.load_texture("thumb", img, TextureOptions::default()));
                }
                Ev::Progress {
                    downloaded,
                    total,
                    speed,
                    eta,
                } => {
                    self.downloaded = downloaded;
                    self.total = total;
                    self.eta = eta;
                    if speed > 0.0 {
                        self.speed_ema = if self.speed_ema <= 0.0 {
                            speed
                        } else {
                            self.speed_ema * 0.72 + speed * 0.28
                        };
                    }
                    if total > 0 {
                        self.progress = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
                    }
                    if matches!(self.phase, Phase::Fetching | Phase::Downloading) {
                        self.phase = Phase::Downloading;
                    }
                }
                Ev::Done { path } => {
                    if !path.is_empty() {
                        self.done_path = path;
                    }
                    if !matches!(self.phase, Phase::Error(_)) {
                        if self.title == "Starting download…" {
                            self.title = "Saved".into();
                            self.fit_cache = None;
                        }
                        self.phase = Phase::Done;
                        self.progress = 1.0;
                        self.close_at = Some(Instant::now() + Duration::from_secs(5));
                    }
                }
                Ev::Failed { msg } => {
                    if !matches!(self.phase, Phase::Done) {
                        self.title = "Download failed".into();
                        self.fit_cache = None;
                        self.phase = Phase::Error(msg);
                        self.close_at = Some(Instant::now() + Duration::from_secs(7));
                    }
                }
            }
        }
    }

    fn start_closing(&mut self, cancel: bool) {
        if self.closing_at.is_some() || self.instant_close {
            return;
        }
        if cancel {
            self.handle.cancel();
            self.instant_close = true;
        } else {
            self.closing_at = Some(Instant::now());
        }
    }

    // ---------------- text plumbing ----------------

    /// Draw text bimodally: complex scripts → OpenType pipeline, Latin → egui.
    fn draw_txt(
        &mut self,
        p: &egui::Painter,
        text: &str,
        pos: Pos2,
        align: egui::Align2,
        style: TextStyle,
        color: Color32,
    ) {
        if text::needs(text) {
            self.pipe.paint(p, text, pos, align, style, color);
        } else {
            p.text(pos, align, text, font_id(style), color);
        }
    }

    fn measure(&self, text: &str, style: TextStyle, p: &egui::Painter) -> f32 {
        if text::needs(text) {
            self.pipe.measure(text, style)
        } else {
            p.fonts(|f| {
                f.layout_no_wrap(text.to_string(), font_id(style), Color32::WHITE)
                    .size()
                    .x
            })
        }
    }

    fn fit(
        &mut self,
        text: &str,
        style: TextStyle,
        max_w: f32,
        p: &egui::Painter,
    ) -> (String, bool) {
        if text::needs(text) {
            self.pipe.fit(text, style, max_w)
        } else {
            text::fit_egui(p, text, &font_id(style), max_w)
        }
    }

    /// The channel/duration/speed/ETA strip. Rebuilt at most every
    /// [`META_THROTTLE`] so the shaping cache isn't invalidated per frame.
    fn meta_line(&mut self) -> String {
        let now = Instant::now();
        if self.meta_cache.is_empty() || now.duration_since(self.meta_built_at) >= META_THROTTLE {
            self.meta_built_at = now;
            self.meta_cache = match &self.phase {
                Phase::Fetching => "fetching info…".to_string(),
                Phase::Downloading => {
                    let mut parts: Vec<String> = Vec::new();
                    if !self.channel.is_empty() {
                        parts.push(self.channel.clone());
                    }
                    if !self.duration.is_empty() {
                        parts.push(self.duration.clone());
                    }
                    if self.speed_ema > 0.0 {
                        parts.push(fmt_speed(self.speed_ema));
                    }
                    if !self.eta.is_empty() {
                        parts.push(format!("ETA {}", self.eta));
                    }
                    if parts.is_empty() {
                        "starting…".into()
                    } else {
                        parts.join(" · ")
                    }
                }
                Phase::Done => PathBuf::from(&self.done_path)
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "saved".into()),
                Phase::Error(e) => e.clone(),
            };
        }
        self.meta_cache.clone()
    }
}

impl eframe::App for CutieApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        to_clear_color(bg())
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            crate::dl::log_line(
                "ui",
                &format!("first_frame_ms={}", self.born.elapsed().as_millis()),
            );
        }
        self.drain(ctx);

        if self.instant_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        let dt = ctx.input(|i| i.stable_dt).min(0.05);
        let time = self.born.elapsed().as_secs_f32();

        let (mut alpha, mut scale, mut slide) = pop_in(time / POP_DUR);
        let mut closing = false;
        if let Some(ca) = self.closing_at {
            let q = ca.elapsed().as_secs_f32() / OUT_DUR;
            if q >= 1.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            let (a, s, d) = pop_out(q);
            alpha *= a;
            scale *= s;
            slide += d;
            closing = true;
        }

        self.shown += (self.progress - self.shown) * (1.0 - (-14.0 * dt).exp());

        if self.closing_at.is_none() {
            if ctx.input(|i| i.key_pressed(Key::Escape)) {
                self.start_closing(true);
            } else if self.close_at.is_some_and(|t| Instant::now() >= t) {
                self.start_closing(false);
            }
        }

        let anim = Anim {
            alpha,
            scale,
            slide,
            closing: closing || self.instant_close,
        };

        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui| self.draw(ui, ctx, &anim, time));

        // repaint governor: 16ms for pop animations, 33ms during download, 100ms idle
        let ms = if anim.closing || time < POP_DUR {
            16
        } else if matches!(self.phase, Phase::Fetching | Phase::Downloading) {
            33
        } else {
            100
        };
        ctx.request_repaint_after(Duration::from_millis(ms));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // no-op once the download finished; otherwise stop yt-dlp + ffmpeg
        if self.handle.is_running() {
            crate::dl::log_line(
                "ui",
                "exited while a download was still running — cancelling",
            );
            self.handle.cancel();
        }
    }
}

// ---------------- drawing ----------------

impl CutieApp {
    fn draw(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, anim: &Anim, time: f32) {
        let full = ui.available_rect_before_wrap();
        let rect = Rect::from_center_size(
            full.center() + Vec2::new(0.0, anim.slide),
            Vec2::new(W, H) * anim.scale,
        );
        let resp = ui.allocate_rect(rect, Sense::click_and_drag());

        // drag to reposition
        if resp.drag_started() {
            self.dragging = true;
        }
        if resp.dragged() && self.dragging {
            let d = resp.drag_delta();
            self.win_pos.x += d.x;
            self.win_pos.y += d.y;
            ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(self.win_pos));
        }
        if resp.drag_stopped() {
            self.dragging = false;
        }
        if !anim.closing && resp.hovered() {
            ctx.set_cursor_icon(if self.dragging {
                CursorIcon::Grabbing
            } else {
                CursorIcon::Grab
            });
        }

        let card = Card {
            rect,
            a8: alpha8(anim.alpha),
            left: rect.left() + PAD,
            right: rect.right() - PAD,
            text_x: rect.left() + PAD + THUMB_W + 14.0,
            time,
            closing: anim.closing,
        };

        let p = ui.painter();
        let zero = Card::corner();
        p.rect_filled(rect, zero, alpha_c(bg(), card.a8));
        p.rect_stroke(
            rect,
            zero,
            Stroke::new(1.0_f32, alpha_c(border(), card.a8)),
            egui::StrokeKind::Inside,
        );
        gradient_bar(
            p,
            Rect::from_min_size(
                Pos2::new(rect.left(), rect.top()),
                Vec2::new(rect.width(), 3.0),
            ),
            zero,
            card.a8,
        );

        self.draw_thumb(p, card);
        self.draw_close(ctx, ui, p, card);
        self.draw_title(ui, p, card);
        self.draw_meta(p, card);
        self.draw_bar(p, card);
        self.draw_footer(ui, ctx, p, card);
    }

    fn draw_thumb(&self, p: &egui::Painter, c: Card) {
        let thumb_rect = Rect::from_min_size(
            Pos2::new(c.left, c.rect.top() + 16.0),
            Vec2::new(THUMB_W, THUMB_H),
        );
        if let Some(tex) = &self.thumb {
            p.image(
                tex.id(),
                thumb_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                alpha_c(Color32::WHITE, c.a8),
            );
        } else {
            p.rect_filled(
                thumb_rect,
                Card::corner(),
                alpha_c(Color32::from_rgb(38, 38, 46), c.a8),
            );
        }
        p.rect_stroke(
            thumb_rect,
            Card::corner(),
            Stroke::new(1.0_f32, alpha_c(Color32::from_rgb(50, 50, 60), c.a8)),
            egui::StrokeKind::Inside,
        );
    }

    fn draw_close(&mut self, ctx: &egui::Context, ui: &egui::Ui, p: &egui::Painter, c: Card) {
        let close_r = Rect::from_min_size(
            Pos2::new(c.rect.right() - 32.0, c.rect.top() + 6.0),
            Vec2::splat(26.0),
        );
        let hover = !c.closing && ui.rect_contains_pointer(close_r);
        if hover {
            p.rect_filled(close_r, Card::corner(), alpha_c(hover_bg(), c.a8));
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        p.text(
            close_r.center(),
            egui::Align2::CENTER_CENTER,
            "✕",
            FontId::new(12.0, FontFamily::Proportional),
            alpha_c(if hover { text_c() } else { dim_c() }, c.a8),
        );
        if ui
            .interact(close_r, ui.id().with("close"), Sense::click())
            .clicked()
            && !c.closing
        {
            self.start_closing(true);
        }
    }

    fn draw_title(&mut self, ui: &egui::Ui, p: &egui::Painter, c: Card) {
        // status dot
        let dot_pos = Pos2::new(c.text_x + 4.0, c.rect.top() + 24.0);
        let dot = match &self.phase {
            Phase::Fetching => {
                let t = ((c.time * 3.0).sin() * 0.5 + 0.5) * 255.0;
                Color32::from_rgba_unmultiplied(
                    accent_a().r(),
                    accent_a().g(),
                    accent_b().b(),
                    t as u8,
                )
            }
            Phase::Downloading => {
                let t = 215.0 + (c.time * 4.0).sin() * 40.0;
                Color32::from_rgba_unmultiplied(
                    accent_b().r(),
                    accent_b().g(),
                    accent_b().b(),
                    t.clamp(0.0, 255.0) as u8,
                )
            }
            Phase::Done => ok_c(),
            Phase::Error(_) => err_c(),
        };
        p.circle_filled(dot_pos, 3.5, alpha_c(dot, c.a8));

        // fitted title
        let max_w = (c.right - 40.0) - (c.text_x + 12.0);
        let color = alpha_c(
            if matches!(self.phase, Phase::Error(_)) {
                err_c()
            } else {
                text_c()
            },
            c.a8,
        );
        let cache_ok = self
            .fit_cache
            .as_ref()
            .is_some_and(|(t, w, _)| t == &self.title && (*w - max_w).abs() <= 0.5);
        let (shown, truncated) = if cache_ok {
            self.fit_cache.as_ref().unwrap().2.clone()
        } else {
            let title = self.title.clone();
            let r = self.fit(&title, TITLE, max_w, p);
            self.fit_cache = Some((self.title.clone(), max_w, r.clone()));
            r
        };

        let title_clip = Rect::from_min_max(
            Pos2::new(c.text_x + 8.0, c.rect.top() + 2.0),
            Pos2::new(c.right - 38.0, c.rect.top() + 32.0),
        );
        let clipped = p.with_clip_rect(title_clip);
        self.draw_txt(
            &clipped,
            &shown,
            Pos2::new(c.text_x + 12.0, c.rect.top() + 10.0),
            egui::Align2::LEFT_TOP,
            TITLE,
            color,
        );

        // tooltip with the full title when truncated
        let title_area = Rect::from_min_max(
            Pos2::new(c.text_x + 10.0, c.rect.top() + 6.0),
            Pos2::new(c.right - 40.0, c.rect.top() + 30.0),
        );
        if truncated && !c.closing && ui.rect_contains_pointer(title_area) {
            let full = self.title.clone();
            let tw = (self.measure(&full, TOOLTIP, p) + 18.0).clamp(80.0, W - 24.0);
            let tip = Rect::from_min_size(
                Pos2::new(c.text_x + 8.0, c.rect.top() + 52.0),
                Vec2::new(tw, 22.0),
            );
            p.rect_filled(tip, Card::corner(), alpha_c(hover_bg(), c.a8));
            p.rect_stroke(
                tip,
                Card::corner(),
                Stroke::new(1.0_f32, alpha_c(border(), c.a8)),
                egui::StrokeKind::Inside,
            );
            let tip_clip =
                Rect::from_min_max(tip.min + Vec2::new(8.0, 2.0), tip.max - Vec2::new(8.0, 2.0));
            let clipped = p.with_clip_rect(tip_clip);
            self.draw_txt(
                &clipped,
                &full,
                Pos2::new(tip.left() + 8.0, tip.center().y - 7.0),
                egui::Align2::LEFT_TOP,
                TOOLTIP,
                alpha_c(text_c(), c.a8),
            );
        }
    }

    fn draw_meta(&mut self, p: &egui::Painter, c: Card) {
        let meta = self.meta_line();
        let color = match &self.phase {
            Phase::Error(_) => err_c(),
            Phase::Done => ok_c(),
            _ => dim_c(),
        };
        let meta_clip = Rect::from_min_max(
            Pos2::new(c.text_x + 8.0, c.rect.top() + 30.0),
            Pos2::new(c.right - 4.0, c.rect.top() + 54.0),
        );
        let clipped = p.with_clip_rect(meta_clip);
        self.draw_txt(
            &clipped,
            &text::truncate(&meta, 52),
            Pos2::new(c.text_x + 12.0, c.rect.top() + 34.0),
            egui::Align2::LEFT_TOP,
            META,
            alpha_c(color, c.a8),
        );
    }

    fn draw_bar(&mut self, p: &egui::Painter, c: Card) {
        let bar = Rect::from_min_size(
            Pos2::new(c.left, c.rect.bottom() - 38.0),
            Vec2::new((c.right - c.left - 48.0).max(80.0), 6.0),
        );
        p.rect_filled(bar, Card::corner(), alpha_c(track(), c.a8));

        let frac = match &self.phase {
            Phase::Done => 1.0,
            Phase::Error(_) => 0.0,
            _ => self.shown,
        };
        let indeterminate = matches!(self.phase, Phase::Fetching)
            || (matches!(self.phase, Phase::Downloading) && self.total == 0);

        if indeterminate {
            let w = bar.width() * 0.3;
            let pos = ((c.time * 1.4) % 1.0) * (bar.width() + w) - w;
            let x0 = (bar.left() + pos).max(bar.left());
            let x1 = (x0 + w).min(bar.right());
            if x1 > x0 {
                gradient_bar(
                    p,
                    Rect::from_min_max(Pos2::new(x0, bar.top()), Pos2::new(x1, bar.bottom())),
                    Card::corner(),
                    c.a8,
                );
            }
        } else if frac > 0.001 {
            let fw = bar.width() * frac;
            gradient_bar(
                p,
                Rect::from_min_max(bar.min, Pos2::new(bar.left() + fw, bar.bottom())),
                Card::corner(),
                c.a8,
            );
            if matches!(self.phase, Phase::Downloading) {
                // travelling sheen
                let sh_w = 18.0f32.min(fw * 0.4);
                let ph = (c.time * 1.7) % 1.0;
                let sx = bar.left() + ph * (fw + sh_w * 2.0) - sh_w;
                let sx0 = sx.max(bar.left());
                let sx1 = (sx + sh_w).min(bar.left() + fw);
                if sx1 > sx0 {
                    p.rect_filled(
                        Rect::from_min_max(
                            Pos2::new(sx0, bar.top() + 1.0),
                            Pos2::new(sx1, bar.bottom() - 1.0),
                        ),
                        Card::corner(),
                        Color32::from_rgba_unmultiplied(
                            255,
                            255,
                            255,
                            (55.0 * f32::from(c.a8) / 255.0) as u8,
                        ),
                    );
                }
            }
        }

        // percent readout
        let pct = match &self.phase {
            Phase::Done => "100%".to_string(),
            Phase::Downloading if self.total > 0 => format!("{}%", (frac * 100.0).round()),
            _ => String::new(),
        };
        if !pct.is_empty() {
            p.text(
                Pos2::new(bar.right() + 8.0, bar.top() - 3.0),
                egui::Align2::LEFT_TOP,
                pct,
                FontId::new(11.0, FontFamily::Proportional),
                alpha_c(dim_c(), c.a8),
            );
        }
    }

    fn draw_footer(&mut self, ui: &egui::Ui, ctx: &egui::Context, p: &egui::Painter, c: Card) {
        let bottom = match &self.phase {
            Phase::Done => short_folder(&self.dir),
            Phase::Downloading => {
                if self.total > 0 {
                    format!("{} / {}", fmt_bytes(self.downloaded), fmt_bytes(self.total))
                } else if let Some(sh) = self.size_hint {
                    format!("est {}", fmt_bytes(sh))
                } else {
                    short_folder(&self.dir)
                }
            }
            _ => String::new(),
        };
        if !bottom.is_empty() {
            let clip_x = if matches!(self.phase, Phase::Done) {
                c.right - 96.0
            } else {
                c.right
            };
            let bot_clip = Rect::from_min_max(
                Pos2::new(c.left - 2.0, c.rect.bottom() - 28.0),
                Pos2::new(clip_x, c.rect.bottom() - 4.0),
            );
            let clipped = p.with_clip_rect(bot_clip);
            self.draw_txt(
                &clipped,
                &text::truncate(&bottom, 52),
                Pos2::new(c.left, c.rect.bottom() - 24.0),
                egui::Align2::LEFT_TOP,
                TextStyle::new(10.0, false),
                alpha_c(dim_c(), dim_alpha(c.a8)),
            );
        }

        if !matches!(self.phase, Phase::Done) || c.closing {
            return;
        }
        // "Open folder" is only interesting once there's a file to show
        let btn = Rect::from_min_size(
            Pos2::new(c.right - 90.0, c.rect.bottom() - 30.0),
            Vec2::new(90.0, 22.0),
        );
        let hover = ui.rect_contains_pointer(btn);
        if hover {
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        p.rect_filled(
            btn,
            Card::corner(),
            alpha_c(if hover { hover_bg() } else { track() }, c.a8),
        );
        p.text(
            btn.center(),
            egui::Align2::CENTER_CENTER,
            "Open folder",
            FontId::new(10.5, FontFamily::Proportional),
            alpha_c(text_c(), c.a8),
        );
        if ui
            .interact(btn, ui.id().with("open_folder"), Sense::click())
            .clicked()
        {
            let _ = std::process::Command::new("explorer")
                .arg(&self.dir)
                .spawn();
            self.start_closing(false);
        }
    }
}

fn font_id(style: TextStyle) -> FontId {
    if style.bold {
        FontId::new(style.size, FontFamily::Name(BOLD_FONT.into()))
    } else {
        FontId::new(style.size, FontFamily::Proportional)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_id_picks_family() {
        assert_eq!(
            font_id(TextStyle::new(14.0, false)).family,
            FontFamily::Proportional
        );
        assert_eq!(font_id(TITLE).family, FontFamily::Name(BOLD_FONT.into()));
    }

    #[test]
    fn meta_throttle_is_reasonable() {
        assert!(META_THROTTLE.as_millis() >= 100);
        assert!(META_THROTTLE.as_millis() <= 1000);
    }
}
