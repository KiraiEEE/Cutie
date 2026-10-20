# cutie

A tiny Windows context-menu downloader: right-click a folder, hit **Download with Cutie**,
and a small popup shows progress while `yt-dlp` saves the link from your clipboard into that
folder. Also works as a plain CLI.

- **Windows 10/11**, no admin rights required (installs to `%LOCALAPPDATA%\cutie`)
- **Clipboard-aware** — launch it with a video link already copied
- **Cute progress popup** (egui/eframe) with download speed, ETA, and thumbnail
- **Right-to-left text** — Arabic/Hebrew titles render properly (custom OpenType shaping
  pipeline with `unicode-bidi` + `rustybuzz`)
- **Headless mode** for scripts/terminals with exit codes
- **Errors surface visually** — a toast pops instead of failing silently

## Requirements

| | |
|---|---|
| [yt-dlp](https://github.com/yt-dlp/yt-dlp) | on `PATH`, or `yt-dlp.exe` next to `cutie.exe` |
| [ffmpeg](https://ffmpeg.org) | for merging/extracting (usually ships with yt-dlp installs) |
| `node` (optional) | yt-dlp uses it for JS-challenge sites |

## Install the context menu

```
cutie.exe --install
```

Copy itself into `%LOCALAPPDATA%\cutie`, registers a **Download with Cutie** entry on the
background of any folder in Explorer, and adds a Start-Menu shortcut. Remove everything
with:

```
cutie.exe --uninstall
```

## Usage

```
cutie.exe                          download the clipboard link into Downloads
cutie.exe <dir>                    …into <dir>
cutie.exe <url> [dir]              download <url> (popup)
cutie.exe --download <url> [dir]   headless: progress to stdout, exit code 0/1
cutie.exe --install / --uninstall  manage the Explorer context menu
cutie.exe --help / --version
```

Headless example:

```
cutie.exe --download "https://www.youtube.com/watch?v=jNQXAC9IVRw" --into D:\videos
```

## Build from source

```
cargo build --release
cargo test
```

Rust 1.80+, targets Windows only (`windows-sys` for the native bits). The app icon is
generated at build time by `build.rs` — no binary assets in the repo.

## Troubleshooting

- Log with yt-dlp's full output: `%TEMP%\cutie.log` (rotated, capped at 256 KB)
- Popup opens but nothing happens → check that `yt-dlp --version` runs in a terminal
- CLI prints nothing → make sure you are running `cutie.exe` from a terminal, not
  double-clicked (GUI apps only print when attached to a console)

## License

MIT
