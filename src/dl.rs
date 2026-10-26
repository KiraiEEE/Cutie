use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// What to download. Chosen by the CLI flags, forwarded to yt-dlp.
#[derive(Clone, Copy, Debug, Default)]
pub struct Options {
    /// Extract the audio track as mp3 instead of the best video.
    pub audio_only: bool,
    /// Grab every entry of a playlist instead of only the first one.
    pub playlist: bool,
}

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
        /// 1-based position inside a playlist.
        index: Option<u64>,
        /// How many entries the playlist has.
        count: Option<u64>,
    },
    Progress {
        downloaded: u64,
        total: u64,
        speed: f64,
        eta: String,
    },
    /// Every byte is in; yt-dlp is merging/converting the result.
    Processing,
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

/// One progress sample for the *whole* download.
#[derive(Debug, Clone, Copy)]
pub struct Progress {
    pub downloaded: u64,
    pub total: u64,
    pub speed: f64,
    /// Seconds left, `0` when unknown.
    pub eta: u64,
}

/// Turns yt-dlp's per-stream byte counters into whole-download progress.
///
/// yt-dlp fetches the video and the audio stream as two separate transfers,
/// each with its own counter, so a naive `downloaded / total` restarts at
/// 0 % in the middle of a download. We add up the streams that already
/// finished and use `filesize_approx` (the size of the whole video) as the
/// grand total, so the bar only ever moves forward. ʕ•ᴥ•ʔ
pub struct ProgressTracker {
    /// Bytes of streams that already finished.
    finished: u64,
    /// Byte counter of the stream that is downloading right now.
    cur_downloaded: u64,
    /// Size of the stream that is downloading right now.
    cur_total: u64,
    /// Size of the whole download (video + audio) when yt-dlp reported it.
    grand_total: Option<u64>,
    /// Smoothed transfer rate in bytes/s.
    speed_ema: f64,
}

impl Default for ProgressTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl ProgressTracker {
    pub fn new() -> Self {
        Self {
            finished: 0,
            cur_downloaded: 0,
            cur_total: 0,
            grand_total: None,
            speed_ema: 0.0,
        }
    }

    /// Start a new video — playlist items each get their own progress.
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// The size of the whole download, when yt-dlp knows it.
    pub fn set_grand_total(&mut self, total: Option<u64>) {
        self.grand_total = total.filter(|t| *t > 0);
    }

    /// Feed one raw yt-dlp sample; returns the progress of the whole download.
    pub fn push(&mut self, downloaded: u64, total: u64, speed: f64) -> Progress {
        if downloaded < self.cur_downloaded {
            // The counter went backwards → a new stream started, the old one
            // is done and its bytes count towards the grand total.
            self.finished += self.cur_total;
            self.cur_downloaded = 0;
            self.cur_total = 0;
        }
        self.cur_downloaded = downloaded;
        if total > 0 {
            self.cur_total = total;
        }
        if speed > 0.0 {
            self.speed_ema = if self.speed_ema <= 0.0 {
                speed
            } else {
                self.speed_ema * 0.72 + speed * 0.28
            };
        }

        let done = self.finished + self.cur_downloaded;
        let mut whole = self.grand_total.unwrap_or(self.finished + self.cur_total);
        if whole < done {
            whole = done;
        }
        let eta = if self.speed_ema > 0.0 && whole > done {
            ((whole - done) as f64 / self.speed_ema).round() as u64
        } else {
            0
        };
        Progress {
            downloaded: done,
            total: whole,
            speed: self.speed_ema,
            eta,
        }
    }
}

/// Handle for cancelling a running download.
///
/// Cancellation is synchronous: the process tree is killed before returning so
/// closing the window can never leave a detached yt-dlp/ffmpeg behind. The
/// `cancelled` flag covers the window where yt-dlp has been spawned but its pid
/// is not registered yet.
pub struct DlHandle {
    pids: Arc<Mutex<Vec<u32>>>,
    cancelled: Arc<AtomicBool>,
    dir: PathBuf,
    snap: Arc<HashSet<String>>,
}

impl DlHandle {
    /// Kill the download *and* delete its partial files — the user aborted,
    /// the leftovers are junk.
    pub fn cancel(&self) {
        if self.stop() {
            sweep_leftovers(&self.dir, &self.snap);
        }
    }

