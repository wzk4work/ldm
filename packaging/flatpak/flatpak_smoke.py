"""Opt-in smoke test of the *installed* Flatpak io.github.wzk4work.LDM (run via packaging/flatpak/smoke.sh).

Needs `flatpak` with io.github.wzk4work.LDM installed (e.g. from dist/LDM-<ver>.flatpak), the KDE runtime,
and XDG_RUNTIME_DIR = /run/user/<uid> (the xdg-run/ldm socket share relies on it). Not part of
scripts/check.sh: it needs the ~5 GB runtime and a real Flatpak.

Isolation: every `flatpak run` gets HOME=<tmp>/home, so the app's config/data/cache
(~/.var/app/io.github.wzk4work.LDM) and xdg-download (~/Downloads) are throw-away; FLATPAK_USER_DIR keeps
pointing at the real per-user installation. Refuses to run while another ldmd owns the socket.
Downloaded media is ffprobe'd and then deleted together with the temporary HOME.
"""

from __future__ import annotations

import functools
import hashlib
import http.server
import json
import os
import platform
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(ROOT / "tests" / "e2e"))

from conftest import Ipc, rand_bytes, sha256  # noqa: E402
from test_appimage import QuietHandler, hello_via  # noqa: E402
from test_pac import HttpProxy  # noqa: E402

APP = "io.github.wzk4work.LDM"
FLATPAK = shutil.which("flatpak")
SAMPLE = ROOT / "tests" / "fixtures" / "sample"
FAKE_YTDLP = ROOT / "crates" / "ldm-core" / "tests" / "fixtures" / "fake-yt-dlp"
EXT_ID = (ROOT / "extension" / "EXTENSION_ID").read_text().strip()
RUNDIR = os.environ.get("XDG_RUNTIME_DIR", "")
SOCK = Path(RUNDIR) / "ldm" / "ldm.sock"


def _installed() -> bool:
    return bool(FLATPAK) and subprocess.run([FLATPAK, "info", APP], capture_output=True).returncode == 0


pytestmark = [
    pytest.mark.skipif(not _installed(), reason=f"flatpak or {APP} not installed"),
    pytest.mark.skipif(RUNDIR != f"/run/user/{os.getuid()}", reason="XDG_RUNTIME_DIR must be /run/user/<uid>"),
]


def _live(sock: Path) -> bool:
    s = socket.socket(socket.AF_UNIX)
    try:
        s.connect(str(sock))
        return True
    except OSError:
        return False
    finally:
        s.close()


class Fp:
    def __init__(self, work: Path):
        self.work = work
        self.home = work / "home"
        self.home.mkdir()
        # like a desktop session; flatpak resolves xdg-download from it (no file -> no grant)
        (self.home / ".config").mkdir()
        (self.home / ".config/user-dirs.dirs").write_text('XDG_DOWNLOAD_DIR="$HOME/Downloads"\n')
        self.var = self.home / ".var/app" / APP
        self.env = dict(os.environ, HOME=str(self.home))
        self.env.pop("LDM_SOCKET", None)
        self.www = work / "www"
        (self.www / "hls").mkdir(parents=True)
        shutil.copy(SAMPLE / "video.mp4", self.www / "video.mp4")
        shutil.copy(SAMPLE / "hls" / "seg0.ts", self.www / "hls" / "seg0.ts")
        # the checked-in playlist points at the VM share; serve a relative copy
        (self.www / "hls" / "index.m3u8").write_text(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.0,\nseg0.ts\n#EXT-X-ENDLIST\n"
        )
        self.httpd = http.server.ThreadingHTTPServer(("127.0.0.1", 0), functools.partial(QuietHandler, directory=str(self.www)))
        self.base = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def run(self, command: str, *args: str, timeout: float = 120, **kw) -> subprocess.CompletedProcess:
        return subprocess.run([FLATPAK, "run", f"--command={command}", APP, *args], env=self.env, capture_output=True, text=True, timeout=timeout, **kw)

    def ldm(self, *args: str, **kw) -> subprocess.CompletedProcess:
        return self.run("ldm", *args, **kw)

    def log(self) -> str:
        p = self.var / "cache/ldm/ldmd.log"
        return p.read_text() if p.exists() else ""

    def ipc(self) -> Ipc:
        return Ipc(str(SOCK))

    def add(self, url: str, **extra) -> str:
        r = self.ipc().call("DownloadAdd", {"url": url, **extra})
        assert r["type"] != "Error", r
        return r["payload"]["jobId"]


