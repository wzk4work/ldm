#!/usr/bin/env bash
# Build dist/ldm_<version>_<arch>.deb from a release build (system-wide layout).
set -euo pipefail
# Override with LDM_MAINTAINER="Name <email>" if needed.
MAINTAINER="${LDM_MAINTAINER:-Nicolas <243065664+wzk4work@users.noreply.github.com>}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
BIN_SRC="${1:-$ROOT/target/release}"
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
DESTDIR="$STAGE" "$ROOT/packaging/install.sh" --system --bin-dir "$BIN_SRC" --no-systemd
install -d "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<EOC
Package: ldm
Version: $VERSION
Architecture: $ARCH
Maintainer: $MAINTAINER
Depends: libc6
Recommends: ffmpeg, python3-pyside6.qtwidgets | python3-pyside6
Suggests: gpg
Section: net
Priority: optional
Description: LDM - Linux download manager with browser video sniffing
 Multi-connection HTTP downloads with resume, HLS/DASH capture,
 native-messaging browser integration, CLI, daemon and Qt GUI.
 .
 yt-dlp and deno are not shipped: on first use (with consent) LDM downloads
 the official GitHub releases into each user's data dir
 (~/.local/share/ldm/tools), verified by SHA-256 (and GPG for yt-dlp when
 gpg is installed); see \`ldm tools\`.
EOC
# Debian copyright file (machine-readable format) + the full third-party notices
DOC="$STAGE/usr/share/doc/ldm"
install -d "$DOC"
{
  cat <<EOCR
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: ldm
Source: https://github.com/wzk4work/ldm

Files: *
Copyright: 2026 Nicolas
License: MIT

Files: statically linked Rust crates (ldmd, ldm, ldm-native-host)
Copyright: see /usr/share/doc/ldm/THIRD_PARTY_NOTICES
License: MIT or Apache-2.0 or ISC or BSD or Unicode-3.0 or CDLA-Permissive-2.0 (per crate)
Comment: Every crate and its license text is listed in THIRD_PARTY_NOTICES.

Files: bundled SQLite (in ldmd)
Copyright: none (public domain)
License: public-domain
 The SQLite source code is in the public domain: https://www.sqlite.org/copyright.html

License: MIT
EOCR
  sed -n '/^Permission is hereby granted/,$p' "$ROOT/LICENSE" | sed -e 's/^$/./' -e 's/^/ /'
} > "$DOC/copyright"
install -m644 "$ROOT/THIRD_PARTY_NOTICES" "$DOC/THIRD_PARTY_NOTICES"
gzip -9n "$DOC/THIRD_PARTY_NOTICES"

mkdir -p "$ROOT/dist"
OUT="$ROOT/dist/ldm_${VERSION}_${ARCH}.deb"
dpkg-deb --root-owner-group --build "$STAGE" "$OUT" >/dev/null
echo "$OUT"
