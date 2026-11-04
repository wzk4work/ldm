"""GUI smoke against a real ldmd: offscreen platform and Xvfb (xvfb-run)."""

from __future__ import annotations

import json
import shutil
import subprocess
import sys

import pytest

from conftest import IDS, ROOT, rand_bytes


def run_gui(env, extra_env, prefix=(), shot=None):
    e = dict(env.env)
    e.setdefault("LDM_LANG", "zh")
    e.update(extra_env)
    e["PYTHONPATH"] = str(ROOT / "gui")
    cmd = [*prefix, sys.executable, "-m", "ldm_gui", "--smoke", "3", "--no-autostart"]
    if shot:
        cmd += ["--screenshot", str(shot)]
    r = subprocess.run(cmd, env=e, capture_output=True, text=True, timeout=90)
    assert r.returncode == 0, r.stderr
    return json.loads(r.stdout.strip().splitlines()[-1])


def test_gui_offscreen_lists_jobs(env, tmp_path):
    (env.www / "g.bin").write_bytes(rand_bytes(200_000, 1))
    ipc = env.start_daemon(ws=False)
    jid = ipc.call("DownloadAdd", {"url": env.base + "/g.bin"})["payload"]["jobId"]
    ipc.wait_state(jid, "Completed")
    out = run_gui(env, {"QT_QPA_PLATFORM": "offscreen"}, shot=tmp_path / "gui.png")
    assert out == {**out, "connected": True, "jobs": 1, "platform": "offscreen"}
    assert (tmp_path / "gui.png").stat().st_size > 1000


@pytest.mark.skipif(not shutil.which("xvfb-run"), reason="xvfb-run missing")
def test_gui_under_xvfb(env, tmp_path):
    env.start_daemon(ws=False)
    out = run_gui(env, {"QT_QPA_PLATFORM": "xcb"}, prefix=("xvfb-run", "-a", "-s", "-screen 0 1280x800x24"), shot=tmp_path / "x.png")
    assert out["connected"] and out["platform"] == "xcb"


def test_gui_english_ui(env, tmp_path):
    env.start_daemon(ws=False)
    out = run_gui(env, {"QT_QPA_PLATFORM": "offscreen", "LDM_LANG": "en"}, shot=tmp_path / "en.png")
    assert out["connected"] and out["status"].startswith(("Connected to ldmd", "Active 0")), out


@pytest.mark.skipif(not shutil.which("dbus-run-session") or not shutil.which("dbus-send"), reason="dbus tools missing")
def test_dbus_interface(env, tmp_path):
    (env.www / "db.bin").write_bytes(rand_bytes(50_000, 3))
    ipc = env.start_daemon(ws=False)
    script = tmp_path / "dbus.sh"
    script.write_text(
        f"""set -e
N={IDS["LDM_DBUS_NAME"]} P={IDS["LDM_DBUS_PATH"]}
{sys.executable} -m ldm_gui.dbus_service > {tmp_path}/svc.log 2>&1 &
for i in $(seq 50); do grep -q serving {tmp_path}/svc.log && break; sleep 0.1; done
dbus-send --session --print-reply --dest=$N $P $N.AddUrl string:{env.base}/db.bin
dbus-send --session --print-reply --dest=$N $P $N.Status
echo TOOLS
dbus-send --session --print-reply --dest=$N $P $N.ToolsStatus
echo STOPREC
dbus-send --session --print-reply --dest=$N $P $N.StopRecording string:nosuchjob
kill %1
"""
    )
    e = dict(env.env)
    e["PYTHONPATH"] = str(ROOT / "gui")
    e["QT_QPA_PLATFORM"] = "offscreen"
    r = subprocess.run(["dbus-run-session", "--", "bash", str(script)], env=e, capture_output=True, text=True, timeout=60)
    assert r.returncode == 0, r.stdout + r.stderr + (tmp_path / "svc.log").read_text()
    jobs = [j for j in ipc.jobs() if j["url"].endswith("/db.bin")]
    assert jobs and jobs[0]["id"] in r.stdout, r.stdout
    assert '"version"' in r.stdout
    tools = r.stdout.split("TOOLS", 1)[1].split("STOPREC", 1)[0]
    assert '"yt-dlp"' in tools and '"deno"' in tools, tools
    assert "boolean false" in r.stdout.split("STOPREC", 1)[1], "StopRecording on a job that is not recording"
    ipc.wait_state(jobs[0]["id"], "Completed", 30)
