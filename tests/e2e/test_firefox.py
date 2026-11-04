"""Real-browser E2E in Firefox: headless Firefox (geckodriver + selenium) loads the built
MV3 Firefox extension as a temporary add-on, talks to ldm-native-host → ldmd through native
messaging, reports page media (webRequest + optional MAIN-world hook), shows the in-page
Shadow-DOM button and menu (ranked, ad-like entries greyed), downloads an HLS stream with a
separate audio rendition (muxed, named after the page title), takes a context-menu link
download and hands a browser download over to LDM."""

from __future__ import annotations

import json
import os
import re
import shutil
import subprocess
import time
from pathlib import Path

import pytest

from conftest import (
    BIN,
    MSE_HOOK_JS,
    ROOT,
    hook_probe_js,
    hook_probe_seen,
    make_bitrate_fixture,
    make_mse_fixture,
    make_placement_fixture,
    rand_bytes,
    sha256,
)

EXT_DIR = ROOT / "extension" / "dist" / "firefox"
ADDON_ID = "ldm@wzk4work.github.io"
EXT_UUID = "0b1d2c3e-4f50-4a6b-8c7d-9e0f1a2b3c4d"
_OPT = Path("/opt/ff")
FIREFOX = os.environ.get("LDM_FIREFOX") or shutil.which("firefox") or (str(_OPT / "firefox/firefox") if (_OPT / "firefox/firefox").exists() else None)
GECKODRIVER = os.environ.get("LDM_GECKODRIVER") or shutil.which("geckodriver") or (str(_OPT / "geckodriver") if (_OPT / "geckodriver").exists() else None)

try:
    from selenium import webdriver
    from selenium.webdriver.common.action_chains import ActionChains
    from selenium.webdriver.common.by import By
    from selenium.webdriver.firefox.options import Options
    from selenium.webdriver.firefox.service import Service
except ImportError:  # pragma: no cover
    webdriver = None

pytestmark = pytest.mark.skipif(
    not (FIREFOX and GECKODRIVER and webdriver and shutil.which("ffmpeg") and (EXT_DIR / "manifest.json").exists()),
    reason="firefox, geckodriver, selenium, ffmpeg or built extension missing",
)


def install_manifest(home: Path) -> Path:
    m = {
        "name": "io.github.wzk4work.ldm.browser_host",
        "description": "LDM native host (test)",
        "path": str(BIN / "ldm-native-host"),
        "type": "stdio",
        "allowed_extensions": [ADDON_ID],
    }
    d = home / ".mozilla" / "native-messaging-hosts"
    d.mkdir(parents=True, exist_ok=True)
    (d / "io.github.wzk4work.ldm.browser_host.json").write_text(json.dumps(m))
    return d


def start_firefox(env: dict, home: Path, dl: Path, log: Path, headless: bool = True):
    o = Options()
    o.binary_location = FIREFOX
    if headless:
        o.add_argument("-headless")
    prefs = {
        "extensions.webextensions.uuids": json.dumps({ADDON_ID: EXT_UUID}),
        "devtools.console.stdout.chrome": True,
        "devtools.console.stdout.content": True,
        "browser.download.folderList": 2,
        "browser.download.dir": str(dl),
        "browser.download.useDownloadDir": True,
        "browser.download.always_ask_before_handling_new_types": False,
        "browser.download.alwaysOpenPanel": False,
        "media.autoplay.default": 0,
        "intl.locale.requested": "en-US",
        "datareporting.policy.dataSubmissionEnabled": False,
        "toolkit.telemetry.reportingpolicy.firstRun": False,
        "browser.shell.checkDefaultBrowser": False,
    }
    for k, v in prefs.items():
        o.set_preference(k, v)
    fenv = dict(env)
    fenv["HOME"] = str(home)
    fenv["LDM_NO_AUTOSTART"] = "1"
    # chrome-context scripting is used only to activate the add-on's context-menu item
    svc = Service(executable_path=GECKODRIVER, log_output=str(log), env=fenv, service_args=["--allow-system-access"])
    drv = webdriver.Firefox(options=o, service=svc)
    drv.install_addon(str(EXT_DIR), temporary=True)
    return drv


