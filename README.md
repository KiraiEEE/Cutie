# cutie ✨

A tiny Windows context-menu downloader: right-click a folder, hit **Download with Cutie**,
and a small popup shows progress while `yt-dlp` saves the link from your clipboard into that
folder. Also works as a plain CLI.

- **Windows 10/11**, no admin rights required (installs to `%LOCALAPPDATA%\cutie`)
- **Clipboard-aware** — launch it with a video link already copied
- **Queue friendly** — several links on the clipboard are downloaded one after another
- **Cute progress popup** (egui/eframe) with download speed, ETA, and thumbnail
- **Honest progress** — video and audio streams are summed up, so the bar never restarts
- **Right-to-left text** — Arabic/Hebrew titles render properly (custom OpenType shaping
  pipeline with `unicode-bidi` + `rustybuzz`)
- **Headless mode** for scripts/terminals with exit codes
- **Readable errors** — yt-dlp's message without the extractor noise, a hint when the
  fix is known (outdated yt-dlp, missing ffmpeg), and a *Copy error* button
- **Resumable retries** — a failed download keeps its `.part` files, so *Retry*
  continues where it stopped instead of starting over
- **Ctrl+C safe** — in headless mode, Ctrl+C kills the whole yt-dlp/ffmpeg tree
  (nothing keeps downloading in the background)

## Requirements

| | |
|---|---|
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | on `PATH`, next to `cutie.exe`, or in the winget / scoop / Chocolatey folders |
| [ffmpeg](https://ffmpeg.org) | for merging/extracting (usually ships with yt-dlp installs) |
| `node` (optional) | yt-dlp uses it for JS-challenge sites |

## Install

**Setup (recommended):** run `cutie-<version>-setup.exe`. It needs no admin rights, and it:

- installs cutie into `%LOCALAPPDATA%\cutie`
- adds **Download with Cutie** to the Explorer context menu, plus a Start-Menu shortcut
- installs whatever is missing of yt-dlp, ffmpeg and Deno (yt-dlp's JavaScript runtime
  for YouTube) through winget — or downloads `yt-dlp.exe` directly when winget is missing
- shows up in *Settings → Apps*, where uninstalling removes all of it

**Portable:** unzip `cutie-<version>-portable.zip` and register the menu yourself:

```
cutie.exe --install
```

Copy itself into `%LOCALAPPDATA%\cutie`, registers a **Download with Cutie** entry on the
background of any folder in Explorer, and adds a Start-Menu shortcut. On Windows 11 the
entry lives under *Show more options*. Remove everything with:

```
cutie.exe --uninstall
```

## Usage

```
cutie.exe                          download the clipboard link into Downloads
cutie.exe <dir>                    …into <dir>
cutie.exe <url> [dir]              download <url> (popup)
cutie.exe <url> <url2> [dir]       download several links, one after another
cutie.exe --download <url>… [dir]  headless: progress to stdout, exit code 0/1
cutie.exe --into <dir>             pick the target folder (also --into=<dir>)
cutie.exe --audio                  save the audio track as mp3
cutie.exe --yes-playlist           download every entry of a playlist
cutie.exe --install / --uninstall  manage the Explorer context menu
cutie.exe --help / --version
```

Headless example:

```
cutie.exe --download "https://www.youtube.com/watch?v=jNQXAC9IVRw" --into D:\videos
```

Exit codes: `0` success · `1` the download failed · `2` bad usage · `130` interrupted (Ctrl+C).

## Notes

- **Progress** — `yt-dlp` fetches the video and the audio stream as two separate transfers.
  cutie adds up the finished streams and uses yt-dlp's `filesize_approx` as the yardstick,
  so the bar only ever moves forward. While the streams are being merged the popup says
  *merging…* instead of freezing at 100 %.
- **The popup** shows the title, channel, duration, size, speed and time left, then
  *Finishing up…* while yt-dlp merges the streams. Drag anywhere on the card to move it,
  click the thumbnail to open the source page. When it is done: **Play** opens the file,
  **Show in folder** selects it in Explorer. `Esc` cancels, `Enter` shows the file
  (or retries after an error). Errors stay up until you close them.
- **The queue** — if the clipboard holds more than one link, cutie downloads them one after
  another and shows `2/4` while it works. A failing link does not stop the rest, and the
  failure count is kept for the whole queue. Duplicate links are downloaded once.
- **Retry resumes** — when a download fails, its `.part` files stay in the folder and
  *Retry* continues them. Closing the popup with ✕/Esc aborts and cleans them up.
- **Placement** — the popup opens next to the cursor and uses the DPI of *that* monitor, so
  it lands correctly on mixed-DPI setups.
- **Finding tools** — Explorer keeps the `PATH` it started with, so a yt-dlp/ffmpeg installed
  afterwards would be invisible to the context menu until you sign in again. cutie also
  looks in the winget, scoop and Chocolatey folders and passes that wider `PATH` on to
  yt-dlp.

## Build from source

```
cargo build --release
cargo test
```

Release artifacts (setup + portable zip, into `dist\`) need
[Inno Setup 6](https://jrsoftware.org/isinfo.php):

```
powershell -File installeruild.ps1
```

Rust 1.80+, targets Windows only (`windows-sys` for the native bits). The app icon is
generated at build time by `build.rs` — no binary assets in the repo.

### Debugging

Release builds are GUI-subsystem apps (no console window from Explorer). **Debug builds keep
the console**, so panics and logs show up in the terminal you ran them from:

```
set CUTIE_DEBUG=1
cargo run -- "https://www.youtube.com/watch?v=jNQXAC9IVRw" D:\videos
```

`CUTIE_DEBUG=1` mirrors every log line (including yt-dlp's raw output) to stderr; it works
for release builds too when started from a terminal. Panics are always written to the log,
even in release builds where they would otherwise vanish.

## Troubleshooting

- Log with yt-dlp's full output: `%TEMP%\cutie.log` (rotated, capped at 256 KB).
  *Copy error* in the popup copies the message, the link and the log path in one go.
- Errors like *HTTP Error 403*, *nsig extraction failed* or *Requested format is not
  available* usually mean yt-dlp is out of date — update it (`winget upgrade yt-dlp`,
  `choco upgrade yt-dlp` or `yt-dlp -U`)
- Popup opens but nothing happens → check that `yt-dlp --version` runs in a terminal
- CLI prints nothing → make sure you are running `cutie.exe` from a terminal, not
  double-clicked (GUI apps only print when attached to a console)

## License

MIT
