//! Small auto-dismissing error toast (no clipboard URL, no yt-dlp, …).

use std::time::Instant;

use eframe::egui::{self, CornerRadius, Frame, Pos2, Rect, Vec2};

use crate::text::{self, TextPipeline, TextStyle};

const MSG_STYLE: TextStyle = TextStyle::new(13.0, false);
use crate::theme::{
    alpha8, alpha_c, bg, border, gradient_bar, pop_in, pop_out, text_c, to_clear_color, OUT_DUR,
    POP_DUR,
};

const W: f32 = 380.0;
const H: f32 = 80.0;
/// Dismiss on its own after this long.
const AUTO_CLOSE: f32 = 2.4;

pub struct ErrApp {
    msg: String,
    born: Instant,
    closing_at: Option<Instant>,
    pipe: TextPipeline,
}

impl ErrApp {
    pub fn new(msg: String, pipe: TextPipeline) -> Self {
        Self {
            msg,
            born: Instant::now(),
            closing_at: None,
            pipe,
        }
    }
}

impl eframe::App for ErrApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        to_clear_color(bg())
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let elapsed = self.born.elapsed().as_secs_f32();

        let (mut alpha, mut scale, mut slide) = pop_in(elapsed / POP_DUR);
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
        } else if elapsed > AUTO_CLOSE || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.closing_at = Some(Instant::now());
        }

        let a8 = alpha8(alpha);
        let zero = CornerRadius::same(0);

        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui| {
                let size = Vec2::new(W, H) * scale;
                let rect = Rect::from_center_size(
                    ui.available_rect_before_wrap().center() + Vec2::new(0.0, slide),
                    size,
                );
                ui.allocate_rect(rect, egui::Sense::hover());

                let p = ui.painter();
                p.rect_filled(rect, zero, alpha_c(bg(), a8));
                p.rect_stroke(
                    rect,
                    zero,
                    egui::Stroke::new(1.0_f32, alpha_c(border(), a8)),
                    egui::StrokeKind::Inside,
                );
                gradient_bar(
                    p,
                    Rect::from_min_size(
                        Pos2::new(rect.left(), rect.top()),
                        Vec2::new(rect.width(), 3.0),
                    ),
                    zero,
                    a8,
                );

                let clip = Rect::from_min_max(
                    rect.min + Vec2::new(14.0, 8.0),
                    rect.max - Vec2::new(14.0, 8.0),
                );
                let clipped = p.with_clip_rect(clip);
                let color = alpha_c(text_c(), a8);
                let pos = rect.center();
                if text::needs(&self.msg) {
                    self.pipe.paint(
                        &clipped,
                        &self.msg,
                        pos,
                        egui::Align2::CENTER_CENTER,
                        MSG_STYLE,
                        color,
                    );
                } else {
                    clipped.text(
                        pos,
                        egui::Align2::CENTER_CENTER,
                        &self.msg,
                        egui::FontId::new(13.0, egui::FontFamily::Proportional),
                        color,
                    );
                }
            });

        let animating = self.closing_at.is_some() || elapsed < POP_DUR;
        ctx.request_repaint_after(std::time::Duration::from_millis(if animating {
            16
        } else {
            100
        }));
    }
}