def wait(cond, timeout, msg):
    t0 = time.time()
    while True:
        try:
            v = cond()
        except Exception:  # element re-rendered meanwhile
            v = None
        if v:
            return v
        assert time.time() - t0 < timeout, msg() if callable(msg) else msg
        time.sleep(0.25)


def ext_message(drv, msg: dict):
    """chrome.runtime.sendMessage from an extension page (the popup), as the popup itself does."""
    cur = drv.current_window_handle
    drv.switch_to.new_window("tab")
    drv.get(f"moz-extension://{EXT_UUID}/popup.html")
    # let the popup finish its own first refresh before closing it again
    wait(lambda: "ok" in drv.find_element(By.ID, "status").get_attribute("class"), 10, "popup did not show 'connected'")
    r = drv.execute_async_script(
        "const done = arguments[arguments.length-1];"
        "chrome.runtime.sendMessage(arguments[0]).then(r => done(JSON.stringify(r ?? null)), e => done('ERR ' + e));",
        msg,
    )
    drv.close()
    drv.switch_to.window(cur)
    return r


def make_media(www: Path):
    ff = ["ffmpeg", "-v", "error", "-y"]
    subprocess.run(
        ff
        + [
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x180:rate=25",
            "-f",
            "lavfi",
            "-i",
            "sine=frequency=440:sample_rate=44100",
            "-t",
            "6",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
            "-g",
            "50",
            "-c:a",
            "aac",
            "-shortest",
            str(www / "main.mp4"),
        ],
        check=True,
    )
    (www / "ads").mkdir()
    subprocess.run(
        ff + ["-f", "lavfi", "-i", "testsrc2=size=160x90:rate=25", "-t", "2", "-c:v", "libx264", "-pix_fmt", "yuv420p", str(www / "ads" / "preroll-ad.mp4")],
        check=True,
    )
    st = www / "stream"
    st.mkdir()
    subprocess.run(
        ff
        + [
            "-i",
            str(www / "main.mp4"),
            "-map",
            "0:v",
            "-map",
            "0:a",
            "-c",
            "copy",
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_playlist_type",
            "vod",
            "-master_pl_name",
            "master.m3u8",
            "-var_stream_map",
            "v:0,agroup:aud a:0,agroup:aud,default:yes",
            str(st / "s_%v" / "index.m3u8"),
        ],
        check=True,
    )


def ffprobe_streams(p: str) -> list[str]:
    out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "stream=codec_type", "-of", "csv=p=0", p], capture_output=True, text=True).stdout
    return sorted(out.split())


