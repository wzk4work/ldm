#!/usr/bin/env bash
# Print the GitHub release notes for a version: its CHANGELOG.md entry plus install instructions.
#   scripts/release-notes.sh [VERSION]     (default: version from Cargo.toml)
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
V="${1:-$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)}"
# the "## [V]" section without its heading, up to the next "## " heading or link reference
body="$(awk -v v="$V" '
  $0 ~ "^## \\[" v "\\]" { on = 1; next }
  on && (/^## / || /^\[[^]]+\]: /) { exit }
  on { print }' "$ROOT/CHANGELOG.md")"
[ -n "$(printf '%s' "$body" | tr -d '[:space:]')" ] || { echo "CHANGELOG.md has no entry for $V" >&2; exit 1; }
printf '%s\n' "$body" | sed -e '/./,$!d'   # drop leading blank lines
FFMPEG_SRC="$(sed -n 's|^ *url: https://ffmpeg.org/releases/\(ffmpeg-[^ ]*\)$|\1|p' "$ROOT/packaging/flatpak/io.github.wzk4work.LDM.yml" | head -1)"
[ -n "$FFMPEG_SRC" ] || { echo "no ffmpeg source in the Flatpak manifest" >&2; exit 1; }
cat <<EOF2

## Install

Every package is built for **x86_64** (amd64) and **aarch64** (arm64); pick the file for your machine (\`uname -m\`).

| Format | x86_64 | aarch64 |
|---|---|---|
| Debian / Ubuntu | \`ldm_${V}_amd64.deb\` | \`ldm_${V}_arm64.deb\` |
| Fedora / openSUSE | \`ldm-${V}-1.x86_64.rpm\` | \`ldm-${V}-1.aarch64.rpm\` |
| AppImage | \`LDM-${V}-x86_64.AppImage\` | \`LDM-${V}-aarch64.AppImage\` |
| Flatpak bundle | \`LDM-${V}-x86_64.flatpak\` | \`LDM-${V}-aarch64.flatpak\` |
| Browser extension | \`ldm-extension-chrome-${V}.zip\`, \`ldm-extension-firefox-${V}.zip\` (both architectures) | |

\`\`\`sh
# Debian 13 / Ubuntu 24.10+ (the GUI needs python3-pyside6; on Ubuntu 24.04 use the AppImage or Flatpak for it)
sudo apt install ./ldm_${V}_amd64.deb
# Fedora / openSUSE
sudo dnf install ./ldm-${V}-1.x86_64.rpm          # or: sudo zypper install ./ldm-${V}-1.x86_64.rpm
# AppImage (needs FUSE and glibc 2.39+)
chmod +x LDM-${V}-x86_64.AppImage && ./LDM-${V}-x86_64.AppImage --install-integration
# Flatpak (pulls the org.kde.Platform 6.11 runtime from Flathub)
flatpak install --user ./LDM-${V}-x86_64.flatpak && flatpak run io.github.wzk4work.LDM
\`\`\`

Then load the browser extension: unzip \`ldm-extension-chrome-${V}.zip\` and choose "Load unpacked" in \`chrome://extensions\` (developer mode), or load \`ldm-extension-firefox-${V}.zip\` in Firefox 140+ as a temporary add-on in \`about:debugging\`. The extension zips are unsigned (not from a store); the .deb and .rpm also install the unpacked extension under \`/usr/lib/ldm/extension\`. Run \`ldm doctor\` to check the setup. See the [README](https://github.com/wzk4work/ldm#installing) for details (Flatpak host integration, AppImage roles).

The Flatpak bundles a remux-only FFmpeg build (LGPL-2.1); its unmodified source is attached as \`${FFMPEG_SRC}\`.

## Checksums

\`\`\`sh
sha256sum -c --ignore-missing SHA256SUMS
\`\`\`
EOF2
