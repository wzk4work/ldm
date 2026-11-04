"""E2E: LDM-managed yt-dlp / deno against a local fake GitHub release server (no network).

The release layout mirrors GitHub's (`/api/repos/<repo>/releases/latest`,
`/<repo>/releases/download/<tag>/<asset>`); `ytdlp.release_base_url` points LDM at it
(honoured for loopback http:// URLs only)."""

from __future__ import annotations

import hashlib
import io
import json
import os
import platform
import stat
import time
import zipfile
from pathlib import Path

import pytest

FAKE = Path(__file__).resolve().parents[2] / "crates/ldm-core/tests/fixtures/fake-yt-dlp"
YT = "https://www.youtube.com/watch?v=aqz-KE-bpKQ"
ARCH = platform.machine()
YT_ASSET = {"x86_64": "yt-dlp_linux", "aarch64": "yt-dlp_linux_aarch64"}.get(ARCH, "yt-dlp")
DENO_ASSET = f"deno-{ARCH}-unknown-linux-gnu.zip"

pytestmark = pytest.mark.skipif(ARCH not in ("x86_64", "aarch64"), reason="release assets exist for x86_64/aarch64 only")


def ytdlp_wrapper(version: str, mode: str = "ok") -> bytes:
    return f'#!/bin/sh\nFAKE_YTDLP_VERSION={version} FAKE_YTDLP_MODE={mode} exec python3 {FAKE} "$@"\n'.encode()


def publish_ytdlp(www: Path, version: str, mode: str = "ok", latest: bool = True, sums: str | None = "good"):
    d = www / "yt-dlp/yt-dlp/releases/download" / version
    d.mkdir(parents=True, exist_ok=True)
    data = ytdlp_wrapper(version, mode)
    (d / YT_ASSET).write_bytes(data)
    h = hashlib.sha256(data).hexdigest()
    if sums == "bad":
        h = hashlib.sha256(b"something else").hexdigest()
    if sums is not None:
        (d / "SHA2-256SUMS").write_text(f"{h}  {YT_ASSET}\n{'0' * 64}  yt-dlp.exe\n")
    if latest:
        api = www / "api/repos/yt-dlp/yt-dlp/releases/latest"
        api.parent.mkdir(parents=True, exist_ok=True)
        api.write_text(json.dumps({"tag_name": version, "name": f"yt-dlp {version}"}))
        api.with_name("latest.etag").write_text(f'W/"{version}"')


def publish_deno(www: Path, version: str):
    d = www / "denoland/deno/releases/download" / f"v{version}"
    d.mkdir(parents=True, exist_ok=True)
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", zipfile.ZIP_DEFLATED) as z:
        info = zipfile.ZipInfo("deno")
        info.external_attr = (0o755 | stat.S_IFREG) << 16
        z.writestr(info, f"#!/bin/sh\necho 'deno {version} (stable, release, {ARCH}-unknown-linux-gnu)'\n")
    data = buf.getvalue()
    (d / DENO_ASSET).write_bytes(data)
    (d / f"{DENO_ASSET}.sha256sum").write_text(f"{hashlib.sha256(data).hexdigest()}  {DENO_ASSET}\n")
    api = www / "api/repos/denoland/deno/releases/latest"
    api.parent.mkdir(parents=True, exist_ok=True)
    api.write_text(json.dumps({"tag_name": f"v{version}"}))
    api.with_name("latest.etag").write_text(f'W/"v{version}"')


def configure(env, **ytdlp):
    # keep any real yt-dlp / deno on the developer's PATH out of the lookup
    env.env["PATH"] = "/usr/bin:/bin"
    env.env["FAKE_YTDLP_LOG"] = str(env.tmp / "argv.jsonl")
    env.write_config({"ytdlp": {"release_base_url": env.base, "auto_update": "off", **ytdlp}})


def tools_root(env) -> Path:
    return env.home / ".local/share/ldm/tools"


def requests(env, needle: str) -> list[dict]:
    return [r for r in env.server_stats()["requests"] if needle in r["path"]]


