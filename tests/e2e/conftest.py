"""Fixtures: isolated XDG environment, local test server, ldmd with WS bridge."""

from __future__ import annotations

import hashlib
import json
import os
import shutil
import signal
import socket
import subprocess
import time
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[2]
# public identifiers (app id, D-Bus name, native host, Firefox id): packaging/identifiers.env
IDS = {
    k.strip(): v.strip() for k, v in (ln.split("=", 1) for ln in (ROOT / "packaging/identifiers.env").read_text().splitlines() if ln and not ln.startswith("#"))
}
BIN = Path(os.environ.get("LDM_BIN_DIR", ROOT / "target" / "debug"))
SAMPLE = ROOT / "tests" / "fixtures" / "sample"  # small local page with an mp4 and an HLS playlist


def sha256(p) -> str:
    return hashlib.sha256(Path(p).read_bytes()).hexdigest()


def rand_bytes(n: int, seed: int) -> bytes:
    out = bytearray()
    h = hashlib.sha256(str(seed).encode()).digest()
    while len(out) < n:
        h = hashlib.sha256(h).digest()
        out += h
    return bytes(out[:n])


class Ipc:
    """NDJSON client for the ldmd Unix socket."""

    def __init__(self, path: str):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.connect(path)
        self.sock.settimeout(15)
        self.buf = b""
        self.next_id = 1
        self.events: list[dict] = []

    def _readline(self) -> dict:
        while b"\n" not in self.buf:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise EOFError("daemon closed")
            self.buf += chunk
        line, self.buf = self.buf.split(b"\n", 1)
        return json.loads(line)

    def call(self, typ: str, payload: dict | None = None, replies: int = 1):
        mid = self.next_id
        self.next_id += 1
        self.sock.sendall((json.dumps({"v": 1, "id": mid, "type": typ, "payload": payload or {}}) + "\n").encode())
        got = []
        while len(got) < replies:
            m = self._readline()
            if m["id"] == mid:
                got.append(m)
            else:
                self.events.append(m)
        return got[0] if replies == 1 else got

    def job(self, jid: str) -> dict:
        return self.call("JobGet", {"id": jid})["payload"]["job"]

    def jobs(self) -> list[dict]:
        return self.call("JobList", {})["payload"]["jobs"]

    def wait_state(self, jid: str, state: str, timeout: float = 30) -> dict:
        t0 = time.time()
        while True:
            j = self.job(jid)
            if j["state"] == state:
                return j
            if j["state"] == "Failed" and state != "Failed":
                raise AssertionError(f"job failed: {j}")
            if time.time() - t0 > timeout:
                raise AssertionError(f"timeout waiting for {state}: {j}")
            time.sleep(0.05)

    def close(self):
        self.sock.close()


