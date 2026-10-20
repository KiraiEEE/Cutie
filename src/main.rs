#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod dl;
mod errwin;
mod install;
mod text;
mod theme;

use std::path::{Path, PathBuf};
use std::sync::mpsc;

use eframe::egui::{self, Pos2, Vec2};
use theme::BOLD_FONT;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const POPUP_SIZE: Vec2 = Vec2::new(420.0, 132.0);
const ERR_SIZE: Vec2 = Vec2::new(380.0, 80.0);

enum Mode {
    /// Normal popup: clipboard (or given) URL, GUI progress card.
    Popup {
        url: Option<String>,
        dir: PathBuf,
    },
    /// `--download <url> [dir]`: console progress, exit code 0/1.
    Headless {
        url: String,
        dir: PathBuf,
    },
    /// `--download` without any URL to download.
    HeadlessNoUrl,
    NoUrl,
    Install,
    Uninstall,
    Help,
    Version,
}

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
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cli = parse_args(&args);
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
        Mode::Uninstall => {
            install::uninstall();
            println!("uninstalled");
        }
        Mode::Headless { url, dir } => std::process::exit(run_headless(url, dir)),
        Mode::HeadlessNoUrl => {
            eprintln!("--download needs a URL");
            std::process::exit(2);
        }
        Mode::NoUrl => return run_popup(None, default_dir()),
        Mode::Popup { url, dir } => return run_popup(url, dir),
    }
    Ok(())
}

fn parse_args(args: &[String]) -> Cli {
    let mut positional: Vec<String> = Vec::new();
    let mut headless = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--install" | "-i" => return Cli::console(Mode::Install),
            "--uninstall" | "-u" => return Cli::console(Mode::Uninstall),
            "--help" | "-h" => return Cli::console(Mode::Help),
            "--version" | "-V" => return Cli::console(Mode::Version),
            "--download" | "-d" => headless = true,
            "--into" => {
                if let Some(d) = it.next() {
                    positional.push(d.clone());
                }
            }
            other => positional.push(other.to_string()),
        }
    }

    let url = positional.iter().find(|s| is_url(s)).cloned();
    let dir_arg = positional.iter().find(|s| !is_url(s)).cloned();

    if headless {
        return match (url, dir_arg) {
            (Some(url), dir) => Cli::console(Mode::Headless {
                url,
                dir: dir.map(|d| normalize_dir(&d)).unwrap_or_else(default_dir),
            }),
            (None, _) => Cli::console(Mode::HeadlessNoUrl),
        };
    }

    match (url, dir_arg) {
        // a bare `cutie.exe` with no link on the clipboard shows the error toast
        (None, None) if args.is_empty() => Cli {
            mode: Mode::Popup {
                url: None,
                dir: default_dir(),
            },
            needs_console: false,
        },
        (None, None) => Cli {
            mode: Mode::NoUrl,
            needs_console: false,
        },
        (url, dir) => Cli {
            mode: Mode::Popup {
                url,
                dir: dir.map(|d| normalize_dir(&d)).unwrap_or_else(default_dir),
            },
            needs_console: false,
        },
    }
}

fn print_help() {
    println!("cutie {VERSION} — tiny yt-dlp context-menu downloader");
    println!();
    println!("  cutie.exe                    download the link on the clipboard into Downloads");
    println!("  cutie.exe <dir>              …into <dir>");
    println!("  cutie.exe <url> [dir]        download <url>");
    println!(
        "  cutie.exe --download <url> [dir]   headless download, prints progress to the console"
    );
    println!(
        "  cutie.exe --install          install the Explorer context menu + start-menu shortcut"
    );
    println!("  cutie.exe --uninstall        remove the context menu, shortcut and app files");
    println!("  cutie.exe --version          print version");
    println!();
    println!("requires yt-dlp (and ffmpeg) on PATH, or yt-dlp.exe next to cutie.exe");
}

/// Console-mode download: same worker as the GUI, progress written to stdout.
fn run_headless(url: String, dir: PathBuf) -> i32 {
    let (tx, rx) = mpsc::channel();
    let _handle = dl::spawn(url, dir, tx);
    let mut last_pct: Option<u64> = None;
    while let Ok(ev) = rx.recv() {
        match ev {
            dl::Ev::Meta { title, channel, .. } => {
                println!(
                    "{title}{}",
                    if channel.is_empty() {
                        String::new()
                    } else {
                        format!(" — {channel}")
                    }
                );
            }
            dl::Ev::Progress {
                downloaded, total, ..
            } => {
                // checked_div: `total == 0` (unknown size) yields None
                if let Some(pct) = downloaded.saturating_mul(100).checked_div(total) {
                    if last_pct != Some(pct) {
                        last_pct = Some(pct);
                        print!("\r{pct:>3}%  {}", dl::fmt_bytes(downloaded));
                        use std::io::Write;
                        let _ = std::io::stdout().flush();
                    }
                }
            }
            dl::Ev::Thumbnail { .. } => {}
            dl::Ev::Done { path } => {
                println!("\nsaved: {path}");
                return 0;
            }
            dl::Ev::Failed { msg } => {
                eprintln!("\nfailed: {msg}");
                return 1;
            }
        }
    }
    eprintln!("download worker exited unexpectedly");
    1
}