def test_cli_install_update_check_rollback_remove(env):
    publish_ytdlp(env.www, "2026.09.01", latest=False)
    publish_ytdlp(env.www, "2026.10.21")
    publish_deno(env.www, "2.9.8")
    configure(env)
    root = tools_root(env)
    # no daemon: the CLI installs directly
    r = env.cli("tools", "install", "yt-dlp", "--version", "2026.09.01")
    assert "yt-dlp 2026.09.01" in r.stdout and "SHA-256（SHA2-256SUMS）" in r.stdout, r.stdout
    assert os.access(root / "yt-dlp/2026.09.01/yt-dlp", os.X_OK)
    assert os.readlink(root / "yt-dlp/current") == "2026.09.01"  # relative symlink, swapped atomically
    st = json.loads(env.cli("--json", "tools", "status").stdout)
    assert st["yt-dlp"]["source"] == "managed" and st["yt-dlp"]["managed_current"] == "2026.09.01", st
    assert st["deno"]["path"] is None
    # check: first a full API answer (ETag stored), then a conditional request (304)
    r = env.cli("tools", "check", "yt-dlp")
    assert "当前 2026.09.01，最新 2026.10.21" in r.stdout and "可更新" in r.stdout, r.stdout
    env.cli("tools", "check", "yt-dlp")
    api = requests(env, "/api/repos/yt-dlp/yt-dlp/releases/latest")
    assert api[0]["if_none_match"] is None and api[-1]["if_none_match"], api
    r = env.cli("tools", "update")
    assert "已更新 2026.09.01 → yt-dlp 2026.10.21" in r.stdout, r.stdout
    # the daemon is used when it runs; deno via the default "both" install
    ipc = env.start_daemon(ws=False)
    r = env.cli("tools", "install", "deno")
    assert "deno 2.9.8" in r.stdout and f"{DENO_ASSET}.sha256sum" in r.stdout, r.stdout
    r = env.cli("doctor")
    assert "2026.10.21（来源：LDM 托管" in r.stdout and "deno:" in r.stdout and "LDM 托管" in r.stdout.split("deno:")[1], r.stdout
    st = ipc.call("ToolsStatus")["payload"]
    assert st["yt-dlp"]["managed_previous"] == "2026.09.01" and st["deno"]["version"] == "2.9.8", st
    r = env.cli("tools", "rollback", "yt-dlp")
    assert "2026.10.21 → 2026.09.01" in r.stdout, r.stdout
    assert ipc.call("YtdlpStatus")["payload"]["source"] == "managed"
    # re-installing a version still on disk only moves the pointer
    r = env.cli("--json", "tools", "install", "yt-dlp", "--version", "2026.10.21")
    assert json.loads(r.stdout)["yt-dlp"]["reused"] is True
    env.cli("tools", "remove", "deno")
    assert not (root / "deno").exists() and (root / "yt-dlp").exists()
    # nothing half-written is left behind
    leftovers = [p.name for p in root.rglob("*") if p.name.startswith(".stage") or p.suffix == ".tmp"]
    assert not leftovers, leftovers


def test_checksum_mismatch_or_missing_is_refused(env):
    publish_ytdlp(env.www, "2026.10.21", sums="bad")
    configure(env)
    r = env.cli("tools", "install", "yt-dlp", check=False)
    assert r.returncode != 0 and "checksum mismatch" in r.stdout, (r.stdout, r.stderr)
    assert not (tools_root(env) / "yt-dlp/2026.10.21").exists()
    publish_ytdlp(env.www, "2026.10.22", sums=None)
    r = env.cli("tools", "install", "yt-dlp", check=False)
    assert r.returncode != 0 and "SHA2-256SUMS" in r.stdout + r.stderr and "unverified" in r.stdout + r.stderr, r.stdout
    assert env.cli("--json", "tools", "status").stdout.count('"managed_current": null') == 2
    # a non-loopback release_base_url is ignored (always the real GitHub, never contacted here)
    tmp = tools_root(env) / "tmp"
    assert not tmp.exists() or not any(tmp.iterdir())


