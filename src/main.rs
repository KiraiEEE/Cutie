// Release builds are GUI apps (no console flashes from Explorer); debug builds
// keep the console so panics and `CUTIE_DEBUG=1` logs are visible.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod dl;
mod errwin;
mod install;
mod text;
mod theme;
mod win;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui::{self, Pos2, Vec2};
use theme::BOLD_FONT;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const POPUP_SIZE: Vec2 = Vec2::new(app::W, app::H);
const ERR_SIZE: Vec2 = Vec2::new(380.0, 80.0);

#[derive(Debug)]
enum Mode {
    /// Normal popup: clipboard (or given) URL, GUI progress card.
    Popup {
        /// Empty means "use the clipboard".
        urls: Vec<String>,
        dir: PathBuf,
        opts: dl::Options,
    },
    /// `--download <url> [dir]`: console progress, exit code 0/1.
    Headless {
        urls: Vec<String>,
        dir: PathBuf,
        opts: dl::Options,
    },
    /// `--download` without any URL to download.
    HeadlessNoUrl,
    Install,
    Uninstall,
    Help,
    Version,
}

#[derive(Debug)]
struct Cli {
    mode: Mode,
    /// Windows GUI subsystem apps only print when a console is attached.
    needs_console: bool,
}

impl Cli {
    fn console(mode: Mode) -> Self {
        Self {
            mode,
            needs_console: true,
        }
    }
}

fn main() -> eframe::Result {
    // A GUI-subsystem app has nowhere to print a panic, and release builds
    // abort on panic — so the log is the only place the reason survives.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        dl::log_line("panic", &info.to_string());
        default_hook(info);
    }));

    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = match parse_args(&args) {
        Ok(c) => c,
        Err(e) => {
            attach_console();
            eprintln!("cutie: {e}");
            eprintln!("try `cutie.exe --help`");
            std::process::exit(2);
        }
    };
    if cli.needs_console {
        attach_console();
    }

    match cli.mode {
        Mode::Version => println!("cutie {VERSION}"),
        Mode::Help => print_help(),
        Mode::Install => match install::install() {
            Ok(p) => {
                println!("installed → {}", p.display());
                println!("context menu: right-click a folder background → Download with Cutie");
            }
            Err(e) => {
                eprintln!("install failed: {e}");
                std::process::exit(1);
            }
        },
        Mode::Uninstall => match install::uninstall() {
            Ok(()) => println!("uninstalled ✨"),
            Err(e) => {
                eprintln!("uninstall failed: {e}");
                std::process::exit(1);
            }
        },
        Mode::Headless { urls, dir, opts } => std::process::exit(run_headless(urls, dir, opts)),
        Mode::HeadlessNoUrl => {
            eprintln!("--download needs a URL");
            std::process::exit(2);
        }
        Mode::Popup { urls, dir, opts } => return run_popup(urls, dir, opts),
    }
    Ok(())
}

fn parse_args(args: &[String]) -> Result<Cli, String> {
    let mut positional: Vec<String> = Vec::new();
    let mut headless = false;
    let mut opts = dl::Options::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--install" | "-i" => return Ok(Cli::console(Mode::Install)),
            "--uninstall" | "-u" => return Ok(Cli::console(Mode::Uninstall)),
            "--help" | "-h" => return Ok(Cli::console(Mode::Help)),
            "--version" | "-V" => return Ok(Cli::console(Mode::Version)),
            "--download" | "-d" => headless = true,
            "--audio" | "-a" => opts.audio_only = true,
            "--yes-playlist" | "-p" => opts.playlist = true,
            "--into" => {
                let d = it
                    .next()
                    .ok_or_else(|| "--into needs a folder".to_string())?;
                positional.push(d.clone());
            }
            other if other.starts_with("--into=") => {
                let v = other["--into=".len()..].trim();
                if v.is_empty() {
                    return Err("--into needs a folder".into());
                }
                positional.push(v.to_string());
            }
            other if other.len() > 1 && other.starts_with('-') => {
                return Err(format!("unknown option: {other}"));
            }
            other => positional.push(other.to_string()),
        }
    }

    let urls: Vec<String> = positional.iter().filter(|s| is_url(s)).cloned().collect();
    let dir_arg = positional.iter().find(|s| !is_url(s)).cloned();
    let dir = dir_arg
        .map(|d| normalize_dir(&d))
        .unwrap_or_else(default_dir);

    if headless {
        if urls.is_empty() {
            return Ok(Cli::console(Mode::HeadlessNoUrl));
        }
        return Ok(Cli::console(Mode::Headless { urls, dir, opts }));
    }

    Ok(Cli {
        mode: Mode::Popup { urls, dir, opts },
        needs_console: false,
    })
}

