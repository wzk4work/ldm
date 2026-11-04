#!/bin/sh
# Opt-in smoke test of the INSTALLED Flatpak (not part of scripts/check.sh: it needs the
# ~5 GB KDE/PySide runtime). Usage:
#   flatpak install --user dist/LDM-<ver>.flatpak   # once
#   packaging/flatpak/smoke.sh [pytest args]
# Covers: CLI, bundled ffmpeg/ffprobe, daemon start/stop, HTTP, HLS + remux, LDM-managed yt-dlp
# (install/update/rollback from a local fake release server, inside the sandbox), PAC, FTPS (release build, per-site certificate override), native host via
# host-integration.sh (stdio handshake + real Chrome/Firefox when available), GUI under Xvfb
# owning io.github.wzk4work.LDM.Daemon1. Python deps: pytest (+ pyftpdlib/pyOpenSSL/cryptography for FTPS,
# selenium + geckodriver for Firefox); uses .venv/bin/python when present.
set -eu
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
PY=${PYTHON:-$ROOT/.venv/bin/python}
[ -x "$PY" ] || PY=python3
# own basetemp outside /tmp: the sandbox has a private /tmp (pytest wipes this dir per run)
BT=${XDG_CACHE_HOME:-$HOME/.cache}/ldm-flatpak-smoke-pytest
exec "$PY" -m pytest -p no:cacheprovider --basetemp="$BT" -v "$ROOT/packaging/flatpak/flatpak_smoke.py" "$@"
