"""End-to-end tests: ldmd + WebSocket bridge (JSON envelopes), native host, CLI,
crash/resume and queue limits. Run: .venv/bin/pytest tests/e2e -v"""

from __future__ import annotations

import asyncio
import json
import os
import struct
import subprocess
import time

import pytest
import websockets
from websockets.exceptions import InvalidStatus

from conftest import BIN, Ipc, rand_bytes, sha256

EXT_ORIGIN = "chrome-extension://abcdefghijklmnopabcdefghijklmnop"
SAMPLE_UA = "Mozilla/5.0 (X11; Linux x86_64) LDM-E2E/1.0"


async def reply_to(ws, rid, timeout=10):
    """Next envelope answering request `rid`, skipping pushed events."""
    while True:
        m = json.loads(await asyncio.wait_for(ws.recv(), timeout))
        if m.get("id") == rid:
            return m


def test_ws_json_capture_mp4_and_hls(env):
    """Browser-style captures over the JSON WS bridge: a progressive mp4 is
    downloaded with the page's request context, an m3u8 becomes an HLS job and
    a segment of that playlist is absorbed (no extra job)."""
    sample = env.install_sample()
    ipc = env.start_daemon()
    page = env.base + "/sample/index.html"

    async def run():
        async with websockets.connect(f"ws://127.0.0.1:{env.ws_port}/", subprotocols=["ldm.bridge.v1"], origin=EXT_ORIGIN) as ws:
            assert ws.subprotocol == "ldm.bridge.v1"
            await ws.send(json.dumps({"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "version": "120"}}))
            assert (await reply_to(ws, 1))["type"] == "HelloAck"
            common = {"referrer": page, "pageUrl": page, "userAgent": SAMPLE_UA}
            await ws.send(json.dumps({"v": 1, "id": 2, "type": "DownloadAdd", "payload": {"url": env.base + "/sample/video.mp4", **common}}))
            r = await reply_to(ws, 2)
            assert r["type"] == "InterceptDecision" and r["payload"]["action"] == "take", r
            await ws.send(json.dumps({"v": 1, "id": 3, "type": "DownloadAdd", "payload": {"url": env.base + "/sample/hls/index.m3u8", **common}}))
            r = await reply_to(ws, 3)
            assert r["payload"]["action"] == "take", r
            await ws.send(json.dumps({"v": 1, "id": 4, "type": "DownloadAdd", "payload": {"url": env.base + "/sample/hls/seg0.ts", **common}}))
            r = await reply_to(ws, 4)
            assert r["payload"]["action"] == "pass", r

    asyncio.run(run())
    jobs = ipc.jobs()
    assert len(jobs) == 2, jobs  # mp4 + HLS; the .ts segment was absorbed
    mp4 = next(j for j in jobs if j["url"].endswith("video.mp4"))
    hls = next(j for j in jobs if j["url"].endswith("index.m3u8"))
    assert mp4["source"] == "extension" and hls["kind"] == "hls"
    assert mp4["referrer"].endswith("/sample/index.html")
    assert SAMPLE_UA in (mp4["user_agent"] or "")
    j = ipc.wait_state(mp4["id"], "Completed")
    assert sha256(j["final_path"]) == sha256(sample / "video.mp4")
    assert j["total"] == 22297
    h = ipc.wait_state(hls["id"], "Completed")
    assert os.path.getsize(h["final_path"]) > 0
    probe = subprocess.run(["ffprobe", "-v", "error", "-show_entries", "format=duration", "-of", "csv=p=0", h["final_path"]], capture_output=True, text=True)
    assert abs(float(probe.stdout.strip()) - 2.0) < 0.5
    # request context (UA/Referer) reached the server
    reqs = [r for r in env.server_stats()["requests"] if r["path"].endswith("video.mp4")]
    assert any(SAMPLE_UA in (r["ua"] or "") for r in reqs)
    assert all((r["referer"] or "").endswith("index.html") for r in reqs)


def test_ws_json_bridge_and_origin_policy(env):
    data = rand_bytes(1_500_000, 3)
    (env.www / "j.bin").write_bytes(data)
    ipc = env.start_daemon()

    async def run():
        async with websockets.connect(f"ws://127.0.0.1:{env.ws_port}/", subprotocols=["ldm.bridge.v1"], origin=EXT_ORIGIN) as ws:
            await ws.send(json.dumps({"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "version": "120"}}))
            a = json.loads(await ws.recv())
            b = json.loads(await ws.recv())
            assert a["type"] == "HelloAck" and a["payload"]["protocolVersion"] == 1 and "hls" in a["payload"]["features"]
            assert b["type"] == "RuleSetPush" and "mp4" in b["payload"]["captureExts"]
            await ws.send(json.dumps({"v": 1, "id": 2, "type": "Subscribe", "payload": {}}))
            assert json.loads(await ws.recv())["type"] == "Ack"
            await ws.send(json.dumps({"v": 1, "id": 3, "type": "DownloadAdd", "payload": {"url": env.base + "/j.bin", "referrer": "http://page/"}}))
            seen_progress = False
            jid = None
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 20))
                if m["id"] == 3:
                    assert m["type"] == "InterceptDecision" and m["payload"]["action"] == "take"
                    jid = m["payload"]["jobId"]
                elif m["type"] == "JobProgress":
                    seen_progress = True
                elif m["type"] == "JobStateChanged" and jid and m["payload"].get("id") == jid and m["payload"].get("state") == "Completed":
                    break
            await ws.send(json.dumps({"v": 1, "id": 4, "type": "ConfigSet", "payload": {"patch": {"general": {"on_complete_command": ["sh"]}}}}))
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 5))
                if m["id"] == 4:
                    assert m["type"] == "Error" and m["payload"]["code"] == "E_FORBIDDEN"
                    break
            # malformed envelope → Error, connection stays open
            await ws.send("{not json")
            while True:
                m = json.loads(await asyncio.wait_for(ws.recv(), 5))
                if m["type"] == "Error":
                    assert m["payload"]["code"] == "E_FORMAT"
                    break
            return jid, seen_progress

    jid, _ = asyncio.run(run())
    assert sha256(ipc.job(jid)["final_path"]) == sha256(env.www / "j.bin")

    async def bad_origin():
        with pytest.raises(InvalidStatus) as ei:
            async with websockets.connect(f"ws://127.0.0.1:{env.ws_port}/", origin="https://evil.example"):
                pass
        assert ei.value.response.status_code == 403
        with pytest.raises(InvalidStatus):
            async with websockets.connect(f"ws://127.0.0.1:{env.ws_port}/", subprotocols=["bogus.v9"]):
                pass

    asyncio.run(bad_origin())


def test_ws_json_without_subprotocol(env):
    """Clients that offer no sub-protocol still speak JSON envelopes."""
    env.start_daemon()

    async def run():
        async with websockets.connect(f"ws://127.0.0.1:{env.ws_port}/") as ws:
            await ws.send(json.dumps({"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "version": "120"}}))
            return (await reply_to(ws, 1))["type"]

    assert asyncio.run(run()) == "HelloAck"


def native_send(p, obj):
    b = json.dumps(obj).encode()
    p.stdin.write(struct.pack("<I", len(b)) + b)
    p.stdin.flush()


def native_recv(p):
    n = struct.unpack("<I", p.stdout.read(4))[0]
    return json.loads(p.stdout.read(n))


def native_reply(p, rid: int):
    """Next reply to request `rid`, skipping pushed events (Notify, RuleSetPush…)."""
    while True:
        m = native_recv(p)
        if m.get("id") == rid:
            return m


def test_native_host_json(env):
    data = rand_bytes(800_000, 9)
    (env.www / "n.bin").write_bytes(data)
    ipc = env.start_daemon(ws=False)
    p = subprocess.Popen(
        [str(BIN / "ldm-native-host"), EXT_ORIGIN + "/"], env=env.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
    )
    try:
        native_send(p, {"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "version": "120"}})
        assert native_recv(p)["type"] == "HelloAck"
        assert native_recv(p)["type"] == "RuleSetPush"
        native_send(p, {"v": 1, "id": 2, "type": "DownloadAdd", "payload": {"url": env.base + "/n.bin"}})
        r = native_recv(p)
        assert r["type"] == "InterceptDecision", r
        j = ipc.wait_state(r["payload"]["jobId"], "Completed")
        assert sha256(j["final_path"]) == sha256(env.www / "n.bin")
        assert j["source"] == "extension"
        # privileged requests are refused for browser clients, even if they claim another identity
        native_send(p, {"v": 1, "id": 3, "type": "ConfigGet", "payload": {"_client": "ipc"}})
        r = native_reply(p, 3)
        assert r["type"] == "Error" and r["payload"]["code"] == "E_FORBIDDEN", r
        native_send(p, {"v": 1, "id": 4, "type": "JobRemove", "payload": {"completed": True}})
        assert native_reply(p, 4)["payload"]["code"] == "E_FORBIDDEN"
    finally:
        p.stdin.close()
        p.wait(timeout=10)
    # web origins are refused
    r = subprocess.run([str(BIN / "ldm-native-host"), "https://evil.example/"], env=env.env, input=b"", capture_output=True, timeout=10)
    assert r.returncode != 0


def test_native_host_autostarts_daemon(env):
    env.env["LDM_LDMD"] = str(BIN / "ldmd")
    p = subprocess.Popen(
        [str(BIN / "ldm-native-host"), EXT_ORIGIN + "/"], env=env.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
    )
    try:
        native_send(p, {"v": 1, "id": 1, "type": "Ping", "payload": {}})
        assert native_recv(p)["type"] == "Pong"
    finally:
        p.stdin.close()
        p.wait(timeout=10)
        env.cli("daemon", "stop")


def test_kill9_during_download_then_resume(env):
    size = 6_000_000
    data = rand_bytes(size, 42)
    (env.www / "big.bin").write_bytes(data)
    ipc = env.start_daemon(ws=False)
    # 8 connections × 120 kB/s ≈ 1 MB/s → ~6 s total
    jid = ipc.call("DownloadAdd", {"url": env.base + "/big.bin?rate=120000"})["payload"]["jobId"]
    t0 = time.time()
    while ipc.job(jid)["got"] < size * 0.5:
        assert time.time() - t0 < 30
        time.sleep(0.1)
    got_before = ipc.job(jid)["got"]
    ipc.close()
    env.kill_daemon()  # SIGKILL: no graceful persistence
    served_before = env.server_stats()["counters"].get("bytes:/big.bin", 0)
    ipc = env.start_daemon(ws=False)
    j = ipc.wait_state(jid, "Completed", timeout=60)
    assert sha256(j["final_path"]) == sha256(env.www / "big.bin")
    served_total = env.server_stats()["counters"]["bytes:/big.bin"]
    # resumed: the server sent far less than two full copies
    assert served_total < size * 1.35, (served_before, served_total, got_before)
    starts = [int(r["range"][6:].split("-")[0]) for r in env.server_stats()["requests"] if r["path"] == "/big.bin" and r["range"] and r["range"] != "bytes=0-0"]
    assert len(starts) > 8  # new ranges after restart


def test_queue_limit_and_cli(env):
    for i in range(5):
        (env.www / f"q{i}.bin").write_bytes(rand_bytes(400_000, 100 + i))
    env.write_config({"queue": {"global_max_active": 2}})
    ipc = env.start_daemon(ws=False)
    out = env.cli("add", *[f"{env.base}/q{i}.bin?rate=150000" for i in range(5)])
    ids = [ln.split()[0] for ln in out.stdout.splitlines()]
    assert len(ids) == 5
    peak = 0
    t0 = time.time()
    while True:
        st = ipc.call("Status")["payload"]
        peak = max(peak, st["active"])
        if st["completed"] == 5:
            break
        assert time.time() - t0 < 60
        time.sleep(0.05)
    assert peak == 2
    for i, jid in enumerate(ids):
        assert sha256(ipc.job(jid)["final_path"]) == sha256(env.www / f"q{i}.bin")
    # CLI round trip
    js = json.loads(env.cli("--json", "list").stdout)
    assert len(js) == 5 and all(j["state"] == "Completed" for j in js)
    csv = env.cli("export", "--format", "csv").stdout
    assert csv.startswith("id,url,state") and len(csv.splitlines()) == 6
    env.cli("rm", ids[0])
    assert len(json.loads(env.cli("--json", "list").stdout)) == 4
    env.cli("config", "set", "engine.max_connections_default", "4")
    assert env.cli("config", "get", "engine.max_connections_default").stdout.strip() == "4"
    r = env.cli("info", "nonexistent", check=False)
    assert r.returncode == 1
    env.cli("queue", "set", "night", "--concurrent", "1", "--window", "01:00-07:00")
    assert "night" in env.cli("queue", "list").stdout
    # daemon not running → exit code 3
    env.kill_daemon()
    assert env.cli("list", check=False).returncode == 3


def test_cli_wait_pause_resume(env):
    (env.www / "w.bin").write_bytes(rand_bytes(3_000_000, 5))
    ipc = env.start_daemon(ws=False)
    jid = ipc.call("DownloadAdd", {"url": env.base + "/w.bin?rate=100000"})["payload"]["jobId"]
    ipc.wait_state(jid, "Running", 10)
    time.sleep(0.5)
    env.cli("pause", jid[:6])
    j = ipc.wait_state(jid, "Paused", 10)
    assert 0 < j["got"] < 3_000_000
    env.cli("resume", jid)
    j = ipc.wait_state(jid, "Completed", 60)
    assert sha256(j["final_path"]) == sha256(env.www / "w.bin")


def test_cli_metrics_and_diag(env, tmp_path):
    ipc = env.start_daemon(ws=False)
    ipc.call("ConfigSet", {"patch": {"sites": {"logins": [{"host": "h.example", "user": "alice", "password": "s3cret"}]}}})
    (env.www / "d.bin").write_bytes(rand_bytes(10_000, 7))
    jid = ipc.call("DownloadAdd", {"url": env.base + "/d.bin?token=abc123"})["payload"]["jobId"]
    ipc.wait_state(jid, "Completed", 30)
    m = env.cli("metrics").stdout
    assert "ldm_up{" in m and 'ldm_jobs{state="completed"} 1' in m
    out = tmp_path / "diag.json"
    env.cli("diag", "-o", str(out))
    text = out.read_text()
    bundle = json.loads(text)
    assert bundle["daemon_running"] is True and bundle["jobs"][0]["host"] == "127.0.0.1"
    for leak in ("s3cret", "alice", "abc123"):
        assert leak not in text, leak


def test_cli_standalone_hls(env, tmp_path):
    if subprocess.run(["which", "ffmpeg"], capture_output=True).returncode != 0:
        pytest.skip("ffmpeg missing")
    d = env.www / "gen"
    d.mkdir()
    subprocess.run(
        [
            "ffmpeg",
            "-y",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=4",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "25",
            "-f",
            "hls",
            "-hls_time",
            "1",
            "-hls_playlist_type",
            "vod",
            str(d / "p.m3u8"),
        ],
        check=True,
    )
    out = tmp_path / "o.mp4"
    r = env.cli("hls", env.base + "/gen/p.m3u8", "-o", str(out))
    assert r.stdout.strip() == str(out) and out.stat().st_size > 0
    r = env.cli("get", env.base + "/gen/p0.ts", "-o", str(tmp_path / "x.ts"), "-c", "2")
    assert sha256(tmp_path / "x.ts") == sha256(d / "p0.ts")


def test_capture_confirmation_with_gui_subscriber(env):
    """With a GUI attached, a browser capture waits (Paused + CaptureConfirm) until the dialog answers."""
    (env.www / "cc.bin").write_bytes(rand_bytes(300_000, 21))
    ipc = env.start_daemon(ws=False)
    gui = Ipc(env.socket)
    assert gui.call("Subscribe", {"enabled": True, "role": "gui"})["type"] == "Ack"
    p = subprocess.Popen(
        [str(BIN / "ldm-native-host"), EXT_ORIGIN + "/"], env=env.env, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL
    )
    try:
        native_send(p, {"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "version": "120"}})
        native_recv(p), native_recv(p)
        native_send(p, {"v": 1, "id": 2, "type": "DownloadAdd", "payload": {"url": env.base + "/cc.bin", "description": "页面描述"}})
        r = native_recv(p)
        while r["type"] != "InterceptDecision":
            r = native_recv(p)
        jid = r["payload"]["jobId"]
    finally:
        p.stdin.close()
        p.wait(timeout=10)
    ev = None
    t0 = time.time()
    while ev is None and time.time() - t0 < 10:
        m = gui._readline()
        if m["type"] == "CaptureConfirm":
            ev = m["payload"]
    assert ev and ev["id"] == jid and ev["description"] == "页面描述"
    assert ipc.job(jid)["state"] == "Paused"
    # the dialog: rename + start
    gui.call("JobSet", {"id": jid, "filename": "confirmed.bin", "awaiting_confirm": False})
    gui.call("JobResume", {"id": jid})
    j = ipc.wait_state(jid, "Completed")
    assert j["final_path"].endswith("confirmed.bin")
    gui.close()