fn print_help() {
    println!("cutie {VERSION} — tiny yt-dlp context-menu downloader");
    println!();
    println!("  cutie.exe                      download the link on the clipboard into Downloads");
    println!("  cutie.exe <dir>                …into <dir>");
    println!("  cutie.exe <url> [dir]          download <url>");
    println!("  cutie.exe <url> <url2> [dir]   download several links, one after another");
    println!("  cutie.exe --download <url>…    headless: progress on stdout, exit code 0/1");
    println!("  cutie.exe --into <dir>         pick the target folder (also --into=<dir>)");
    println!("  cutie.exe --audio              save the audio track as mp3");
    println!("  cutie.exe --yes-playlist       download every entry of a playlist");
    println!(
        "  cutie.exe --install            install the Explorer context menu + start-menu shortcut"
    );
    println!("  cutie.exe --uninstall          remove the context menu, shortcut and app files");
    println!("  cutie.exe --version            print version");
    println!();
    println!("requires yt-dlp (and ffmpeg) on PATH, or yt-dlp.exe next to cutie.exe");
}

/// One line of headless progress: percent, size, speed and ETA.
fn progress_line(downloaded: u64, total: u64, speed: f64, eta: &str) -> String {
    // checked_mul: `total == 0` (unknown size) yields None
    let pct = match downloaded
        .checked_mul(100)
        .and_then(|d| d.checked_div(total))
    {
        Some(p) => format!("{p:>3}%"),
        None => "  ?%".to_string(),
    };
    let amount = if total > 0 {
        format!("{} / {}", dl::fmt_bytes(downloaded), dl::fmt_bytes(total))
    } else {
        dl::fmt_bytes(downloaded)
    };
    let speed = if speed > 0.0 {
        dl::fmt_speed(speed)
    } else {
        String::new()
    };
    let eta = if eta.is_empty() {
        String::new()
    } else {
        format!("ETA {eta}")
    };
    format!("{pct}  {amount}  {speed}  {eta}")
}

/// Width of the terminal, so the progress line never wraps.
fn console_width() -> usize {
    // The real window width beats the COLUMNS env var (which Windows shells
    // rarely set).
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
        };
        let h = GetStdHandle(STD_OUTPUT_HANDLE);
        if !h.is_null() && h != INVALID_HANDLE_VALUE {
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(h, &mut info) != 0 {
                let w = (info.srWindow.Right - info.srWindow.Left + 1) as usize;
                if w > 20 {
                    return w;
                }
            }
        }
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|c| c.parse::<usize>().ok())
        .filter(|w| *w > 20)
        .unwrap_or(80)
}

/// Console-mode download: same worker as the GUI, progress written to stdout.
fn run_headless(urls: Vec<String>, dir: PathBuf, opts: dl::Options) -> i32 {
    let mut failed = 0usize;
    for (i, url) in urls.iter().enumerate() {
        if dl::ctrl_c_pressed() {
            eprintln!("\ninterrupted");
            return 130;
        }
        if urls.len() > 1 {
            println!("[{}/{}] {url}", i + 1, urls.len());
        }
        if let Err(msg) = run_one_headless(url, &dir, opts) {
            if dl::ctrl_c_pressed() {
                eprintln!("\ninterrupted");
                return 130;
            }
            eprintln!("failed: {msg}");
            failed += 1;
        }
    }
    if failed > 0 {
        eprintln!("{failed} of {} failed", urls.len());
        1
    } else {
        0
    }
}

/// The `\r`-rewritten progress line of a headless run.
struct ProgressOut {
    width: usize,
    last_printed: Instant,
    /// Newest line that was throttled away — printed before the line closes,
    /// so the final state is never a stale `0%`.
    pending: Option<String>,
    /// A progress line is on screen and needs a newline before other output.
    open: bool,
}