def test_firefox_extension_end_to_end(env, tmp_path):
    make_media(env.www)
    (env.www / "page.html").write_text(
        "<!doctype html><title>LDM Firefox page</title>"
        '<video id="main" src="main.mp4" muted autoplay loop width="640" height="360"></video>'
        '<video src="ads/preroll-ad.mp4" muted autoplay width="160" height="90"></video>'
        '<p><a id="z" href="notes.zip">notes.zip</a> <a id="big" href="big.zip?cd=big.zip&amp;rate=1000000">big.zip</a></p>'
        "<script>setTimeout(() => { fetch('stream/master.m3u8'); fetch('hooked/f.m3u8?status=404');"
        " const x = new XMLHttpRequest(); x.open('GET', 'hooked/x.m3u8?status=404'); x.send(); }, 300)</script>"
    )
    (env.www / "notes.zip").write_bytes(rand_bytes(300_000, 81))
    (env.www / "big.zip").write_bytes(rand_bytes(2_500_000, 82))
    ipc = env.start_daemon(ws=False)
    home = Path(env.env["HOME"])
    install_manifest(home)
    dl = tmp_path / "ff-downloads"
    dl.mkdir()
    flog = tmp_path / "firefox.log"
    drv = start_firefox(env.env, home, dl, flog)

    def tail():
        return f"\n--- firefox log ---\n{flog.read_text()[-4000:]}\n--- ldmd log ---\n{(env.tmp / 'ldmd.log').read_text()[-2000:]}"

    try:
        # 1) the background script connects through the native host (Hello with browser="firefox")
        wait(lambda: 'browser="firefox"' in (env.tmp / "ldmd.log").read_text(), 30, lambda: "no Hello from Firefox" + tail())
        # optional MAIN-world page hook: off by default; enable it like the popup does
        r = json.loads(ext_message(drv, {"type": "SetSettings", "settings": {"pageHooks": True}}))
        assert r["ok"] and r["settings"]["pageHooks"] is True, r
        drv.get(env.base + "/page.html")

        def probed():
            return hook_probe_seen(ipc, "ff") or (drv.execute_script("return " + hook_probe_js("ff")) and None)

        assert wait(probed, 15, "MAIN-world hook not injected" + tail())

        def cands():
            return ipc.call("CandidateList", {})["payload"]["candidates"]

        # 2) webRequest sniffing: progressive main video, ad-like file, HLS master; the two 404
        #    playlists are ignored by webRequest (status >= 400), so only the MAIN-world
        #    fetch / XMLHttpRequest hook can report them
        wait(
            lambda: {"main.mp4", "preroll-ad.mp4", "master.m3u8", "f.m3u8?status=404", "x.m3u8?status=404"} <= {c["url"].rsplit("/", 1)[-1] for c in cands()},
            25,
            lambda: f"candidates: {[c['url'] for c in cands()]}" + tail(),
        )

        # 3) in-page Shadow-DOM button next to the 640×360 player
        def shown_host():
            return next((h for h in drv.find_elements(By.CSS_SELECTOR, "ldm-download-button") if h.value_of_css_property("display") == "block"), None)

        host = wait(shown_host, 20, "button not shown" + tail())
        sr = host.shadow_root
        sr.find_element(By.CSS_SELECTOR, ".btn").click()

        def rows():
            out = []
            for r in sr.find_elements(By.CSS_SELECTOR, ".menu .item"):
                out.append((r.get_attribute("class"), r.text, r.get_attribute("title")))
            return out

        menu = wait(lambda: rows() if len(rows()) >= 5 else None, 10, lambda: f"menu rows: {rows()}")
        texts = [t for _, t, _ in menu]
        print("menu:", [(c, t) for c, t, _ in menu])
        assert any("[HLS]" in t and "adish" not in c for c, t, _ in menu), menu
        assert "adish" not in menu[0][0] and "main.mp4" in texts[0] + texts[1], menu  # main video near the top
        ad = [m for m in menu if "preroll-ad" in m[1] or "adish" in m[0]]
        assert ad and "adish" in ad[0][0], f"ad-like entry greyed: {menu}"
        # the trailing "Download with yt-dlp" action is shown whenever yt-dlp is usable or installable
        # (managed install on first use); it is not a candidate, so leave it out of the ordering check
        grey = ["adish" in c for c, t, _ in menu if "yt-dlp" not in t]
        assert grey == sorted(grey), f"greyed entries are listed last, nothing dropped: {menu}"
        # 4) HLS entry → quality list (probed by ldmd) → pick → muxed, named after the page title
        before = {j["id"] for j in ipc.jobs()}
        next(e for e in sr.find_elements(By.CSS_SELECTOR, ".menu .item") if "[HLS]" in e.text).click()

        def video_qualities():
            return [e for e in sr.find_elements(By.CSS_SELECTOR, ".menu .item.q") if re.search(r"\d+p", e.text)]

        q = wait(video_qualities, 20, lambda: f"no qualities: {rows()}" + tail())
        print("qualities:", [e.text for e in sr.find_elements(By.CSS_SELECTOR, ".menu .item.q")])
        q[0].click()
        job = wait(lambda: next((j for j in ipc.jobs() if j["id"] not in before), None), 20, "no job from the menu" + tail())
        j = ipc.wait_state(job["id"], "Completed", 60)
        assert j["kind"] in ("hls", "av"), j
        assert os.path.basename(j["final_path"]) == "LDM Firefox page.mp4", j["final_path"]
        assert ffprobe_streams(j["final_path"]) == ["audio", "video"], "audio + video muxed"
        os.remove(j["final_path"])
        # 5) context menu → "Download link with LDM" on a plain file link
        before = {j["id"] for j in ipc.jobs()}
        ActionChains(drv).context_click(drv.find_element(By.ID, "z")).perform()
        with drv.context(drv.CONTEXT_CHROME):
            label = drv.execute_script(
                """
                const m = document.getElementById('contentAreaContextMenu');
                const it = [...m.querySelectorAll('menuitem')].find(i => i.getAttribute('label') === 'Download link with LDM');
                if (!it) return [...m.querySelectorAll('menuitem')].map(i => i.getAttribute('label')).filter(Boolean).join(' | ');
                it.doCommand(); m.hidePopup(); return 'ok';
                """
            )
        assert label == "ok", f"context menu items: {label}"
        job = wait(
            lambda: next((j for j in ipc.jobs() if j["id"] not in before and j["url"].endswith("/notes.zip")), None),
            20,
            "context-menu link not handed to LDM" + tail(),
        )
        j = ipc.wait_state(job["id"], "Completed", 30)
        assert sha256(j["final_path"]) == sha256(env.www / "notes.zip")
        # 6) a browser download (attachment) is intercepted, handed to LDM and the browser copy dropped
        before = {j["id"] for j in ipc.jobs()}
        drv.find_element(By.ID, "big").click()
        job = wait(lambda: next((j for j in ipc.jobs() if j["id"] not in before and "big.zip" in j["url"]), None), 30, "download not handed over" + tail())
        assert job["source"] == "extension"
        j = ipc.wait_state(job["id"], "Completed", 30)
        assert sha256(j["final_path"]) == sha256(env.www / "big.zip")
        time.sleep(1.5)
        assert not [p.name for p in dl.iterdir() if p.name.startswith("big")], f"browser copy left: {list(dl.iterdir())}"
        # 7) no errors from the add-on's own scripts (background, content, popup) in the browser console
        errs = [ln for ln in flog.read_text().splitlines() if (EXT_UUID in ln or "ldm@wzk4work.github.io" in ln) and "rror" in ln]
        assert not errs, "\n".join(errs)
    finally:
        drv.quit()


