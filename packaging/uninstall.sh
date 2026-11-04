#!/usr/bin/env bash
# Remove a per-user (default) or --system LDM installation. User data
# (~/.local/share/ldm, ~/.config/ldm) is kept unless --purge is given.
set -euo pipefail
PREFIX="${HOME}/.local"; SYSTEM=0; PURGE=0
while [ $# -gt 0 ]; do
  case "$1" in
    --prefix) PREFIX="$2"; shift 2 ;;
    --system) SYSTEM=1; PREFIX=/usr; shift ;;
    --purge) PURGE=1; shift ;;
    *) echo "unknown option $1" >&2; exit 2 ;;
  esac
done
if [ $SYSTEM = 0 ] && command -v systemctl >/dev/null && systemctl --user show-environment >/dev/null 2>&1; then
  systemctl --user disable --now ldmd.service 2>/dev/null || true
fi
"$PREFIX/bin/ldm" daemon stop 2>/dev/null || true
rm -f "$PREFIX/bin/ldmd" "$PREFIX/bin/ldm" "$PREFIX/bin/ldm-gui" "$PREFIX/bin/ldm-dbus"
rm -rf "$PREFIX/lib/ldm" "$PREFIX/share/doc/ldm"
if [ $SYSTEM = 1 ]; then
  rm -f /etc/opt/chrome/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json /etc/chromium/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json \
        /etc/opt/edge/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json "$PREFIX/lib/mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json" \
        "$PREFIX/lib/systemd/user/ldmd.service" "$PREFIX/share/applications/ldm.desktop" "$PREFIX/share/icons/hicolor/128x128/apps/ldm.png"
else
  C="${XDG_CONFIG_HOME:-$HOME/.config}"; S="${XDG_DATA_HOME:-$HOME/.local/share}"
  for d in google-chrome/NativeMessagingHosts chromium/NativeMessagingHosts microsoft-edge/NativeMessagingHosts BraveSoftware/Brave-Browser/NativeMessagingHosts; do
    rm -f "$C/$d/io.github.wzk4work.ldm.browser_host.json"
  done
  rm -f "$HOME/.mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json" "$C/systemd/user/ldmd.service" \
        "$S/applications/ldm.desktop" "$S/icons/hicolor/128x128/apps/ldm.png"
  if [ $PURGE = 1 ]; then rm -rf "$C/ldm" "$S/ldm" "${XDG_CACHE_HOME:-$HOME/.cache}/ldm"; fi
fi
echo "LDM uninstalled."
