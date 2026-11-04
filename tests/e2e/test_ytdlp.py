"""E2E: ldmd + CLI with the fake yt-dlp (crates/ldm-core/tests/fixtures/fake-yt-dlp); no network."""

from __future__ import annotations

import json
import os
from pathlib import Path

FAKE = Path(__file__).resolve().parents[2] / "crates/ldm-core/tests/fixtures/fake-yt-dlp"
YT = "https://www.youtube.com/watch?v=aqz-KE-bpKQ"


def setup(env, **ytdlp):
    env.env["FAKE_YTDLP_LOG"] = str(env.tmp / "argv.jsonl")
    env.write_config({"ytdlp": {"path": str(FAKE), **ytdlp}})
    return env.start_daemon(ws=False)


def calls(env) -> list[list[str]]:
    p = env.tmp / "argv.jsonl"
    return [json.loads(x) for x in p.read_text().splitlines()] if p.exists() else []


def test_youtube_via_ytdlp_cli_doctor_probe_and_status(env):
    ipc = setup(env)
    r = env.cli("doctor")
    assert "[ OK ] yt-dlp:" in r.stdout and "2026.10.10" in r.stdout, r.stdout
    st = ipc.call("YtdlpStatus")["payload"]
    assert st["usable"] and st["version"] == "2026.10.10" and not st["outdated"]
    v = ipc.call("MediaProbe", {"url": YT})["payload"]
    assert v["source"] == "ytdlp" and v["qualities"][1]["label"] == "1080p60 · avc1", v
    # plain `ldm add` on a YouTube URL is routed to yt-dlp; -w waits with progress
    r = env.cli("add", "-w", YT)
    jid = r.stdout.split()[0]
    j = ipc.wait_state(jid, "Completed", 30)
    assert j["kind"] == "ytdlp" and j["via"] == "yt-dlp"
    assert os.path.basename(j["final_path"]) == "Test clip: Big Buck Bunny (trailer) | 100% free.mp4"
    assert os.path.getsize(j["final_path"]) == 400000
    # --ytdlp forces it for any site; quality via --height
    r = env.cli("add", "--ytdlp", "--height", "720", "-w", "https://vimeo.com/76979871")
    j = ipc.wait_state(r.stdout.split()[0], "Completed", 30)
    assert os.path.basename(j["final_path"]) == "Fallback video.mp4"
    dl = [c for c in calls(env) if "-J" not in c and "--version" not in c]
    assert "bv*[height<=720][dynamic_range=SDR]" in " ".join(dl[-1])
    assert not any("--cookies-from-browser" in c for c in calls(env)), "cookies are opt-in"


def test_fallback_after_403_and_playlist(env):
    (env.www / "v.mp4").write_bytes(b"\0" * 4096)
    ipc = setup(env)
    r = ipc.call(
        "DownloadAdd",
        {"url": env.base + "/v.mp4?status=403", "title_hint": "Some page video", "page_url": "https://video.example.com/w/1", "source": "extension"},
    )
    jid = r["payload"]["jobId"]
    # extension source: first a cookie refresh is requested; nothing new comes back → fallback
    ipc.wait_state(jid, "Failed", 20)
    ipc.call("CookiesQueryResult", {"jobId": jid, "cookies": None})
    j = ipc.wait_state(jid, "Completed", 30)
    assert j["via"] == "yt-dlp" and j["fallbackReason"] == "HTTP 403" and j["ytdlp"]["fallback_from"] == "http"
    assert j["ytdlp"]["url"] == "https://video.example.com/w/1"
    assert len(j["history"]) == 2 and "via yt-dlp" in j["history"][0]
    # which yt-dlp ran (version + source: user path / LDM-managed / system PATH)
    assert "yt-dlp 2026.10.10 (用户指定)" in j["history"][1] and j["ytdlp"]["bin_source"] == "user"
    # a CLI-added media job falls back right away
    r = env.cli("add", env.base + "/v.mp4?status=410")
    j = ipc.wait_state(r.stdout.split()[0], "Completed", 30)
    assert j["fallbackReason"] == "HTTP 410"
    # playlist → one job per item
    r = env.cli("add", "--playlist", "--paused", "https://www.youtube.com/playlist?list=PL1")
    assert r.stdout.count("(playlist)") == 3, r.stdout