class Env:
    def __init__(self, tmp: Path):
        self.tmp = tmp
        self.www = tmp / "www"
        self.www.mkdir()
        self.home = tmp / "home"
        self.home.mkdir()
        self.run = tmp / "run"
        self.run.mkdir(mode=0o700)
        self.dl = tmp / "dl"
        self.env = dict(os.environ)
        self.env.update(
            HOME=str(self.home),
            XDG_RUNTIME_DIR=str(self.run),
            XDG_CONFIG_HOME=str(self.home / ".config"),
            XDG_DATA_HOME=str(self.home / ".local/share"),
            XDG_CACHE_HOME=str(self.home / ".cache"),
            XDG_DOWNLOAD_DIR=str(self.dl),
            LDM_NO_NOTIFY="1",
            NO_COLOR="1",  # log assertions match plain text (no ANSI escapes)
            LDM_LOG="info",
        )
        self.env.pop("LDM_SOCKET", None)
        self.socket = str(self.run / "ldm.sock")
        self.srv = None
        self.base = None
        self.daemon = None
        self.ws_port = None
        self.write_config({})

    def write_config(self, extra: dict):
        cfg = self.home / ".config" / "ldm" / "config.toml"
        cfg.parent.mkdir(parents=True, exist_ok=True)
        lines = ["[engine]", "retry_base_ms = 50", "fsync = false"]
        for sect, kv in extra.items():
            if sect == "engine":
                lines += [f"{k} = {json.dumps(v)}" for k, v in kv.items()]
        for sect, kv in extra.items():
            if sect != "engine":
                lines.append(f"[{sect}]")
                lines += [f"{k} = {json.dumps(v)}" for k, v in kv.items()]
        cfg.write_text("\n".join(lines) + "\n")

    def start_server(self):
        self.srv = subprocess.Popen([str(BIN / "ldm-testsrv"), str(self.www), "127.0.0.1:0"], stdout=subprocess.PIPE, text=True)
        line = self.srv.stdout.readline().split()
        assert line[0] == "LISTEN", line
        self.base = f"http://{line[1]}"

    def server_stats(self) -> dict:
        import urllib.request

        with urllib.request.urlopen(self.base + "/__stats") as r:
            return json.loads(r.read())

    def start_daemon(self, ws: bool = True):
        args = [str(BIN / "ldmd")]
        if ws:
            args += ["--ws", "--ws-port", "0"]
        self.log = open(self.tmp / "ldmd.log", "a")
        self.daemon = subprocess.Popen(args, env=self.env, stdout=subprocess.PIPE, stderr=self.log, text=True)
        t0 = time.time()
        while True:
            line = self.daemon.stdout.readline()
            if not line:
                raise RuntimeError("ldmd exited: " + (self.tmp / "ldmd.log").read_text())
            if line.startswith("WS_PORT"):
                self.ws_port = int(line.split()[1])
            if line.startswith("READY"):
                break
            assert time.time() - t0 < 10
        return Ipc(self.socket)

    def kill_daemon(self, sig=signal.SIGKILL):
        if self.daemon and self.daemon.poll() is None:
            self.daemon.send_signal(sig)
            self.daemon.wait(timeout=15)
        self.daemon = None

    def cli(self, *args, check=True):
        r = subprocess.run([str(BIN / "ldm"), *args], env=self.env, capture_output=True, text=True, timeout=120)
        if check and r.returncode != 0:
            raise AssertionError(f"ldm {args} -> {r.returncode}\n{r.stdout}\n{r.stderr}")
        return r

    def install_sample(self):
        dst = self.www / "sample"
        shutil.copytree(SAMPLE, dst)
        return dst

    def stop(self):
        if self.daemon and self.daemon.poll() is None:
            self.daemon.send_signal(signal.SIGTERM)
            try:
                self.daemon.wait(timeout=15)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
        if self.srv:
            self.srv.kill()


@pytest.fixture
def env(tmp_path):
    e = Env(tmp_path)
    e.start_server()
    yield e
    e.stop()


def make_placement_fixture(www: Path, base: str) -> str:
    """In-page button placement fixture: an outstream ad video in a cross-origin iframe, LARGER than the
    main player and above it, inside an ad container (the child frame cannot see that container, so
    only the parent frame's report through the background marks it). Returns the page path."""
    ff = ["ffmpeg", "-v", "error", "-y", "-f", "lavfi"]
    pl = www / "placement"
    (pl / "frames").mkdir(parents=True)
    # main video 40 s, ad clip 6 s; tiny frames and bitrate: only durations matter here
    subprocess.run(
        ff + ["-i", "testsrc=size=160x90:rate=10", "-t", "40", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "50", "-crf", "40", str(pl / "main.mp4")],
        check=True,
    )
    subprocess.run(
        ff + ["-i", "testsrc2=size=160x90:rate=10", "-t", "6", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "40", str(pl / "frames" / "clip7.mp4")],
        check=True,
    )
    (pl / "frames" / "unit7.html").write_text(
        "<!doctype html><title>unit</title><style>body{margin:0}</style>"
        '<video id="adv" src="clip7.mp4" muted autoplay loop playsinline style="width:800px;height:450px;display:block"></video>'
    )
    other = base.replace("127.0.0.1", "localhost")
    (pl / "page.html").write_text(
        "<!doctype html><title>LDM placement page</title><style>body{margin:8px}</style>"
        '<div class="outstream-ad-wrapper"><iframe id="adframe" src="'
        + other
        + '/placement/frames/unit7.html" width="800" height="450" style="border:0;display:block" allow="autoplay"></iframe></div>'
        '<div id="player-main"><video id="main" src="main.mp4" muted autoplay loop playsinline width="640" height="360" style="display:block"></video></div>'
    )
    return "/placement/page.html"


