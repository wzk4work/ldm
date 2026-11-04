#!/usr/bin/env bash
# Release build of ldmd, ldm and ldm-native-host for packaging. Local build paths (the checkout
# and $CARGO_HOME / $HOME) are remapped so they do not end up in the shipped binaries.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CH="${CARGO_HOME:-$HOME/.cargo}"
# when several prefixes match, rustc uses the last one: most specific last
export RUSTFLAGS="${RUSTFLAGS:-} --remap-path-prefix=$HOME=~ --remap-path-prefix=$CH=cargo --remap-path-prefix=$ROOT=ldm"
cd "$ROOT"
exec cargo build --release --locked -p ldm-daemon -p ldm-cli -p ldm-native-host "$@"
