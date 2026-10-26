use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, text::LayoutJob, Color32, ColorImage, CornerRadius, CursorIcon, FontFamily, FontId,
    Frame, Key, Pos2, Rect, Sense, Shape, Stroke, TextureHandle, TextureOptions, Vec2,
};

use crate::dl::{self, fmt_bytes, fmt_bytes_pair, fmt_speed, short_folder, DlHandle, Ev};
use crate::text::{self, TextPipeline, TextStyle};
use crate::theme::{
    accent_a, accent_b, alpha8, alpha_c, bg, border, dim_c, err_c, gradient_bar, hover_bg,
    lerp_color, ok_c, pop_in, pop_out, text_c, to_clear_color, track, BOLD_FONT, OUT_DUR, POP_DUR,
};
use crate::win;

/// Window size (the popup is exactly one card).
pub const W: f32 = 420.0;
pub const H: f32 = 104.0;
const PAD: f32 = 14.0;
const THUMB_W: f32 = 112.0;
const THUMB_H: f32 = 63.0;
/// Left edge of the text column, right of the thumbnail.
const TX: f32 = PAD + THUMB_W + 14.0;
/// Right edge of the content.
const RX: f32 = W - PAD;
// row tops inside the text column
const TITLE_Y: f32 = 12.0;
const META_Y: f32 = 35.0;
const BAR_Y: f32 = 58.0;
const BAR_H: f32 = 5.0;
const FOOT_Y: f32 = 70.0;
const BTN_H: f32 = 22.0;

const TITLE: TextStyle = TextStyle::new(14.0, true);
const META: TextStyle = TextStyle::new(11.5, false);
const FOOT: TextStyle = TextStyle::new(11.0, false);
const TOOLTIP: TextStyle = TextStyle::new(11.5, false);
/// Speed/ETA text is rebuilt at most this often so the numbers stay readable.
const META_THROTTLE: Duration = Duration::from_millis(250);
const FETCHING_TITLE: &str = "Fetching video info…";

pub enum Phase {
    Fetching,
    Downloading,
    /// Every byte is in; yt-dlp is merging/converting.
    Processing,
    Done,
    Error(String),
}

/// Paint state shared by every `draw_*` helper.
#[derive(Clone, Copy)]
struct Card {
    rect: Rect,
    a8: u8,
    time: f32,
    closing: bool,
}

impl Card {
    fn corner() -> CornerRadius {
        CornerRadius::same(0)
    }

    /// A point inside the card, in card coordinates.
    fn at(&self, x: f32, y: f32) -> Pos2 {
        self.rect.min + Vec2::new(x, y)
    }

    /// A rect inside the card, in card coordinates.
    fn area(&self, x0: f32, y0: f32, x1: f32, y1: f32) -> Rect {
        Rect::from_min_max(self.at(x0, y0), self.at(x1, y1))
    }

    /// `c` faded with the card.
    fn col(&self, c: Color32) -> Color32 {
        alpha_c(c, self.a8)
    }
}

/// The last fitted version of one text slot, so text that did not change is
/// not measured again every frame.
#[derive(Default)]
struct Fitted {
    src: String,
    max_w: f32,
    out: String,
    truncated: bool,
}

impl Fitted {
    fn get(
        &mut self,
        pipe: &TextPipeline,
        p: &egui::Painter,
        text: &str,
        style: TextStyle,
        max_w: f32,
    ) -> (String, bool) {
        if self.src != text || (self.max_w - max_w).abs() > 0.5 || self.out.is_empty() {
            let (out, truncated) = if text::needs(text) {
                pipe.fit(text, style, max_w)
            } else {
                text::fit_egui(p, text, &font_id(style), max_w)
            };
            self.src = text.to_string();
            self.max_w = max_w;
            self.out = out;
            self.truncated = truncated;
        }
        (self.out.clone(), self.truncated)
    }
}

pub struct CutieApp {
    rx: Receiver<Ev>,
    handle: DlHandle,
    dir: PathBuf,
    opts: dl::Options,
    /// Links that are still waiting to be downloaded.
    queue: VecDeque<String>,
    /// The link that is downloading right now.
    current: String,
    /// 1-based position of the current link.
    item_no: usize,
    /// How many links the whole queue has.
    item_total: usize,
    /// How many links failed so far.
    failed: usize,
    /// (index, count) of the current video inside a playlist.
    playlist: Option<(u64, u64)>,
    phase: Phase,
    title: String,
    channel: String,
    duration: String,
    size_hint: Option<u64>,
    downloaded: u64,
    total: u64,
    speed: f64,
    eta: String,
    progress: f32,
    shown: f32,
    done_path: String,
    /// Size of the finished file, read once when it lands.
    done_size: Option<u64>,
    born: Instant,
    close_at: Option<Instant>,
    closing_at: Option<Instant>,
    instant_close: bool,
    dragging: bool,
    /// Native drag state (Windows only).
    drag: Option<win::DragState>,
    win_pos: Pos2,
    thumb: Option<TextureHandle>,
    pipe: TextPipeline,
    fit_title: Fitted,
    fit_meta: Fitted,
    fit_foot: Fitted,
    stats_cache: String,
    stats_built_at: Instant,
    last_progress: Instant,
    /// The transfer went quiet — stop advertising a stale speed.
    stalled: bool,
    /// When "Copy error" last put the report on the clipboard.
    copied_at: Option<Instant>,
    first_frame_logged: bool,
}