impl ProgressOut {
    fn new() -> Self {
        Self {
            width: console_width().saturating_sub(1),
            last_printed: Instant::now() - Duration::from_secs(1),
            pending: None,
            open: false,
        }
    }

    fn update(&mut self, line: String) {
        if self.last_printed.elapsed() >= Duration::from_millis(200) {
            self.last_printed = Instant::now();
            self.print(&line);
            self.pending = None;
        } else {
            self.pending = Some(line);
        }
    }

    fn print(&mut self, line: &str) {
        let line: String = line.chars().take(self.width).collect();
        print!("\r{line:<width$}", width = self.width);
        let _ = std::io::stdout().flush();
        self.open = true;
    }

    /// Flush the newest state and end the line.
    fn close(&mut self) {
        if let Some(line) = self.pending.take() {
            self.print(&line);
        }
        if self.open {
            println!();
            self.open = false;
        }
    }
}

fn run_one_headless(url: &str, dir: &Path, opts: dl::Options) -> Result<(), String> {
    let (tx, rx) = mpsc::channel();
    // Ctrl+C must kill the yt-dlp tree, not orphan it.
    dl::watch_ctrl_c(dl::spawn(url.to_string(), dir.to_path_buf(), opts, tx));
    let mut out = ProgressOut::new();
    let result = loop {
        match rx.recv() {
            Err(_) => break Err("the download worker vanished".into()),
            Ok(dl::Ev::Meta { title, channel, .. }) => {
                out.close();
                println!(
                    "{title}{}",
                    if channel.is_empty() {
                        String::new()
                    } else {
                        format!(" — {channel}")
                    }
                );
            }
            Ok(dl::Ev::Progress {
                downloaded,
                total,
                speed,
                eta,
            }) => out.update(progress_line(downloaded, total, speed, &eta)),
            Ok(dl::Ev::Processing) => {
                out.close();
                println!(
                    "{}",
                    if opts.audio_only {
                        "converting to mp3…"
                    } else {
                        "finishing up…"
                    }
                );
            }
            Ok(dl::Ev::Thumbnail { .. }) => {}
            Ok(dl::Ev::Done { path }) => {
                out.close();
                if path.is_empty() {
                    println!("done ✨");
                } else {
                    println!("saved ✨ {path}");
                }
                break Ok(());
            }
            Ok(dl::Ev::Failed { msg }) => {
                out.close();
                break Err(msg);
            }
        }
    };
    dl::unwatch_ctrl_c();
    result
}

fn run_popup(urls: Vec<String>, dir: PathBuf, opts: dl::Options) -> eframe::Result {
    let mut urls = if urls.is_empty() {
        clipboard_urls()
    } else {
        urls
    };
    if urls.is_empty() {
        return run_error_window("Copy a video link first, then try again");
    }

    let first = urls.remove(0);
    let (tx, rx) = mpsc::channel();
    let handle = dl::spawn(first.clone(), dir.clone(), opts, tx);
    let pos = cursor_window_pos(POPUP_SIZE.x, POPUP_SIZE.y);

    let t_window = std::time::Instant::now();
    eframe::run_native(
        "cutie",
        popup_options(POPUP_SIZE, pos),
        Box::new(move |cc| {
            let window_ms = t_window.elapsed().as_millis();
            let t_fonts = std::time::Instant::now();
            setup_fonts(&cc.egui_ctx);
            let mut pipe = text::TextPipeline::new();
            pipe.init_textures(&cc.egui_ctx);
            dl::log_line(
                "ui",
                &format!(
                    "window_ms={window_ms} fonts_ms={}",
                    t_fonts.elapsed().as_millis()
                ),
            );
            Ok(Box::new(app::CutieApp::new(
                rx, handle, dir, opts, first, urls, pos, pipe,
            )))
        }),
    )
}

fn run_error_window(msg: &str) -> eframe::Result {
    let pos = cursor_window_pos(ERR_SIZE.x, ERR_SIZE.y);
    eframe::run_native(
        "cutie",
        popup_options(ERR_SIZE, pos),
        Box::new(move |cc| {
            setup_fonts(&cc.egui_ctx);
            let mut pipe = text::TextPipeline::new();
            pipe.init_textures(&cc.egui_ctx);
            Ok(Box::new(errwin::ErrApp::new(msg.to_string(), pipe)))
        }),
    )
}