def ffprobe(p: str) -> dict:
    r = subprocess.run(["ffprobe", "-v", "error", "-show_format", "-show_streams", "-of", "json", p], capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout)


@pytest.fixture(scope="module")
def fp():
    if _live(SOCK):
        pytest.skip(f"another ldmd is listening on {SOCK}; stop it first")
    # not under /tmp: the sandbox has its own per-app /tmp, so paths there differ inside and out
    cache = Path(os.environ.get("XDG_CACHE_HOME") or Path.home() / ".cache")
    cache.mkdir(parents=True, exist_ok=True)
    f = Fp(Path(tempfile.mkdtemp(prefix="ldm-flatpak-smoke-", dir=cache)))
    # not a pipe: the sandbox's helper processes (bwrap, xdg-dbus-proxy) live as long as the
    # daemon and keep inherited descriptors open
    with open(f.work / "start.out", "w") as out:
        r = subprocess.run([FLATPAK, "run", "--command=ldm", APP, "daemon", "start"], env=f.env, stdout=out, stderr=out, timeout=60)
    assert r.returncode == 0, (f.work / "start.out").read_text()
    t0 = time.time()
    while not _live(SOCK):
        assert time.time() - t0 < 20, "daemon socket did not appear on the host side"
        time.sleep(0.2)
    yield f
    f.ldm("daemon", "stop")
    f.httpd.shutdown()
    shutil.rmtree(f.work, ignore_errors=True)  # downloaded media goes with the temporary HOME


def test_cli_and_bundled_tools(fp):
    r = fp.ldm("--version")
    assert r.returncode == 0 and r.stdout.startswith("ldm "), r
    for tool in ("ffmpeg", "ffprobe"):
        r = fp.run(tool, "-version")
        assert r.returncode == 0 and r.stdout.startswith(f"{tool} version"), r
    r = fp.ldm("--json", "daemon", "status")
    assert r.returncode == 0, r
    st = json.loads(r.stdout)
    assert st["socket"] == str(SOCK) and st["db"].startswith(str(fp.var)), st
    assert f"socket={RUNDIR}/ldm/ldm.sock" in fp.log()


def test_ungranted_dir_is_refused(fp):
    # ~/Downloads is created and shared (xdg-download:create); other folders are not
    assert (fp.home / "Downloads").is_dir()
    r = fp.ldm("add", "-d", str(fp.home / "NotGranted"), fp.base + "/video.mp4")
    assert r.returncode != 0 and "flatpak override" in (r.stdout + r.stderr), r
    assert not (fp.home / "NotGranted").exists()


def test_http_download_via_cli(fp):
    d = fp.home / "Downloads" / "smoke-http"
    r = fp.ldm("add", "-w", "-d", str(d), fp.base + "/video.mp4")
    assert r.returncode == 0, r.stderr
    out = d / "video.mp4"
    assert sha256(out) == sha256(fp.www / "video.mp4")
    assert ffprobe(str(out))["streams"]
    out.unlink()


def test_hls_download_and_ffmpeg_remux(fp):
    ipc = fp.ipc()
    j = ipc.wait_state(fp.add(fp.base + "/hls/index.m3u8"), "Completed", 60)
    p = j["final_path"]
    assert p.startswith(str(fp.home / "Downloads")) and p.endswith(".mp4"), p
    info = ffprobe(p)
    assert "mp4" in info["format"]["format_name"], info["format"]
    assert {s["codec_type"] for s in info["streams"]} >= {"video"}
    Path(p).unlink()


