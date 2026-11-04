"""Flatpak packaging checks that run without a Flatpak runtime (packaging/flatpak/).

- cargo-sources.json is in sync with Cargo.lock (offline vendored build inside the sandbox).
- The manifest has the sandbox permissions LDM relies on; when flatpak-builder is available
  (PATH or $LDM_FLATPAK_BUILDER) it must also parse with --show-manifest.
- install-flatpak.sh lays out a working prefix: the wrappers share $XDG_RUNTIME_DIR/ldm/ldm.sock,
  so the CLI, the daemon, the native host and the GUI started as separate processes meet.
- host-integration.sh points the browser manifests at a `flatpak run` wrapper and removes only its own."""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

import pytest
from test_appimage import ORIGIN, hello_via

ROOT = Path(__file__).resolve().parents[2]
FP = ROOT / "packaging" / "flatpak"
MANIFEST = FP / "io.github.wzk4work.LDM.yml"
BIN = Path(os.environ.get("LDM_BIN_DIR", ROOT / "target" / "debug"))


def test_cargo_sources_in_sync(tmp_path):
    out = tmp_path / "cargo-sources.json"
    subprocess.run([sys.executable, str(FP / "gen-cargo-sources.py"), str(ROOT / "Cargo.lock"), str(out)], check=True, capture_output=True)
    assert out.read_text() == (FP / "cargo-sources.json").read_text(), "run packaging/flatpak/gen-cargo-sources.py"
    srcs = json.loads(out.read_text())
    crates = [s for s in srcs if s["type"] == "archive"]
    assert crates and all(len(s["sha256"]) == 64 and s["url"].startswith("https://static.crates.io/") for s in crates)
    assert srcs[-1]["dest-filename"] == "config.toml" and 'replace-with = "vendored-sources"' in srcs[-1]["contents"]


def test_manifest_permissions():
    text = MANIFEST.read_text()
    for arg in ("--share=network", "--filesystem=xdg-download", "--filesystem=xdg-run/ldm:create", "--socket=wayland", "--socket=fallback-x11"):
        assert f"- {arg}" in text, arg
    assert "cargo-sources.json" in text and "--offline --locked" in text
    assert "skip: [target," in text  # never copy the multi-GB target/ into the build
    fb = os.environ.get("LDM_FLATPAK_BUILDER") or shutil.which("flatpak-builder")
    if not fb:
        pytest.skip("flatpak-builder not available; text checks only")
    r = subprocess.run([fb, "--show-manifest", str(MANIFEST)], capture_output=True, text=True, cwd=FP, timeout=60)
    assert r.returncode == 0, r.stderr
    m = json.loads(r.stdout)
    assert m.get("id", m.get("app-id")) == "io.github.wzk4work.LDM" and m["command"] == "ldm-gui"
    assert [x["name"] for x in m["modules"]] == ["ffmpeg", "ldm"]


@pytest.fixture()
def prefix(tmp_path):
    p, run, home = tmp_path / "app", tmp_path / "run", tmp_path / "home"
    run.mkdir(mode=0o700)
    home.mkdir()
    subprocess.run(["sh", str(FP / "install-flatpak.sh"), str(p), str(BIN)], check=True, capture_output=True)
    env = {"HOME": str(home), "XDG_RUNTIME_DIR": str(run), "PATH": f"{p}/bin:/usr/bin:/bin", "LANG": "C.UTF-8"}
    yield p, env, run
    subprocess.run([str(p / "bin" / "ldm"), "daemon", "stop"], env=env, capture_output=True, timeout=30)


def test_flatpak_layout_runs(prefix):
    p, env, run = prefix
    assert (p / "share/applications/io.github.wzk4work.LDM.desktop").exists() and (p / "share/icons/hicolor/128x128/apps/io.github.wzk4work.LDM.png").exists()
    desk = (p / "share/applications/io.github.wzk4work.LDM.desktop").read_text()
    assert "Exec=ldm-gui" in desk and "Icon=io.github.wzk4work.LDM" in desk
    if shutil.which("desktop-file-validate"):
        r = subprocess.run(["desktop-file-validate", str(p / "share/applications/io.github.wzk4work.LDM.desktop")], capture_output=True, text=True)
        assert r.returncode == 0, r.stdout + r.stderr

    r = subprocess.run([str(p / "bin/ldm"), "daemon", "start"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, r.stderr
    sock = run / "ldm" / "ldm.sock"
    t0 = time.time()
    while not sock.exists() and time.time() - t0 < 10:
        time.sleep(0.1)
    assert sock.exists(), "daemon did not use the shared xdg-run/ldm socket"
    r = subprocess.run([str(p / "bin/ldm"), "--json", "list"], env=env, capture_output=True, text=True, timeout=30)
    assert r.returncode == 0, r.stderr
    assert hello_via([str(p / "bin/ldm-native-host"), ORIGIN], env) == "HelloAck"

    genv = dict(env, LDM_PYTHON=str(ROOT / ".venv" / "bin" / "python"), QT_QPA_PLATFORM="offscreen")
    r = subprocess.run([str(p / "bin/ldm-gui"), "--smoke", "3"], env=genv, capture_output=True, text=True, timeout=90)
    assert r.returncode == 0, r.stderr[-2000:]
    assert json.loads(r.stdout.strip().splitlines()[-1])["connected"] is True


def test_host_integration_install_remove(tmp_path):
    home = tmp_path / "home"
    home.mkdir()
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin"}
    other = home / ".config/chromium/NativeMessagingHosts"
    r = subprocess.run(["sh", str(FP / "host-integration.sh"), "install"], env=env, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    wrap = home / ".local/share/ldm/flatpak/ldm-native-host"
    assert "flatpak run --command=ldm-native-host io.github.wzk4work.LDM" in wrap.read_text() and os.access(wrap, os.X_OK)
    chrome = json.loads((home / ".config/google-chrome/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    firefox = json.loads((home / ".mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    assert chrome["path"] == str(wrap) == firefox["path"] and chrome["allowed_origins"] == [ORIGIN]
    (other / "io.github.wzk4work.ldm.browser_host.json").write_text('{"path": "/usr/lib/ldm/ldm-native-host"}')  # someone else's
    r = subprocess.run(["sh", str(FP / "host-integration.sh"), "remove"], env=env, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    assert not wrap.exists() and not (home / ".mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json").exists()
    assert (other / "io.github.wzk4work.ldm.browser_host.json").exists()
