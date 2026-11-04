#!/bin/sh
# Lay out LDM inside a Flatpak prefix (called from io.github.wzk4work.LDM.yml; also runnable on the host
# for testing: install-flatpak.sh /tmp/prefix target/release).
#   $1 PREFIX (default /app)   $2 directory with ldmd, ldm, ldm-native-host (default target/release)
set -eu
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
P="${1:-/app}"
BIN="${2:-$ROOT/target/release}"
ID=io.github.wzk4work.LDM

install -d "$P/bin" "$P/lib/ldm/bin" "$P/lib/ldm/rules" "$P/lib/ldm/gui" \
  "$P/share/applications" "$P/share/icons/hicolor/128x128/apps" "$P/share/metainfo"
for b in ldmd ldm ldm-native-host; do install -m755 "$BIN/$b" "$P/lib/ldm/bin/"; done
install -m644 "$ROOT"/rules/bundled/*.json "$P/lib/ldm/rules/"
cp -r "$ROOT/gui/ldm_gui" "$P/lib/ldm/gui/"
find "$P/lib/ldm/gui" -name __pycache__ -prune -exec rm -rf {} +

# Every entry point shares one socket under $XDG_RUNTIME_DIR/ldm, which the manifest exposes
# with --filesystem=xdg-run/ldm:create. Separate `flatpak run` instances (the GUI, and the
# native host started by the browser) each get their own private runtime dir otherwise.
cat > "$P/lib/ldm/env.sh" <<EOS
: "\${XDG_RUNTIME_DIR:=/run/user/\$(id -u)}"
export LDM_SOCKET="\${LDM_SOCKET:-\$XDG_RUNTIME_DIR/ldm/ldm.sock}"
mkdir -p "\${LDM_SOCKET%/*}" 2>/dev/null || true
export LDM_LDMD="$P/bin/ldmd"
EOS
for b in ldmd ldm ldm-native-host; do
  printf '#!/bin/sh\n. "%s/lib/ldm/env.sh"\nexec "%s/lib/ldm/bin/%s" "$@"\n' "$P" "$P" "$b" > "$P/bin/$b"
  chmod 755 "$P/bin/$b"
done
printf '#!/bin/sh\n. "%s/lib/ldm/env.sh"\nPYTHONPATH="%s/lib/ldm/gui${PYTHONPATH:+:$PYTHONPATH}" exec "${LDM_PYTHON:-python3}" -m ldm_gui "$@"\n' \
  "$P" "$P" > "$P/bin/ldm-gui"
chmod 755 "$P/bin/ldm-gui"

sed -e "s|@BINDIR@/ldm-gui|ldm-gui|" -e "s|^Icon=.*|Icon=$ID|" "$ROOT/packaging/desktop/ldm.desktop" \
  > "$P/share/applications/$ID.desktop"
install -m644 "$ROOT/packaging/desktop/ldm.png" "$P/share/icons/hicolor/128x128/apps/$ID.png"
install -m644 "$ROOT/packaging/flatpak/$ID.metainfo.xml" "$P/share/metainfo/$ID.metainfo.xml"
# license texts: /app/share/licenses/<app-id> (FFmpeg's own files are added by its module)
install -d "$P/share/licenses/$ID"
install -m644 "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES" "$P/share/licenses/$ID/"