fn run_popup(url: Option<String>, dir: PathBuf) -> eframe::Result {
    let url = match url.or_else(clipboard_url) {
        Some(u) => u,
        None => return run_error_window("Copy a video link first"),
    };

    let (tx, rx) = mpsc::channel();
    let handle = dl::spawn(url, dir.clone(), tx);
    let pos = cursor_window_pos(POPUP_SIZE.x, POPUP_SIZE.y);

    let t_fonts = std::time::Instant::now();
    eframe::run_native(
        "cutie",
        popup_options(POPUP_SIZE, pos),
        Box::new(move |cc| {
            let fonts_ms = t_fonts.elapsed().as_millis();
            setup_fonts(&cc.egui_ctx);
            let mut pipe = text::TextPipeline::new();
            pipe.init_textures(&cc.egui_ctx);
            dl::log_line("ui", &format!("fonts_ms={fonts_ms}"));
            Ok(Box::new(app::CutieApp::new(rx, handle, dir, pos, pipe)))
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

    let windir = std::env::var("WINDIR").unwrap_or_else(|_| "C:\\Windows".into());
    let fonts_dir = Path::new(&windir).join("Fonts");

    let read = |name: &str| -> Option<Vec<u8>> {
        let p = fonts_dir.join(name);
        if p.is_file() {
            std::fs::read(&p).ok()
        } else {
            None
        }
    };

    if let Some(b) = read("segoeui.ttf")
        .or_else(|| read("tahoma.ttf"))
        .or_else(|| read("arial.ttf"))
    {
        fonts
            .font_data
            .insert("sys_main".into(), egui::FontData::from_owned(b).into());
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
            .insert("sys_bold".into(), egui::FontData::from_owned(b).into());
        bold_list.push("sys_bold".into());
    }

    if let Some(b) = read("arial.ttf").or_else(|| read("segoeuiar.ttf")) {
        fonts
            .font_data
            .insert("sys_ar".into(), egui::FontData::from_owned(b).into());
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

fn clipboard_url() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    extract_url(&cb.get_text().ok()?)
}

/// First http(s) URL in a blob of text, trimmed of surrounding punctuation.
fn extract_url(text: &str) -> Option<String> {
    let t = text.trim();
    for pat in ["https://", "http://"] {
        if let Some(i) = t.find(pat) {
            let rest = &t[i..];
            let end = rest
                .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>'))
                .unwrap_or(rest.len());
            let url = trim_url_tail(&rest[..end]);
            if url.len() > pat.len() + 3 {
                return Some(url.to_string());
            }
        }
    }
    None
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
    s.starts_with("https://") || s.starts_with("http://")
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
    if p.is_dir() {
        p
    } else if let Some(parent) = p.parent().filter(|x| x.is_dir()) {
        parent.to_path_buf()
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
#[cfg(windows)]
fn cursor_window_pos(w: f32, h: f32) -> Pos2 {
    use windows_sys::Win32::Foundation::{POINT, RECT};
    use windows_sys::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN,
    };

    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        GetCursorPos(&mut pt);

        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
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

        let mut x = pt.x as f32 + 14.0;
        let mut y = pt.y as f32 + 18.0;
        if x + w > r as f32 - 8.0 {
            x = pt.x as f32 - w - 14.0;
        }
        if y + h > b as f32 - 8.0 {
            y = pt.y as f32 - h - 18.0;
        }
        Pos2::new(
            x.clamp(l as f32, (r as f32 - w).max(l as f32)),
            y.clamp(t as f32, (b as f32 - h).max(t as f32)),
        )
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
    fn is_url_detects_schemes() {
        assert!(is_url("https://x.tld"));
        assert!(is_url("http://x.tld"));
        assert!(!is_url("C:\\Downloads"));
        assert!(!is_url("ftp://x.tld"));
    }

    #[test]
    fn cli_parses_modes() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(matches!(
            parse_args(&args(&["--install"])).mode,
            Mode::Install
        ));
        assert!(matches!(parse_args(&args(&["--help"])).mode, Mode::Help));
        assert!(matches!(
            parse_args(&args(&["--download", "https://x.tld/v"])).mode,
            Mode::Headless { .. }
        ));
        assert!(matches!(
            parse_args(&args(&["https://x.tld/v"])).mode,
            Mode::Popup { url: Some(_), .. }
        ));
        assert!(matches!(
            parse_args(&args(&["--download", "notaurl"])).mode,
            Mode::HeadlessNoUrl
        ));
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
