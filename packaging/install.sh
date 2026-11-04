#!/usr/bin/env bash
# LDM installer. Per-user by default (~/.local); `--system` for /usr (needs root).
#   ./packaging/install.sh [--prefix DIR] [--system] [--bin-dir DIR] [--no-systemd] [--no-build]
# DESTDIR is honoured for packaging (build-deb.sh).
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PREFIX="${HOME}/.local"
SYSTEM=0
BIN_SRC=""
SYSTEMD=1
BUILD=1
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --system) SYSTEM=1; PREFIX="/usr"; shift ;;
    --bin-dir) BIN_SRC="$2"; shift 2 ;;
    --no-systemd) SYSTEMD=0; shift ;;
    --no-build) BUILD=0; shift ;;
    -h|--help) sed -n '2,5p' "$0"; exit 0 ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
D="${DESTDIR:-}"

if [ -z "$BIN_SRC" ]; then
  BIN_SRC="$ROOT/target/release"
  if [ $BUILD = 1 ] && [ ! -x "$BIN_SRC/ldmd" ]; then
    "$ROOT/scripts/build-release.sh"
  fi
fi
for b in ldmd ldm ldm-native-host; do
  [ -x "$BIN_SRC/$b" ] || { echo "missing $BIN_SRC/$b (run scripts/build-release.sh)" >&2; exit 1; }
done

BINDIR="$PREFIX/bin"
LIBDIR="$PREFIX/lib/ldm"
SHARE="$PREFIX/lib/ldm"
echo "==> installing LDM to $D$PREFIX"
install -d "$D$BINDIR" "$D$LIBDIR" "$D$SHARE/rules" "$D$SHARE/gui" "$D$PREFIX/share/doc/ldm"
install -m755 "$BIN_SRC/ldmd" "$BIN_SRC/ldm" "$D$BINDIR/"
install -m755 "$BIN_SRC/ldm-native-host" "$D$LIBDIR/"
ln -sf "$BINDIR/ldmd" "$D$LIBDIR/ldmd"
install -m644 "$ROOT"/rules/bundled/*.json "$D$SHARE/rules/"
cp -r "$ROOT/gui/ldm_gui" "$D$SHARE/gui/"
find "$D$SHARE/gui" -name __pycache__ -prune -exec rm -rf {} +
cat > "$D$BINDIR/ldm-gui" <<EOS
#!/bin/sh
# LDM GUI launcher (needs python3 + PySide6: pip install PySide6 or apt install python3-pyside6)
PYTHONPATH="$SHARE/gui\${PYTHONPATH:+:\$PYTHONPATH}" exec "\${LDM_PYTHON:-python3}" -m ldm_gui "\$@"
EOS
chmod 755 "$D$BINDIR/ldm-gui"
cat > "$D$BINDIR/ldm-dbus" <<EOS
#!/bin/sh
# io.github.wzk4work.LDM.Daemon1 on the session bus without the GUI window (the GUI also exports it)
PYTHONPATH="$SHARE/gui\${PYTHONPATH:+:\$PYTHONPATH}" exec "\${LDM_PYTHON:-python3}" -m ldm_gui.dbus_service "\$@"
EOS
chmod 755 "$D$BINDIR/ldm-dbus"
install -m644 "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES" "$D$PREFIX/share/doc/ldm/" 2>/dev/null || true
if [ -f "$ROOT/extension/dist/chromium/manifest.json" ]; then
  install -d "$D$SHARE/extension"
  cp -r "$ROOT/extension/dist/chromium" "$ROOT/extension/dist/firefox" "$D$SHARE/extension/"
fi

# desktop entry + icon
if [ $SYSTEM = 1 ]; then APPS="$PREFIX/share/applications"; ICONS="$PREFIX/share/icons/hicolor/128x128/apps"
else APPS="${XDG_DATA_HOME:-$HOME/.local/share}/applications"; ICONS="${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor/128x128/apps"; fi
install -d "$D$APPS" "$D$ICONS"
sed "s|@BINDIR@|$BINDIR|g" "$ROOT/packaging/desktop/ldm.desktop" > "$D$APPS/ldm.desktop"
install -m644 "$ROOT/packaging/desktop/ldm.png" "$D$ICONS/ldm.png"

# native messaging manifests (Chrome / Chromium / Edge / Brave / Firefox)
EXT_ID="$(cat "$ROOT/extension/EXTENSION_ID")"
HOST="$LIBDIR/ldm-native-host"
if [ $SYSTEM = 1 ]; then
  CHROME_DIRS="/etc/opt/chrome/native-messaging-hosts /etc/chromium/native-messaging-hosts /etc/opt/edge/native-messaging-hosts"
  FIREFOX_DIRS="$PREFIX/lib/mozilla/native-messaging-hosts"
else
  C="${XDG_CONFIG_HOME:-$HOME/.config}"
  CHROME_DIRS="$C/google-chrome/NativeMessagingHosts $C/chromium/NativeMessagingHosts $C/microsoft-edge/NativeMessagingHosts $C/BraveSoftware/Brave-Browser/NativeMessagingHosts"
  FIREFOX_DIRS="$HOME/.mozilla/native-messaging-hosts"
fi
for d in $CHROME_DIRS; do
  install -d "$D$d"
  sed -e "s|@HOST_PATH@|$HOST|" -e "s|@EXTENSION_ID@|$EXT_ID|" "$ROOT/packaging/native-messaging/chrome.json.in" > "$D$d/io.github.wzk4work.ldm.browser_host.json"
done
for d in $FIREFOX_DIRS; do
  install -d "$D$d"
  sed -e "s|@HOST_PATH@|$HOST|" "$ROOT/packaging/native-messaging/firefox.json.in" > "$D$d/io.github.wzk4work.ldm.browser_host.json"
done

# systemd user unit
if [ $SYSTEM = 1 ]; then UNITDIR="$PREFIX/lib/systemd/user"; else UNITDIR="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"; fi
install -d "$D$UNITDIR"
sed -e "s|@BINDIR@|$BINDIR|g" -e "s|@PREFIX@|$PREFIX|g" "$ROOT/packaging/systemd/ldmd.service" > "$D$UNITDIR/ldmd.service"
if [ $SYSTEMD = 1 ] && [ -z "$D" ] && [ $SYSTEM = 0 ] && command -v systemctl >/dev/null && systemctl --user show-environment >/dev/null 2>&1; then
  systemctl --user daemon-reload
  systemctl --user enable --now ldmd.service || true
fi
echo "==> done. 扩展 ID: $EXT_ID ；在浏览器开发者模式加载 $SHARE/extension/chromium（或 firefox）"
echo "    运行 'ldm doctor' 检查安装。"
