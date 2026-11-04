"""Smoke test of the built AppImage (dist/LDM-*-x86_64.AppImage; skipped when none was built).

Runs every role from the image in a clean environment (no venv, isolated HOME/XDG dirs): the CLI,
the daemon started through the per-user symlink (its own mount, outlives the CLI), a real local
download, the native-messaging host as Chrome and Firefox launch it, the GUI under xvfb, and the
per-user integration install/remove. Build with packaging/appimage/build-appimage.sh."""

from __future__ import annotations

import functools
import hashlib
import http.server
import json
import os
import shutil
import struct
import subprocess
import threading
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
IMAGES = sorted((ROOT / "dist").glob("LDM-*-x86_64.AppImage"))
pytestmark = pytest.mark.skipif(not IMAGES, reason="no AppImage built (packaging/appimage/build-appimage.sh)")
ORIGIN = "chrome-extension://" + (ROOT / "extension" / "EXTENSION_ID").read_text().strip() + "/"


@pytest.fixture()
def ai(tmp_path):
    home, run, www = tmp_path / "home", tmp_path / "run", tmp_path / "www"
    for d in (home, run, www):
        d.mkdir()
    run.chmod(0o700)
    env = {"HOME": str(home), "XDG_RUNTIME_DIR": str(run), "PATH": "/usr/bin:/bin", "LANG": "C.UTF-8"}
    if not Path("/dev/fuse").exists():
        env["APPIMAGE_EXTRACT_AND_RUN"] = "1"
    data = os.urandom(3_000_000)
    (www / "f.bin").write_bytes(data)
    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(QuietHandler, directory=str(www)))
    threading.Thread(target=srv.serve_forever, daemon=True).start()

    def run_(*args, **kw):
        return subprocess.run([str(IMAGES[-1]), *args], env=env, capture_output=True, text=True, timeout=60, **kw)

    run_.env, run_.home, run_.base, run_.digest = env, home, f"http://127.0.0.1:{srv.server_port}", hashlib.sha256(data).hexdigest()
    yield run_
    run_("ldm", "daemon", "stop")
    srv.shutdown()


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *a):
        pass


def frame(obj) -> bytes:
    b = json.dumps(obj).encode()
    return struct.pack("<I", len(b)) + b


def hello_via(args: list[str], env: dict) -> str:
    """One Hello over native messaging, stdin kept open like a browser port; returns the reply type."""
    p = subprocess.Popen(args, env=env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    try:
        p.stdin.write(frame({"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "version": "120"}}))
        p.stdin.flush()
        n = struct.unpack("<I", p.stdout.read(4))[0]
        return json.loads(p.stdout.read(n))["type"]
    finally:
        p.stdin.close()
        p.wait(timeout=10)


def daemon_env(pid: int) -> dict:
    raw = Path(f"/proc/{pid}/environ").read_bytes().split(b"\0")
    return dict(x.decode(errors="replace").split("=", 1) for x in raw if b"=" in x)


def test_appimage_roles(ai):
    r = ai("ldm", "--version")
    assert r.returncode == 0 and r.stdout.startswith("ldm "), r

    # daemon through the per-user symlink -> separate mount that survives the CLI's
    r = ai("ldm", "daemon", "start")
    assert r.returncode == 0, r.stderr
    link = ai.home / ".local/share/ldm/appimage/ldmd"
    assert link.is_symlink() and Path(os.readlink(link)) == IMAGES[-1]
    pids = [int(p) for p in subprocess.run(["pgrep", "-x", "ldmd"], capture_output=True, text=True).stdout.split()]
    ours = [p for p in pids if daemon_env(p).get("HOME") == str(ai.home)]
    assert ours, "ldmd from the image is not running after the CLI exited"
    assert not any(k in daemon_env(ours[0]) for k in ("PYTHONHOME", "PYTHONPATH", "LD_LIBRARY_PATH"))

    # a real download through the image's daemon
    dl = ai.home / "dl"
    r = ai("ldm", "--json", "add", "-d", str(dl), ai.base + "/f.bin")
    assert r.returncode == 0, r.stderr
    t0 = time.time()
    while time.time() - t0 < 30:
        jobs = json.loads(ai("ldm", "--json", "list").stdout or "[]")
        jobs = jobs.get("jobs", jobs) if isinstance(jobs, dict) else jobs
        if jobs and jobs[0].get("state") == "Completed":
            break
        time.sleep(0.3)
    assert jobs and jobs[0]["state"] == "Completed", jobs
    assert hashlib.sha256(Path(jobs[0]["final_path"]).read_bytes()).hexdigest() == ai.digest

    # native host as Chrome launches it (origin argument) and as Firefox does (manifest path + id)
    assert hello_via([str(IMAGES[-1]), ORIGIN], ai.env) == "HelloAck"
    man = ai.home / "io.github.wzk4work.ldm.browser_host.json"
    man.write_text("{}")
    assert hello_via([str(IMAGES[-1]), str(man), "ldm@wzk4work.github.io"], ai.env) == "HelloAck"


@pytest.mark.skipif(not shutil.which("xvfb-run"), reason="xvfb-run missing")
def test_appimage_gui_under_xvfb(ai):
    ai("ldm", "daemon", "start")
    r = subprocess.run(
        ["xvfb-run", "-a", "-s", "-screen 0 1280x800x24", str(IMAGES[-1]), "--smoke", "3"], env=ai.env, capture_output=True, text=True, timeout=90
    )
    assert r.returncode == 0, r.stderr[-2000:]
    out = json.loads(r.stdout.strip().splitlines()[-1])
    assert out["platform"] == "xcb" and out["connected"] is True, out


def test_appimage_integration_install_remove(ai):
    r = ai("--install-integration")
    assert r.returncode == 0, r.stderr
    host = str(ai.home / ".local/share/ldm/appimage/ldm-native-host")
    chrome = json.loads((ai.home / ".config/google-chrome/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    firefox = json.loads((ai.home / ".mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    assert chrome["path"] == host == firefox["path"] and chrome["allowed_origins"] == [ORIGIN]
    desk = (ai.home / ".local/share/applications/ldm.desktop").read_text()
    assert f'Exec="{IMAGES[-1]}"' in desk and (ai.home / ".local/share/icons/hicolor/128x128/apps/ldm.png").exists()
    r = ai("--remove-integration")
    assert r.returncode == 0, r.stderr
    assert not (ai.home / ".config/google-chrome/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").exists()
    assert not (ai.home / ".local/share/applications/ldm.desktop").exists()