impl CutieApp {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        rx: Receiver<Ev>,
        handle: DlHandle,
        dir: PathBuf,
        opts: dl::Options,
        current: String,
        queue: Vec<String>,
        start_pos: Pos2,
        pipe: TextPipeline,
    ) -> Self {
        let item_total = 1 + queue.len();
        Self {
            rx,
            handle,
            dir,
            opts,
            queue: queue.into(),
            current,
            item_no: 1,
            item_total,
            failed: 0,
            playlist: None,
            phase: Phase::Fetching,
            title: String::from(FETCHING_TITLE),
            channel: String::new(),
            duration: String::new(),
            size_hint: None,
            downloaded: 0,
            total: 0,
            speed: 0.0,
            eta: String::new(),
            progress: 0.0,
            shown: 0.0,
            done_path: String::new(),
            done_size: None,
            born: Instant::now(),
            close_at: None,
            closing_at: None,
            instant_close: false,
            dragging: false,
            drag: None,
            win_pos: start_pos,
            thumb: None,
            pipe,
            fit_title: Fitted::default(),
            fit_meta: Fitted::default(),
            fit_foot: Fitted::default(),
            stats_cache: String::new(),
            stats_built_at: Instant::now() - META_THROTTLE,
            last_progress: Instant::now(),
            stalled: false,
            copied_at: None,
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
                    index,
                    count,
                } => {
                    if !matches!(self.phase, Phase::Error(_)) {
                        self.title = title;
                        self.channel = channel;
                        self.duration = duration;
                        self.size_hint = size_hint;
                        if let (Some(i), Some(c)) = (index, count) {
                            if c > 1 {
                                self.playlist = Some((i, c));
                            }
                        }
                        // a playlist's next video starts after the last one
                        // was processed
                        if matches!(self.phase, Phase::Fetching | Phase::Processing) {
                            self.phase = Phase::Downloading;
                            self.progress = 0.0;
                            self.shown = 0.0;
                        }
                    }
                }
                Ev::Thumbnail { rgba, w, h } => {
                    // ColorImage::from_rgba_unmultiplied asserts on a length
                    // mismatch — never trust a decoded payload blindly.
                    let (w, h) = (w as usize, h as usize);
                    if w == 0 || h == 0 || rgba.len() != w * h * 4 {
                        dl::log_line("ui", "dropped malformed thumbnail event");
                        continue;
                    }
                    let img = ColorImage::from_rgba_unmultiplied([w, h], &rgba);
                    self.thumb = Some(ctx.load_texture("thumb", img, TextureOptions::LINEAR));
                }
                Ev::Progress {
                    downloaded,
                    total,
                    speed,
                    eta,
                } => {
                    self.downloaded = downloaded;
                    self.total = total;
                    self.speed = speed;
                    self.eta = eta;
                    self.stalled = false;
                    self.last_progress = Instant::now();
                    if total > 0 {
                        self.progress = (downloaded as f32 / total as f32).clamp(0.0, 1.0);
                    }
                    if matches!(
                        self.phase,
                        Phase::Fetching | Phase::Downloading | Phase::Processing
                    ) {
                        self.phase = Phase::Downloading;
                    }
                }
                Ev::Processing => {
                    if matches!(self.phase, Phase::Fetching | Phase::Downloading) {
                        self.phase = Phase::Processing;
                        self.progress = 1.0;
                    }
                }
                Ev::Done { path } => {
                    if !path.is_empty() {
                        self.done_size = std::fs::metadata(&path).ok().map(|m| m.len());
                        self.done_path = path;
                    }
                    if matches!(self.phase, Phase::Error(_)) {
                        return;
                    }
                    if self.next_in_queue() {
                        return;
                    }
                    if self.title == FETCHING_TITLE {
                        self.title = "Saved".into();
                    }
                    self.phase = Phase::Done;
                    self.progress = 1.0;
                    let linger = if self.failed > 0 { 8 } else { 5 };
                    self.close_at = Some(Instant::now() + Duration::from_secs(linger));
                }
                Ev::Failed { msg } => {
                    if matches!(self.phase, Phase::Done) {
                        return;
                    }
                    dl::log_line("ui", &format!("failed: {msg}"));
                    self.failed += 1;
                    if self.next_in_queue() {
                        return;
                    }
                    self.title = if self.item_total > 1 {
                        format!("{} of {} downloads failed", self.failed, self.item_total)
                    } else {
                        "Download failed".into()
                    };
                    self.phase = Phase::Error(msg);
                    // errors stay up until the user deals with them
                    self.close_at = None;
                }
            }
        }
    }

    /// Start the next queued link; returns true when there was one.
    fn next_in_queue(&mut self) -> bool {
        let Some(next) = self.queue.pop_front() else {
            return false;
        };
        self.item_no += 1;
        self.launch(next);
        true
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

    /// Start a download for `url` and reset the per-item state.
    fn launch(&mut self, url: String) {
        let (tx, rx) = std::sync::mpsc::channel();
        self.handle = dl::spawn(url.clone(), self.dir.clone(), self.opts, tx);
        self.rx = rx;
        self.current = url;
        self.phase = Phase::Fetching;
        self.title = FETCHING_TITLE.into();
        self.channel.clear();
        self.duration.clear();
        self.size_hint = None;
        self.downloaded = 0;
        self.total = 0;
        self.speed = 0.0;
        self.eta.clear();
        self.progress = 0.0;
        self.shown = 0.0;
        self.playlist = None;
        self.done_path.clear();
        self.done_size = None;
        self.close_at = None;
        self.closing_at = None;
        self.instant_close = false;
        self.thumb = None;
        self.stats_cache.clear();
        self.stats_built_at = Instant::now() - META_THROTTLE;
        self.last_progress = Instant::now();
        self.stalled = false;
        self.copied_at = None;
    }

    /// Restart the failed download with the same URL and folder.
    fn retry(&mut self) {
        // Kill the old worker if any, but keep its .part files — yt-dlp
        // resumes them instead of starting over.
        self.handle.stop();
        // it gets another chance, so it no longer counts as failed
        self.failed = self.failed.saturating_sub(1);
        let url = self.current.clone();
        self.launch(url);
    }

    /// Reveal the finished file (selected in Explorer) or open the folder.
    fn open_folder(&mut self) {
        let file = Path::new(&self.done_path);
        if file.is_file() {
            explorer(format!("/select,\"{}\"", file.display()));
        } else {
            explorer(format!("\"{}\"", self.dir.display()));
        }
        self.start_closing(false);
    }

    /// Open the finished file with its default app.
    fn open_file(&mut self) {
        explorer(format!("\"{}\"", self.done_path));
        self.start_closing(false);
    }

    /// Open the source page in the default browser.
    fn open_source(&self) {
        if self.current.is_empty() {
            return;
        }
        let mut cmd = std::process::Command::new("rundll32");
        cmd.arg("url.dll,FileProtocolHandler")
            .arg(&self.current)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        hide_console(&mut cmd);
        let _ = cmd.spawn();
    }

    /// Put the full error (plus what is needed to reproduce it) on the
    /// clipboard — the popup only has room for two lines.
    fn copy_error(&mut self) {
        let Phase::Error(msg) = &self.phase else {
            return;
        };
        let report = format!(
            "{msg}\nlink: {}\nfolder: {}\ncutie {} · log: {}",
            self.current,
            self.dir.display(),
            env!("CARGO_PKG_VERSION"),
            dl::log_path().display()
        );
        match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(report)) {
            Ok(()) => self.copied_at = Some(Instant::now()),
            Err(e) => dl::log_line("ui", &format!("copy error failed: {e}")),
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

    /// The line under the title: who made it, how long, where in the queue.
    fn meta_line(&self) -> String {
        let mut parts: Vec<String> = Vec::new();
        match &self.phase {
            Phase::Done => {
                parts.push(format!("Saved to {}", short_folder(&self.dir)));
                if self.failed > 0 {
                    parts.push(format!("{} failed", self.failed));
                }
                return parts.join(" · ");
            }
            Phase::Error(_) => return String::new(),
            _ => {}
        }
        if !self.channel.is_empty() {
            parts.push(self.channel.clone());
        }
        if !self.duration.is_empty() {
            parts.push(self.duration.clone());
        }
        if parts.is_empty() {
            parts.push(host_of(&self.current).to_string());
        }
        if let Some(tag) = self.item_tag() {
            parts.push(tag);
        }
        if let Some((i, c)) = self.playlist {
            parts.push(format!("video {i}/{c}"));
        }
        parts.join(" · ")
    }

    /// Bytes, speed and time left. Rebuilt at most every [`META_THROTTLE`].
    fn stats_line(&mut self) -> String {
        let now = Instant::now();
        if self.stats_cache.is_empty() || now.duration_since(self.stats_built_at) >= META_THROTTLE {
            self.stats_built_at = now;
            let mut parts: Vec<String> = Vec::new();
            if self.total > 0 {
                parts.push(fmt_bytes_pair(self.downloaded, self.total));
            } else if self.downloaded > 0 {
                parts.push(fmt_bytes(self.downloaded));
            } else if let Some(sh) = self.size_hint {
                parts.push(format!("about {}", fmt_bytes(sh)));
            }
            if !self.stalled && self.speed > 0.0 {
                parts.push(fmt_speed(self.speed));
            }
            if self.stalled {
                parts.push("waiting for data…".into());
            } else if !self.eta.is_empty() {
                parts.push(format!("{} left", self.eta));
            }
            if parts.is_empty() {
                parts.push("starting…".into());
            }
            self.stats_cache = parts.join(" · ");
        }
        self.stats_cache.clone()
    }

    /// "2/4" when more than one link is queued.
    fn item_tag(&self) -> Option<String> {
        (self.item_total > 1).then(|| format!("{}/{}", self.item_no, self.item_total))
    }

    fn is_active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Fetching | Phase::Downloading | Phase::Processing
        )
    }
}