def _publish_fake_ytdlp(fp, version: str) -> None:
    """A fake GitHub release on the local server: the asset is a tiny wrapper around the
    fake yt-dlp, which lives in the app's data dir so the sandbox can run it."""
    fake = fp.var / "data/ldm-smoke-fake"
    fake.mkdir(parents=True, exist_ok=True)
    shutil.copy(FAKE_YTDLP, fake / "fake-yt-dlp")
    shutil.copy(FAKE_YTDLP.with_name("ytdlp-info.json"), fake)  # the fake reads its canned info from beside itself
    asset = {"x86_64": "yt-dlp_linux", "aarch64": "yt-dlp_linux_aarch64"}[platform.machine()]
    data = f'#!/bin/sh\nFAKE_YTDLP_VERSION={version} exec python3 {fake}/fake-yt-dlp "$@"\n'.encode()
    d = fp.www / "yt-dlp/yt-dlp/releases/download" / version
    d.mkdir(parents=True, exist_ok=True)
    (d / asset).write_bytes(data)
    (d / "SHA2-256SUMS").write_text(f"{hashlib.sha256(data).hexdigest()}  {asset}\n")
    api = fp.www / "api/repos/yt-dlp/yt-dlp/releases/latest"
    api.parent.mkdir(parents=True, exist_ok=True)
    api.write_text(json.dumps({"tag_name": version}))


def test_ytdlp_absent_then_managed_install(fp):
    """No yt-dlp in the sandbox → consent hint → `ldm tools install` puts a verified copy in
    ~/.var/app/io.github.wzk4work.LDM/data/ldm/tools (no extra permission) → used, updated, rolled back."""
    ipc = fp.ipc()
    # tests only: loopback release server instead of GitHub; no background update checks
    patch = {"ytdlp": {"release_base_url": fp.base, "auto_update": "off"}}
    assert ipc.call("ConfigSet", {"patch": patch})["type"] != "Error"
    r = fp.ldm("doctor")
    yt = next(line for line in r.stdout.splitlines() if "yt-dlp:" in line)
    assert yt.startswith("[WARN]") and "ldm tools install" in yt, yt
    assert "Native Messaging: 在 Flatpak 沙箱内无法检查" in r.stdout
    url = "https://www.youtube.com/watch?v=aaaaaaaaaaa"
    r = ipc.call("DownloadAdd", {"url": url, "kind": "ytdlp"})
    if r["type"] != "Error":
        j = ipc.wait_state(r["payload"]["jobId"], "Failed", 30)
        assert j.get("error_code") == "E_TOOL_MISSING", j
        r = {"payload": {"message": j.get("error")}}
    assert "ldm tools install" in json.dumps(r, ensure_ascii=False), r
    _publish_fake_ytdlp(fp, "2026.09.01")
    r = fp.ldm("tools", "install", "yt-dlp")
    assert r.returncode == 0 and "2026.09.01" in r.stdout and "SHA2-256SUMS" in r.stdout, r.stdout + r.stderr
    tools = fp.var / "data/ldm/tools/yt-dlp"
    assert (tools / "2026.09.01/yt-dlp").is_file() and os.readlink(tools / "current") == "2026.09.01"
    r = fp.ldm("doctor")
    yt = next(line for line in r.stdout.splitlines() if "yt-dlp:" in line)
    assert yt.startswith("[ OK ]") and "2026.09.01（来源：LDM 托管" in yt, yt
    j = ipc.wait_state(fp.add(url + "b", kind="ytdlp"), "Completed", 60)
    assert Path(j["final_path"]).stat().st_size > 0
    assert (j.get("ytdlp") or {}).get("bin_source") == "managed", j.get("ytdlp")
    Path(j["final_path"]).unlink()
    # update + rollback (the old version stays for rollback)
    _publish_fake_ytdlp(fp, "2026.10.21")
    r = fp.ldm("tools", "update", "yt-dlp")
    assert r.returncode == 0 and "2026.10.21" in r.stdout, r.stdout + r.stderr
    assert os.readlink(tools / "current") == "2026.10.21" and os.readlink(tools / "previous") == "2026.09.01"
    r = fp.ldm("tools", "rollback", "yt-dlp")
    assert r.returncode == 0 and os.readlink(tools / "current") == "2026.09.01", r.stdout + r.stderr
    r = fp.ldm("tools", "remove", "yt-dlp")
    assert r.returncode == 0 and not (tools / "current").exists(), r.stdout + r.stderr


