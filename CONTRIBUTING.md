# Contributing

Thanks for helping. Bug reports, fixes and small features are welcome.

1. Open an issue first for larger changes, so we can agree on the approach.
2. Build and run the full check before sending a pull request:

   ```sh
   python3 -m venv .venv && .venv/bin/pip install -r requirements-dev.txt
   (cd extension && npm ci)
   scripts/check.sh
   ```

   It runs `cargo fmt --check`, `clippy -D warnings`, the Rust tests, the extension type check,
   tests and builds (normal and store), `ruff`, and the GUI and end-to-end tests.
3. After changing `Cargo.lock`, also run:
   - `python3 scripts/gen_third_party_notices.py` (the license notices);
   - `python3 packaging/flatpak/gen-cargo-sources.py` (the Flatpak's offline crate list).
4. Public identifiers (app id, D-Bus name, native-messaging host name, Firefox add-on id) are
   listed in `packaging/identifiers.env`; `tests/e2e/test_identifiers.py` keeps them consistent.

Ground rules:

- Only submit code you wrote yourself or that is under a license compatible with MIT. Say where
  any third-party code comes from.
- No features that bypass DRM, logins, paywalls, CAPTCHAs or other anti-bot measures.
- By contributing you agree that your contribution is licensed under the MIT License.