impl eframe::App for CutieApp {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        to_clear_color(bg())
    }

    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            dl::log_line(
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

        // A stalled stream should not keep advertising its last speed/ETA.
        if matches!(self.phase, Phase::Downloading)
            && !self.stalled
            && self.last_progress.elapsed() >= Duration::from_secs(3)
        {
            self.stalled = true;
            self.stats_cache.clear();
        }

        let mut alpha = pop_in(time / POP_DUR);
        let mut closing = false;
        if let Some(ca) = self.closing_at {
            let q = ca.elapsed().as_secs_f32() / OUT_DUR;
            if q >= 1.0 {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                return;
            }
            alpha *= pop_out(q);
            closing = true;
        }

        self.shown += (self.progress - self.shown) * (1.0 - (-14.0 * dt).exp());

        if self.closing_at.is_none() {
            let (esc, enter) =
                ctx.input(|i| (i.key_pressed(Key::Escape), i.key_pressed(Key::Enter)));
            if esc {
                self.start_closing(true);
            } else if enter && matches!(self.phase, Phase::Done) {
                self.open_folder();
            } else if enter && matches!(self.phase, Phase::Error(_)) {
                self.retry();
            } else if self.close_at.is_some_and(|t| Instant::now() >= t) {
                // Keep the toast around while the user is pointing at it
                // (reaching for Open / Show in folder).
                let hovering = ctx
                    .input(|i| i.pointer.hover_pos())
                    .is_some_and(|p| ctx.screen_rect().contains(p));
                if hovering {
                    self.close_at = Some(Instant::now() + Duration::from_secs(2));
                } else {
                    self.start_closing(false);
                }
            }
        }

        egui::CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui| self.draw(ui, ctx, alpha, closing, time));

        // One atlas upload per frame, not one per text run.
        self.pipe.flush();

        // repaint governor: 16ms for fades, 33ms while working, 100ms idle
        let ms = if closing || time < POP_DUR {
            16
        } else if self.is_active() {
            33
        } else {
            100
        };
        ctx.request_repaint_after(Duration::from_millis(ms));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Set the cancel flag even when yt-dlp has not registered a pid yet
        // (spawn race / immediately after window open), then kill the tree.
        if self.handle.is_running() {
            dl::log_line(
                "ui",
                "exited while a download was still running — cancelling",
            );
        }
        self.handle.cancel();
    }
}

