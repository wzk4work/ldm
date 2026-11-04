"""install.sh / uninstall.sh into a throw-away HOME, and the .deb layout."""

from __future__ import annotations

import json
import os
import shutil
import subprocess

import pytest

from conftest import BIN, ROOT


def sh(args, home, **kw):
    env = {"HOME": str(home), "PATH": os.environ["PATH"], "XDG_RUNTIME_DIR": str(home / "run")}
    return subprocess.run(args, env=env, capture_output=True, text=True, timeout=120, **kw)


def test_user_install_and_uninstall(tmp_path):
    home = tmp_path / "home"
    (home / "run").mkdir(parents=True)
    r = sh([str(ROOT / "packaging/install.sh"), "--bin-dir", str(BIN), "--no-systemd"], home)
    assert r.returncode == 0, r.stderr
    loc = home / ".local"
    for f in [
        "bin/ldmd",
        "bin/ldm",
        "bin/ldm-gui",
        "lib/ldm/ldm-native-host",
        "lib/ldm/ldmd",
        "lib/ldm/rules/example-hls-player.json",
        "lib/ldm/gui/ldm_gui/app.py",
    ]:
        assert (loc / f).exists(), f
    ext_id = (ROOT / "extension/EXTENSION_ID").read_text().strip()
    m = json.loads((home / ".config/google-chrome/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    assert m["name"] == "io.github.wzk4work.ldm.browser_host" and m["type"] == "stdio"
    assert m["path"] == str(loc / "lib/ldm/ldm-native-host") and os.path.isabs(m["path"])
    assert m["allowed_origins"] == [f"chrome-extension://{ext_id}/"]
    assert (home / ".config/chromium/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").exists()
    ff = json.loads((home / ".mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json").read_text())
    assert ff["allowed_extensions"] == ["ldm@wzk4work.github.io"]
    unit = (home / ".config/systemd/user/ldmd.service").read_text()
    assert f"ExecStart={loc}/bin/ldmd" in unit
    desk = (home / ".local/share/applications/ldm.desktop").read_text()
    assert f"Exec={loc}/bin/ldm-gui" in desk
    if shutil.which("desktop-file-validate"):
        assert sh(["desktop-file-validate", str(home / ".local/share/applications/ldm.desktop")], home).returncode == 0
    # installed native host talks to the installed (auto-started) daemon, and the bundled rule is pushed
    hello = json.dumps({"v": 1, "id": 1, "type": "Hello", "payload": {"browser": "chrome", "extVersion": "0"}}).encode()
    p = subprocess.Popen(
        [str(m["path"]), f"chrome-extension://{ext_id}/"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        env={"HOME": str(home), "PATH": os.environ["PATH"], "XDG_RUNTIME_DIR": str(home / "run"), "LDM_NO_NOTIFY": "1"},
    )
    p.stdin.write(len(hello).to_bytes(4, "little") + hello)
    p.stdin.flush()
    msgs = []
    try:
        for _ in range(2):
            n = int.from_bytes(p.stdout.read(4), "little")
            msgs.append(json.loads(p.stdout.read(n)))
    finally:
        p.stdin.close()
        p.wait(timeout=10)
    push = [x for x in msgs if x["type"] == "RuleSetPush"]
    assert push and any(a.get("id") == "example-hls-player" for a in push[0]["payload"]["siteAdapters"]), msgs
    assert sh([str(loc / "bin/ldm"), "daemon", "stop"], home).returncode == 0
    r = sh([str(ROOT / "packaging/uninstall.sh")], home)
    assert r.returncode == 0, r.stderr
    assert not (loc / "bin/ldmd").exists() and not (loc / "lib/ldm").exists()
    assert not (home / ".config/google-chrome/NativeMessagingHosts/io.github.wzk4work.ldm.browser_host.json").exists()
    assert (home / ".local/share/ldm/downloads.db").exists(), "user data must survive uninstall"


@pytest.mark.skipif(not shutil.which("dpkg-deb"), reason="dpkg-deb missing")
def test_build_deb(tmp_path):
    r = subprocess.run([str(ROOT / "packaging/build-deb.sh"), str(BIN)], capture_output=True, text=True, timeout=300)
    assert r.returncode == 0, r.stderr
    deb = r.stdout.strip().splitlines()[-1]
    listing = subprocess.run(["dpkg-deb", "-c", deb], capture_output=True, text=True).stdout
    for f in [
        "./usr/bin/ldmd",
        "./usr/lib/ldm/ldm-native-host",
        "./etc/opt/chrome/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json",
        "./usr/lib/mozilla/native-messaging-hosts/io.github.wzk4work.ldm.browser_host.json",
        "./usr/lib/systemd/user/ldmd.service",
        "./usr/share/applications/ldm.desktop",
    ]:
        assert f in listing, f
    os.unlink(deb)