fn popup_options(size: Vec2, pos: Pos2) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(false)
            .with_active(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false)
            .with_inner_size(size)
            .with_min_inner_size(size)
            .with_max_inner_size(size)
            .with_position(pos),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// fonts
// ---------------------------------------------------------------------------

/// Windows system fonts: Latin (Segoe UI) + one bold face + one Arabic-capable
/// fallback. No CJK (multi-MB TTCs killed startup time).
fn setup_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();

    // `text::font_bytes` caches the file contents globally, so the shaping
    // pipeline and egui share one read/allocation instead of loading the fonts
    // twice at startup.
    let read = text::font_bytes;

    if let Some(b) = read("segoeui.ttf")
        .or_else(|| read("tahoma.ttf"))
        .or_else(|| read("arial.ttf"))
    {
        fonts
            .font_data
            .insert("sys_main".into(), egui::FontData::from_static(b).into());
        fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default()
            .insert(0, "sys_main".into());
    }

    let mut bold_list: Vec<String> = Vec::new();
    if let Some(b) = read("seguisb.ttf")
        .or_else(|| read("arialbd.ttf"))
        .or_else(|| read("tahomabd.ttf"))
    {
        fonts
            .font_data
            .insert("sys_bold".into(), egui::FontData::from_static(b).into());
        bold_list.push("sys_bold".into());
    }

    if let Some(b) = read("arial.ttf").or_else(|| read("segoeuiar.ttf")) {
        fonts
            .font_data
            .insert("sys_ar".into(), egui::FontData::from_static(b).into());
        bold_list.push("sys_ar".into());
        let fam = fonts
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        if !fam.iter().any(|k| k == "sys_ar") {
            fam.push("sys_ar".into());
        }
    }

    if let Some(props) = fonts.families.get(&egui::FontFamily::Proportional) {
        for k in props {
            if !bold_list.contains(k) {
                bold_list.push(k.clone());
            }
        }
    }
    fonts
        .families
        .insert(egui::FontFamily::Name(BOLD_FONT.into()), bold_list);

    ctx.set_fonts(fonts);
}

// ---------------------------------------------------------------------------
// clipboard / paths / window placement
// ---------------------------------------------------------------------------

/// Every http(s) link on the clipboard — the popup downloads them in order.
///
/// Another app can hold the clipboard open for a moment (right after a copy,
/// or a clipboard manager peeking at it), so a busy clipboard is retried
/// instead of being reported as "no link".
fn clipboard_urls() -> Vec<String> {
    for attempt in 0..6 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = arboard::Clipboard::new().and_then(|mut cb| cb.get_text());
        match text {
            Ok(text) => return extract_urls(&text),
            // no text at all (an image, files, empty) — retrying will not help
            Err(arboard::Error::ContentNotAvailable) => return Vec::new(),
            Err(e) => dl::log_line("clipboard", &format!("attempt {attempt}: {e}")),
        }
    }
    Vec::new()
}

