"""The public identifiers (packaging/identifiers.env) are used consistently everywhere.

Run `python3 tests/e2e/test_identifiers.py --list` to print every file that mentions each one.
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def ids() -> dict[str, str]:
    out = {}
    for line in (ROOT / "packaging/identifiers.env").read_text().splitlines():
        if line and not line.startswith("#") and "=" in line:
            k, v = line.split("=", 1)
            out[k.strip()] = v.strip()
    return out


# where each identifier is defined (one place per language / format)
DEFINED = {
    "LDM_APP_ID": [
        "packaging/flatpak/install-flatpak.sh",
        "packaging/flatpak/host-integration.sh",
        "packaging/flatpak/flatpak_smoke.py",
    ],
    "LDM_DBUS_NAME": ["gui/ldm_gui/dbus_service.py"],
    "LDM_DBUS_PATH": ["gui/ldm_gui/dbus_service.py"],
    "LDM_NATIVE_HOST": [
        "crates/ldm-ipc/src/lib.rs",
        "extension/src/shared/protocol.ts",
        "packaging/native-messaging/chrome.json.in",
        "packaging/native-messaging/firefox.json.in",
    ],
    "LDM_FIREFOX_ID": [
        "extension/static/manifest.firefox.json",
        "packaging/native-messaging/firefox.json.in",
    ],
}


def tracked_files() -> list[Path]:
    skip = (".venv", "node_modules", "target", "dist", ".git", "__pycache__")
    out = []
    for p in ROOT.rglob("*"):
        rel = p.relative_to(ROOT)
        if p.is_file() and not any(part in skip or part.startswith("dist") for part in rel.parts) and p.stat().st_size < 2_000_000:
            out.append(p)
    return out


def test_identifiers_defined_where_expected():
    v = ids()
    assert set(v) == set(DEFINED)
    for key, files in DEFINED.items():
        for f in files:
            assert v[key] in (ROOT / f).read_text(), f"{f} does not use {key}={v[key]}"
    manifest = ROOT / f"packaging/flatpak/{v['LDM_APP_ID']}.yml"
    text = manifest.read_text()
    assert f"app-id: {v['LDM_APP_ID']}" in text and f"--own-name={v['LDM_DBUS_NAME']}" in text
    assert v["LDM_DBUS_NAME"].startswith(v["LDM_APP_ID"] + "."), "the D-Bus name must sit under the app id"
    meta = (ROOT / f"packaging/flatpak/{v['LDM_APP_ID']}.metainfo.xml").read_text()
    assert f"<id>{v['LDM_APP_ID']}</id>" in meta


def test_native_host_name_is_valid():
    # Chrome: lowercase alphanumerics, '_' and '.', no leading/trailing or doubled dots
    n = ids()["LDM_NATIVE_HOST"]
    assert all(c.islower() or c.isdigit() or c in "._" for c in n) and not n.startswith(".") and not n.endswith(".") and ".." not in n


def test_no_stale_identifiers():
    stale = ["org" + ".ldm.", "ldm@ldm" + ".local", "/org" + "/ldm/"]  # split so this file does not match itself
    bad = []
    for p in tracked_files():
        try:
            t = p.read_text()
        except (UnicodeDecodeError, OSError):
            continue
        bad += [f"{p.relative_to(ROOT)}: {s}" for s in stale if s in t]
    assert not bad, bad


if __name__ == "__main__" and "--list" in sys.argv:
    for key, val in ids().items():
        skip = [f"--exclude-dir={d}" for d in (".venv", "node_modules", "target", ".git")]
        r = subprocess.run(["grep", "-rlF", *skip, val, str(ROOT)], capture_output=True, text=True)
        print(f"{key}={val}")
        for f in sorted(r.stdout.split()):
            print("   ", Path(f).relative_to(ROOT))