def test_firefox_button_goes_to_main_player_not_outstream_ad(env, tmp_path):
    """Placement scoring in real Firefox: a larger outstream ad video in a cross-origin iframe inside an
    ad container never gets the button; exactly one button, on the main player; the clip is greyed."""
    path = make_placement_fixture(env.www, env.base)
    env.start_daemon(ws=False)
    home = Path(env.env["HOME"])
    install_manifest(home)
    dl = tmp_path / "ff-downloads"
    dl.mkdir()
    flog = tmp_path / "firefox.log"
    drv = start_firefox(env.env, home, dl, flog)

    def tail():
        return f"\n--- firefox log ---\n{flog.read_text()[-3000:]}"

    try:
        wait(lambda: 'browser="firefox"' in (env.tmp / "ldmd.log").read_text(), 30, lambda: "no Hello from Firefox" + tail())
        drv.set_window_size(1300, 1000)
        drv.get(env.base + path)

        def shown():
            return [h for h in drv.find_elements(By.CSS_SELECTOR, "ldm-download-button") if h.value_of_css_property("display") == "block"]

        wait(lambda: shown() or None, 20, lambda: "button not shown on the main player" + tail())
        frame = drv.find_element(By.ID, "adframe")
        for _ in range(10):
            hosts = shown()
            assert len(hosts) == 1, hosts
            pos = drv.execute_script(
                "const s = arguments[0].style, r = document.getElementById('main').getBoundingClientRect();"
                "return [parseFloat(s.top), parseFloat(s.left), r.top, r.right];",
                hosts[0],
            )
            assert abs(pos[0] - (pos[2] + 8)) < 2 and pos[3] - 200 <= pos[1] < pos[3], pos
            drv.switch_to.frame(frame)
            try:
                adv = drv.execute_script("const v = document.getElementById('adv'); return [v.paused, v.getBoundingClientRect().width];")
                assert adv[0] is False and adv[1] > 700, adv  # the ad really is bigger and playing
                assert not shown(), "button shown in the ad frame"
            finally:
                drv.switch_to.default_content()
            time.sleep(0.5)

        sr = shown()[0].shadow_root
        sr.find_element(By.CSS_SELECTOR, ".btn").click()

        def rows():
            return [(r.get_attribute("class"), r.text, r.get_attribute("title") or "") for r in sr.find_elements(By.CSS_SELECTOR, ".menu .item")]

        def ready():
            rs = rows()
            ad = [r for r in rs if "clip7" in r[1]]
            return rs if ad and "adish" in ad[0][0] and any("main.mp4" in r[1] for r in rs) else None

        menu = wait(ready, 20, lambda: f"menu rows: {rows()}")
        main_i = next(i for i, r in enumerate(menu) if "main.mp4" in r[1])
        ad_i = next(i for i, r in enumerate(menu) if "clip7" in r[1])
        assert main_i < ad_i and "adish" not in menu[main_i][0], menu
        assert "ad frame" in menu[ad_i][2] or "广告框架" in menu[ad_i][2], menu
    finally:
        drv.quit()