    /// Kill the download but keep partial files, so a Retry can resume them.
    /// Returns true when something was actually still running.
    pub fn stop(&self) -> bool {
        self.cancelled.store(true, Ordering::SeqCst);
        let pids: Vec<u32> = lock(&self.pids).drain(..).collect();
        for pid in &pids {
            kill_pid(*pid);
        }
        !pids.is_empty()
    }

    /// True while a yt-dlp process is still registered as running.
    pub fn is_running(&self) -> bool {
        !lock(&self.pids).is_empty()
    }
}

/// Never leave an orphaned yt-dlp behind, whatever drops the handle
/// (window-creation failure, early return, panic unwind).
impl Drop for DlHandle {
    fn drop(&mut self) {
        if self.is_running() {
            log_line("dl", "handle dropped while running — cancelling");
            self.cancel();
        }
    }
}

// ---------------------------------------------------------------------------
// ctrl-C cleanup for headless mode
// ---------------------------------------------------------------------------

static CTRL_C: AtomicBool = AtomicBool::new(false);
static CTRL_HANDLE: Mutex<Option<DlHandle>> = Mutex::new(None);

/// True once Ctrl+C (or Ctrl+Break) was pressed in a headless run.
pub fn ctrl_c_pressed() -> bool {
    CTRL_C.load(Ordering::SeqCst)
}

/// Register the console ctrl handler and remember the live download, so
/// Ctrl+C kills the yt-dlp tree instead of orphaning it. Idempotent.
pub fn watch_ctrl_c(handle: DlHandle) {
    #[cfg(windows)]
    {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| unsafe {
            windows_sys::Win32::System::Console::SetConsoleCtrlHandler(Some(on_ctrl), 1);
        });
    }
    *lock(&CTRL_HANDLE) = Some(handle);
}

/// Forget the tracked download (it finished on its own).
pub fn unwatch_ctrl_c() {
    *lock(&CTRL_HANDLE) = None;
}

#[cfg(windows)]
unsafe extern "system" fn on_ctrl(_kind: u32) -> i32 {
    CTRL_C.store(true, Ordering::SeqCst);
    if let Some(h) = lock(&CTRL_HANDLE).as_ref() {
        h.cancel();
    }
    1 // handled — the main loop notices and exits with code 130
}

fn kill_pid(pid: u32) {
    let mut c = Command::new("taskkill");
    c.args(["/PID", &pid.to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    hide_console(&mut c);
    let _ = c.status();
}

pub fn spawn(url: String, dir: PathBuf, opts: Options, tx: Sender<Ev>) -> DlHandle {
    let pids = Arc::new(Mutex::new(Vec::new()));
    let cancelled = Arc::new(AtomicBool::new(false));
    let snap = Arc::new(snapshot_dir(&dir));
    let worker = WorkerArgs {
        url,
        dir: dir.clone(),
        tx,
        pids: pids.clone(),
        cancelled: cancelled.clone(),
        snap: snap.clone(),
        opts,
    };
    std::thread::spawn(move || worker.run());
    DlHandle {
        pids,
        cancelled,
        dir,
        snap,
    }
}

struct WorkerArgs {
    url: String,
    dir: PathBuf,
    tx: Sender<Ev>,
    pids: Arc<Mutex<Vec<u32>>>,
    cancelled: Arc<AtomicBool>,
    snap: Arc<HashSet<String>>,
    opts: Options,
}

// ---------------------------------------------------------------------------
// logging — one shared file in %TEMP%, size-capped
// ---------------------------------------------------------------------------

pub fn log_path() -> PathBuf {
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
                let old = temp.join("cutie.log.1");
                // Windows rename does not replace an existing destination.
                let _ = std::fs::remove_file(&old);
                let _ = std::fs::rename(&p, old);
            }
        }
    });
}