MSE_PLAYER_JS = r"""
// Minimal self-written MSE player for the E2E fixture: fetches a static DASH manifest (SegmentTemplate
// with a fixed segment duration, one Representation per AdaptationSet), creates one SourceBuffer per
// AdaptationSet and appends init + media segments in order.
async function ldmFixturePlay(video, mpdUrl) {
  const base = new URL(mpdUrl, location.href);
  const doc = new DOMParser().parseFromString(await (await fetch(base)).text(), "application/xml");
  const total = (s => { const m = /PT(?:(\d+)H)?(?:(\d+)M)?([\d.]+)S/.exec(s || ""); return m ? (+m[1] || 0) * 3600 + (+m[2] || 0) * 60 + +m[3] : 0; })(
    doc.documentElement.getAttribute("mediaPresentationDuration"));
  const ms = new MediaSource();
  video.src = URL.createObjectURL(ms);
  await new Promise(r => ms.addEventListener("sourceopen", r, { once: true }));
  const feeds = [...doc.getElementsByTagName("AdaptationSet")].map(as => {
    const rep = as.getElementsByTagName("Representation")[0];
    const t = rep.getElementsByTagName("SegmentTemplate")[0] || as.getElementsByTagName("SegmentTemplate")[0];
    const mime = rep.getAttribute("mimeType") || as.getAttribute("mimeType");
    const sb = ms.addSourceBuffer(`${mime}; codecs="${rep.getAttribute("codecs")}"`);
    const fill = (p, n) => p.replace("$RepresentationID$", rep.getAttribute("id"))
      .replace(/\$Number%0(\d)d\$/, (_, w) => String(n).padStart(+w, "0")).replace("$Number$", n);
    const count = Math.ceil(total / (+t.getAttribute("duration") / +(t.getAttribute("timescale") || 1)));
    const first = +(t.getAttribute("startNumber") || 1);
    const urls = [fill(t.getAttribute("initialization"), 0)];
    for (let i = 0; i < count; i++) urls.push(fill(t.getAttribute("media"), first + i));
    return (async () => {
      for (const u of urls) {
        const buf = await (await fetch(new URL(u, base))).arrayBuffer();
        sb.appendBuffer(buf);
        await new Promise(r => sb.addEventListener("updateend", r, { once: true }));
      }
    })();
  });
  video.play().catch(() => {});
  await Promise.all(feeds);
  if (ms.readyState === "open") ms.endOfStream();
}
"""


def make_mse_fixture(www: Path) -> str:
    """MSE fixture: two DASH streams (H.264 + AAC, fMP4 segments) played by the self-written MSE player
    above. The big main player plays main/main.mpd; a small second player loads second/second.mpd later
    (so it is the most recent manifest, listed first on a tie) and pauses after a moment. Same duration,
    no ad words: both are blob: players, so without the MSE hook every blob player maps to every manifest
    and the two rows score the same. Returns the page path."""
    root = www / "mse"
    for name, src, secs in (("main", "testsrc=size=160x90:rate=10", 40), ("second", "testsrc2=size=160x90:rate=10", 40)):
        d = root / name
        d.mkdir(parents=True)
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", src, "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100", "-t", str(secs)]
            + ["-c:v", "libx264", "-pix_fmt", "yuv420p", "-g", "20", "-keyint_min", "20", "-sc_threshold", "0", "-crf", "40", "-c:a", "aac", "-b:a", "48k"]
            + ["-f", "dash", "-seg_duration", "2", "-use_template", "1", "-use_timeline", "0", "-adaptation_sets", "id=0,streams=v id=1,streams=a"]
            + ["-init_seg_name", "init-$RepresentationID$.m4s", "-media_seg_name", "seg-$RepresentationID$-$Number%03d$.m4s", str(d / f"{name}.mpd")],
            check=True,
        )
    (root / "player.js").write_text(MSE_PLAYER_JS)
    (root / "page.html").write_text(
        '<!doctype html><title>LDM MSE page</title><style>body{margin:8px}</style><script src="player.js"></script>'
        '<video id="main" muted playsinline width="640" height="360" style="display:block"></video>'
        '<video id="second" muted playsinline width="320" height="180" style="display:block;margin-top:20px"></video>'
        "<script>"
        "ldmFixturePlay(document.getElementById('main'), 'main/main.mpd');"
        "setTimeout(() => { const p = document.getElementById('second'); ldmFixturePlay(p, 'second/second.mpd');"
        " setTimeout(() => p.pause(), 2500); }, 2500);"
        "</script>"
    )
    return "/mse/page.html"