MSE_MAIN_TEXT = ("a playing MSE player is fed from it", "正在播放的 MSE 播放器使用此流")


@pytest.mark.skipif(not shutil.which("ffmpeg"), reason="ffmpeg missing")
def test_firefox_mse_hook_codecs_and_ranking(env, tmp_path):
    """MSE observation hook in real Firefox (MAIN-world script): codecs in the menu label, the playing
    MSE stream ranked first, the hooks keep the original names and descriptors."""
    path = make_mse_fixture(env.www)
    env.start_daemon(ws=False)
    home = Path(env.env["HOME"])
    install_manifest(home)
    dl = tmp_path / "ff-downloads"
    dl.mkdir()
    flog = tmp_path / "firefox.log"
    drv = start_firefox(env.env, home, dl, flog)

    def tail():
        return f"\n--- firefox log ---\n{flog.read_text()[-3000:]}"

    try:
        wait(lambda: 'browser="firefox"' in (env.tmp / "ldmd.log").read_text(), 30, lambda: "no Hello from Firefox" + tail())
        r = json.loads(ext_message(drv, {"type": "SetSettings", "settings": {"pageHooks": True}}))
        assert r["ok"] and r["settings"]["pageHooks"] is True, r
        drv.set_window_size(1300, 1000)
        drv.get(env.base + path)

        def nat():
            return drv.execute_script("return " + MSE_HOOK_JS)

        wait(lambda: nat().get("playing") or None, 20, lambda: f"MSE fixture not playing: {nat()}" + tail())
        n = nat()
        assert all(n.values()), f"hooks must keep names / descriptors and the player must play: {n}"

        def shown():
            return [h for h in drv.find_elements(By.CSS_SELECTOR, "ldm-download-button") if h.value_of_css_property("display") == "block"]

        wait(lambda: shown() or None, 20, lambda: "button not shown" + tail())
        pos = drv.execute_script(
            "const s = arguments[0].style, r = document.getElementById('main').getBoundingClientRect();"
            "return [parseFloat(s.top), parseFloat(s.left), r.top, r.right];",
            shown()[0],
        )
        assert abs(pos[0] - (pos[2] + 8)) < 2 and pos[3] - 200 <= pos[1] < pos[3], pos
        time.sleep(8)
        sr = shown()[0].shadow_root
        sr.find_element(By.CSS_SELECTOR, ".btn").click()

        def rows():
            return [(r.get_attribute("class"), r.text, r.get_attribute("title") or "") for r in sr.find_elements(By.CSS_SELECTOR, ".menu .item")]

        def ready():
            rs = rows()
            m = [r for r in rs if "main.mpd" in r[1]]
            p = [r for r in rs if "second.mpd" in r[1]]
            return rs if m and p and "H.264+AAC" in m[0][1] and any(x in m[0][2] for x in MSE_MAIN_TEXT) else None

        menu = wait(ready, 25, lambda: f"menu rows: {rows()}")
        print("menu:", menu)
        m = next(r for r in menu if "main.mpd" in r[1])
        p = next(r for r in menu if "second.mpd" in r[1])
        assert "[DASH]" in m[1] and "codecs: avc1." in m[2] and "mp4a.40.2" in m[2], m
        assert menu.index(m) < menu.index(p), f"playing MSE stream first: {menu}"
        assert not any(x in p[2] for x in MSE_MAIN_TEXT), p
    finally:
        drv.quit()