def test_pac(fp):
    proxy = HttpProxy()
    (fp.www / "proxy.pac").write_text(f'function FindProxyForURL(url, host) {{ return "PROXY 127.0.0.1:{proxy.port}"; }}\n')
    (fp.www / "pac.bin").write_bytes(rand_bytes(300_000, 5))
    ipc = fp.ipc()
    try:
        assert ipc.call("ConfigSet", {"patch": {"engine": {"proxy": "pac", "pac_url": fp.base + "/proxy.pac"}}})["type"] != "Error"
        j = ipc.wait_state(fp.add(fp.base + "/pac.bin"), "Completed", 60)
        assert sha256(j["final_path"]) == sha256(fp.www / "pac.bin")
        assert any("/pac.bin" in s for s in proxy.seen), proxy.seen
        assert "PAC script loaded" in fp.log()
        Path(j["final_path"]).unlink()
    finally:
        ipc.call("ConfigSet", {"patch": {"engine": {"proxy": "system", "pac_url": ""}}})
        proxy.close()


def test_ftps_release_build(fp, tmp_path):
    pytest.importorskip("pyftpdlib")
    pytest.importorskip("OpenSSL")
    import ipaddress

    from cryptography import x509
    from cryptography.hazmat.primitives import serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from test_ftps import DATA, SHA, Server, _cert

    # self-signed server certificate: a release build has no test CA, so verification must fail
    k = ec.generate_private_key(ec.SECP256R1())
    c = _cert("127.0.0.1", k, "127.0.0.1", k, sans=[x509.IPAddress(ipaddress.ip_address("127.0.0.1"))])
    pem = tmp_path / "srv.pem"
    pem.write_bytes(
        c.public_bytes(serialization.Encoding.PEM)
        + k.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption())
    )
    root = tmp_path / "ftp"
    root.mkdir()
    (root / "f.bin").write_bytes(DATA)
    srv = Server(root, cert=pem)
    ipc = fp.ipc()
    try:
        j = ipc.wait_state(fp.add(f"ftpes://127.0.0.1:{srv.port}/f.bin"), "Failed", 30)
        assert "certificate" in (j.get("error") or "").lower(), j
        r = ipc.call("ConfigSet", {"patch": {"sites": {"tls": [{"host": "127.0.0.1", "accept_invalid_cert": True}]}}})
        assert r["type"] != "Error", r
        j = ipc.wait_state(fp.add(f"ftpes://alice:s3cret@127.0.0.1:{srv.port}/f.bin?2"), "Completed", 60)
        assert sha256(j["final_path"]) == SHA
        assert "FTPS data channel (PROT P)" in fp.log()
        Path(j["final_path"]).unlink()
    finally:
        ipc.call("ConfigSet", {"patch": {"sites": {"tls": []}}})
        srv.close()


@pytest.fixture(scope="module")
def host_integration(fp):
    r = subprocess.run([str(HERE / "host-integration.sh"), "install"], env=fp.env, capture_output=True, text=True)
    assert r.returncode == 0, r.stderr
    yield fp.home / ".local/share/ldm/flatpak/ldm-native-host"
    subprocess.run([str(HERE / "host-integration.sh"), "remove"], env=fp.env, capture_output=True)