/// ASCII-case-insensitive `str::find`; returns a char boundary.
fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.as_bytes()
        .windows(needle.len())
        .position(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

/// First http(s) URL in a blob of text, trimmed of surrounding punctuation.
#[cfg(test)]
fn extract_url(text: &str) -> Option<String> {
    extract_urls(text).into_iter().next()
}

/// All http(s) URLs in a blob of text, in order, duplicates dropped
/// (ASCII case-insensitive — the same link copied twice counts once).
fn extract_urls(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut rest = text;
    while let Some((idx, len)) = find_scheme(rest) {
        let after = &rest[idx + len..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
            .unwrap_or(after.len());
        let url = trim_url_tail(&rest[idx..idx + len + end]);
        if url.len() > 10 && !out.iter().any(|u| u.eq_ignore_ascii_case(url)) {
            out.push(url.to_string());
        }
        rest = &after[end..];
    }
    out
}

/// The first `http://` / `https://` in `hay` as (byte offset, scheme length).
fn find_scheme(hay: &str) -> Option<(usize, usize)> {
    let https = find_ci(hay, "https://").map(|i| (i, 8usize));
    let http = find_ci(hay, "http://").map(|i| (i, 7usize));
    match (https, http) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

/// Strip trailing punctuation, but never break balanced brackets —
/// `…/wiki/A_(b)` keeps its `)`, `…/wiki/A_(b))` loses exactly one.
fn trim_url_tail(s: &str) -> &str {
    let mut end = s.len();
    while end > 0 {
        let Some(last) = s[..end].chars().next_back() else {
            break;
        };
        let opener = match last {
            ')' => Some('('),
            ']' => Some('['),
            '}' => Some('{'),
            '.' | ',' | ';' => Some('\0'),
            _ => None,
        };
        let Some(opener) = opener else { break };
        let head = &s[..end];
        if opener == '\0' || head.matches(last).count() > head.matches(opener).count() {
            end -= last.len_utf8();
        } else {
            break;
        }
    }
    &s[..end]
}

fn is_url(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 7 && b[..7].eq_ignore_ascii_case(b"http://"))
        || (b.len() >= 8 && b[..8].eq_ignore_ascii_case(b"https://"))
}

fn normalize_dir(s: &str) -> PathBuf {
    let t = s.trim().trim_matches('"');
    let t = t.trim_end_matches(['\\', '/']);
    // `C:\` trims down to `C:` which means "cwd on drive C" — put the slash back
    let t = if t.len() == 2 && t.ends_with(':') {
        format!("{t}\\")
    } else {
        t.to_string()
    };
    let p = PathBuf::from(&t);
    if p.is_file() {
        return p
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(default_dir);
    }
    if p.is_dir() {
        return p;
    }
    // Keep the requested folder even if it does not exist yet: the worker
    // creates it. A relative path always works (the cwd is its parent); an
    // absolute one only if some ancestor exists (e.g. no missing drive).
    let creatable = if p.is_absolute() {
        p.ancestors().skip(1).any(|a| a.is_dir())
    } else {
        true
    };
    if creatable {
        p
    } else {
        default_dir()
    }
}

fn default_dir() -> PathBuf {
    std::env::var("USERPROFILE")
        .map(|u| Path::new(&u).join("Downloads"))
        .ok()
        .filter(|p| p.is_dir())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// Position near the cursor, clamped inside the monitor that owns it.
///
/// Cursor/monitor APIs speak physical pixels while egui's initial window
/// position is in logical points scaled by the monitor's DPI, so every value
/// is divided by that scale first (otherwise the popup lands offset on
/// 125%/150% displays). The DPI comes from the monitor under the cursor —
/// the popup lands there, and a second monitor may use a different scale.
#[cfg(windows)]
fn cursor_window_pos(w: f32, h: f32) -> Pos2 {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::HiDpi::{GetDpiForMonitor, GetDpiForSystem, MDT_EFFECTIVE_DPI};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);

        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);

        let mut dpi_x = 0u32;
        let mut dpi_y = 0u32;
        let hr = GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
        let dpi = if hr == 0 && dpi_x > 0 {
            dpi_x
        } else {
            GetDpiForSystem()
        };
        let scale = if dpi >= 48 { dpi as f32 / 96.0 } else { 1.0 };

        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            rcMonitor: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            rcWork: RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            },
            dwFlags: 0,
        };
        let (l, t, r, b) = if monitor.is_null() || GetMonitorInfoW(monitor, &mut mi) == 0 {
            let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
            (0, 0, sw, sh)
        } else {
            let rc = mi.rcMonitor;
            (rc.left, rc.top, rc.right, rc.bottom)
        };
        let (l, t, r, b) = (
            l as f32 / scale,
            t as f32 / scale,
            r as f32 / scale,
            b as f32 / scale,
        );
        let (cx, cy) = (pt.x as f32 / scale, pt.y as f32 / scale);

        let mut x = cx + 14.0;
        let mut y = cy + 18.0;
        if x + w > r - 8.0 {
            x = cx - w - 14.0;
        }
        if y + h > b - 8.0 {
            y = cy - h - 18.0;
        }
        Pos2::new(x.clamp(l, (r - w).max(l)), y.clamp(t, (b - h).max(t)))
    }
}

#[cfg(not(windows))]
fn cursor_window_pos(_w: f32, _h: f32) -> Pos2 {
    Pos2::new(64.0, 64.0)
}