def test_fallback_disabled_and_missing_binary(env):
    (env.www / "v.mp4").write_bytes(b"\0" * 4096)
    ipc = setup(env, auto_fallback=False)
    r = env.cli("add", env.base + "/v.mp4?status=403")
    j = ipc.wait_state(r.stdout.split()[0], "Failed", 20)
    assert j["kind"] == "http" and not calls(env)
    env.stop()
    env.write_config({"ytdlp": {"path": "/nonexistent/yt-dlp"}})
    env.start_server()
    ipc = env.start_daemon(ws=False)
    r = env.cli("doctor")
    assert "[WARN] yt-dlp: 未找到" in r.stdout, r.stdout
    assert ipc.call("YtdlpStatus")["payload"]["usable"] is False


def _alive(pid: int) -> bool:
    try:
        with open(f"/proc/{pid}/stat") as f:
            return f.read().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False


def test_remove_live_job_kills_ytdlp_children_and_deletes_parts(env):
    """A live stream: yt-dlp hands off to an ffmpeg child that inherits its pipes and never
    ends. Removing the job must stop the whole process tree and delete the .part files."""
    import time

    pidfile = env.tmp / "child.pid"
    env.env["FAKE_YTDLP_MODE"] = "live"
    env.env["FAKE_YTDLP_CHILD_PID"] = str(pidfile)
    ipc = setup(env)
    r = env.cli("add", "--ytdlp", "https://www.youtube.com/watch?v=live1")
    jid = r.stdout.split()[0]
    t0 = time.time()
    while not pidfile.exists() or not _parts(env.tmp):
        assert time.time() - t0 < 20, "fake live download did not start"
        time.sleep(0.1)
    child = int(pidfile.read_text())
    assert _alive(child)
    try:
        ipc.call("JobRemove", {"id": jid, "deleteFiles": True})
        t0 = time.time()
        while _alive(child) or _parts(env.tmp):
            assert time.time() - t0 < 15, f"child alive={_alive(child)} parts={_parts(env.tmp)}"
            time.sleep(0.2)
        assert not any(j["id"] == jid for j in ipc.jobs())
        # no zombie yt-dlp left under ldmd
        zombies = [p for p in os.listdir("/proc") if p.isdigit() and _is_zombie_child(int(p), env.daemon.pid)]
        assert not zombies, zombies

    finally:
        if _alive(child):
            os.kill(child, 9)


def _is_zombie_child(pid: int, ppid: int) -> bool:
    try:
        with open(f"/proc/{pid}/stat") as f:
            st = f.read().rsplit(")", 1)[1].split()
        return st[0] == "Z" and int(st[1]) == ppid
    except (FileNotFoundError, ProcessLookupError):
        return False


def _parts(root) -> list[str]:
    return [os.path.join(d, f) for d, _, fs in os.walk(root) for f in fs if f.endswith(".part")]


def test_cli_stop_recording_live_job_gives_playable_file(env):
    """`ldm stop-recording <id>`: a live yt-dlp job (fake yt-dlp running a real ffmpeg) is
    stopped gracefully (SIGINT → ffmpeg 'q' → merge) into a file that ffprobe can read."""
    import shutil
    import subprocess
    import time

    import pytest

    if not shutil.which("ffmpeg") or not shutil.which("ffprobe"):
        pytest.skip("ffmpeg missing")
    pidfile = env.tmp / "child.pid"
    env.env["FAKE_YTDLP_MODE"] = "liveff"
    env.env["FAKE_YTDLP_CHILD_PID"] = str(pidfile)
    ipc = setup(env)
    r = env.cli("add", "--ytdlp", "https://www.youtube.com/watch?v=live2")
    jid = r.stdout.split()[0]
    t0 = time.time()
    while True:
        j = next(j for j in ipc.jobs() if j["id"] == jid)
        if j["state"] == "Recording" and (j.get("recordedSecs") or 0) >= 2 and pidfile.exists():
            break
        assert time.time() - t0 < 30, j
        time.sleep(0.2)
    assert j.get("live") is True and j.get("total") is None
    child = int(pidfile.read_text())
    try:
        assert "nosuch" not in env.cli("stop-recording", jid).stdout
        j = ipc.wait_state(jid, "Completed", 40)
        path = j.get("final_path") or next(x for x in ipc.jobs() if x["id"] == jid)["final_path"]
        out = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", path], capture_output=True, text=True).stdout
        assert float(out.strip()) > 1.0, out
        os.remove(path)
        t0 = time.time()
        while _alive(child):
            assert time.time() - t0 < 10, "ffmpeg child left running"
            time.sleep(0.2)
        assert not _parts(env.tmp)
        # a second stop is an error: the job is no longer recording
        assert env.cli("stop-recording", jid, check=False).returncode != 0
    finally:
        if _alive(child):
            os.kill(child, 9)