# Tests detect the hooks behaviourally: a playlist that answers
# 404 is ignored by webRequest (status >= 400), only the MAIN-world fetch hook reports it.
def hook_probe_js(tag: str) -> str:
    return f"fetch('/hookprobe-{tag}.m3u8?status=404').catch(() => 0), true"


def hook_probe_seen(ipc, tag: str) -> bool:
    return any(f"/hookprobe-{tag}.m3u8" in c["url"] for c in ipc.call("CandidateList", {})["payload"]["candidates"])


# page-side checks that the MSE hooks keep the original name/length/descriptor flags (so feature
# checks in page code keep working) and that the player still plays with the hooks on.
MSE_HOOK_JS = (
    "(() => { const add = MediaSource.prototype.addSourceBuffer, app = SourceBuffer.prototype.appendBuffer,"
    " src = Object.getOwnPropertyDescriptor(HTMLMediaElement.prototype, 'src');"
    " const d = Object.getOwnPropertyDescriptor(MediaSource.prototype, 'addSourceBuffer');"
    " return { add: add.name === 'addSourceBuffer' && add.length === 1,"
    " app: app.name === 'appendBuffer' && app.length === 1,"
    " src: src.set.name === 'set src' && typeof src.get === 'function',"
    " desc: d.writable && d.enumerable && d.configurable,"
    " playing: (v => !v.paused && v.currentTime > 1 && v.src.startsWith('blob:'))(document.getElementById('main')) }; })()"
)


# a TikTok-style MSE player: one <video>, one SourceBuffer, a progressive fMP4 file read with Range
# requests; after a while it switches to another bitrate of the same video and re-feeds the timeline
BITRATE_PLAYER_JS = r"""
async function ldmFeedFile(sb, url) {
  let at = 0, total = Infinity;
  while (at < total) {
    const r = await fetch(url, { headers: { Range: `bytes=${at}-${at + 98303}` } });
    total = +/\/(\d+)$/.exec(r.headers.get("content-range") || "/0")[1] || 0;
    const buf = await r.arrayBuffer();
    at += buf.byteLength;
    sb.appendBuffer(buf);
    await new Promise(r => sb.addEventListener("updateend", r, { once: true }));
    if (!buf.byteLength) break;
  }
}
async function ldmBitrateSwap(video, first, second) {
  const ms = new MediaSource();
  video.src = URL.createObjectURL(ms);
  await new Promise(r => ms.addEventListener("sourceopen", r, { once: true }));
  const sb = ms.addSourceBuffer('video/mp4; codecs="avc1.42c01e"');
  await ldmFeedFile(sb, first);
  video.play().catch(() => {});
  await new Promise(r => setTimeout(r, 3000));
  await ldmFeedFile(sb, second);
  window.ldmSwapped = true;
}
"""


def make_bitrate_fixture(www: Path) -> str:
    """One <video> fed through MSE from low.mp4, then from high.mp4: the same 20 s clip (fragmented
    MP4, video only) at 320x180 and 640x360. `?order=hl` plays high first, then low. Returns the page path."""
    root = www / "bitrate"
    root.mkdir(parents=True)
    for name, size, crf in (("low", "320x180", "36"), ("high", "640x360", "24")):
        subprocess.run(
            ["ffmpeg", "-v", "error", "-y", "-f", "lavfi", "-i", f"testsrc=size={size}:rate=15", "-t", "20"]
            + ["-c:v", "libx264", "-profile:v", "baseline", "-level", "3.0", "-pix_fmt", "yuv420p", "-g", "15", "-crf", crf]
            + ["-movflags", "frag_keyframe+empty_moov+default_base_moof", str(root / f"{name}.mp4")],
            check=True,
        )
    (root / "player.js").write_text(BITRATE_PLAYER_JS)
    (root / "page.html").write_text(
        '<!doctype html><title>LDM bitrate swap</title><style>body{margin:8px}</style><script src="player.js"></script>'
        '<video id="main" muted playsinline width="640" height="360" style="display:block"></video>'
        "<script>const hl = location.search.includes('order=hl');"
        "ldmBitrateSwap(document.getElementById('main'), hl ? 'high.mp4' : 'low.mp4', hl ? 'low.mp4' : 'high.mp4');</script>"
    )
    return "/bitrate/page.html"
