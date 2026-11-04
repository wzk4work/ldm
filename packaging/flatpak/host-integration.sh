#!/bin/sh
# Host-side integration for the Flatpak build: run on the host (not inside the sandbox).
#   host-integration.sh install | remove
# Installs a wrapper that starts the sandboxed native host with `flatpak run`, and points the
# per-user Chrome/Chromium/Edge/Brave and Firefox native-messaging manifests at it.
# The browser itself must run on the host: Flatpak/Snap browsers cannot start host programs.
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
APP=io.github.wzk4work.LDM
DATA="${XDG_DATA_HOME:-$HOME/.local/share}/ldm/flatpak"
WRAP="$DATA/ldm-native-host"
C="${XDG_CONFIG_HOME:-$HOME/.config}"
CHROME_DIRS="$C/google-chrome/NativeMessagingHosts $C/chromium/NativeMessagingHosts $C/microsoft-edge/NativeMessagingHosts $C/BraveSoftware/Brave-Browser/NativeMessagingHosts"
FIREFOX_DIRS="$HOME/.mozilla/native-messaging-hosts"
NAME=io.github.wzk4work.ldm.browser_host.json

case "${1:-}" in
install)
  EXT_ID="$(cat "$ROOT/extension/EXTENSION_ID")"
  mkdir -p "$DATA"
  printf '#!/bin/sh\nexec flatpak run --command=ldm-native-host %s "$@"\n' "$APP" > "$WRAP"
  chmod 755 "$WRAP"
  for d in $CHROME_DIRS; do
    mkdir -p "$d"
    sed -e "s|@HOST_PATH@|$WRAP|" -e "s|@EXTENSION_ID@|$EXT_ID|" \
      "$ROOT/packaging/native-messaging/chrome.json.in" > "$d/$NAME"
  done
  for d in $FIREFOX_DIRS; do
    mkdir -p "$d"
    sed -e "s|@HOST_PATH@|$WRAP|" "$ROOT/packaging/native-messaging/firefox.json.in" > "$d/$NAME"
  done
  echo "native messaging -> $WRAP"
  echo "load the extension from extension/dist/{chromium,firefox} (npm run build) or a signed build"
  ;;
remove)
  for d in $CHROME_DIRS $FIREFOX_DIRS; do
    f="$d/$NAME"
    if [ -f "$f" ] && grep -q "\"$WRAP\"" "$f"; then rm -f "$f"; fi
  done
  rm -f "$WRAP"
  rmdir "$DATA" 2>/dev/null || true
  ;;
*) echo "usage: $0 install|remove" >&2; exit 2 ;;
esac
