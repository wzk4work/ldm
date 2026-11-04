#!/usr/bin/env bash
# Full local verification: fmt, clippy, cargo tests, extension, ruff, pytest (E2E + GUI).
# Prereqs: rust (+clippy/rustfmt), node>=20, python venv at .venv (see README), ffmpeg, xvfb-run, google-chrome (optional), firefox + geckodriver + selenium (optional; tests/e2e/test_firefox.py).
set -euo pipefail
cd "$(dirname "$0")/.."
step() { printf '\n==> %s\n' "$*"; }
step "cargo fmt --check";   cargo fmt --all --check
step "cargo clippy";        cargo clippy --workspace --all-targets -- -D warnings
step "cargo test";          cargo test --workspace
step "cargo build (bins for E2E)"; cargo build --workspace
step "extension";           (cd extension && { [ -d node_modules ] || npm ci; } && npm run check)
step "ruff";                .venv/bin/ruff check gui tests; .venv/bin/ruff format --check gui tests  # separate: `a && b` would mask a failing `a` under set -e
step "GUI unit tests";      (cd gui && QT_QPA_PLATFORM=offscreen ../.venv/bin/pytest -q tests)
step "E2E";                 .venv/bin/pytest -q tests/e2e
echo; echo "ALL CHECKS PASSED"
