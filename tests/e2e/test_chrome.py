"""Real-browser E2E: headless Google Chrome loads the built MV3 extension
(Extensions.loadUnpacked over a CDP pipe), talks to ldm-native-host → ldmd,
reports page media and hands an attachment download over to LDM."""

from __future__ import annotations

import json
import os
import select
import shutil
import signal
import subprocess
import time
from pathlib import Path

import pytest

from conftest import (
    BIN,
    MSE_HOOK_JS,
    ROOT,
    SAMPLE,
    hook_probe_js,
    hook_probe_seen,
    make_bitrate_fixture,
    make_mse_fixture,
    make_placement_fixture,
    rand_bytes,
    sha256,
)

EXT_DIR = ROOT / "extension" / "dist" / "chromium"
EXT_ID = (ROOT / "extension" / "EXTENSION_ID").read_text().strip()
CHROME = shutil.which("google-chrome") or shutil.which("chromium")

pytestmark = pytest.mark.skipif(not CHROME or not (EXT_DIR / "manifest.json").exists(), reason="chrome or built extension missing")


class Cdp:
    def __init__(self, env: dict, user_data: Path):
        r1, w1 = os.pipe()
        r2, w2 = os.pipe()

        def pre():
            os.dup2(r1, 3)
            os.dup2(w2, 4)
            # dup2(fd, fd) is a no-op that keeps the pipe non-inheritable (when the pipe already got fd 3)
            os.set_inheritable(3, True)
            os.set_inheritable(4, True)

        self.proc = subprocess.Popen(
            [
                CHROME,
                "--headless=new",
                "--no-sandbox",
                "--remote-debugging-pipe",
                "--enable-unsafe-extension-debugging",
                f"--user-data-dir={user_data}",
                "--no-first-run",
                "--no-default-browser-check",
                "about:blank",
            ],
            preexec_fn=pre,
            close_fds=False,
            start_new_session=True,  # google-chrome is a wrapper script: close() kills the whole group
            env=env,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
        )
        os.close(r1)
        os.close(w2)
        self.w, self.r = w1, r2
        self.buf = b""
        self.next = 1
        self.events: list[dict] = []

    def call(self, method: str, params: dict | None = None, session: str | None = None, timeout=15):
        i = self.next
        self.next += 1
        msg = {"id": i, "method": method, "params": params or {}}
        if session:
            msg["sessionId"] = session
        os.write(self.w, json.dumps(msg).encode() + b"\0")
        t0 = time.time()
        while time.time() - t0 < timeout:
            while b"\0" in self.buf:
                raw, self.buf = self.buf.split(b"\0", 1)
                d = json.loads(raw)
                if d.get("id") == i:
                    if "error" in d:
                        raise RuntimeError(f"{method}: {d['error']}")
                    return d.get("result", {})
                self.events.append(d)
            # select so a silent browser hits the timeout instead of blocking in read()
            if select.select([self.r], [], [], 0.5)[0]:
                chunk = os.read(self.r, 1 << 16)
                if not chunk:
                    raise RuntimeError(f"{method}: browser closed the debugging pipe")
                self.buf += chunk
        raise TimeoutError(method)

    def close(self):
        try:
            os.killpg(self.proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        self.proc.wait()
        os.close(self.w)
        os.close(self.r)


def install_manifest(dirs: list[Path]):
    m = {
        "name": "io.github.wzk4work.ldm.browser_host",
        "description": "LDM native host (test)",
        "path": str(BIN / "ldm-native-host"),
        "type": "stdio",
        "allowed_origins": [f"chrome-extension://{EXT_ID}/"],
    }
    for d in dirs:
        d.mkdir(parents=True, exist_ok=True)
        (d / "io.github.wzk4work.ldm.browser_host.json").write_text(json.dumps(m))


def test_chrome_extension_end_to_end(env, tmp_path):
    shutil.copy(SAMPLE / "video.mp4", env.www / "v.mp4")
    (env.www / "page.html").write_text(
        '<!doctype html><title>LDM test page</title><video src="v.mp4" controls muted autoplay width="640" height="360"></video>'
        '<a id="z" href="big.zip?cd=big.zip">zip</a>'
        '<p><a id="alt" href="alt.zip?cd=alt.zip" style="display:inline-block;width:200px;height:30px">alt</a></p>'
        '<p><a id="ins" href="notes.txt" style="display:inline-block;width:200px;height:30px">notes</a></p>'
    )
    (env.www / "alt.zip").write_bytes(rand_bytes(300_000, 78))
    (env.www / "notes.txt").write_bytes(b"plain text line\n" * 2000)
    # served at ~1 MB/s: under CDP-controlled downloads Chrome may skip the
    # onDeterminingFilename hold, so leave the extension time to pause it
    data = rand_bytes(2_500_000, 77)
    (env.www / "big.zip").write_bytes(data)
    ipc = env.start_daemon(ws=False)
    ud = tmp_path / "chrome-profile"
    install_manifest([ud / "NativeMessagingHosts", Path(env.env["HOME"]) / ".config/google-chrome/NativeMessagingHosts"])
    chrome_env = dict(env.env)
    chrome_env["LDM_NO_AUTOSTART"] = "1"
    cdp = Cdp(chrome_env, ud)
    try:
        assert cdp.call("Extensions.loadUnpacked", {"path": str(EXT_DIR)})["id"] == EXT_ID
        dl = tmp_path / "chrome-downloads"
        dl.mkdir()
        cdp.call("Browser.setDownloadBehavior", {"behavior": "allow", "downloadPath": str(dl), "eventsEnabled": True})
        # 1) the service worker connects through the native host: wait for the Hello in ldmd's log
        log = env.tmp / "ldmd.log"
        t0 = time.time()
        while 'browser="chrome"' not in log.read_text():
            assert time.time() - t0 < 20, log.read_text()[-2000:]
            time.sleep(0.2)
        # 2) open a page with a <video>: the extension reports a media candidate
        page = cdp.call("Target.createTarget", {"url": env.base + "/page.html"})["targetId"]
        t0 = time.time()
        cands = []
        while not cands:
            assert time.time() - t0 < 20, "no media candidate reported"
            cands = [c for c in ipc.call("CandidateList", {})["payload"]["candidates"] if c["url"].endswith("/v.mp4")]
            time.sleep(0.2)
        assert cands[0]["kind"] == "progressive" and cands[0]["page_url"].endswith("/page.html")
        sess = cdp.call("Target.attachToTarget", {"targetId": page, "flatten": True})["sessionId"]

        def js(expr):
            return cdp.call("Runtime.evaluate", {"expression": expr, "returnByValue": True}, session=sess)["result"].get("value")

        # the floating "download this video" button is shown next to the 640×360 player
        t0 = time.time()
        while js("(document.querySelector('ldm-download-button')||{style:{}}).style.display") != "block":
            assert time.time() - t0 < 15, "floating button not shown"
            time.sleep(0.2)
        # …and positioned over the player's top-right area
        top, left = js("(s => [parseFloat(s.top), parseFloat(s.left)])(document.querySelector('ldm-download-button').style)")
        vr = js("(r => [r.top, r.right])(document.querySelector('video').getBoundingClientRect())")
        assert abs(top - (vr[0] + 8)) < 2 and left < vr[1]

        def click(sel, modifiers=0):
            x, y = js(f"(r => [r.left + 10, r.top + 10])(document.querySelector('{sel}').getBoundingClientRect())")
            for t in ("mousePressed", "mouseReleased"):
                cdp.call("Input.dispatchMouseEvent", {"type": t, "x": x, "y": y, "button": "left", "clickCount": 1, "modifiers": modifiers}, session=sess)

        # modifier keys: Alt+click leaves the download to the browser (bypass_modifier = alt)
        click("#alt", modifiers=1)
        t0 = time.time()
        while not (dl / "alt.zip").exists():
            assert time.time() - t0 < 20, "Alt+click download did not stay in the browser"
            time.sleep(0.2)
        assert not any("alt.zip" in j["url"] for j in ipc.jobs())
        # Insert held: LDM takes a type it would normally ignore (force_modifier = insert)
        cdp.call("Input.dispatchKeyEvent", {"type": "rawKeyDown", "key": "Insert", "code": "Insert", "windowsVirtualKeyCode": 45}, session=sess)
        js("document.getElementById('ins').setAttribute('download', 'notes.txt')")
        click("#ins")
        cdp.call("Input.dispatchKeyEvent", {"type": "keyUp", "key": "Insert", "code": "Insert", "windowsVirtualKeyCode": 45}, session=sess)
        t0 = time.time()
        forced = None
        while forced is None:
            assert time.time() - t0 < 20, "Insert+click was not forced into LDM"
            forced = next((j for j in ipc.jobs() if j["url"].endswith("/notes.txt")), None)
            time.sleep(0.2)
        ipc.wait_state(forced["id"], "Completed", 20)
        # 3) a browser download of an attachment is intercepted and handed to LDM
        cdp.call("Target.createTarget", {"url": env.base + "/big.zip?cd=big.zip&rate=1000000"})
        t0 = time.time()
        job = None
        while job is None:
            assert time.time() - t0 < 30, "download was not handed over"
            job = next((j for j in ipc.jobs() if "big.zip" in j["url"]), None)
            time.sleep(0.2)
        assert job["source"] == "extension"
        j = ipc.wait_state(job["id"], "Completed", 30)
        assert sha256(j["final_path"]) == sha256(env.www / "big.zip")
        time.sleep(1.0)
        assert not (dl / "big.zip").exists(), "the browser copy must be canceled"
        # 4) optional page-world hook (off by default): enable it from an extension page, then a
        #    playlist fetched by page script is reported by the hook
        js(hook_probe_js("off"))
        time.sleep(2.0)
        assert not hook_probe_seen(ipc, "off"), "hook must be off by default"
        ext = cdp.call("Target.createTarget", {"url": f"chrome-extension://{EXT_ID}/popup.html"})["targetId"]
        es = cdp.call("Target.attachToTarget", {"targetId": ext, "flatten": True})["sessionId"]
        time.sleep(0.5)
        cdp.call(
            "Runtime.evaluate",
            {"expression": "chrome.runtime.sendMessage({type:'SetSettings',settings:{pageHooks:true}})", "awaitPromise": True},
            session=es,
        )
        (env.www / "hook.m3u8").write_text("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n")
        (env.www / "hook.html").write_text("<!doctype html><title>hook</title><script>setTimeout(() => fetch('hook.m3u8'), 300)</script>")
        hp = cdp.call("Target.createTarget", {"url": env.base + "/hook.html"})["targetId"]
        hs = cdp.call("Target.attachToTarget", {"targetId": hp, "flatten": True})["sessionId"]
        t0 = time.time()
        while not hook_probe_seen(ipc, "on"):
            assert time.time() - t0 < 15, "page hook not injected after enabling"
            cdp.call("Runtime.evaluate", {"expression": hook_probe_js("on"), "returnByValue": True}, session=hs)
            time.sleep(0.5)
        t0 = time.time()
        while not any(c["url"].endswith("/hook.m3u8") for c in ipc.call("CandidateList", {})["payload"]["candidates"]):
            assert time.time() - t0 < 15, "playlist from page script not reported"
            time.sleep(0.3)
    finally:
        cdp.close()


@pytest.mark.skipif(not shutil.which("ffmpeg"), reason="ffmpeg missing")
def test_chrome_button_goes_to_main_player_not_outstream_ad(env, tmp_path):
    """Placement scoring: an outstream ad video in a cross-origin ad iframe, larger than the main
    player, must not get the in-page button; exactly one button is shown, on the main player."""
    path = make_placement_fixture(env.www, env.base)
    env.start_daemon(ws=False)
    ud = tmp_path / "chrome-profile"
    install_manifest([ud / "NativeMessagingHosts"])
    chrome_env = dict(env.env)
    chrome_env["LDM_NO_AUTOSTART"] = "1"
    cdp = Cdp(chrome_env, ud)
    try:
        assert cdp.call("Extensions.loadUnpacked", {"path": str(EXT_DIR)})["id"] == EXT_ID
        log = env.tmp / "ldmd.log"
        t0 = time.time()
        while 'browser="chrome"' not in log.read_text():
            assert time.time() - t0 < 20, log.read_text()[-2000:]
            time.sleep(0.2)
        page = cdp.call("Target.createTarget", {"url": "about:blank"})["targetId"]
        sess = cdp.call("Target.attachToTarget", {"targetId": page, "flatten": True})["sessionId"]
        cdp.call("Emulation.setDeviceMetricsOverride", {"width": 1280, "height": 900, "deviceScaleFactor": 1, "mobile": False}, session=sess)
        cdp.call("Page.navigate", {"url": env.base + path}, session=sess)

        def js(expr, s=sess):
            return cdp.call("Runtime.evaluate", {"expression": expr, "returnByValue": True}, session=s)["result"].get("value")

        t0 = time.time()
        while js("(document.querySelector('ldm-download-button')||{style:{}}).style.display") != "block":
            assert time.time() - t0 < 20, "button not shown on the main player"
            time.sleep(0.25)
        # the ad really is bigger and playing (the fixture is what it claims to be)
        frame = None
        t0 = time.time()
        while frame is None:
            assert time.time() - t0 < 10, "ad iframe target not found"
            frame = next((t for t in cdp.call("Target.getTargets")["targetInfos"] if t["type"] == "iframe" and "unit7.html" in t["url"]), None)
            time.sleep(0.2)
        fs = cdp.call("Target.attachToTarget", {"targetId": frame["targetId"], "flatten": True})["sessionId"]
        assert js("(v => !v.paused && v.getBoundingClientRect().width > 700)(document.getElementById('adv'))", fs)
        # stays on the main player for a while (re-evaluation must not flip it to the ad frame)
        for _ in range(12):
            top, left = js("(s => [parseFloat(s.top), parseFloat(s.left)])(document.querySelector('ldm-download-button').style)")
            vr = js("(r => [r.top, r.right])(document.getElementById('main').getBoundingClientRect())")
            assert js("document.querySelector('ldm-download-button').style.display") == "block"
            assert abs(top - (vr[0] + 8)) < 2 and vr[1] - 200 <= left < vr[1], (top, left, vr)
            assert js("[...document.querySelectorAll('ldm-download-button')].filter(b => b.style.display === 'block').length", fs) == 0, (
                "button shown in the ad frame"
            )
            time.sleep(0.5)

        # ranking: the clip played in the ad frame is greyed (ad context) and listed after the main video
        # the shadow root is closed: reach it through the DOM domain (pierce) and click with real input
        def shadow_rows():
            doc = cdp.call("DOM.getDocument", {"depth": -1, "pierce": True}, session=sess)["root"]
            stack = [doc]
            while stack:
                n = stack.pop()
                if n.get("localName") == "ldm-download-button" and n.get("shadowRoots"):
                    obj = cdp.call("DOM.resolveNode", {"backendNodeId": n["shadowRoots"][0]["backendNodeId"]}, session=sess)["object"]
                    fn = "function(){return [...this.querySelectorAll('.menu .item')].map(r => [r.className, r.textContent, r.title || ''])}"
                    return cdp.call("Runtime.callFunctionOn", {"objectId": obj["objectId"], "functionDeclaration": fn, "returnByValue": True}, session=sess)[
                        "result"
                    ]["value"]
                stack += n.get("children", []) + n.get("shadowRoots", [])
            return []

        bx, by = js("(s => [parseFloat(s.left) + 20, parseFloat(s.top) + 12])(document.querySelector('ldm-download-button').style)")
        for t in ("mousePressed", "mouseReleased"):
            cdp.call("Input.dispatchMouseEvent", {"type": t, "x": bx, "y": by, "button": "left", "clickCount": 1}, session=sess)
        t0 = time.time()
        while True:
            rows = shadow_rows()
            ad = [r for r in rows if "clip7" in r[1]]
            if ad and "adish" in ad[0][0] and any("main.mp4" in r[1] for r in rows):
                break
            assert time.time() - t0 < 20, f"menu rows: {rows}"
            time.sleep(0.5)
        main_i = next(i for i, r in enumerate(rows) if "main.mp4" in r[1])
        ad_i = next(i for i, r in enumerate(rows) if "clip7" in r[1])
        assert main_i < ad_i and "adish" not in rows[main_i][0], rows
        assert "ad frame" in ad[0][2] or "广告框架" in ad[0][2], ad
    finally:
        cdp.close()


MSE_MAIN_TEXT = ("a playing MSE player is fed from it", "正在播放的 MSE 播放器使用此流")


@pytest.mark.skipif(not shutil.which("ffmpeg"), reason="ffmpeg missing")
def test_chrome_mse_hook_codecs_and_ranking(env, tmp_path):
    """MSE observation hook (opt-in page hooks): a self-written MSE player plays a local DASH stream;
    the menu row of that manifest shows its codecs and is ranked first (mse-main), ahead of a more
    recently loaded, paused second MSE stream of the same length; the hooks keep the original names and descriptors."""
    path = make_mse_fixture(env.www)
    env.start_daemon(ws=False)
    ud = tmp_path / "chrome-profile"
    install_manifest([ud / "NativeMessagingHosts"])
    chrome_env = dict(env.env)
    chrome_env["LDM_NO_AUTOSTART"] = "1"
    cdp = Cdp(chrome_env, ud)
    try:
        assert cdp.call("Extensions.loadUnpacked", {"path": str(EXT_DIR)})["id"] == EXT_ID
        log = env.tmp / "ldmd.log"
        t0 = time.time()
        while 'browser="chrome"' not in log.read_text():
            assert time.time() - t0 < 20, log.read_text()[-2000:]
            time.sleep(0.2)
        ext = cdp.call("Target.createTarget", {"url": f"chrome-extension://{EXT_ID}/popup.html"})["targetId"]
        es = cdp.call("Target.attachToTarget", {"targetId": ext, "flatten": True})["sessionId"]
        time.sleep(0.5)
        r = cdp.call(
            "Runtime.evaluate",
            {"expression": "chrome.runtime.sendMessage({type:'SetSettings',settings:{pageHooks:true}})", "awaitPromise": True, "returnByValue": True},
            session=es,
        )["result"]["value"]
        assert r["settings"]["pageHooks"] is True, r
        time.sleep(0.5)
        page = cdp.call("Target.createTarget", {"url": "about:blank"})["targetId"]
        sess = cdp.call("Target.attachToTarget", {"targetId": page, "flatten": True})["sessionId"]
        cdp.call("Emulation.setDeviceMetricsOverride", {"width": 1280, "height": 900, "deviceScaleFactor": 1, "mobile": False}, session=sess)
        cdp.call("Page.navigate", {"url": env.base + path}, session=sess)

        def js(expr):
            return cdp.call("Runtime.evaluate", {"expression": expr, "returnByValue": True}, session=sess)["result"].get("value")

        t0 = time.time()
        while not (js(MSE_HOOK_JS) or {}).get("playing"):
            assert time.time() - t0 < 20, f"MSE fixture not playing: {js(MSE_HOOK_JS)}"
            time.sleep(0.3)
        nat = js(MSE_HOOK_JS)
        assert all(nat.values()), f"hooks must keep names / descriptors and the player must play: {nat}"

        def shadow_rows():
            doc = cdp.call("DOM.getDocument", {"depth": -1, "pierce": True}, session=sess)["root"]
            stack = [doc]
            while stack:
                n = stack.pop()
                if n.get("localName") == "ldm-download-button" and n.get("shadowRoots"):
                    obj = cdp.call("DOM.resolveNode", {"backendNodeId": n["shadowRoots"][0]["backendNodeId"]}, session=sess)["object"]
                    fn = "function(){return [...this.querySelectorAll('.menu .item')].map(r => [r.className, r.textContent, r.title || ''])}"
                    return cdp.call("Runtime.callFunctionOn", {"objectId": obj["objectId"], "functionDeclaration": fn, "returnByValue": True}, session=sess)[
                        "result"
                    ]["value"]
                stack += n.get("children", []) + n.get("shadowRoots", [])
            return []

        t0 = time.time()
        while js("(document.querySelector('ldm-download-button')||{style:{}}).style.display") != "block":
            assert time.time() - t0 < 20, "button not shown"
            time.sleep(0.25)
        # the button sits on the playing MSE player (src match through the MSE link)
        top, left = js("(s => [parseFloat(s.top), parseFloat(s.left)])(document.querySelector('ldm-download-button').style)")
        vr = js("(r => [r.top, r.right])(document.getElementById('main').getBoundingClientRect())")
        assert abs(top - (vr[0] + 8)) < 2 and vr[1] - 200 <= left < vr[1], (top, left, vr)
        time.sleep(8)  # the promo stream is loaded and paused, signals sent
        bx, by = js("(s => [parseFloat(s.left) + 20, parseFloat(s.top) + 12])(document.querySelector('ldm-download-button').style)")
        for t in ("mousePressed", "mouseReleased"):
            cdp.call("Input.dispatchMouseEvent", {"type": t, "x": bx, "y": by, "button": "left", "clickCount": 1}, session=sess)
        t0 = time.time()
        while True:
            rows = shadow_rows()
            m = [r for r in rows if "main.mpd" in r[1]]
            p = [r for r in rows if "second.mpd" in r[1]]
            if m and p and "H.264+AAC" in m[0][1] and any(x in m[0][2] for x in MSE_MAIN_TEXT):
                break
            assert time.time() - t0 < 25, f"menu rows: {rows}"
            time.sleep(0.5)
        print("menu:", rows)
        assert "[DASH]" in m[0][1] and "codecs: avc1." in m[0][2] and "mp4a.40.2" in m[0][2], m
        assert rows.index(m[0]) < rows.index(p[0]), f"playing MSE stream first: {rows}"
        assert not any(x in p[0][2] for x in MSE_MAIN_TEXT), p
    finally:
        cdp.close()


ALT_TAGS = ("Alt. quality", "其他码率")


def chrome_with_page_hooks(env, tmp_path) -> Cdp:
    """Chrome with the extension loaded, connected to the daemon, page hooks switched on."""
    env.start_daemon(ws=False)
    ud = tmp_path / "chrome-profile"
    install_manifest([ud / "NativeMessagingHosts"])
    chrome_env = dict(env.env)
    chrome_env["LDM_NO_AUTOSTART"] = "1"
    cdp = Cdp(chrome_env, ud)
    assert cdp.call("Extensions.loadUnpacked", {"path": str(EXT_DIR)})["id"] == EXT_ID
    log = env.tmp / "ldmd.log"
    t0 = time.time()
    while 'browser="chrome"' not in log.read_text():
        assert time.time() - t0 < 20, log.read_text()[-2000:]
        time.sleep(0.2)
    ext = cdp.call("Target.createTarget", {"url": f"chrome-extension://{EXT_ID}/popup.html"})["targetId"]
    es = cdp.call("Target.attachToTarget", {"targetId": ext, "flatten": True})["sessionId"]
    time.sleep(0.5)
    r = cdp.call(
        "Runtime.evaluate",
        {"expression": "chrome.runtime.sendMessage({type:'SetSettings',settings:{pageHooks:true}})", "awaitPromise": True, "returnByValue": True},
        session=es,
    )["result"]["value"]
    assert r["settings"]["pageHooks"] is True, r
    time.sleep(0.5)
    return cdp


def chrome_open(cdp: Cdp, url: str):
    """New tab at `url`; returns (session, js)."""
    page = cdp.call("Target.createTarget", {"url": "about:blank"})["targetId"]
    sess = cdp.call("Target.attachToTarget", {"targetId": page, "flatten": True})["sessionId"]
    cdp.call("Emulation.setDeviceMetricsOverride", {"width": 1280, "height": 900, "deviceScaleFactor": 1, "mobile": False}, session=sess)
    cdp.call("Page.navigate", {"url": url}, session=sess)

    def js(expr):
        return cdp.call("Runtime.evaluate", {"expression": expr, "returnByValue": True}, session=sess)["result"].get("value")

    return sess, js


def chrome_click_button(cdp: Cdp, sess: str, js):
    t0 = time.time()
    while js("(document.querySelector('ldm-download-button')||{style:{}}).style.display") != "block":
        assert time.time() - t0 < 20, "button not shown"
        time.sleep(0.25)
    bx, by = js("(s => [parseFloat(s.left) + 20, parseFloat(s.top) + 12])(document.querySelector('ldm-download-button').style)")
    for t in ("mousePressed", "mouseReleased"):
        cdp.call("Input.dispatchMouseEvent", {"type": t, "x": bx, "y": by, "button": "left", "clickCount": 1}, session=sess)


def chrome_menu_rows(cdp: Cdp, sess: str) -> list:
    """The open menu's rows [class, text, title]."""
    doc = cdp.call("DOM.getDocument", {"depth": -1, "pierce": True}, session=sess)["root"]
    stack = [doc]
    while stack:
        n = stack.pop()
        if n.get("localName") == "ldm-download-button" and n.get("shadowRoots"):
            obj = cdp.call("DOM.resolveNode", {"backendNodeId": n["shadowRoots"][0]["backendNodeId"]}, session=sess)["object"]
            fn = "function(){return [...this.querySelectorAll('.menu .item')].map(r => [r.className, r.textContent, r.title || ''])}"
            return cdp.call("Runtime.callFunctionOn", {"objectId": obj["objectId"], "functionDeclaration": fn, "returnByValue": True}, session=sess)["result"][
                "value"
            ]
        stack += n.get("children", []) + n.get("shadowRoots", [])
    return []


def check_bitrate_menu(rows: list, main: str, alt: str):
    m = [r for r in rows if main in r[1]]
    a = [r for r in rows if alt in r[1]]
    assert m and a, rows
    assert rows.index(a[0]) == rows.index(m[0]) + 1, f"the alternate sits right under the main entry: {rows}"
    assert "alt" in a[0][0].split() and "adish" not in a[0][0], a
    assert any(t in a[0][1] for t in ALT_TAGS), a
    assert not any(t in m[0][1] for t in ALT_TAGS) and "alt" not in m[0][0].split(), m
    assert rows.index(m[0]) == 0, f"the video is the first entry: {rows}"


@pytest.mark.skipif(not shutil.which("ffmpeg"), reason="ffmpeg missing")
def test_chrome_bitrate_swap_groups_alternates(env, tmp_path):
    """One MSE <video> fed from low.mp4, then from high.mp4 (and, in a second tab, the other way round):
    one logical video — the file the player ended up using is the main entry, the other bitrate an
    indented "alternate quality" row right under it, not a tied duplicate."""
    path = make_bitrate_fixture(env.www)
    cdp = chrome_with_page_hooks(env, tmp_path)
    try:
        for query, main, alt in (("", "high.mp4", "low.mp4"), ("?order=hl", "low.mp4", "high.mp4")):
            sess, js = chrome_open(cdp, env.base + path + query)
            t0 = time.time()
            while not js("window.ldmSwapped === true && !document.getElementById('main').paused"):
                assert time.time() - t0 < 30, "fixture did not swap"
                time.sleep(0.3)
            time.sleep(3)  # signals (MSE report, players) reach the background
            chrome_click_button(cdp, sess, js)
            t0 = time.time()
            while True:
                rows = chrome_menu_rows(cdp, sess)
                try:
                    check_bitrate_menu(rows, main, alt)
                    break
                except AssertionError:
                    assert time.time() - t0 < 20, f"menu rows ({query or 'low→high'}): {rows}"
                    time.sleep(1)
            print("menu", query or "lh", rows)
    finally:
        cdp.close()