def test_consent_ask_then_install_requeues_job(env):
    publish_ytdlp(env.www, "2026.10.21")
    publish_deno(env.www, "2.9.8")
    configure(env)  # auto_install = ask (default)
    ipc = env.start_daemon(ws=False)
    r = ipc.call("DownloadAdd", {"url": YT, "source": "extension"})["payload"]
    assert r["toolsNeeded"] == "yt-dlp", r
    j = ipc.wait_state(r["jobId"], "Failed", 20)
    assert j["error_code"] == "E_TOOL_MISSING", j
    # CLI without a terminal: no prompt, a hint instead
    r2 = env.cli("add", "--ytdlp", "https://vimeo.com/1")
    assert "ldm tools install" in r2.stderr, r2.stderr
    # the GUI/extension answers "install": jobs waiting for yt-dlp are re-queued
    res = ipc.call("ToolsAction", {"action": "install", "wait": True})["payload"]["result"]
    assert res["yt-dlp"]["version"] == "2026.10.21" and res["deno"]["version"] == "2.9.8" and res["requeued"] >= 1, res
    j = ipc.wait_state(r["jobId"], "Completed", 30)
    assert j["ytdlp"]["bin_source"] == "managed" and j["ytdlp"]["bin_version"] == "2026.10.21", j["ytdlp"]
    # YouTube with the managed deno: passed explicitly
    dl = [json.loads(x) for x in (env.tmp / "argv.jsonl").read_text().splitlines()]
    yt = [c for c in dl if YT in c and "-J" not in c]
    assert any(a.startswith("deno:") and "/tools/deno/2.9.8/deno" in a for a in yt[-1]), yt[-1]


def test_auto_install_always_and_never(env):
    publish_ytdlp(env.www, "2026.10.21")
    publish_deno(env.www, "2.9.8")
    configure(env, auto_install="never")
    ipc = env.start_daemon(ws=False)
    # (an explicit yt-dlp job: with `never` and no yt-dlp, plain routing would not pick yt-dlp)
    r = ipc.call("DownloadAdd", {"url": "https://www.youtube.invalid/watch?v=x", "kind": "ytdlp"})["payload"]
    assert "toolsNeeded" not in r
    j = ipc.wait_state(r["jobId"], "Failed", 20)
    assert "ldm tools install" in j["error"], j
    assert not tools_root(env).joinpath("yt-dlp").exists()
    env.stop()
    env.start_server()
    configure(env, auto_install="always")
    ipc = env.start_daemon(ws=False)
    jid = ipc.call("DownloadAdd", {"url": YT})["payload"]["jobId"]
    j = ipc.wait_state(jid, "Completed", 60)
    assert j["ytdlp"]["bin_source"] == "managed"


def test_outdated_extractor_updates_once_and_retries(env):
    publish_ytdlp(env.www, "2026.09.01", mode="outdated")
    configure(env, auto_update="weekly")
    env.cli("tools", "install", "yt-dlp")
    publish_ytdlp(env.www, "2026.10.21")  # the fix is released
    ipc = env.start_daemon(ws=False)
    jid = ipc.call("DownloadAdd", {"url": YT, "kind": "ytdlp"})["payload"]["jobId"]
    j = ipc.wait_state(jid, "Completed", 60)
    assert j["ytdlp"]["update_retried"] is True and j["ytdlp"]["bin_version"] == "2026.10.21", j["ytdlp"]
    assert any("2026.10.21" in h for h in j["history"]), j["history"]
    # still broken after the update → fails, no second update
    publish_ytdlp(env.www, "2026.10.22", mode="outdated")
    env.cli("tools", "update", "yt-dlp")
    publish_ytdlp(env.www, "2026.10.23", mode="outdated")
    jid = ipc.call("DownloadAdd", {"url": YT + "&x=2", "kind": "ytdlp"})["payload"]["jobId"]
    j = ipc.wait_state(jid, "Failed", 60)
    assert j["ytdlp"]["update_retried"] is True
    time.sleep(0.5)
    st = ipc.call("ToolsStatus")["payload"]["yt-dlp"]
    assert st["managed_current"] == "2026.10.23", st  # updated once for this job, then gave up
    jid2 = j["id"]
    assert sum("nsig" in h or "更新" in h or "update" in h.lower() for h in ipc.job(jid2)["history"]) >= 1


def test_user_path_beats_managed(env):
    publish_ytdlp(env.www, "2026.10.21")
    configure(env)
    env.cli("tools", "install", "yt-dlp")
    configure(env, path=str(FAKE))
    ipc = env.start_daemon(ws=False)
    st = ipc.call("YtdlpStatus")["payload"]
    assert st["source"] == "user" and st["path"] == str(FAKE), st
    jid = ipc.call("DownloadAdd", {"url": YT})["payload"]["jobId"]
    j = ipc.wait_state(jid, "Completed", 30)
    assert j["ytdlp"]["bin_source"] == "user"
