"""yt-dlp / deno detection and the managed-tools view for the Options dialog
(same rules as ldm-core's ``ytdlp`` / ``tools`` modules).

LDM never bundles yt-dlp or deno. Lookup order: 1. the user's path (``ytdlp.path`` /
``ytdlp.deno_path``), 2. LDM's managed copy (``$XDG_DATA_HOME/ldm/tools/<tool>/current``,
downloaded from the official GitHub releases by ``ldm tools install``; inside Flatpak that is
``~/.var/app/io.github.wzk4work.LDM/data/ldm/tools``), 3. ``$PATH`` (and ``~/.local/bin/yt-dlp`` /
``~/.deno/bin/deno``). Updating the managed copy goes through ``ldm tools update`` (checksum
verified, atomic switch); a system copy keeps the old ``yt-dlp -U`` / pip route.
"""

from __future__ import annotations

import datetime as _dt
import json
import os
import shutil
import subprocess
import sys
from dataclasses import dataclass, field

from PySide6.QtCore import QProcess
from PySide6.QtWidgets import QDialog, QDialogButtonBox, QLabel, QPlainTextEdit, QVBoxLayout

from ldm_gui.i18n import tr

OUTDATED_DAYS = 90


TOOLS = ("yt-dlp", "deno")
SOURCE_LABELS = {"user": "用户指定", "managed": "LDM 托管", "system": "系统 PATH"}


def _exe(p: str) -> bool:
    return os.path.isfile(p) and os.access(p, os.X_OK)


def tools_root(tools_dir: str = "") -> str:
    d = (tools_dir or "").strip()
    if d:
        return os.path.expanduser(d)
    data = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
    return os.path.join(data, "ldm", "tools")


def _link(root: str, tool: str, name: str) -> str | None:
    try:
        return os.path.basename(os.readlink(os.path.join(root, tool, name)).rstrip("/")) or None
    except OSError:
        return None


def managed_bin(tool: str = "yt-dlp", tools_dir: str = "") -> str | None:
    root = tools_root(tools_dir)
    v = _link(root, tool, "current")
    p = os.path.join(root, tool, v, tool) if v else None
    return p if p and _exe(p) else None


def find(tool: str = "yt-dlp", path: str = "", tools_dir: str = "") -> tuple[str | None, str | None]:
    """(binary, source) with source user / managed / system; a broken user path → (None, None)."""
    p = (path or "").strip()
    if p:
        p = os.path.expanduser(p)
        if os.sep in p:
            return (p, "user") if _exe(p) else (None, None)
        w = shutil.which(p)
        return (w, "user") if w else (None, None)
    m = managed_bin(tool, tools_dir)
    if m:
        return m, "managed"
    found = shutil.which(tool)
    if found:
        return found, "system"
    local = os.path.expanduser("~/.local/bin/yt-dlp" if tool == "yt-dlp" else "~/.deno/bin/deno")
    return (local, "system") if _exe(local) else (None, None)


def find_binary(path: str = "", tools_dir: str = "") -> str | None:
    return find("yt-dlp", path, tools_dir)[0]


def ldm_cli() -> str:
    """The `ldm` CLI next to this GUI (AppImage / deb / Flatpak) or on $PATH."""
    if os.environ.get("LDM_CLI"):
        return os.environ["LDM_CLI"]
    for d in (os.path.dirname(os.path.realpath(sys.argv[0] or "")), "/app/bin", "/usr/bin"):
        c = os.path.join(d, "ldm")
        if _exe(c):
            return c
    return shutil.which("ldm") or "ldm"


def version_age_days(version: str, today: _dt.date | None = None) -> int | None:
    try:
        y, m, d = (int(x) for x in version.strip().split(".")[:3])
        return ((today or _dt.date.today()) - _dt.date(y, m, d)).days
    except (ValueError, TypeError):
        return None


def install_kind(binary: str, tools_dir: str = "") -> str:
    real = os.path.realpath(binary)
    if real.startswith(os.path.realpath(tools_root(tools_dir)) + os.sep):
        return "managed"
    if any(s in real for s in ("/site-packages/", "/dist-packages/", "/pipx/")):
        return "pip"
    try:
        size = os.path.getsize(real)
    except OSError:
        return "unknown"
    if size < 16 * 1024:
        try:
            with open(real, "rb") as f:
                return "pip" if b"yt_dlp" in f.read() else "unknown"
        except OSError:
            return "unknown"
    return "binary"


def update_command(binary: str | None, tools_dir: str = "") -> list[str]:
    """Managed copy or no yt-dlp at all → `ldm tools …` (verified download into the data dir)."""
    if not binary:
        return [ldm_cli(), "tools", "install", "yt-dlp"]
    kind = install_kind(binary, tools_dir)
    if kind == "managed":
        return [ldm_cli(), "tools", "update", "yt-dlp"]
    if kind == "binary":
        return [binary, "-U"]
    return ["python3", "-m", "pip", "install", "-U", "--user", "yt-dlp"]


