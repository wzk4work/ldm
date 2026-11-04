#!/usr/bin/env bash
# Build dist/LDM-<version>-x86_64.AppImage: ldmd, ldm (CLI), ldm-native-host, the PySide6 GUI with
# a bundled CPython + trimmed PySide6/Qt, the rules, the browser extension builds, desktop file
# and icon. Nothing here downloads Python packages; inputs are local:
#   BIN_DIR   release binaries (default: $CARGO_TARGET_DIR/release or target/release; built if missing)
#   PYTHON    a Python with PySide6 installed (default: .venv/bin/python); its base interpreter
#             and stdlib are bundled
#   RUNTIME   AppImage type-2 runtime file (default: downloaded once from the AppImage project's
#             type2-runtime releases into $CACHE and checked against RUNTIME_SHA256 if set)
# Output: dist/LDM-<version>-x86_64.AppImage (not committed). The work dir is removed at the end.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
HERE="$ROOT/packaging/appimage"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
PYTHON="${PYTHON:-$ROOT/.venv/bin/python}"
BIN_DIR="${BIN_DIR:-${CARGO_TARGET_DIR:-$ROOT/target}/release}"
CACHE="${CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/ldm-appimage}"
RUNTIME_URL="${RUNTIME_URL:-https://github.com/AppImage/type2-runtime/releases/download/continuous/runtime-x86_64}"
OUT="$ROOT/dist/LDM-$VERSION-x86_64.AppImage"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/ldm-appimage.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
A="$WORK/AppDir"
LIB="$A/usr/lib/ldm"

for b in ldmd ldm ldm-native-host; do
  if [ ! -x "$BIN_DIR/$b" ]; then "$ROOT/scripts/build-release.sh"; break; fi
done
[ -f "$ROOT/extension/dist/chromium/manifest.json" ] || (cd "$ROOT/extension" && npm run build)
command -v mksquashfs >/dev/null || { echo "mksquashfs (squashfs-tools) is required" >&2; exit 1; }

if [ -z "${RUNTIME:-}" ]; then
  RUNTIME="$CACHE/runtime-x86_64"
  [ -s "$RUNTIME" ] || { mkdir -p "$CACHE"; curl -fsSL -o "$RUNTIME.part" "$RUNTIME_URL"; mv "$RUNTIME.part" "$RUNTIME"; }
fi
if [ -n "${RUNTIME_SHA256:-}" ]; then echo "$RUNTIME_SHA256  $RUNTIME" | sha256sum -c - >/dev/null; fi

echo "==> AppDir"
install -d "$A/usr/bin" "$LIB/rules" "$LIB/gui" "$LIB/pylib" "$LIB/native-messaging" "$LIB/extension"
install -m755 "$BIN_DIR/ldmd" "$BIN_DIR/ldm" "$A/usr/bin/"
install -m755 "$BIN_DIR/ldm-native-host" "$LIB/"
install -m644 "$ROOT"/rules/bundled/*.json "$LIB/rules/"
cp -r "$ROOT/gui/ldm_gui" "$LIB/gui/"
cp -r "$ROOT/extension/dist/chromium" "$ROOT/extension/dist/firefox" "$LIB/extension/"
install -m644 "$ROOT/extension/EXTENSION_ID" "$ROOT"/packaging/native-messaging/*.json.in "$LIB/"
mv "$LIB"/*.json.in "$LIB/native-messaging/"
install -m644 "$HERE/sitecustomize.py" "$LIB/pylib/"
install -m755 "$HERE/AppRun" "$A/AppRun"
sed -e "s|^Exec=.*|Exec=ldm-gui|" "$ROOT/packaging/desktop/ldm.desktop" > "$A/ldm.desktop"
echo "X-AppImage-Version=$VERSION" >> "$A/ldm.desktop"
install -m644 "$ROOT/packaging/desktop/ldm.png" "$A/ldm.png"
ln -s ldm.png "$A/.DirIcon"
install -d "$A/usr/share/applications" "$A/usr/share/icons/hicolor/128x128/apps" "$A/usr/share/metainfo"
cp "$A/ldm.desktop" "$A/usr/share/applications/"
cp "$A/ldm.png" "$A/usr/share/icons/hicolor/128x128/apps/"
install -m644 "$ROOT/README.md" "$ROOT/LICENSE" "$ROOT/THIRD_PARTY_NOTICES" "$LIB/"
# license texts of the bundled third-party components (see THIRD_PARTY_NOTICES)
install -d "$LIB/licenses"
install -m644 "$ROOT"/packaging/licenses/LGPL-3.0.txt "$ROOT"/packaging/licenses/GPL-3.0.txt "$LIB/licenses/"

echo "==> Python"
read -r PYBASE PYVER PYSTD PYDYN SITE < <("$PYTHON" -c 'import os,sys,sysconfig,PySide6
print(os.path.realpath(sys._base_executable), "%d.%d" % sys.version_info[:2], sysconfig.get_path("stdlib"), sysconfig.get_path("platstdlib") + "/lib-dynload", os.path.dirname(os.path.dirname(PySide6.__file__)))')
PY="$LIB/python"
install -d "$PY/bin" "$PY/lib/python$PYVER"
install -m755 "$PYBASE" "$PY/bin/python3"
# stdlib without tests, IDLE, Tk, ensurepip/venv bootstrap wheels, distro site-packages and bytecode
(cd "$PYSTD" && tar --exclude='./test' --exclude='./idlelib' --exclude='./tkinter' --exclude='./turtledemo' \
   --exclude='./ensurepip' --exclude='./lib2to3' --exclude='./site-packages' --exclude='./dist-packages' \
   --exclude='./config-*' --exclude='__pycache__' --exclude='*/tests' -cf - .) | (cd "$PY/lib/python$PYVER" && tar xf -)