// ---------------- drawing ----------------

impl CutieApp {
    fn draw(
        &mut self,
        ui: &mut egui::Ui,
        ctx: &egui::Context,
        alpha: f32,
        closing: bool,
        time: f32,
    ) {
        let rect = Rect::from_min_size(ui.max_rect().min, Vec2::new(W, H));
        // The whole card is the drag handle; the buttons and the thumbnail
        // are registered later, so they sit on top and keep their clicks.
        let bg_resp = ui.interact(rect, ui.id().with("drag"), Sense::drag());
        if bg_resp.drag_started() {
            self.drag = win::DragState::begin();
            self.dragging = true;
        }
        if bg_resp.dragged() {
            self.drag_window(ctx, bg_resp.drag_delta());
        }
        if bg_resp.drag_stopped() {
            self.dragging = false;
            self.drag = None;
        }
        if self.dragging {
            ctx.set_cursor_icon(CursorIcon::Grabbing);
        }

        let c = Card {
            rect,
            a8: alpha8(alpha),
            time,
            closing,
        };

        let p = ui.painter().clone();
        let zero = Card::corner();
        p.rect_filled(rect, zero, c.col(bg()));
        p.rect_stroke(
            rect,
            zero,
            Stroke::new(1.0_f32, c.col(border())),
            egui::StrokeKind::Inside,
        );
        gradient_bar(&p, c.area(0.0, 0.0, W, 3.0), zero, c.a8);

        self.draw_thumb(ctx, ui, &p, c);
        self.draw_close(ctx, ui, &p, c);
        self.draw_title(&p, c);
        if let Phase::Error(msg) = &self.phase {
            let msg = msg.clone();
            self.draw_error(&p, c, &msg);
        } else {
            self.draw_meta(&p, c);
            self.draw_bar(&p, c);
        }
        self.draw_footer(ui, ctx, &p, c);
        // last, so it covers the rows below the title
        self.draw_title_tooltip(ui, &p, c);
    }