pub fn log_line(tag: &str, s: &str) {
    use std::io::Write;
    // `CUTIE_DEBUG=1` mirrors the log to stderr (debug builds have a console).
    static MIRROR: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("CUTIE_DEBUG").is_some());
    if *MIRROR {
        eprintln!("[{tag}] {s}");
    }
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
        // `.part`/`.f137.mp4` leftovers are resume data, never the result.
        if is_partial_name(&name) {
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
            cancelled,
            snap,
            opts,
        } = self;
        log_line("run", &format!("url={url} dir={}", dir.display()));

        let Some(ytdlp) = find_ytdlp() else {
            let _ = tx.send(Ev::Failed {
                msg: "yt-dlp is missing ✨ winget install yt-dlp".into(),
            });
            return;
        };

        if let Err(e) = std::fs::create_dir_all(&dir) {
            let _ = tx.send(Ev::Failed {
                msg: format!("can't make the folder: {e}"),
            });
            return;
        }

        let tmpl = "<<<CUTIE>>>PROG|%(progress.downloaded_bytes)s|%(progress.total_bytes)s|%(progress.total_bytes_estimate)s|%(progress.speed)s|%(progress.eta)s";
        let meta_tpl = "before_dl:<<<CUTIE>>>META|%(title)s\u{1f}%(uploader)s\u{1f}%(duration)s\u{1f}%(filesize_approx)s\u{1f}%(thumbnail)s\u{1f}%(playlist_index)s\u{1f}%(playlist_count)s";
        let file_tpl = "after_move:<<<CUTIE>>>FILE|%(filepath)s";
        // printed right before the merger/extractor runs
        let post_tpl = "post_process:<<<CUTIE>>>POST";

        let mut cmd = Command::new(&ytdlp);
        // Explorer may hand us a PATH from before ffmpeg/node were installed;
        // yt-dlp looks them up there, so give it the widened one.
        if let Ok(path) = std::env::join_paths(tool_dirs()) {
            cmd.env("PATH", path);
        }
        cmd.current_dir(&dir)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .args([
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
                "--concurrent-fragments",
                "4",
            ]);

        if opts.playlist {
            cmd.args(["--yes-playlist"]);
        } else {
            cmd.args(["--no-playlist"]);
        }

        if opts.audio_only {
            cmd.args([
                "-f",
                "ba/b",
                "-x",
                "--audio-format",
                "mp3",
                "--audio-quality",
                "0",
            ]);
        } else {
            cmd.args([
                "-f",
                "bv*[vcodec^=avc1]+ba[ext=m4a]/bv*+ba/b[ext=mp4]/b",
                "--merge-output-format",
                "mp4/mkv",
            ]);
        }

        cmd.args([
            "--embed-metadata",
            "--no-mtime",
            "--windows-filenames",
            "--progress-template",
            tmpl,
            "--print",
            meta_tpl,
            "--print",
            file_tpl,
            "--print",
            post_tpl,
            "-o",
            "%(title).150B [%(id)s].%(ext)s",
            &url,
        ]);

        if which("node.exe").is_some() {
            cmd.args(["--js-runtimes", "node"]);
        }

        if cancelled.load(Ordering::SeqCst) {
            log_line("run", "cancelled before spawn");
            return;
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

        // `cancel()` may have run between the spawn and the pid registration.
        if cancelled.load(Ordering::SeqCst) {
            kill_pid(my_pid);
            let _ = child.wait();
            lock(&pids).retain(|&p| p != my_pid);
            sweep_leftovers(&dir, &snap);
            log_line("dl", "cancelled right after spawn");
            return;
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // yt-dlp prints the final path (after_move) as a marked line;
        // everything else on stdout is either our marker or noise.
        let final_path: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let tracker: Arc<Mutex<ProgressTracker>> = Arc::new(Mutex::new(ProgressTracker::new()));

        let tx1 = tx.clone();
        let fp1 = final_path.clone();
        let tr1 = tracker.clone();
        let t = std::thread::spawn(move || {
            for line in lines(stdout) {
                log_line("out", &line);
                handle_line(&line, &tx1, &tr1, Some(&fp1));
            }
        });

        let last_err: Arc<Mutex<ErrLines>> = Arc::new(Mutex::new(ErrLines::default()));
        let le2 = last_err.clone();
        let tx2 = tx.clone();
        let tr2 = tracker.clone();
        let t2 = std::thread::spawn(move || {
            for line in lines(stderr) {
                lock(&le2).push(&line);
                log_line("err", &line);
                handle_line(&line, &tx2, &tr2, None);
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
                // Keep the .part files: yt-dlp resumes them on Retry. The
                // snapshot still tracks pre-existing files, so a later
                // successful download cannot pick up an old leftover.
                let msg = lock(&last_err).message();
                let _ = tx.send(Ev::Failed { msg });
            }
            Err(e) => {
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

fn handle_line(
    line: &str,
    tx: &Sender<Ev>,
    tracker: &Arc<Mutex<ProgressTracker>>,
    final_path: Option<&Arc<Mutex<String>>>,
) {
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
        let pr = lock(tracker).push(downloaded, total, speed);
        let _ = tx.send(Ev::Progress {
            downloaded: pr.downloaded,
            total: pr.total,
            speed: pr.speed,
            eta: if pr.eta > 0 {
                fmt_dur(pr.eta)
            } else {
                String::new()
            },
        });
    } else if let Some(rest) = line.strip_prefix("<<<CUTIE>>>META|") {
        let meta = parse_meta_line(rest);
        {
            // A new video starts here — playlist items get their own progress.
            let mut tr = lock(tracker);
            tr.reset();
            tr.set_grand_total(meta.size_hint);
        }
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
            index: meta.playlist_index,
            count: meta.playlist_count,
        });
    } else if line == "<<<CUTIE>>>POST" {
        let _ = tx.send(Ev::Processing);
    } else if let Some(rest) = line.strip_prefix("<<<CUTIE>>>FILE|") {
        if let Some(fp) = final_path {
            let path = rest.trim();
            if !path.is_empty() {
                *lock(fp) = path.to_string();
            }
        }
    }
}

struct MetaLine {
    title: String,
    channel: String,
    duration: String,
    size_hint: Option<u64>,
    thumbnail: Option<String>,
    playlist_index: Option<u64>,
    playlist_count: Option<u64>,
}

/// yt-dlp writes `NA` for every field it does not know.
fn na(s: &str) -> &str {
    let s = s.trim();
    if s.is_empty() || s == "NA" {
        ""
    } else {
        s
    }
}

fn parse_meta_line(rest: &str) -> MetaLine {
    let f: Vec<&str> = rest.split('\u{1f}').collect();
    let get = |i: usize| -> &str { na(f.get(i).copied().unwrap_or("")) };
    let title = get(0);
    MetaLine {
        title: if title.is_empty() {
            "something lovely ✨".into()
        } else {
            title.to_string()
        },
        channel: get(1).to_string(),
        duration: get(2)
            .parse::<f64>()
            .ok()
            .filter(|v| *v >= 0.0)
            .map(|v| fmt_dur(v as u64))
            .unwrap_or_default(),
        size_hint: get(3).parse::<u64>().ok().filter(|v| *v > 0),
        thumbnail: Some(get(4).to_string()).filter(|s| s.starts_with("http")),
        playlist_index: get(5).parse::<u64>().ok(),
        playlist_count: get(6).parse::<u64>().ok(),
    }
}

fn fetch_thumbnail(url: &str) -> Option<(Vec<u8>, u32, u32)> {
    let resp = ureq::get(url)
        .set("User-Agent", concat!("cutie/", env!("CARGO_PKG_VERSION")))
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

/// The stderr lines worth showing when yt-dlp fails.
#[derive(Default)]
struct ErrLines {
    /// Last `ERROR:` line — the real reason.
    error: String,
    /// Last other non-warning line, for when there is no `ERROR:`.
    other: String,
}

impl ErrLines {
    fn push(&mut self, line: &str) {
        let t = line.trim();
        if t.is_empty() || t.starts_with("<<<CUTIE>>>") || t.starts_with("WARNING") {
            return;
        }
        let slot = if t.starts_with("ERROR:") {
            &mut self.error
        } else {
            &mut self.other
        };
        *slot = t.chars().take(600).collect();
    }

    fn message(&self) -> String {
        let raw = if self.error.is_empty() {
            &self.other
        } else {
            &self.error
        };
        if raw.is_empty() {
            "yt-dlp failed without saying why".into()
        } else {
            friendly_err(raw)
        }
    }
}

/// `ERROR: [youtube] abc123: Video unavailable` → `Video unavailable`, plus a
/// hint for the failures that have a known fix.
fn friendly_err(line: &str) -> String {
    let mut s = strip_err(line);
    // drop the `[extractor] video-id: ` prefix
    if let Some((_, rest)) = s.strip_prefix('[').and_then(|r| r.split_once("] ")) {
        s = match rest.split_once(": ") {
            Some((id, msg)) if !id.contains(' ') && !msg.is_empty() => msg,
            _ => rest,
        };
    }
    // `… (caused by <HTTPError 404: Not Found>)` only repeats the message
    if let Some(i) = s.find(" (caused by ") {
        s = &s[..i];
    }
    let lower = s.to_ascii_lowercase();
    let hint = if lower.contains("unsupported url") {
        Some("this site isn't supported by yt-dlp")
    } else if lower.contains("ffmpeg") && lower.contains("not found") {
        Some("ffmpeg is missing — winget install ffmpeg")
    } else if [
        "http error 403",
        "nsig extraction failed",
        "signature extraction failed",
        "unable to extract",
        "requested format is not available",
    ]
    .iter()
    .any(|k| lower.contains(k))
    {
        Some("updating yt-dlp usually fixes this")
    } else {
        None
    };
    match hint {
        Some(h) => format!("{s} — {h}"),
        None => s.to_string(),
    }
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

/// `48.9 / 255.5 MB` — the unit is only spelled out once when both match.
pub fn fmt_bytes_pair(done: u64, total: u64) -> String {
    let (a, b) = (fmt_bytes(done), fmt_bytes(total));
    match (a.rsplit_once(' '), b.rsplit_once(' ')) {
        (Some((av, au)), Some((_, bu))) if au == bu => format!("{av} / {b}"),
        _ => format!("{a} / {b}"),
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
    which("yt-dlp.exe")
}

/// Where tools are looked up: the folder of cutie.exe first, then PATH, then
/// the folders the usual installers use. Explorer keeps the PATH it started
/// with, so a yt-dlp installed later is otherwise invisible to the context
/// menu until the next sign-in.
fn tool_dirs() -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(Path::to_path_buf))
    {
        dirs.push(dir);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let known = [
        env("LOCALAPPDATA").map(|d| d.join(r"Microsoft\WinGet\Links")),
        env("USERPROFILE").map(|d| d.join(r"scoop\shims")),
        env("ProgramData").map(|d| d.join(r"chocolatey\bin")),
        env("ProgramFiles").map(|d| d.join("nodejs")),
    ];
    for d in known.into_iter().flatten() {
        if d.is_dir() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

fn which(name: &str) -> Option<PathBuf> {
    tool_dirs()
        .into_iter()
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_line_parses_full() {
        let rest = "عنوان الفيديو\u{1f}Channel Name\u{1f}633\u{1f}13644933\u{1f}https://i.ytimg.com/vi/x/hq.jpg\u{1f}NA\u{1f}NA";
        let m = parse_meta_line(rest);
        assert_eq!(m.title, "عنوان الفيديو");
        assert_eq!(m.channel, "Channel Name");
        assert_eq!(m.duration, "10:33");
        assert_eq!(m.size_hint, Some(13644933));
        assert!(m.thumbnail.unwrap().starts_with("http"));
        assert_eq!(m.playlist_index, None);
    }

    #[test]
    fn meta_line_na_fields() {
        let m = parse_meta_line("T\u{1f}NA\u{1f}NA\u{1f}NA\u{1f}NA\u{1f}NA\u{1f}NA");
        assert_eq!(m.title, "T");
        assert_eq!(m.channel, "");
        assert_eq!(m.duration, "");
        assert_eq!(m.size_hint, None);
        assert!(m.thumbnail.is_none());
    }

    #[test]
    fn meta_line_short_and_empty() {
        let m = parse_meta_line("");
        assert!(!m.title.is_empty());
        assert!(m.channel.is_empty() && m.duration.is_empty());
        assert!(m.thumbnail.is_none());
    }

    #[test]
    fn meta_line_playlist_position() {
        let m = parse_meta_line("T\u{1f}c\u{1f}10\u{1f}1\u{1f}NA\u{1f}2\u{1f}5");
        assert_eq!(m.playlist_index, Some(2));
        assert_eq!(m.playlist_count, Some(5));
    }

    #[test]
    fn progress_tracker_sums_streams() {
        let mut t = ProgressTracker::new();
        // the video stream
        t.push(100, 400, 100.0);
        let p = t.push(200, 400, 100.0);
        assert_eq!(p.downloaded, 200);
        assert_eq!(p.total, 400);
        // a new stream starts → the finished one counts towards the total
        let p = t.push(50, 200, 100.0);
        assert_eq!(p.downloaded, 450, "finished stream must be added");
        assert_eq!(p.total, 600);
    }

    #[test]
    fn progress_tracker_uses_grand_total() {
        let mut t = ProgressTracker::new();
        t.set_grand_total(Some(600));
        let p = t.push(100, 400, 100.0);
        assert_eq!(p.total, 600, "the whole video is the yardstick");
        let p = t.push(50, 200, 100.0);
        assert_eq!(p.downloaded, 450);
        assert_eq!(p.total, 600);
    }

    #[test]
    fn progress_tracker_never_goes_backwards() {
        let mut t = ProgressTracker::new();
        t.set_grand_total(Some(600));
        let mut last = 0;
        for (d, tot) in [(10, 400), (200, 400), (400, 400), (10, 200), (200, 200)] {
            let p = t.push(d, tot, 100.0);
            assert!(p.downloaded >= last, "progress went backwards: {p:?}");
            last = p.downloaded;
        }
        assert_eq!(last, 600);
    }

    #[test]
    fn progress_tracker_eta_and_speed() {
        let mut t = ProgressTracker::new();
        t.set_grand_total(Some(1000));
        let p = t.push(0, 1000, 100.0);
        assert_eq!(p.speed, 100.0);
        assert_eq!(p.eta, 10);
        // a stalled stream reports no speed → no eta
        let mut t2 = ProgressTracker::new();
        t2.set_grand_total(Some(1000));
        let p = t2.push(0, 1000, 0.0);
        assert_eq!(p.speed, 0.0);
        assert_eq!(p.eta, 0);
    }

    #[test]
    fn progress_tracker_resets_for_the_next_video() {
        let mut t = ProgressTracker::new();
        t.push(100, 400, 100.0);
        t.reset();
        let p = t.push(10, 200, 100.0);
        assert_eq!(p.downloaded, 10);
        assert_eq!(p.total, 200);
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
    fn find_new_media_ignores_resume_leftovers() {
        let dir = std::env::temp_dir().join("cutie-test-find-new-media");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let snap: HashSet<String> = ["old [id].mp4".to_string()].into_iter().collect();
        std::fs::write(dir.join("clip [id].f137.mp4"), b"x").unwrap();
        std::fs::write(dir.join("clip [id].part"), b"x").unwrap();
        assert!(
            find_new_media(&dir, &snap).is_none(),
            "resume leftovers must not be reported as the finished file"
        );
        std::fs::write(dir.join("clip [id].mp4"), b"x").unwrap();
        let found = find_new_media(&dir, &snap).unwrap();
        assert!(found.ends_with("clip [id].mp4"), "got {found:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ctrl_c_flag_starts_clear() {
        assert!(!ctrl_c_pressed());
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
    fn friendly_err_drops_extractor_noise() {
        assert_eq!(
            friendly_err("ERROR: [youtube] xxxxxxxxxxx: This video is unavailable"),
            "This video is unavailable"
        );
        assert_eq!(
            friendly_err("ERROR: [generic] 'foo' is not a valid URL."),
            "'foo' is not a valid URL."
        );
        assert_eq!(
            friendly_err("ERROR: [generic] x: Unable to download webpage: HTTP Error 404: Not Found (caused by <HTTPError 404: Not Found>)"),
            "Unable to download webpage: HTTP Error 404: Not Found"
        );
        assert!(friendly_err("ERROR: Unsupported URL: https://x.tld").contains("isn't supported"));
        assert!(
            friendly_err("ERROR: [youtube] abc: HTTP Error 403: Forbidden")
                .ends_with("updating yt-dlp usually fixes this")
        );
    }

    #[test]
    fn err_lines_prefer_the_error() {
        let mut e = ErrLines::default();
        e.push("WARNING: something");
        e.push("ERROR: [youtube] abc: Private video");
        e.push("some trailing noise");
        assert_eq!(e.message(), "Private video");
        assert!(!ErrLines::default().message().is_empty());
    }

    #[test]
    fn bytes_pair_shares_the_unit() {
        assert_eq!(fmt_bytes_pair(5 * 1048576, 10 * 1048576), "5.0 / 10.0 MB");
        assert_eq!(fmt_bytes_pair(512, 10 * 1048576), "512 B / 10.0 MB");
    }

    #[test]
    fn post_marker_becomes_processing() {
        let (tx, rx) = std::sync::mpsc::channel();
        let tr = Arc::new(Mutex::new(ProgressTracker::new()));
        handle_line("<<<CUTIE>>>POST", &tx, &tr, None);
        assert!(matches!(rx.try_recv(), Ok(Ev::Processing)));
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