ALT_TAGS = ("Alt. quality", "其他码率")


@pytest.mark.skipif(not shutil.which("ffmpeg"), reason="ffmpeg missing")
def test_firefox_bitrate_swap_groups_alternates(env, tmp_path):
    """Real Firefox: one MSE <video> fed from low.mp4, then high.mp4 (and the other way round in a
    second tab): the file the player ended up using is the main entry, the other bitrate an indented
    "alternate quality" row right under it."""
    path = make_bitrate_fixture(env.www)
    env.start_daemon(ws=False)
    home = Path(env.env["HOME"])
    install_manifest(home)
    dl = tmp_path / "ff-downloads"
    dl.mkdir()
    flog = tmp_path / "firefox.log"
    drv = start_firefox(env.env, home, dl, flog)
    try:
        wait(lambda: 'browser="firefox"' in (env.tmp / "ldmd.log").read_text(), 30, lambda: "no Hello from Firefox")
        r = json.loads(ext_message(drv, {"type": "SetSettings", "settings": {"pageHooks": True}}))
        assert r["ok"] and r["settings"]["pageHooks"] is True, r
        drv.set_window_size(1300, 1000)
        for query, main, alt in (("", "high.mp4", "low.mp4"), ("?order=hl", "low.mp4", "high.mp4")):
            if query:
                drv.switch_to.new_window("tab")
            drv.get(env.base + path + query)
            wait(lambda: drv.execute_script("return window.ldmSwapped === true") or None, 30, lambda: "no swap")
            time.sleep(3)

            def shown():
                return [h for h in drv.find_elements(By.CSS_SELECTOR, "ldm-download-button") if h.value_of_css_property("display") == "block"]

            wait(lambda: shown() or None, 20, lambda: "button not shown")
            sr = shown()[0].shadow_root
            sr.find_element(By.CSS_SELECTOR, ".btn").click()

            def rows(sr=sr):
                return [(r.get_attribute("class"), r.text, r.get_attribute("title") or "") for r in sr.find_elements(By.CSS_SELECTOR, ".menu .item")]

            def ready(main=main, alt=alt):
                rs = rows()
                m = [r for r in rs if main in r[1]]
                a = [r for r in rs if alt in r[1]]
                ok = m and a and rs.index(a[0]) == rs.index(m[0]) + 1 and rs.index(m[0]) == 0 and "alt" in a[0][0].split()
                return rs if ok else None

            menu = wait(ready, 25, lambda q=query: f"menu rows ({q or 'low→high'}): {rows()}")
            print("menu", query or "lh", menu)
            a = next(r for r in menu if alt in r[1])
            m = next(r for r in menu if main in r[1])
            assert "adish" not in a[0] and any(t in a[1] for t in ALT_TAGS), a
            assert not any(t in m[1] for t in ALT_TAGS), m
    finally:
        drv.quit()