    /// Move the popup.
    ///
    /// The OS path is exact: the window follows the cursor in physical pixels.
    /// The egui path is the fallback for platforms without a native window.
    fn drag_window(&mut self, ctx: &egui::Context, fallback: Vec2) {
        if let Some(drag) = self.drag.as_ref() {
            if drag.follow().is_some() {
                return;
            }
        }
        self.win_pos += fallback;
        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(self.win_pos));
    }

    fn draw_thumb(&mut self, ctx: &egui::Context, ui: &egui::Ui, p: &egui::Painter, c: Card) {
        let top = (H - THUMB_H) / 2.0;
        let thumb_rect = c.area(PAD, top, PAD + THUMB_W, top + THUMB_H);
        if let Some(tex) = &self.thumb {
            p.image(
                tex.id(),
                thumb_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                c.col(Color32::WHITE),
            );
        } else {
            p.rect_filled(
                thumb_rect,
                Card::corner(),
                c.col(Color32::from_rgb(38, 38, 46)),
            );
            // a small drawn "play" mark instead of an empty box
            let m = thumb_rect.center();
            p.add(Shape::convex_polygon(
                vec![
                    Pos2::new(m.x - 6.0, m.y - 8.0),
                    Pos2::new(m.x + 8.0, m.y),
                    Pos2::new(m.x - 6.0, m.y + 8.0),
                ],
                c.col(Color32::from_rgb(66, 66, 80)),
                Stroke::NONE,
            ));
        }
        p.rect_stroke(
            thumb_rect,
            Card::corner(),
            Stroke::new(1.0_f32, c.col(Color32::from_rgb(50, 50, 60))),
            egui::StrokeKind::Inside,
        );

        // clicking the thumbnail opens the source page in the browser
        if !c.closing && self.thumb.is_some() {
            let resp = ui.interact(thumb_rect, ui.id().with("thumb"), Sense::click());
            if resp.hovered() && !self.dragging {
                ctx.set_cursor_icon(CursorIcon::PointingHand);
                p.rect_filled(
                    thumb_rect,
                    Card::corner(),
                    c.col(Color32::from_black_alpha(90)),
                );
                // a little drawn arrow (the "↗" glyph is not always there)
                let col = c.col(text_c());
                let mid = thumb_rect.center();
                let a = 8.0f32;
                let stroke = Stroke::new(1.5_f32, col);
                let tip = Pos2::new(mid.x + a, mid.y - a);
                p.line_segment([Pos2::new(mid.x - a, mid.y + a), tip], stroke);
                p.line_segment([Pos2::new(tip.x - 6.0, tip.y), tip], stroke);
                p.line_segment([Pos2::new(tip.x, tip.y + 6.0), tip], stroke);
            }
            if resp.clicked() {
                self.open_source();
            }
        }
    }

    fn draw_close(&mut self, ctx: &egui::Context, ui: &egui::Ui, p: &egui::Painter, c: Card) {
        let close_r = Rect::from_min_size(c.at(W - 32.0, 7.0), Vec2::splat(24.0));
        let resp = ui.interact(close_r, ui.id().with("close"), Sense::click());
        let hover = !c.closing && resp.hovered();
        if hover {
            p.rect_filled(close_r, Card::corner(), c.col(hover_bg()));
            ctx.set_cursor_icon(CursorIcon::PointingHand);
        }
        // A drawn cross — the "✕" glyph is missing from the Windows fonts and
        // would show up as a little empty box.
        let col = c.col(if hover { text_c() } else { dim_c() });
        let mid = close_r.center();
        let r = 4.0f32;
        let stroke = Stroke::new(1.4_f32, col);
        p.line_segment(
            [
                Pos2::new(mid.x - r, mid.y - r),
                Pos2::new(mid.x + r, mid.y + r),
            ],
            stroke,
        );
        p.line_segment(
            [
                Pos2::new(mid.x - r, mid.y + r),
                Pos2::new(mid.x + r, mid.y - r),
            ],
            stroke,
        );
        if resp.clicked() && !c.closing {
            self.start_closing(true);
        }
    }

    fn draw_title(&mut self, p: &egui::Painter, c: Card) {
        // status dot
        let dot = match &self.phase {
            Phase::Fetching => {
                let t = ((c.time * 3.0).sin() * 0.5 + 0.5) * 255.0;
                alpha_c(accent_a(), t as u8)
            }
            Phase::Downloading | Phase::Processing => {
                let t = 215.0 + (c.time * 4.0).sin() * 40.0;
                alpha_c(accent_b(), t.clamp(0.0, 255.0) as u8)
            }
            Phase::Done => ok_c(),
            Phase::Error(_) => err_c(),
        };
        let dot_a = (u16::from(dot.a()) * u16::from(c.a8) / 255) as u8;
        p.circle_filled(c.at(TX + 3.5, TITLE_Y + 9.5), 3.5, alpha_c(dot, dot_a));

        let x = TX + 12.0;
        let max_w = (W - 40.0) - x;
        let color = c.col(if matches!(self.phase, Phase::Error(_)) {
            err_c()
        } else {
            text_c()
        });
        let (shown, _) = self.fit_title.get(&self.pipe, p, &self.title, TITLE, max_w);
        let clipped = p.with_clip_rect(c.area(x - 2.0, 2.0, W - 38.0, META_Y - 1.0));
        self.draw_txt(
            &clipped,
            &shown,
            c.at(x, TITLE_Y),
            egui::Align2::LEFT_TOP,
            TITLE,
            color,
        );
    }

    /// The full title, while the pointer rests on a truncated one.
    fn draw_title_tooltip(&mut self, ui: &egui::Ui, p: &egui::Painter, c: Card) {
        let title_area = c.area(TX, TITLE_Y - 4.0, W - 40.0, TITLE_Y + 20.0);
        if !self.fit_title.truncated
            || c.closing
            || self.dragging
            || !ui.rect_contains_pointer(title_area)
        {
            return;
        }
        let full = self.title.clone();
        let max_w = RX - TX;
        let tw = (self.measure(&full, TOOLTIP, p) + 16.0).clamp(80.0, max_w);
        let tip = Rect::from_min_size(c.at(TX, META_Y - 3.0), Vec2::new(tw, 22.0));
        p.rect_filled(tip, Card::corner(), c.col(hover_bg()));
        p.rect_stroke(
            tip,
            Card::corner(),
            Stroke::new(1.0_f32, c.col(border())),
            egui::StrokeKind::Inside,
        );
        let (shown, _) = self.fit_once(p, &full, tw - 16.0);
        let clipped = p.with_clip_rect(tip.shrink2(Vec2::new(6.0, 2.0)));
        self.draw_txt(
            &clipped,
            &shown,
            Pos2::new(tip.left() + 8.0, tip.center().y),
            egui::Align2::LEFT_CENTER,
            TOOLTIP,
            c.col(text_c()),
        );
    }

    /// One-off fit that does not disturb the per-slot caches.
    fn fit_once(&self, p: &egui::Painter, text: &str, max_w: f32) -> (String, bool) {
        if text::needs(text) {
            self.pipe.fit(text, TOOLTIP, max_w)
        } else {
            text::fit_egui(p, text, &font_id(TOOLTIP), max_w)
        }
    }

    fn draw_meta(&mut self, p: &egui::Painter, c: Card) {
        let meta = self.meta_line();
        let color = match &self.phase {
            Phase::Done => ok_c(),
            _ => dim_c(),
        };
        let (shown, _) = self.fit_meta.get(&self.pipe, p, &meta, META, RX - TX);
        let clipped = p.with_clip_rect(c.area(TX - 1.0, META_Y - 2.0, RX + 1.0, BAR_Y - 2.0));
        self.draw_txt(
            &clipped,
            &shown,
            c.at(TX, META_Y),
            egui::Align2::LEFT_TOP,
            META,
            c.col(color),
        );
    }

    /// The error text, wrapped over the meta and progress rows.
    fn draw_error(&mut self, p: &egui::Painter, c: Card, msg: &str) {
        let width = RX - TX;
        let clipped = p.with_clip_rect(c.area(TX - 1.0, META_Y - 3.0, RX + 1.0, FOOT_Y - 1.0));
        let color = c.col(lerp_color(err_c(), text_c(), 0.35));
        if text::needs(msg) {
            // the shaping pipeline is single-line
            let (shown, _) = self.fit_meta.get(&self.pipe, p, msg, META, width);
            self.draw_txt(
                &clipped,
                &shown,
                c.at(TX, META_Y),
                egui::Align2::LEFT_TOP,
                META,
                color,
            );
            return;
        }
        let mut job = LayoutJob::simple(msg.to_string(), font_id(META), color, width);
        job.wrap.max_rows = 2;
        job.wrap.break_anywhere = false;
        let galley = p.layout_job(job);
        clipped.galley(c.at(TX, META_Y - 1.0), galley, color);
    }

    fn draw_bar(&mut self, p: &egui::Painter, c: Card) {
        let bar = c.area(TX, BAR_Y, RX, BAR_Y + BAR_H);
        p.rect_filled(bar, Card::corner(), c.col(track()));

        match &self.phase {
            Phase::Done => {
                p.rect_filled(bar, Card::corner(), c.col(ok_c()));
                return;
            }
            Phase::Error(_) => return,
            _ => {}
        }

        let indeterminate = matches!(self.phase, Phase::Fetching)
            || (matches!(self.phase, Phase::Downloading) && self.total == 0);
        if indeterminate {
            let w = bar.width() * 0.3;
            let pos = ((c.time * 0.9) % 1.0) * (bar.width() + w) - w;
            let x0 = (bar.left() + pos).max(bar.left());
            let x1 = (bar.left() + pos + w).min(bar.right());
            if x1 > x0 {
                gradient_bar(
                    p,
                    Rect::from_min_max(Pos2::new(x0, bar.top()), Pos2::new(x1, bar.bottom())),
                    Card::corner(),
                    c.a8,
                );
            }
            return;
        }

        let frac = if matches!(self.phase, Phase::Processing) {
            1.0
        } else {
            self.shown
        };
        if frac <= 0.001 {
            return;
        }
        let fw = bar.width() * frac.min(1.0);
        gradient_bar(
            p,
            Rect::from_min_max(bar.min, Pos2::new(bar.left() + fw, bar.bottom())),
            Card::corner(),
            c.a8,
        );
        // travelling sheen — faster while merging, so it reads as "busy"
        let speed = if matches!(self.phase, Phase::Processing) {
            1.2
        } else {
            0.7
        };
        let sh_w = 28.0f32.min(fw * 0.4);
        let ph = (c.time * speed) % 1.0;
        let sx = bar.left() + ph * (fw + sh_w * 2.0) - sh_w;
        let sx0 = sx.max(bar.left());
        let sx1 = (sx + sh_w).min(bar.left() + fw);
        if sx1 > sx0 {
            p.rect_filled(
                Rect::from_min_max(Pos2::new(sx0, bar.top()), Pos2::new(sx1, bar.bottom())),
                Card::corner(),
                Color32::from_white_alpha((60.0 * f32::from(c.a8) / 255.0) as u8),
            );
        }
    }

    fn draw_footer(&mut self, ui: &egui::Ui, ctx: &egui::Context, p: &egui::Painter, c: Card) {
        let mid_y = FOOT_Y + BTN_H / 2.0;

        // right side: buttons, or the percentage while working
        let mut left_end = RX;
        match &self.phase {
            Phase::Done => {
                let has_file = Path::new(&self.done_path).is_file();
                let labels: &[(&str, bool)] = if has_file {
                    &[("Show in folder", false), ("Play", true)]
                } else {
                    &[("Open folder", true)]
                };
                let (clicked, x) = self.buttons(ui, ctx, p, c, labels);
                left_end = x;
                match (clicked, has_file) {
                    (Some(0), true) => self.open_folder(),
                    (Some(1), true) => self.open_file(),
                    (Some(_), false) => self.open_folder(),
                    _ => {}
                }
            }
            Phase::Error(_) => {
                let (clicked, x) =
                    self.buttons(ui, ctx, p, c, &[("Copy error", false), ("Retry", true)]);
                left_end = x;
                match clicked {
                    Some(0) => self.copy_error(),
                    Some(1) => self.retry(),
                    _ => {}
                }
            }
            Phase::Downloading if self.total > 0 => {
                let pct = format!("{}%", (self.shown * 100.0).floor().min(99.0));
                let r = p.text(
                    c.at(RX, mid_y),
                    egui::Align2::RIGHT_CENTER,
                    pct,
                    font_id(FOOT),
                    c.col(text_c()),
                );
                left_end = r.left() - c.rect.left();
            }
            Phase::Processing => {
                let r = p.text(
                    c.at(RX, mid_y),
                    egui::Align2::RIGHT_CENTER,
                    "100%",
                    font_id(FOOT),
                    c.col(text_c()),
                );
                left_end = r.left() - c.rect.left();
            }
            _ => {}
        }

        // left side: what is going on right now
        let (left, color) = match &self.phase {
            Phase::Fetching => (format!("into {}", short_folder(&self.dir)), dim_c()),
            Phase::Downloading => (self.stats_line(), dim_c()),
            Phase::Processing => (
                if self.opts.audio_only {
                    "Converting to mp3…".to_string()
                } else {
                    "Finishing up…".to_string()
                },
                dim_c(),
            ),
            Phase::Done => (self.done_size.map(fmt_bytes).unwrap_or_default(), dim_c()),
            Phase::Error(_) => {
                if self
                    .copied_at
                    .is_some_and(|t| t.elapsed() < Duration::from_secs(2))
                {
                    ("Copied".to_string(), ok_c())
                } else {
                    (String::new(), dim_c())
                }
            }
        };
        if left.is_empty() {
            return;
        }
        let max_w = (left_end - 10.0 - TX).max(20.0);
        let (shown, _) = self.fit_foot.get(&self.pipe, p, &left, FOOT, max_w);
        let clipped = p.with_clip_rect(c.area(TX - 1.0, FOOT_Y, left_end - 4.0, FOOT_Y + BTN_H));
        self.draw_txt(
            &clipped,
            &shown,
            c.at(TX, mid_y),
            egui::Align2::LEFT_CENTER,
            FOOT,
            c.col(color),
        );
    }

    /// Footer buttons, laid out right to left in reverse order (the last
    /// label sits at the right edge). Returns the index of the clicked one
    /// and the card x where the leftmost button starts.
    fn buttons(
        &self,
        ui: &egui::Ui,
        ctx: &egui::Context,
        p: &egui::Painter,
        c: Card,
        labels: &[(&str, bool)],
    ) -> (Option<usize>, f32) {
        let font = FontId::new(11.0, FontFamily::Proportional);
        let mut x = RX;
        let mut clicked = None;
        for (i, (label, primary)) in labels.iter().enumerate().rev() {
            let tw = p.fonts(|f| {
                f.layout_no_wrap(label.to_string(), font.clone(), Color32::WHITE)
                    .size()
                    .x
            });
            let w = (tw + 22.0).max(56.0);
            let rect = c.area(x - w, FOOT_Y, x, FOOT_Y + BTN_H);
            x -= w + 6.0;
            if c.closing {
                continue;
            }
            let resp = ui.interact(rect, ui.id().with(("btn", *label)), Sense::click());
            let hover = resp.hovered() && !self.dragging;
            if hover {
                ctx.set_cursor_icon(CursorIcon::PointingHand);
            }
            let fill = match (*primary, hover) {
                (true, false) => lerp_color(track(), accent_b(), 0.45),
                (true, true) => lerp_color(track(), accent_b(), 0.65),
                (false, false) => track(),
                (false, true) => lerp_color(track(), text_c(), 0.12),
            };
            p.rect_filled(rect, Card::corner(), c.col(fill));
            p.text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                *label,
                font.clone(),
                c.col(text_c()),
            );
            if resp.clicked() {
                clicked = Some(i);
            }
        }
        (clicked, x + 6.0)
    }
}