/// Print-mode apps have no console under `windows_subsystem = "windows"`;
/// re-attach the parent's so `--help` etc. are visible.
#[cfg(windows)]
fn attach_console() {
    use windows_sys::Win32::Foundation::{GENERIC_WRITE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, GetFileType, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_TYPE_UNKNOWN,
        OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };

    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return; // no parent console (double-clicked exe) — nothing to do
        }
        // Redirected/piped output already has a valid std handle — don't touch it.
        let before = GetStdHandle(STD_OUTPUT_HANDLE);
        let valid = !before.is_null()
            && before != INVALID_HANDLE_VALUE
            && GetFileType(before) != FILE_TYPE_UNKNOWN;
        if valid {
            return; // already redirected (pipe/file) or a live console handle
        }
        // A GUI-subsystem process starts with invalid std handles and
        // AttachConsole does not install them for us, so reopen CONOUT$ and
        // wire it up by hand. Rust's stdio resolves GetStdHandle lazily on
        // first use, which is why this must run before any print!/eprintln!.
        let name: Vec<u16> = "CONOUT$".encode_utf16().chain(Some(0)).collect();
        let h = CreateFileW(
            name.as_ptr(),
            GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        );
        if h != INVALID_HANDLE_VALUE {
            // intentionally not closed — this handle *is* stdout/stderr now
            SetStdHandle(STD_OUTPUT_HANDLE, h);
            SetStdHandle(STD_ERROR_HANDLE, h);
        }
    }
}
#[cfg(not(windows))]
fn attach_console() {}