[ -d "$PY/lib/python$PYVER/lib-dynload" ] || cp -r "$PYDYN" "$PY/lib/python$PYVER/lib-dynload"
rm -f "$PY/lib/python$PYVER"/lib-dynload/_tkinter*.so
if [ -f "$PYSTD/LICENSE.txt" ]; then install -m644 "$PYSTD/LICENSE.txt" "$LIB/licenses/Python-LICENSE.txt"
else echo "warning: no CPython LICENSE.txt in $PYSTD; add the PSF license to $LIB/licenses by hand" >&2; fi

echo "==> PySide6 (QtCore/Gui/Widgets/DBus/Network + xcb/wayland/offscreen platforms)"
S="$SITE/PySide6"; D="$LIB/pylib/PySide6"
install -d "$D/Qt/lib" "$D/Qt/plugins" "$D/Qt/translations"
cp "$S"/__init__.py "$S"/_config.py "$S"/_git_pyside_version.py "$S"/libpyside6.abi3.so.* "$D/"
for m in QtCore QtGui QtWidgets QtDBus QtNetwork; do cp "$S/$m.abi3.so" "$D/"; done
[ -d "$S/support" ] && cp -r "$S/support" "$D/"
cp -r "$SITE/shiboken6" "$LIB/pylib/"
find "$LIB/pylib" "$LIB/gui" -name __pycache__ -prune -exec rm -rf {} +
find "$LIB/pylib" -name '*.pyi' -delete
for d in platforms xcbglintegrations platforminputcontexts wayland-shell-integration \
         wayland-decoration-client wayland-graphics-integration-client; do
  [ -d "$S/Qt/plugins/$d" ] && cp -r "$S/Qt/plugins/$d" "$D/Qt/plugins/"
done
rm -f "$D"/Qt/plugins/platforminputcontexts/libqtvirtualkeyboardplugin.so  # pulls in QtQuick/Qml
find "$D/Qt/plugins/platforms" -type f ! \( -name 'libqxcb.so' -o -name 'libqwayland*.so' -o -name 'libqoffscreen.so' -o -name 'libqminimal.so' \) -delete
install -d "$D/Qt/plugins/imageformats" "$D/Qt/plugins/platformthemes"
for f in libqgif.so libqico.so libqjpeg.so; do [ -f "$S/Qt/plugins/imageformats/$f" ] && cp "$S/Qt/plugins/imageformats/$f" "$D/Qt/plugins/imageformats/"; done
[ -f "$S/Qt/plugins/platformthemes/libqxdgdesktopportal.so" ] && cp "$S/Qt/plugins/platformthemes/libqxdgdesktopportal.so" "$D/Qt/plugins/platformthemes/"
cp "$S"/Qt/translations/qtbase_*.qm "$D/Qt/translations/" 2>/dev/null || true

echo "==> shared-library closure"
python3 "$HERE/bundle_libs.py" "$A" "$SITE" "$LIB/pylib" "$PY/lib"

echo "==> squashfs + runtime"
SQ="$WORK/ldm.squashfs"
mksquashfs "$A" "$SQ" -root-owned -noappend -comp zstd -Xcompression-level 19 -quiet >/dev/null
mkdir -p "$ROOT/dist"
cat "$RUNTIME" "$SQ" > "$OUT.part"
chmod 755 "$OUT.part"
mv "$OUT.part" "$OUT"
echo "==> $OUT ($(du -h "$OUT" | cut -f1); AppDir $(du -sh "$A" | cut -f1))"