fn font_id(style: TextStyle) -> FontId {
    if style.bold {
        FontId::new(style.size, FontFamily::Name(BOLD_FONT.into()))
    } else {
        FontId::new(style.size, FontFamily::Proportional)
    }
}

/// `https://www.youtube.com/watch?v=…` → `youtube.com`
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    host.strip_prefix("www.").unwrap_or(host)
}

/// Run explorer with `arg` passed through untouched. Explorer parses its own
/// command line, so `Command::arg`'s CRT-style `\"` escaping of an embedded
/// quoted path (`/select,"C:\a b.mp4"`) would break it.
fn explorer(arg: String) {
    let mut cmd = std::process::Command::new("explorer");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.raw_arg(arg);
    }
    #[cfg(not(windows))]
    cmd.arg(arg);
    hide_console(&mut cmd);
    let _ = cmd.spawn();
}

/// Windows: don't flash a console window for helper processes (explorer,
/// rundll32). Other platforms do not have the flag.
fn hide_console(cmd: &mut std::process::Command) -> &mut std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
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

    #[test]
    fn host_of_strips_scheme_and_www() {
        assert_eq!(host_of("https://www.youtube.com/watch?v=x"), "youtube.com");
        assert_eq!(host_of("http://youtu.be/abc"), "youtu.be");
        assert_eq!(host_of("https://x.tld?q=1"), "x.tld");
    }

    #[test]
    #[allow(clippy::assertions_on_constants)] // the layout constants are the point
    fn rows_fit_inside_the_card() {
        assert!(TITLE_Y < META_Y && META_Y < BAR_Y && BAR_Y + BAR_H < FOOT_Y);
        assert!(FOOT_Y + BTN_H <= H - 8.0, "footer must not touch the edge");
        assert!(THUMB_H < H && TX < RX);
    }
}
