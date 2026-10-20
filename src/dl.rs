use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};

#[cfg(windows)]
use std::os::windows::process::CommandExt;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Thumbnails are decorative; refuse anything bigger than this.
const MAX_THUMB_BYTES: u64 = 8 * 1024 * 1024;
/// Roll the log over once it grows past this.
const LOG_MAX_BYTES: u64 = 256 * 1024;

fn hide_console(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}

/// Lock that survives a poisoned mutex (we only ever store trivial data).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Debug, Clone)]
pub enum Ev {
    Meta {
        title: String,
        channel: String,
        duration: String,
        size_hint: Option<u64>,
    },
    Progress {
        downloaded: u64,
        total: u64,
        speed: f64,
        eta: String,
    },
    Done {
        path: String,
    },
    Failed {
        msg: String,
    },
    Thumbnail {
        rgba: Vec<u8>,
        w: u32,
        h: u32,
    },
}

/// Handle for cancelling a running download. Cancel is non-blocking: it spawns
/// a background thread that taskkills the process tree and sweeps leftovers.
pub struct DlHandle {
    pids: Arc<Mutex<Vec<u32>>>,
    dir: PathBuf,
    snap: Arc<HashSet<String>>,
}

impl DlHandle {
    pub fn cancel(&self) {
        let pids: Vec<u32> = lock(&self.pids).drain(..).collect();
        if pids.is_empty() {
            return;
        }
        let dir = self.dir.clone();
        let snap = self.snap.clone();
        std::thread::spawn(move || {
            for pid in pids {
                let mut c = Command::new("taskkill");
                c.args(["/PID", &pid.to_string(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null());
                hide_console(&mut c);
                let _ = c.status();
            }
            sweep_leftovers(&dir, &snap);
        });
    }

    /// True while a yt-dlp process is still registered as running.
    pub fn is_running(&self) -> bool {
        !lock(&self.pids).is_empty()
    }
}

pub fn spawn(url: String, dir: PathBuf, tx: Sender<Ev>) -> DlHandle {
    let pids = Arc::new(Mutex::new(Vec::new()));
    let snap = Arc::new(snapshot_dir(&dir));
    let worker = WorkerArgs {
        url,
        dir: dir.clone(),
        tx,
        pids: pids.clone(),
        snap: snap.clone(),
    };
    std::thread::spawn(move || worker.run());
    DlHandle { pids, dir, snap }
}

struct WorkerArgs {
    url: String,
    dir: PathBuf,
    tx: Sender<Ev>,
    pids: Arc<Mutex<Vec<u32>>>,
    snap: Arc<HashSet<String>>,
}

// ---------------------------------------------------------------------------
// logging — one shared file in %TEMP%, size-capped
// ---------------------------------------------------------------------------

fn log_path() -> PathBuf {
    std::env::temp_dir().join("cutie.log")
}

/// One-time log housekeeping: drop the per-pid logs older builds wrote and
/// roll the current file over if it got huge.
fn init_logs() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let temp = std::env::temp_dir();
        if let Ok(rd) = std::fs::read_dir(&temp) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with("cutie-dl-") && name.ends_with(".log") {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let p = log_path();
        if let Ok(md) = std::fs::metadata(&p) {
            if md.len() > LOG_MAX_BYTES {
                let _ = std::fs::rename(&p, temp.join("cutie.log.1"));
            }
        }
    });
}

pub fn log_line(tag: &str, s: &str) {
    use std::io::Write;
    init_logs();
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
    {
        let _ = writeln!(f, "[{}][{tag}] {s}", std::process::id());
    }
}

// ---------------------------------------------------------------------------
// dir snapshot / leftover sweep / done-file detection
// ---------------------------------------------------------------------------

fn snapshot_dir(dir: &Path) -> HashSet<String> {
    let mut set = HashSet::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            set.insert(e.file_name().to_string_lossy().into_owned());
        }
    }
    set
}

fn is_partial_name(n: &str) -> bool {
    let lower = n.to_ascii_lowercase();
    lower.ends_with(".part")
        || lower.ends_with(".ytdl")
        || lower.ends_with(".aria2")
        || lower.ends_with(".temp")
        || lower.contains(".part-frag")
        || has_format_fragment(&lower)
}

