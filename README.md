# LDM – Linux Download Manager

**English** | [简体中文](README.zh-CN.md)

![LDM main window with a download in progress](assets/screenshots/en/hero.png)

LDM is a download manager for Linux with a focus on finding and saving video from web pages.
It has a background daemon, a command-line client, a Qt (PySide6) desktop GUI and a browser
extension for Chromium-based browsers and Firefox.

## Features

- **HTTP(S) / FTP / FTPS**: multiple connections per file with dynamic splitting, resume
  (also after a crash), global and per-download speed limits, mirrors, SHA-256/MD5 checks,
  per-site logins, cookie refresh from the browser, queues with time windows, and
  "when done" actions.
- **Streaming media**: HLS (AES-128, fMP4, byte ranges, separate audio, audio-only) and DASH
  (SegmentTemplate / Timeline / List), muxed into MP4 with ffmpeg. Live recording with a
  duration limit and a clean "stop recording". Subtitle tracks are saved as `.vtt`/`.srt`.
  Encrypted (DRM) streams are not supported.
- **Daemon** `ldmd`: a Unix-socket IPC and a session D-Bus interface (`AddUrl`, `List`,
  `Pause`, `Resume`, `Status`, tool management).
- **Browser integration**: a Manifest V3 extension (Chromium and Firefox) that can take over
  downloads by file type, shows a "Download this video" button on the main player with a
  quality menu, ranks detected media so likely ads are listed last (never hidden), and offers
  "Download all links". It talks to the daemon through a native-messaging host; an optional
  local WebSocket bridge is also available.
- **Clients**: `ldm` CLI and `ldm-gui` (categories, per-connection progress, scheduler,
  batch downloads, site grabber, clipboard monitor, import/export; English and Simplified
  Chinese).
- **yt-dlp integration** (optional): YouTube and a fallback for other sites, with yt-dlp and
  deno downloaded and verified by LDM on first use.
- **Operations**: `ldm doctor`, `ldm diag` (redacted diagnostics bundle), `ldm metrics`
  (Prometheus text format). URLs and credentials in logs are redacted.

## Screenshots

| | |
|---|---|
| ![Main window](assets/screenshots/en/main-window.png) | ![Download progress](assets/screenshots/en/progress.png) |
| **Main window**: downloads sorted by category, with a details panel. | **Progress**: multiple connections with live per-segment progress. |
| ![Add download](assets/screenshots/en/add-download.png) | ![Options](assets/screenshots/en/settings.png) |
| **Add download**: folder, file name, queue, connections and login. | **Options**: connections, limits, retries and per-host limits. |
| ![Scheduler](assets/screenshots/en/scheduler.png) | |
| **Scheduler**: queues with time windows and weekdays. | |

## Repository layout

```
crates/ldm-core        engine: HTTP/FTP, HLS/DASH, live recording, muxing, SQLite store,
                       scheduler, PAC, tool manager
crates/ldm-ipc         JSON envelope protocol {v,id,type,payload} and framing
crates/ldm-daemon      ldmd (Unix socket, single instance)
crates/ldm-native-host browser native-messaging host (starts ldmd when needed)
crates/ldm-ws-bridge   optional WebSocket bridge on 127.0.0.1
crates/ldm-cli         ldm command-line client
tools/ldm-testsrv      HTTP/HLS/DASH test server used by the tests
extension/             TypeScript MV3 extension (built with esbuild)
gui/ldm_gui/           PySide6 GUI and D-Bus service
packaging/             install scripts, deb, AppImage, Flatpak, systemd, desktop files
rules/bundled/         example site rules
tests/e2e/             end-to-end tests (pytest)
scripts/check.sh       runs every check
```

## Building

Requirements: Rust 1.85+, Node.js 20+ (extension), Python 3.10+ with PySide6 (GUI),
and ffmpeg (recommended; without it HLS/DASH are saved as raw `.ts`/fMP4).

