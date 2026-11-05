#!/usr/bin/env bash
# Lay out the system-wide (/usr) install tree shared by the .deb and the .rpm:
#   packaging/stage-system.sh STAGE [BIN_DIR]
# = install.sh --system into DESTDIR=STAGE, plus the AppStream metainfo (pointing at ldm.desktop).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
STAGE="${1:?usage: stage-system.sh STAGE [BIN_DIR]}"
BIN_SRC="${2:-$ROOT/target/release}"
chmod 755 "$STAGE"
DESTDIR="$STAGE" "$ROOT/packaging/install.sh" --system --bin-dir "$BIN_SRC" --no-systemd >/dev/null
. "$ROOT/packaging/identifiers.env"
install -d "$STAGE/usr/share/metainfo"
sed -e "s|<launchable type=\"desktop-id\">.*</launchable>|<launchable type=\"desktop-id\">ldm.desktop</launchable>|" \
  "$ROOT/packaging/flatpak/$LDM_APP_ID.metainfo.xml" > "$STAGE/usr/share/metainfo/$LDM_APP_ID.metainfo.xml"
chmod 644 "$STAGE/usr/share/metainfo/$LDM_APP_ID.metainfo.xml"