/// yt-dlp intermediate format files: `name.f123.mp4` — must end in a media ext.
/// Byte-wise so multibyte titles can never panic the matcher.
fn has_format_fragment(lower: &str) -> bool {
    let b = lower.as_bytes();
    let Some(dot_ext) = lower.rfind('.') else {
        return false;
    };
    let ext = &lower[dot_ext + 1..];
    if !MEDIA_EXT.contains(&ext) {
        return false;
    }
    let mut i = 0usize;
    while i + 3 < dot_ext {
        if b[i] == b'.' && b[i + 1] == b'f' && b[i + 2].is_ascii_digit() {
            let rest = &b[i + 3..dot_ext];
            if !rest.is_empty() && rest.iter().all(u8::is_ascii_digit) {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn sweep_leftovers(dir: &Path, snap: &HashSet<String>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if snap.contains(&name) {
                continue;
            }
            if is_partial_name(&name) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

const MEDIA_EXT: [&str; 6] = ["mp4", "m4a", "webm", "mkv", "mp3", "mov"];

/// New (not in snapshot) media file with newest mtime — fallback output probe
/// for when `--print after_move:filepath` didn't yield a usable path.
fn find_new_media(dir: &Path, snap: &HashSet<String>) -> Option<PathBuf> {
    let mut best: Option<(std::time::SystemTime, PathBuf)> = None;
    let rd = std::fs::read_dir(dir).ok()?;
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if snap.contains(&name) {
            continue;
        }
        let ext = e
            .path()
            .extension()
            .map(|x| x.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !MEDIA_EXT.contains(&ext.as_str()) {
            continue;
        }
        if let Ok(md) = e.metadata() {
            if let Ok(modified) = md.modified() {
                if best.as_ref().map(|(t, _)| modified > *t).unwrap_or(true) {
                    best = Some((modified, e.path()));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

// ---------------------------------------------------------------------------
// worker — single yt-dlp pass
// ---------------------------------------------------------------------------

impl WorkerArgs {
    fn run(self) {
        let Self {
            url,
            dir,
            tx,
            pids,
            snap,
        } = self;
        log_line("run", &format!("url={url} dir={}", dir.display()));

        let Some(ytdlp) = find_ytdlp() else {
            let _ = tx.send(Ev::Failed {
                msg: "yt-dlp not found. Install it and try again.".into(),
            });
            return;
        };

        if let Err(e) = std::fs::create_dir_all(&dir) {
            let _ = tx.send(Ev::Failed {
                msg: format!("can't create folder: {e}"),
            });
            return;
        }

        let tmpl = "<<<CUTIE>>>PROG|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s";
        let meta_tpl = "before_dl:<<<CUTIE>>>META|%(title)s\u{1f}%(uploader)s\u{1f}%(duration_string)s\u{1f}%(filesize_approx)s\u{1f}%(thumbnail)s";

        let mut cmd = Command::new(&ytdlp);
        cmd.current_dir(&dir)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .args([
                "--no-playlist",
                "--no-warnings",
                "--newline",
                "--progress",
                "--no-simulate",
                "--socket-timeout",
                "20",
                "--retries",
                "5",
                "--fragment-retries",
                "10",
                "-4",
                "--concurrent-fragments",
                "4",
                "-f",
                "bv*[vcodec^=avc1]+ba[ext=m4a]/bv*+ba/b[ext=mp4]/b",
                "--merge-output-format",
                "mp4",
                "--embed-metadata",
                "--no-mtime",
                "--windows-filenames",
                "--progress-template",
                tmpl,
                "--print",
                meta_tpl,
                "--print",
                "after_move:filepath",
                "-o",
                "%(title).150B [%(id)s].%(ext)s",
                &url,
            ]);

        if which("node.exe").or_else(|| which("node")).is_some() {
            cmd.args(["--js-runtimes", "node"]);
        }

        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        hide_console(&mut cmd);

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(Ev::Failed {
                    msg: format!("spawn: {e}"),
                });
                return;
            }
        };
        let my_pid = child.id();
        lock(&pids).push(my_pid);

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // yt-dlp prints the final path (after_move:filepath) as a bare line;
        // everything else on stdout is either our marker or noise.
        let final_path: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));

        let tx1 = tx.clone();
        let fp1 = final_path.clone();
        let t = std::thread::spawn(move || {
            for line in lines(stdout) {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && !line.starts_with("<<<CUTIE>>>")
                    && Path::new(trimmed).extension().is_some_and(|e| {
                        MEDIA_EXT.contains(&e.to_string_lossy().to_lowercase().as_str())
                    })
                {
                    *lock(&fp1) = trimmed.to_string();
                }
                log_line("out", &line);
                handle_line(&line, &tx1);
            }
        });

        let last_err: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let le2 = last_err.clone();
        let tx2 = tx.clone();
        let t2 = std::thread::spawn(move || {
            for line in lines(stderr) {
                let trimmed = line.trim();
                if !trimmed.is_empty()
                    && !trimmed.starts_with("<<<CUTIE>>>")
                    && !trimmed.starts_with("WARNING")
                {
                    *lock(&le2) = strip_err(trimmed).chars().take(160).collect();
                }
                log_line("err", &line);
                handle_line(&line, &tx2);
            }
        });

        let status = child.wait();
        let _ = t.join();
        let _ = t2.join();
        lock(&pids).retain(|&p| p != my_pid);
        log_line("dl", &format!("status={status:?}"));

        match status {
            Ok(st) if st.success() => {
                let reported = lock(&final_path).clone();
                let path = if !reported.is_empty() && Path::new(&reported).is_file() {
                    reported
                } else {
                    find_new_media(&dir, &snap)
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default()
                };
                let _ = tx.send(Ev::Done { path });
            }
            Ok(_) => {
                sweep_leftovers(&dir, &snap);
                let msg = {
                    let e = lock(&last_err);
                    if e.is_empty() {
                        "yt-dlp failed".to_string()
                    } else {
                        e.clone()
                    }
                };
                let _ = tx.send(Ev::Failed { msg });
            }
            Err(e) => {
                sweep_leftovers(&dir, &snap);
                let _ = tx.send(Ev::Failed {
                    msg: format!("process error: {e}"),
                });
            }
        }
    }
}

/// Read a pipe line-by-line, replacing invalid UTF-8 instead of stopping.
fn lines<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> impl Iterator<Item = String> {
    struct LineIter {
        reader: Option<BufReader<Box<dyn std::io::Read + Send>>>,
    }
    impl Iterator for LineIter {
        type Item = String;
        fn next(&mut self) -> Option<String> {
            let reader = self.reader.as_mut()?;
            let mut buf = Vec::new();
            match reader.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => {
                    self.reader = None;
                    None
                }
                Ok(_) => {
                    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
                        buf.pop();
                    }
                    Some(String::from_utf8_lossy(&buf).into_owned())
                }
            }
        }
    }
    LineIter {
        reader: pipe.map(|r| BufReader::new(Box::new(r) as Box<dyn std::io::Read + Send>)),
    }
}

// ---------------------------------------------------------------------------
// line handling
// ---------------------------------------------------------------------------

fn handle_line(line: &str, tx: &Sender<Ev>) {
    let line = line.trim();
    if let Some(rest) = line.strip_prefix("<<<CUTIE>>>PROG|") {
        let p: Vec<&str> = rest.split('|').collect();
        let downloaded = p.first().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
        let total = p
            .get(1)
            .and_then(|s| s.parse::<u64>().ok())
            .or_else(|| p.get(2).and_then(|s| s.parse::<u64>().ok()))
            .unwrap_or(0);
        let speed = p
            .get(3)
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|v| *v > 0.0)
            .unwrap_or(0.0);
        let eta = p
            .get(4)
            .and_then(|s| s.parse::<f64>().ok())
            .map(|e| fmt_dur(e as u64))
            .unwrap_or_default();
        let _ = tx.send(Ev::Progress {
            downloaded,
            total,
            speed,
            eta,
        });
    } else if let Some(rest) = line.strip_prefix("<<<CUTIE>>>META|") {
        let meta = parse_meta_line(rest);
        if let Some(thumb_url) = meta.thumbnail.clone() {
            let tx_t = tx.clone();
            std::thread::spawn(move || {
                if let Some((rgba, w, h)) = fetch_thumbnail(&thumb_url) {
                    let _ = tx_t.send(Ev::Thumbnail { rgba, w, h });
                }
            });
        }
        let _ = tx.send(Ev::Meta {
            title: meta.title,
            channel: meta.channel,
            duration: meta.duration,
            size_hint: meta.size_hint,
        });
    }
}

struct MetaLine {
    title: String,
    channel: String,
    duration: String,
    size_hint: Option<u64>,
    thumbnail: Option<String>,
}

fn parse_meta_line(rest: &str) -> MetaLine {
    let f: Vec<&str> = rest.split('\u{1f}').collect();
    let get = |i: usize| -> &str { f.get(i).copied().unwrap_or("").trim() };
    let title = get(0);
    MetaLine {
        title: if title.is_empty() || title == "NA" {
            "Unknown".into()
        } else {
            title.to_string()
        },
        channel: get(1).to_string(),
        duration: get(2).to_string(),
        size_hint: get(3).parse::<u64>().ok().filter(|v| *v > 0),
        thumbnail: Some(get(4).to_string()).filter(|s| s.starts_with("http")),
    }
}

fn fetch_thumbnail(url: &str) -> Option<(Vec<u8>, u32, u32)> {
    let resp = ureq::get(url)
        .set("User-Agent", "cutie/0.2")
        .timeout(std::time::Duration::from_secs(10))
        .call()
        .ok()?;
    let mut bytes = Vec::new();
    resp.into_reader()
        .take(MAX_THUMB_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > MAX_THUMB_BYTES {
        log_line("thumb", "skipped: larger than cap");
        return None;
    }
    let img = image::load_from_memory(&bytes).ok()?;
    // cover-crop to exactly 160x90 (no letterboxing)
    let (w, h) = (img.width() as f32, img.height() as f32);
    let s = (160.0 / w).max(90.0 / h);
    let nw = (w * s).ceil().max(1.0) as u32;
    let nh = (h * s).ceil().max(1.0) as u32;
    let img = img.resize_exact(nw, nh, image::imageops::FilterType::Triangle);
    let img = img.crop_imm((nw - 160) / 2, (nh - 90) / 2, 160, 90);
    let rgba = img.to_rgba8();
    let (w, h) = (rgba.width(), rgba.height());
    Some((rgba.into_raw(), w, h))
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

pub fn fmt_speed(bps: f64) -> String {
    if bps >= 1_048_576.0 {
        format!("{:.1} MB/s", bps / 1_048_576.0)
    } else if bps >= 1024.0 {
        format!("{:.0} KB/s", bps / 1024.0)
    } else {
        format!("{:.0} B/s", bps)
    }
}

fn strip_err(s: &str) -> &str {
    let t = s.trim();
    for p in ["ERROR:", "error:", "WARNING: "] {
        if let Some(r) = t.strip_prefix(p) {
            return r.trim_start();
        }
    }
    t
}

pub fn fmt_dur(secs: u64) -> String {
    let h = secs / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

pub fn fmt_bytes(n: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < 4 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", U[i])
    }
}

pub fn short_folder(p: &Path) -> String {
    let s = p.to_string_lossy();
    if let Ok(home) = std::env::var("USERPROFILE") {
        if let Some(rest) = s.strip_prefix(&home) {
            let rest = rest.trim_start_matches(['\\', '/']);
            return format!("~\\{rest}");
        }
    }
    let parts: Vec<&str> = s.split(['\\', '/']).filter(|p| !p.is_empty()).collect();
    if parts.len() >= 2 {
        format!("…\\{}\\{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else {
        s.to_string()
    }
}

fn find_ytdlp() -> Option<PathBuf> {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["yt-dlp.exe", "yt-dlp"] {
                let p = dir.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }
    which("yt-dlp.exe").or_else(|| which("yt-dlp"))
}

fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let full = dir.join(name);
        if full.is_file() {
            return Some(full);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_line_parses_full() {
        let rest = "عنوان الفيديو\u{1f}Channel Name\u{1f}10:33\u{1f}13644933\u{1f}https://i.ytimg.com/vi/x/hq.jpg";
        let m = parse_meta_line(rest);
        assert_eq!(m.title, "عنوان الفيديو");
        assert_eq!(m.channel, "Channel Name");
        assert_eq!(m.duration, "10:33");
        assert_eq!(m.size_hint, Some(13644933));
        assert!(m.thumbnail.unwrap().starts_with("http"));
    }

    #[test]
    fn meta_line_na_fields() {
        let m = parse_meta_line("T\u{1f}NA\u{1f}NA\u{1f}NA\u{1f}NA");
        assert_eq!(m.title, "T");
        assert_eq!(m.channel, "NA");
        assert_eq!(m.size_hint, None);
        assert!(m.thumbnail.is_none());
    }

    #[test]
    fn meta_line_short_and_empty() {
        let m = parse_meta_line("");
        assert_eq!(m.title, "Unknown");
        assert!(m.channel.is_empty() && m.duration.is_empty());
        assert!(m.thumbnail.is_none());
    }

    #[test]
    fn partial_names_detected() {
        assert!(is_partial_name("video [id].part"));
        assert!(is_partial_name("video [id].f140.m4a"));
        assert!(is_partial_name("video [id].mp4.aria2"));
        assert!(is_partial_name("video [id].ytdl"));
        assert!(!is_partial_name("video [id].mp4"));
        assert!(!is_partial_name("my.f2024.notes.txt"));
    }

    #[test]
    fn partial_names_survive_multibyte_titles() {
        // regression: the old byte-slicing matcher could panic on non-ASCII
        assert!(is_partial_name("تعلم اللغة.f140.m4a"));
        assert!(is_partial_name("🎬 movie [id].f399.mp4.part"));
        assert!(!is_partial_name("تعلم اللغة [id].mp4"));
    }

    #[test]
    fn fmt_basics() {
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(2048), "2.0 KB");
        assert_eq!(fmt_dur(65), "1:05");
        assert_eq!(fmt_dur(3725), "1:02:05");
        assert_eq!(fmt_speed(2_097_152.0), "2.0 MB/s");
        assert_eq!(fmt_speed(1536.0), "2 KB/s");
    }

    #[test]
    fn strip_err_prefixes() {
        assert_eq!(strip_err("ERROR: Video unavailable"), "Video unavailable");
        assert_eq!(strip_err("plain"), "plain");
    }

    #[test]
    fn short_folder_home_and_deep() {
        let deep = Path::new("C:\\a\\b\\c");
        assert_eq!(short_folder(deep), "…\\b\\c");
        assert_eq!(short_folder(Path::new("C:\\")), "C:\\");
    }
}