@dataclass
class Status:
    path: str | None
    version: str | None
    age_days: int | None
    install: str | None
    source: str | None = None

    @property
    def outdated(self) -> bool:
        return self.age_days is not None and self.age_days > OUTDATED_DAYS

    def text(self, enabled: bool = True) -> tuple[str, bool]:
        """(message, ok) for the status label."""
        if not enabled:
            return tr("yt-dlp 已关闭（YouTube 将由 LDM 自己尝试，通常无法下载）"), False
        if not self.path:
            return (
                tr(
                    "未找到 yt-dlp：YouTube 下载与回退不可用。点击“安装 yt-dlp”从 GitHub 官方发布页下载（校验 SHA-256），或在下方填写路径。"
                ),
                False,
            )
        if not self.version:
            return tr("{path}：无法运行 --version").format(path=self.path), False
        src = tr(SOURCE_LABELS.get(self.source or "", "?"))
        msg = tr("{path}  版本 {ver}（来源：{src}；{kind}）").format(path=self.path, ver=self.version, src=src, kind=self.install)
        if self.outdated:
            return msg + tr("  ⚠ 已 {n} 天未更新，站点变化后可能失效，建议更新").format(n=self.age_days), False
        return msg, True


def status(path: str = "", timeout: float = 10.0, tools_dir: str = "") -> Status:
    b, src = find("yt-dlp", path, tools_dir)
    ver = None
    if b:
        try:
            r = subprocess.run([b, "--version"], capture_output=True, text=True, timeout=timeout, stdin=subprocess.DEVNULL)
            ver = r.stdout.strip() or None if r.returncode == 0 else None
        except (OSError, subprocess.SubprocessError):
            ver = None
    return Status(b, ver, version_age_days(ver) if ver else None, install_kind(b, tools_dir) if b else None, src)


@dataclass
class ToolInfo:
    tool: str
    path: str | None = None
    source: str | None = None
    current: str | None = None
    previous: str | None = None
    versions: list[str] = field(default_factory=list)
    meta: dict = field(default_factory=dict)
    latest: str | None = None
    last_check_ms: int = 0
    last_error: str | None = None


def _vkey(v: str) -> tuple:
    out = []
    for x in v.lstrip("v").split("."):
        out.append(int(x) if x.isdigit() else 0)
    return tuple(out)


def tools_info(tool: str, path: str = "", tools_dir: str = "") -> ToolInfo:
    """Read the managed layout + state.json directly (no subprocess; the GUI stays responsive)."""
    root = tools_root(tools_dir)
    b, src = find(tool, path, tools_dir)
    info = ToolInfo(tool, b, src, _link(root, tool, "current"), _link(root, tool, "previous"))
    try:
        names = os.listdir(os.path.join(root, tool))
    except OSError:
        names = []
    info.versions = sorted((n for n in names if n[:1].isdigit() and _exe(os.path.join(root, tool, n, tool))), key=_vkey, reverse=True)
    if info.previous not in info.versions:
        info.previous = None
    if info.current:
        try:
            with open(os.path.join(root, tool, info.current, "meta.json")) as f:
                info.meta = json.load(f)
        except (OSError, ValueError):
            pass
    try:
        with open(os.path.join(root, "state.json")) as f:
            st = (json.load(f).get("tools") or {}).get(tool) or {}
        info.latest, info.last_check_ms, info.last_error = st.get("latest"), int(st.get("last_check_ms") or 0), st.get("last_error")
    except (OSError, ValueError):
        pass
    return info


def update_available(info: ToolInfo) -> bool:
    return bool(info.latest) and (not info.current or _vkey(info.latest) > _vkey(info.current))


class UpdateDialog(QDialog):
    """Runs the update command and streams its output."""

    def __init__(self, cmd: list[str], parent=None, title: str = "更新 yt-dlp"):
        super().__init__(parent)
        self.setWindowTitle(tr(title))
        self.resize(640, 360)
        self.cmd = cmd
        self.out = QPlainTextEdit()
        self.out.setReadOnly(True)
        self.state = QLabel("$ " + " ".join(cmd))
        bb = QDialogButtonBox(QDialogButtonBox.StandardButton.Close)
        bb.rejected.connect(self.reject)
        lay = QVBoxLayout(self)
        lay.addWidget(self.state)
        lay.addWidget(self.out)
        lay.addWidget(bb)
        self.proc = QProcess(self)
        self.proc.setProcessChannelMode(QProcess.ProcessChannelMode.MergedChannels)
        self.proc.readyReadStandardOutput.connect(self._read)
        self.proc.finished.connect(self._done)
        self.proc.errorOccurred.connect(lambda _e: self._done(-1, None))
        self.exit_code: int | None = None

    def start(self) -> None:
        self.proc.start(self.cmd[0], self.cmd[1:])

    def _read(self) -> None:
        self.out.appendPlainText(bytes(self.proc.readAllStandardOutput()).decode(errors="replace").rstrip())

    def _done(self, code, _status) -> None:
        if self.exit_code is not None:
            return
        self.exit_code = int(code)
        self.state.setText((tr("完成") if code == 0 else tr("失败（退出码 {code}）").format(code=code)) + tr("：") + " ".join(self.cmd))
