#!/usr/bin/env bash
# Build dist/ldm-<version>-1.<arch>.rpm from a release build (same /usr layout as build-deb.sh).
#   packaging/build-rpm.sh [BIN_DIR]     needs rpmbuild (Debian/Ubuntu: apt install rpm)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
ARCH="${LDM_RPM_ARCH:-$(uname -m)}"
BIN_SRC="${1:-$ROOT/target/release}"
command -v rpmbuild >/dev/null || { echo "rpmbuild is required (apt install rpm / dnf install rpm-build)" >&2; exit 1; }
TOP="$(mktemp -d)"
trap 'rm -rf "$TOP"' EXIT
STAGE="$TOP/stage"
mkdir -p "$STAGE"
"$ROOT/packaging/stage-system.sh" "$STAGE" "$BIN_SRC"
rpmbuild -bb --quiet --target "$ARCH" \
  --define "_topdir $TOP" --define "_rpmdir $TOP/out" --define "_build_name_fmt %%{NAME}-%%{VERSION}-%%{RELEASE}.%%{ARCH}.rpm" \
  --define "ldm_version $VERSION" --define "ldm_stage $STAGE" \
  "$ROOT/packaging/rpm/ldm.spec" >&2
mkdir -p "$ROOT/dist"
OUT="$ROOT/dist/ldm-$VERSION-1.$ARCH.rpm"
mv "$TOP/out/ldm-$VERSION-1.$ARCH.rpm" "$OUT"
echo "$OUT"