```sh
cargo build --release                                     # ldmd, ldm, ldm-native-host
scripts/build-release.sh                                  # same, for packages: strips local build paths
(cd extension && npm ci && npm run build)                 # extension/dist/{chromium,firefox}
python3 -m venv .venv && .venv/bin/pip install -r requirements-dev.txt   # GUI and tests
```

### Store build of the extension

```sh
(cd extension && npm run build:store)     # or: LDM_STORE_BUILD=1 npm run build
```

This writes `extension/dist-store/{chromium,firefox}`. The store build has **no yt-dlp
integration**: no yt-dlp menu entries or page rules, and nothing is offered on YouTube pages.
It also drops the pinned `"key"` from the Chromium manifest, because stores assign their own
extension ID. Add that ID to the native-messaging manifest (`allowed_origins`) when you install
LDM for a store-installed extension. The normal build (`npm run build`) is unchanged.

## Installing

```sh
packaging/install.sh                 # per user, into ~/.local
sudo packaging/install.sh --system   # system-wide, into /usr
packaging/build-deb.sh               # dist/ldm_<version>_<arch>.deb
packaging/uninstall.sh [--purge]     # --purge also removes user data
```

`install.sh` installs `ldmd`, `ldm`, `ldm-gui`, `ldm-dbus` and the native host, the rules,
GUI and extension, the native-messaging manifests for Chrome, Chromium, Edge, Brave and Firefox,
the systemd user service `ldmd.service` and a desktop entry. `build-deb.sh` takes the maintainer
from `LDM_MAINTAINER="Name <email>"`.

Loading the extension:

- Chrome / Chromium: open `chrome://extensions`, turn on developer mode, choose "Load unpacked"
  and pick `~/.local/lib/ldm/extension/chromium`. The `key` in the manifest pins the ID in
  `extension/EXTENSION_ID`, which the native-messaging manifest allows.
- Firefox: open `about:debugging`, choose "Load Temporary Add-on" and pick
  `extension/dist/firefox/manifest.json`.

Run `ldm doctor` afterwards to check ffmpeg, the socket, the daemon and the browser manifests.

### AppImage

```sh
packaging/appimage/build-appimage.sh      # dist/LDM-<version>-x86_64.AppImage
```

The image contains ldmd, the CLI, the native host and the GUI with its own CPython and a
trimmed PySide6 (yt-dlp is not included). Run it directly for the GUI, as
`LDM-….AppImage ldm …` for the CLI, and with `--install-integration` to add the desktop entry
and the browser native-messaging manifests for the current user (`--remove-integration` removes
them). It needs FUSE and a glibc at least as new as the build machine's.

The build needs `mksquashfs` (squashfs-tools) and a Python with PySide6 (`PYTHON`, default
`.venv/bin/python`); it builds the release binaries and the extension if they are missing. The
AppImage type-2 runtime is downloaded once into `~/.cache/ldm-appimage`; pass a local file with
`RUNTIME=…`, or pin it with `RUNTIME_SHA256=…`. License texts (LDM, third-party notices, Qt/PySide6
LGPL, CPython) are installed in the image under `usr/lib/ldm/`.

### Flatpak

The manifest is `packaging/flatpak/io.github.wzk4work.LDM.yml` (org.kde.Platform 6.11 and
io.qt.PySide.BaseApp; ffmpeg is built from source as a module; Rust crates are vendored for an
offline build).

```sh
python3 packaging/flatpak/gen-cargo-sources.py     # after every Cargo.lock change
flatpak install --user flathub org.kde.Sdk//6.11 org.kde.Platform//6.11 \
    io.qt.PySide.BaseApp//6.11 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --force-clean --repo=repo build-dir packaging/flatpak/io.github.wzk4work.LDM.yml
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    repo dist/LDM-0.9.0.flatpak io.github.wzk4work.LDM
flatpak install --user dist/LDM-0.9.0.flatpak
flatpak run io.github.wzk4work.LDM                         # GUI
flatpak run --command=ldm io.github.wzk4work.LDM --help    # CLI
packaging/flatpak/host-integration.sh install               # on the host, for the extension
packaging/flatpak/smoke.sh                                  # optional smoke test of the installed app
```

