#!/usr/bin/env bash
# Check that every place that carries the release version agrees with Cargo.toml:
#   scripts/check-version.sh [TAG]      TAG (e.g. v0.9.0 or refs/tags/v0.9.0) must be v<version>
# Checked: extension package.json + both manifests, the AppStream metainfo <release>, and a
# CHANGELOG.md entry. Prints the version on success.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
V="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
[ -n "$V" ] || { echo "no version in Cargo.toml" >&2; exit 1; }
fail=0
bad() { echo "version mismatch: $*" >&2; fail=1; }
for f in extension/package.json extension/static/manifest.chromium.json extension/static/manifest.firefox.json; do
  got="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["version"])' "$ROOT/$f")"
  [ "$got" = "$V" ] || bad "$f has $got, Cargo.toml has $V"
done
grep -q "<release version=\"$V\" date=\"[0-9-]*\"" "$ROOT/packaging/flatpak/io.github.wzk4work.LDM.metainfo.xml" \
  || bad "metainfo has no <release version=\"$V\" date=...>"
grep -q "^## \[$V\]" "$ROOT/CHANGELOG.md" || bad "CHANGELOG.md has no '## [$V]' entry"
if [ $# -gt 0 ] && [ -n "$1" ]; then
  tag="${1#refs/tags/}"
  [ "$tag" = "v$V" ] || bad "tag $tag != v$V (Cargo.toml)"
fi
[ $fail = 0 ] || exit 1
echo "$V"
