# Changelog

All notable changes to LDM are listed here. Versions follow [Semantic Versioning](https://semver.org/).

## [0.9.0] - 2026-11-05

Initial public release.

### Features

- **HTTP(S) / FTP / FTPS downloads**: multiple connections per file with dynamic splitting, resume (also after a crash), global and per-download speed limits, mirrors, SHA-256/MD5 checks, per-site logins, cookie refresh from the browser, queues with time windows and "when done" actions.
- **Streaming media**: HLS (AES-128, fMP4, byte ranges, separate audio, audio-only) and DASH (SegmentTemplate / Timeline / List), muxed into MP4 with ffmpeg; live recording with a duration limit; subtitle tracks saved as `.vtt`/`.srt`. DRM-protected streams are not supported.
- **Daemon `ldmd`** with a Unix-socket IPC and a session D-Bus interface, plus a systemd user unit.
- **Browser extension** (Manifest V3, Chromium-based browsers and Firefox): takes over downloads by file type, a "Download this video" button with a quality menu, ad-aware ranking of detected media, "Download all links"; talks to the daemon through a native-messaging host (optional local WebSocket bridge).
- **Clients**: the `ldm` CLI and the `ldm-gui` Qt (PySide6) desktop app with categories, per-connection progress, scheduler, batch downloads, site grabber, clipboard monitor and import/export, in English and Simplified Chinese.
- **Optional yt-dlp integration** (YouTube and a fallback for other sites); yt-dlp and deno are downloaded and verified by LDM on first use, with consent.
- **Operations**: `ldm doctor`, `ldm diag` (redacted diagnostics bundle), `ldm metrics` (Prometheus text format); URLs and credentials in logs are redacted.
- **Packages** for x86_64 and aarch64: `.deb`, `.rpm`, AppImage and Flatpak bundle, plus the browser extension as zip files.

[0.9.0]: https://github.com/wzk4work/ldm/releases/tag/v0.9.0