- The app can write to `~/Downloads` (xdg-download). For other folders, run
  `flatpak override --user --filesystem=<dir> io.github.wzk4work.LDM` first; otherwise the
  daemon refuses the folder and says so.
- License texts (LDM, third-party notices, FFmpeg) are installed in
  `/app/share/licenses/io.github.wzk4work.LDM/`. The bundle contains FFmpeg (LGPL-2.1+). If you
  distribute the bundle, also offer the matching sources, for example by building with
  `flatpak-builder --bundle-sources` and publishing the resulting source runtime.

## Usage

```sh
ldm daemon start                     # usually started by systemd or the native host
ldm add URL [-d DIR] [-n NAME] [-c CONNS] [--referrer R] [--cookie-file cookies.txt] [-w]
ldm add URL --kind hls --height 720
ldm list [--json] | ldm info ID | ldm watch [ID]
ldm pause ID|--all | ldm resume ID|--all | ldm rm ID [--delete-files]
ldm stop-recording ID                # stop a live recording and keep a playable file
ldm set ID --priority 5 --queue main --conns 8
ldm probe URL.m3u8                   # list HLS/DASH variants
ldm queue list | ldm queue set main --concurrent 2 --window 01:00-07:00
ldm config get | ldm config set engine.speed_limit_bps 1048576 | ldm config path
ldm export --format csv -o jobs.csv | ldm import jobs.json
ldm get URL [-d DIR|-o FILE] | ldm hls URL | ldm dash URL   # one-off, without the daemon
ldm add ftpes://user:pass@host/file.iso   # explicit FTPS; ftps:// is implicit (port 990)
ldm diag [-o FILE] | ldm metrics | ldm doctor
ldm-gui                              # GUI (LDM_LANG=en for English)
ldm-dbus                             # only the D-Bus service
```

IDs may be shortened to a unique prefix. `ldm` exits with code 3 when the daemon is not running.

### Configuration

`~/.config/ldm/config.toml` (override with `LDM_CONFIG`); database
`~/.local/share/ldm/downloads.db` (`LDM_DB`); socket `$XDG_RUNTIME_DIR/ldm.sock`
(`LDM_SOCKET`); downloads go to `XDG_DOWNLOAD_DIR` by default. Common keys:

| Key | Default | Meaning |
|---|---|---|
| `engine.proxy` | `system` | `none`, `system`, `browser` (follow the extension), `pac`, or a proxy URL |
| `engine.pac_url` / `engine.pac_refresh_secs` | `""` / `1800` | PAC script (`http(s)://`, `file://` or a path) and refresh interval |
| `engine.conflict_policy` | `rename_numbered` | `rename_numbered`, `ask` or `overwrite` |
| `ftp.tls` | `off` | `off`, `try` or `require` for `ftp://` |
| `media.live_record` / `media.live_max_duration` | `true` / `0` | live recording and its limit in seconds (0 = none) |
| `media.subtitles` | `true` | save subtitle tracks |
| `media.ad_ranking` / `media.ad_domains_file` | `true` / `""` | rank likely ads last; extra ad-domain list |
| `media.audio_preference` | `aac` | `aac`, `most_channels` or `language:<code>` |
| `browser.ws_bridge_enabled` / `browser.ws_bridge_token` | `false` / `""` | optional WebSocket bridge, see [SECURITY.md](SECURITY.md) |

PAC scripts run in a small sandboxed interpreter that implements the usual PAC subset and
helper functions; unsupported syntax makes the script fail to load (logged), and LDM then
connects directly, like browsers do. TLS uses rustls with the bundled webpki roots;
`[[sites.tls]] accept_invalid_cert = true` can be set per host for self-signed FTPS servers.

### WebSocket bridge (off by default)

`ldmd --ws [--ws-port 17001]` or `browser.ws_bridge_enabled = true`. It listens on 127.0.0.1
only, prints the actual port as `WS_PORT n`, uses the `ldm.bridge.v1` subprotocol (JSON
envelopes), rejects web-page origins and can require a token. See [SECURITY.md](SECURITY.md).