def test_native_host_stdio_handshake(fp, host_integration):
    chrome = json.loads((fp.home / ".config/google-chrome/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    ff = fp.home / ".mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json"
    assert chrome["path"] == str(host_integration) == json.loads(ff.read_text())["path"]
    # argv exactly as the browsers pass it
    assert hello_via([chrome["path"], f"chrome-extension://{EXT_ID}/"], fp.env) == "HelloAck"
    assert hello_via([chrome["path"], str(ff), "ldm@wzk4work.github.io"], fp.env) == "HelloAck"


def _wait_log(fp, needle: str, timeout: float, after: int = 0) -> None:
    """Wait for a NEW occurrence of `needle` (the handshake test logs the same hello lines)."""
    t0 = time.time()
    while fp.log().count(needle) <= after:
        assert time.time() - t0 < timeout, fp.log()[-2000:]
        time.sleep(0.3)


def test_real_chrome_through_flatpak_host(fp, host_integration):
    from test_chrome import CHROME, EXT_DIR, Cdp

    if not CHROME or not (EXT_DIR / "manifest.json").exists():
        pytest.skip("chrome or built extension missing")
    # Chrome refuses DevTools on its default profile dir, so use another one and copy in the
    # manifest host-integration.sh wrote (Chrome reads <user-data-dir>/NativeMessagingHosts)
    ud = fp.work / "chrome-profile"
    shutil.copytree(fp.home / ".config/google-chrome/NativeMessagingHosts", ud / "NativeMessagingHosts")
    page = fp.www / "page.html"
    page.write_text('<!doctype html><title>t</title><video src="video.mp4" controls muted autoplay width="640" height="360"></video>')
    seen = fp.log().count('browser="chrome"')
    cdp = Cdp(dict(fp.env, LDM_NO_AUTOSTART="1"), ud)
    try:
        assert cdp.call("Extensions.loadUnpacked", {"path": str(EXT_DIR)})["id"] == EXT_ID
        _wait_log(fp, 'browser="chrome"', 40, seen)
        cdp.call("Target.createTarget", {"url": fp.base + "/page.html"})
        t0 = time.time()
        while not [c for c in fp.ipc().call("CandidateList", {})["payload"]["candidates"] if c["url"].endswith("/video.mp4")]:
            assert time.time() - t0 < 30, "no media candidate reported through the Flatpak native host:\n" + fp.log()[-3000:]
            time.sleep(0.3)
    finally:
        cdp.close()


def test_real_firefox_through_flatpak_host(fp, host_integration, tmp_path):
    import test_firefox as tf

    if not (tf.FIREFOX and tf.GECKODRIVER and tf.webdriver and (tf.EXT_DIR / "manifest.json").exists()):
        pytest.skip("firefox, geckodriver, selenium or built extension missing")
    dl = tmp_path / "ffdl"
    dl.mkdir()
    seen = fp.log().count('browser="firefox"')
    drv = tf.start_firefox(fp.env, fp.home, dl, tmp_path / "gecko.log")
    try:
        _wait_log(fp, 'browser="firefox"', 60, seen)
        # the popup shows "connected" only after a round trip through the native host
        r = tf.ext_message(drv, {"type": "SetSettings", "settings": {"pageHooks": False}})
        assert r and not r.startswith("ERR"), r
    finally:
        drv.quit()


@pytest.mark.skipif(not (shutil.which("xvfb-run") and shutil.which("dbus-run-session")), reason="xvfb-run / dbus-run-session missing")
def test_gui_under_xvfb_owns_dbus_name(fp, tmp_path):
    script = tmp_path / "gui.sh"
    script.write_text(
        "set -e\n"
        f"flatpak run {APP} --smoke 12 > {tmp_path}/smoke.json 2> {tmp_path}/gui.err &\n"
        "for i in $(seq 1 40); do\n"
        "  dbus-send --session --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner"
        " string:io.github.wzk4work.LDM.Daemon1 2>/dev/null | grep -q 'boolean true' && break; sleep 0.25; done\n"
        "dbus-send --session --print-reply --dest=io.github.wzk4work.LDM.Daemon1 /io/github/wzk4work/LDM/Daemon1 io.github.wzk4work.LDM.Daemon1.Status\n"
        f"dbus-send --session --print-reply --dest=io.github.wzk4work.LDM.Daemon1 /io/github/wzk4work/LDM/Daemon1 io.github.wzk4work.LDM.Daemon1.AddUrl string:{fp.base}/video.mp4?dbus\n"
        "wait\n"
    )
    r = subprocess.run(["xvfb-run", "-a", "dbus-run-session", "--", "sh", str(script)], env=fp.env, capture_output=True, text=True, timeout=120)
    assert r.returncode == 0, (r.stdout, r.stderr, (tmp_path / "gui.err").read_text())
    assert "io.github.wzk4work.LDM.Daemon1" not in r.stderr
    smoke = json.loads((tmp_path / "smoke.json").read_text().strip().splitlines()[-1])
    assert smoke["connected"] is True and smoke["platform"] == "xcb", smoke
    jid = re.search(r'string "([0-9a-f]{12})', r.stdout)
    assert jid, r.stdout
    j = fp.ipc().wait_state(jid.group(1), "Completed", 30)
    Path(j["final_path"]).unlink()


def test_daemon_stop(fp):
    r = fp.ldm("daemon", "stop")
    assert r.returncode == 0, r.stderr
    t0 = time.time()
    while _live(SOCK):
        assert time.time() - t0 < 15, "daemon still listening after stop"
        time.sleep(0.2)
