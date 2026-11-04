"""Asynchronous NDJSON envelope client over QLocalSocket (ldmd Unix socket)."""

from __future__ import annotations

import json
import os
from collections.abc import Callable

from PySide6.QtCore import QObject, QTimer, Signal
from PySide6.QtNetwork import QLocalSocket


def socket_path() -> str:
    if os.environ.get("LDM_SOCKET"):
        return os.environ["LDM_SOCKET"]
    run = os.environ.get("XDG_RUNTIME_DIR") or f"/tmp/ldm-{os.getuid()}"
    return os.path.join(run, "ldm.sock")


class DaemonClient(QObject):
    connected = Signal()
    disconnected = Signal()
    event = Signal(str, dict)  # type, payload

    def __init__(self, path: str | None = None, parent: QObject | None = None, role: str = "gui"):
        super().__init__(parent)
        # role "gui" tells ldmd a window can show the capture confirmation dialog
        self.role = role
        self.path = path or socket_path()
        self.sock = QLocalSocket(self)
        self.sock.readyRead.connect(self._read)
        self.sock.connected.connect(self._on_connected)
        self.sock.disconnected.connect(self._on_disconnected)
        self.sock.errorOccurred.connect(lambda _e: self._schedule_retry())
        self.buf = b""
        self.next_id = 1
        self.pending: dict[int, Callable[[dict], None]] = {}
        self.retry = QTimer(self)
        self.retry.setSingleShot(True)
        self.retry.timeout.connect(self.connect_daemon)
        self.is_connected = False

    def connect_daemon(self) -> None:
        if self.sock.state() == QLocalSocket.LocalSocketState.UnconnectedState:
            self.sock.connectToServer(self.path)

    def _schedule_retry(self) -> None:
        if not self.retry.isActive():
            self.retry.start(2000)

    def _on_connected(self) -> None:
        self.is_connected = True
        self.request("Subscribe", {"enabled": True, "role": self.role})
        self.connected.emit()

    def _on_disconnected(self) -> None:
        self.is_connected = False
        self.pending.clear()
        self.disconnected.emit()
        self._schedule_retry()

    def request(self, typ: str, payload: dict | None = None, callback: Callable[[dict], None] | None = None) -> int:
        mid = self.next_id
        self.next_id += 1
        if callback:
            self.pending[mid] = callback
        line = json.dumps({"v": 1, "id": mid, "type": typ, "payload": payload or {}}, ensure_ascii=False) + "\n"
        self.sock.write(line.encode())
        self.sock.flush()
        return mid

    def _read(self) -> None:
        self.buf += bytes(self.sock.readAll().data())
        while b"\n" in self.buf:
            line, self.buf = self.buf.split(b"\n", 1)
            if not line.strip():
                continue
            try:
                env = json.loads(line)
            except ValueError:
                continue
            cb = self.pending.pop(env.get("id", 0), None)
            if cb:
                cb(env)
            elif env.get("id", 0) == 0:
                self.event.emit(env.get("type", ""), env.get("payload") or {})