### Optional page hook (off by default)

The "Page hook" checkbox in the extension popup installs read-only observers in the page:
`fetch`/`XMLHttpRequest` URLs, Media Source Extensions codecs, and the count and size of
`appendBuffer` calls. It only observes: every hook calls the original and returns its result
unchanged, and media data is never read, copied or stored. It helps match the player that is
actually playing to its stream and shows codecs in the menu. With the box unchecked (the
default) no LDM code runs in the page's own JavaScript context.

## yt-dlp integration

LDM does not ship yt-dlp and does not implement YouTube signature or token handling itself.
When yt-dlp is needed for the first time, LDM asks (GUI dialog, CLI prompt, or
`ldm tools install`) and then downloads the official releases of yt-dlp and deno from GitHub
into `$XDG_DATA_HOME/ldm/tools/`. Downloads are verified (SHA-256, plus the yt-dlp GPG
signature when `gpg` is available), installed atomically, and the previous version is kept for
rollback. A yt-dlp you set in `ytdlp.path`, or one on `PATH`, works too.

```sh
ldm tools status
ldm tools install [yt-dlp|deno] [--version X]
ldm tools update | check | rollback | remove [yt-dlp|deno]
ldm add --ytdlp URL
```

- YouTube pages (youtube.com, youtu.be, music and shorts) are handed to yt-dlp. The default
  format is up to 1080p, non-HDR, H.264 preferred, with AAC audio, merged into MP4.
- For other sites yt-dlp is used only as a fallback: when no media is found on the page, or
  when an LDM media download fails for a reason yt-dlp might fix (403/410, expired signed URL,
  not media, manifest parse error). It is tried at most once per download.
- No cookies are passed unless you set `ytdlp.cookies_from_browser`. LDM does no login
  automation and does not try to get around "confirm you're not a bot" pages; such failures are
  reported as they are.

| Key | Default | Meaning |
|---|---|---|
| `ytdlp.enabled` | `true` | master switch |
| `ytdlp.path` / `ytdlp.deno_path` | `""` | explicit paths (default: managed, then `PATH`) |
| `ytdlp.auto_install` | `ask` | `ask`, `always` or `never` |
| `ytdlp.auto_update` | `daily` | `daily`, `weekly` or `off` |
| `ytdlp.auto_fallback` | `true` | allow the fallback described above |
| `ytdlp.site_rules` | `{}` | per domain `on`, `off` or `always` |
| `ytdlp.extra_args` | `[]` | extra arguments |
| `ytdlp.cookies_from_browser` | `""` | e.g. `firefox`; empty = no cookies |
| `ytdlp.playlist` | `false` | one download per playlist entry |
| `ytdlp.max_height` | `1080` | default height limit |

## Identifiers

Public identifiers are listed in [`packaging/identifiers.env`](packaging/identifiers.env):

| | |
|---|---|
| App ID (Flatpak, desktop, AppStream) | `io.github.wzk4work.LDM` |
| D-Bus name / path | `io.github.wzk4work.LDM.Daemon1` / `/io/github/wzk4work/LDM/Daemon1` |
| Native-messaging host | `io.github.wzk4work.ldm.browser_host` |
| Firefox add-on ID | `ldm@wzk4work.github.io` |

`tests/e2e/test_identifiers.py` checks that every file uses the same values.

## Development

```sh
scripts/check.sh    # fmt, clippy -D warnings, cargo test, extension checks and builds, ruff, GUI and E2E tests
```

See [CONTRIBUTING.md](CONTRIBUTING.md). Security reports: [SECURITY.md](SECURITY.md).

## License

LDM is released under the [MIT License](LICENSE). Third-party components and their licenses are
listed in [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) (regenerate with
`scripts/gen_third_party_notices.py`). The Flatpak also contains FFmpeg (LGPL-2.1+), and the
AppImage contains Qt/PySide6 (LGPL-3.0) and CPython (PSF License).