// the icon generator is shared with build.rs; pull it into tests too.
// `write_ico_file` is only used by build.rs, hence the allow.
#[cfg(test)]
#[path = "../shared/icon_gen.rs"]
#[allow(dead_code)]
mod icon_gen;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_url_basics() {
        assert_eq!(
            extract_url("check this https://youtube.com/watch?v=abc123 out"),
            Some("https://youtube.com/watch?v=abc123".to_string())
        );
        assert_eq!(extract_url("no links here"), None);
        assert_eq!(
            extract_url("https://youtu.be/xyz"),
            Some("https://youtu.be/xyz".to_string())
        );
        assert_eq!(
            extract_url("(see https://example.com/a_(b))"),
            Some("https://example.com/a_(b)".to_string())
        );
        assert_eq!(
            extract_url("https://example.com/x)."),
            Some("https://example.com/x".to_string())
        );
        assert_eq!(
            extract_url("HTTPS://Example.com/Video?x=1"),
            Some("HTTPS://Example.com/Video?x=1".to_string())
        );
        assert_eq!(
            extract_url("first HTTP://a.tld then https://b.tld"),
            Some("HTTP://a.tld".to_string())
        );
    }

    #[test]
    fn trim_url_tail_keeps_balance() {
        assert_eq!(trim_url_tail("https://x/a_(b))"), "https://x/a_(b)");
        assert_eq!(trim_url_tail("https://x/a,"), "https://x/a");
        assert_eq!(trim_url_tail("https://x/a[b]"), "https://x/a[b]");
        assert_eq!(trim_url_tail("https://x/a]"), "https://x/a");
    }

    #[test]
    fn default_dir_exists() {
        assert!(default_dir().is_dir());
    }

    #[test]
    fn normalize_dir_falls_back_to_default() {
        assert_eq!(normalize_dir("Z:\\definitely\\not\\here"), default_dir());
    }

    #[test]
    fn normalize_dir_keeps_existing() {
        let d = default_dir();
        assert_eq!(normalize_dir(&format!("\"{}\"", d.display())), d);
    }

    #[test]
    fn normalize_dir_keeps_missing_leaf_under_existing_parent() {
        let base = std::env::temp_dir().join("cutie-normalize-test");
        let nested = base.join("new-folder");
        assert_eq!(normalize_dir(&nested.to_string_lossy()), nested);
    }

    #[test]
    fn normalize_dir_keeps_relative_paths() {
        assert_eq!(normalize_dir("new-folder"), PathBuf::from("new-folder"));
        assert_eq!(normalize_dir("a\\b"), PathBuf::from("a\\b"));
    }

    #[test]
    fn is_url_detects_schemes() {
        assert!(is_url("https://x.tld"));
        assert!(is_url("http://x.tld"));
        assert!(is_url("HTTPS://x.tld"));
        assert!(!is_url("C:\\Downloads"));
        assert!(!is_url("ftp://x.tld"));
    }

    #[test]
    fn cli_parses_modes() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches!(
            parse_args(&args(&["--install"])).unwrap().mode,
            Mode::Install
        ));
        assert!(matches!(
            parse_args(&args(&["--help"])).unwrap().mode,
            Mode::Help
        ));
        assert!(matches!(
            parse_args(&args(&["--download", "https://x.tld/v"]))
                .unwrap()
                .mode,
            Mode::Headless { .. }
        ));
        assert!(matches!(
            parse_args(&args(&["https://x.tld/v"])).unwrap().mode,
            Mode::Popup { .. }
        ));
        assert!(matches!(
            parse_args(&args(&["--download", "notaurl"])).unwrap().mode,
            Mode::HeadlessNoUrl
        ));
        match parse_args(&args(&["--download", "https://x.tld/v", "--into=."]))
            .unwrap()
            .mode
        {
            Mode::Headless { urls, dir, .. } => {
                assert_eq!(urls, vec!["https://x.tld/v".to_string()]);
                assert_eq!(dir, PathBuf::from("."));
            }
            _ => panic!("expected headless mode"),
        }
    }

    #[test]
    fn cli_collects_several_urls() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_args(&args(&["https://a.tld/1", "https://b.tld/2"]))
            .unwrap()
            .mode
        {
            Mode::Popup { urls, .. } => {
                assert_eq!(urls.len(), 2);
                assert_eq!(urls[1], "https://b.tld/2");
            }
            _ => panic!("expected popup mode"),
        }
    }

    #[test]
    fn cli_rejects_unknown_options() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let err = parse_args(&args(&["--bogus"])).unwrap_err();
        assert!(err.contains("--bogus"), "got {err}");
        assert!(parse_args(&args(&["--into"])).is_err());
        assert!(parse_args(&args(&["--into="])).is_err());
    }

    #[test]
    fn cli_reads_the_switches() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        match parse_args(&args(&["--audio", "--yes-playlist", "https://x.tld/v"]))
            .unwrap()
            .mode
        {
            Mode::Popup { opts, .. } => {
                assert!(opts.audio_only);
                assert!(opts.playlist);
            }
            _ => panic!("expected popup mode"),
        }
    }

    #[test]
    fn extract_urls_finds_every_link() {
        let all = extract_urls("one https://a.tld/1 two http://b.tld/2 three");
        assert_eq!(all.len(), 2);
        assert_eq!(all[0], "https://a.tld/1");
        assert_eq!(all[1], "http://b.tld/2");
        assert!(extract_urls("nothing here").is_empty());
    }

    #[test]
    fn extract_urls_drops_duplicates() {
        let all = extract_urls("https://a.tld/1 https://a.tld/1 http://b.tld/2 HTTPS://A.TLD/1");
        assert_eq!(all, vec!["https://a.tld/1", "http://b.tld/2"]);
    }

    #[test]
    fn extract_urls_handles_punctuation() {
        let all = extract_urls("(https://a.tld/x), https://b.tld/y.");
        assert_eq!(all, vec!["https://a.tld/x", "https://b.tld/y"]);
    }

    #[test]
    fn headless_progress_line_shows_size_speed_eta() {
        let line = progress_line(5 * 1024 * 1024, 10 * 1024 * 1024, 2_097_152.0, "0:05");
        assert!(line.contains(" 50%"));
        assert!(line.contains("5.0 MB / 10.0 MB"));
        assert!(line.contains("2.0 MB/s"));
        assert!(line.contains("ETA 0:05"));
        assert!(progress_line(1024, 0, 0.0, "").contains("?%"));
    }

    #[test]
    fn icon_is_valid_ico() {
        let mut ico = Vec::new();
        icon_gen::write_ico(&mut ico).expect("icon generation");
        assert_eq!(&ico[..4], &[0, 0, 1, 0], "ICO header");
        let count = u16::from_le_bytes([ico[4], ico[5]]);
        assert_eq!(count, 4, "one directory entry per size");
        assert!(ico.len() > 6 + 16 * count as usize);
    }
}
