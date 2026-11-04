"""Session-bus interface io.github.wzk4work.LDM.Daemon1.

A thin bridge for desktop integration (file managers, scripts, other apps): each D-Bus method
forwards one request to ldmd over its Unix socket. Started by the GUI when a session bus is
available, or on its own with `python -m ldm_gui.dbus_service` (no window).

  N=io.github.wzk4work.LDM.Daemon1 P=/io/github/wzk4work/LDM/Daemon1
  busctl --user call $N $P $N AddUrl s https://…/f.zip
  dbus-send --session --print-reply --dest=$N $P $N.List

Methods: AddUrl(s url) → s job id · List() → s JSON array · Pause(s id) → b · Resume(s id) → b ·
StopRecording(s id) → b (live: keep what was recorded) · Status() → s JSON ·
ToolsStatus() → s JSON (managed yt-dlp / deno) · ToolsInstall(s tool) → b · ToolsUpdate(s tool) → b
(tool "" = yt-dlp and deno; runs in the background, progress as ldmd events).
Only the session bus of the same user can reach it.
"""

from __future__ import annotations

import json
import socket
import sys

from PySide6.QtCore import ClassInfo, QCoreApplication, QObject, Slot
from PySide6.QtDBus import QDBusConnection

from ldm_gui.ipc import socket_path

SERVICE = "io.github.wzk4work.LDM.Daemon1"
PATH = "/io/github/wzk4work/LDM/Daemon1"


def call(typ: str, payload: dict, path: str | None = None, timeout: float = 10.0) -> dict:
    """One synchronous request to ldmd; returns the reply payload (raises on error replies)."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as s:
        s.settimeout(timeout)
        s.connect(path or socket_path())
        s.sendall((json.dumps({"v": 1, "id": 1, "type": typ, "payload": payload}) + "\n").encode())
        buf = b""
        while True:
            chunk = s.recv(65536)
            if not chunk:
                raise ConnectionError("ldmd closed the connection")
            buf += chunk
            while b"\n" in buf:
                line, buf = buf.split(b"\n", 1)
                env = json.loads(line)
                if env.get("id") != 1:
                    continue  # events
                if env.get("type") == "Error":
                    raise RuntimeError(f"{env['payload'].get('code')}: {env['payload'].get('message')}")
                return env.get("payload") or {}


@ClassInfo({"D-Bus Interface": SERVICE})
class Daemon1(QObject):
    def __init__(self, sock: str | None = None, parent=None):
        super().__init__(parent)
        self.sock = sock

    @Slot(str, result=str)
    def AddUrl(self, url: str) -> str:  # noqa: N802 (D-Bus method name)
        try:
            return str(call("DownloadAdd", {"url": url, "source": "dbus"}, self.sock).get("jobId") or "")
        except Exception as e:  # reported to the caller as an empty id
            print(f"ldm-dbus AddUrl: {e}", file=sys.stderr)
            return ""

    @Slot(result=str)
    def List(self) -> str:  # noqa: N802
        jobs = call("JobList", {}, self.sock).get("jobs", [])
        keep = ("id", "url", "state", "filename", "total", "got", "final_path", "recordedSecs")
        return json.dumps([{k: j.get(k) for k in keep} for j in jobs], ensure_ascii=False)

    @Slot(str, result=bool)
    def Pause(self, job_id: str) -> bool:  # noqa: N802
        return self._simple("JobPause", job_id)

    @Slot(str, result=bool)
    def Resume(self, job_id: str) -> bool:  # noqa: N802
        return self._simple("JobResume", job_id)

    @Slot(str, result=bool)
    def StopRecording(self, job_id: str) -> bool:  # noqa: N802
        return self._simple("JobStopRecording", job_id)

    @Slot(result=str)
    def Status(self) -> str:  # noqa: N802
        return json.dumps(call("Status", {}, self.sock), ensure_ascii=False)

    @Slot(result=str)
    def ToolsStatus(self) -> str:  # noqa: N802
        return json.dumps(call("ToolsStatus", {}, self.sock), ensure_ascii=False)

    @Slot(str, result=bool)
    def ToolsInstall(self, tool: str) -> bool:  # noqa: N802
        return self._tools("install", tool)

    @Slot(str, result=bool)
    def ToolsUpdate(self, tool: str) -> bool:  # noqa: N802
        return self._tools("update", tool)

    def _tools(self, action: str, tool: str) -> bool:
        try:
            call("ToolsAction", {"action": action, "tool": tool or None}, self.sock)
            return True
        except Exception as e:
            print(f"ldm-dbus Tools{action.title()}: {e}", file=sys.stderr)
            return False

    def _simple(self, typ: str, job_id: str) -> bool:
        try:
            call(typ, {"id": job_id}, self.sock)
            return True
        except Exception:
            return False


def register(parent: QObject | None = None, sock: str | None = None) -> Daemon1 | None:
    """Export the object on the session bus; None when there is no bus or the name is taken."""
    bus = QDBusConnection.sessionBus()
    if not bus.isConnected():
        return None
    obj = Daemon1(sock, parent)
    if not bus.registerService(SERVICE):
        return None
    bus.registerObject(PATH, obj, QDBusConnection.RegisterOption.ExportAllSlots)
    return obj


def main() -> int:
    app = QCoreApplication(sys.argv[:1])
    obj = register(app)
    if obj is None:
        print("ldm-dbus: no session bus or io.github.wzk4work.LDM.Daemon1 already owned", file=sys.stderr)
        return 1
    print(f"ldm-dbus: serving {SERVICE} at {PATH}", flush=True)
    return app.exec()


if __name__ == "__main__":
    sys.exit(main())
