"""Scheduler / queue manager: create & delete queues, time window, weekdays, concurrency, file order, post-queue action."""

from __future__ import annotations

import re
import shlex

from PySide6.QtCore import Qt, QTime
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QFormLayout,
    QGroupBox,
    QHBoxLayout,
    QInputDialog,
    QLineEdit,
    QListWidget,
    QListWidgetItem,
    QMessageBox,
    QPushButton,
    QSpinBox,
    QSplitter,
    QTimeEdit,
    QVBoxLayout,
    QWidget,
)

from ldm_gui.i18n import queue_label, tr
from ldm_gui.model import STATE_ZH, job_name

WEEKDAYS = ["周一", "周二", "周三", "周四", "周五", "周六", "周日"]
ON_DONE = [("", "无操作"), ("shutdown", "关机"), ("command", "运行命令")]


def queue_id_for(name: str, taken: set[str]) -> str:
    base = re.sub(r"[^a-z0-9]+", "-", name.lower()).strip("-") or "queue"
    qid, n = base, 2
    while qid in taken:
        qid, n = f"{base}-{n}", n + 1
    return qid


class QueueDialog(QDialog):
    def __init__(self, client, queues: list[dict], jobs: list[dict], parent=None):
        super().__init__(parent)
        self.client = client
        self.queues = [dict(q) for q in queues] or [{"id": "main", "name": "主队列", "concurrent": 3, "enabled": True}]
        self.jobs = jobs
        self.setWindowTitle("计划任务 / 队列")
        self.resize(820, 520)

        self.list = QListWidget()
        self.list.currentRowChanged.connect(self._load)
        b_new, b_del = QPushButton("新建队列"), QPushButton("删除队列")
        b_new.clicked.connect(self.new_queue)
        b_del.clicked.connect(self.delete_queue)
        left = QWidget()
        ll = QVBoxLayout(left)
        ll.addWidget(self.list)
        row = QHBoxLayout()
        row.addWidget(b_new)
        row.addWidget(b_del)
        ll.addLayout(row)

        self.name = QLineEdit()
        self.conc = QSpinBox()
        self.conc.setRange(1, 32)
        self.enabled = QCheckBox("启用（按计划自动运行）")
        self.use_start = QCheckBox("开始时间")
        self.start = QTimeEdit(QTime(1, 0))
        self.use_end = QCheckBox("停止时间")
        self.end = QTimeEdit(QTime(7, 0))
        for t in (self.start, self.end):
            t.setDisplayFormat("HH:mm")
        self.days = [QCheckBox(tr(d)) for d in WEEKDAYS]
        drow = QHBoxLayout()
        for c in self.days:
            drow.addWidget(c)
        self.on_done = QComboBox()
        for v, t in ON_DONE:
            self.on_done.addItem(t, v)
        self.cmd = QLineEdit()
        self.cmd.setPlaceholderText("例如：notify-send 队列完成")
        self.on_done.currentIndexChanged.connect(lambda: self.cmd.setEnabled(self.on_done.currentData() == "command"))
        sched = QGroupBox("计划")
        sf = QFormLayout(sched)
        trow = QHBoxLayout()
        trow.addWidget(self.use_start)
        trow.addWidget(self.start)
        trow.addWidget(self.use_end)
        trow.addWidget(self.end)
        trow.addStretch(1)
        sf.addRow(trow)
        sf.addRow("星期", drow)
        form = QFormLayout()
        form.addRow("名称", self.name)
        form.addRow("同时下载数", self.conc)
        form.addRow("", self.enabled)
        form.addRow("队列完成后", self.on_done)
        form.addRow("命令", self.cmd)

        self.files = QListWidget()
        up, down = QPushButton("上移"), QPushButton("下移")
        up.clicked.connect(lambda: self.move(-1))
        down.clicked.connect(lambda: self.move(1))
        frow = QHBoxLayout()
        frow.addWidget(up)
        frow.addWidget(down)
        frow.addStretch(1)
        files = QGroupBox("队列中的文件（自上而下下载）")
        fl = QVBoxLayout(files)
        fl.addWidget(self.files)
        fl.addLayout(frow)

        right = QWidget()
        rl = QVBoxLayout(right)
        rl.addLayout(form)
        rl.addWidget(sched)
        rl.addWidget(files)
        split = QSplitter()
        split.addWidget(left)
        split.addWidget(right)
        split.setStretchFactor(1, 1)

        self.b_start, self.b_stop, self.b_apply, b_close = (
            QPushButton("立即开始"),
            QPushButton("停止"),
            QPushButton("应用"),
            QPushButton("关闭"),
        )
        self.b_start.clicked.connect(lambda: self._cur() and self.client.request("QueueStart", {"id": self._cur()["id"]}))
        self.b_stop.clicked.connect(lambda: self._cur() and self.client.request("QueueStop", {"id": self._cur()["id"]}))
        self.b_apply.clicked.connect(self.apply)
        b_close.clicked.connect(self.accept)
        brow = QHBoxLayout()
        brow.addWidget(self.b_start)
        brow.addWidget(self.b_stop)
        brow.addStretch(1)
        brow.addWidget(self.b_apply)
        brow.addWidget(b_close)
        lay = QVBoxLayout(self)
        lay.addWidget(split)
        lay.addLayout(brow)
        self._fill()

    # ---------------------------------------------------------------- helpers
    def _fill(self, select: int = 0):
        self.list.blockSignals(True)
        self.list.clear()
        for q in self.queues:
            self.list.addItem(f"{queue_label(q)} ({q['id']})")
        self.list.blockSignals(False)
        self.list.setCurrentRow(min(select, len(self.queues) - 1))
        self._load(self.list.currentRow())

    def _cur(self) -> dict | None:
        r = self.list.currentRow()
        return self.queues[r] if 0 <= r < len(self.queues) else None

    def _load(self, row: int):
        if not (0 <= row < len(self.queues)):
            return
        q = self.queues[row]
        sch = q.get("schedule") or {}
        self.name.setText(queue_label(q) if q.get("name") else "")
        self.conc.setValue(int(q.get("concurrent") or 1))
        self.enabled.setChecked(bool(q.get("enabled", True)))
        for flag, edit, key in ((self.use_start, self.start, "start"), (self.use_end, self.end, "end")):
            flag.setChecked(bool(sch.get(key)))
            if sch.get(key):
                edit.setTime(QTime.fromString(sch[key], "HH:mm"))
        days = sch.get("days") or []
        for i, c in enumerate(self.days):
            c.setChecked(not days or (i + 1) in days)
        self.on_done.setCurrentIndex(max(0, self.on_done.findData(q.get("on_done") or "")))
        self.cmd.setText(shlex.join(q.get("on_done_command") or []))
        self.cmd.setEnabled(self.on_done.currentData() == "command")
        self.files.clear()
        qjobs = [j for j in self.jobs if j.get("queue_id") == q["id"] and j.get("state") not in ("Completed", "Canceled")]
        qjobs.sort(key=lambda j: (-(j.get("priority") or 0), j.get("created_at") or 0))
        for j in qjobs:
            st = j.get("state") or ""
            it = QListWidgetItem(f"{job_name(j)}   [{tr(STATE_ZH.get(st, st))}]")
            it.setData(Qt.ItemDataRole.UserRole, j)
            self.files.addItem(it)

    def move(self, d: int):
        r = self.files.currentRow()
        n = r + d
        if r < 0 or not (0 <= n < self.files.count()):
            return
        it = self.files.takeItem(r)
        self.files.insertItem(n, it)
        self.files.setCurrentRow(n)
        self._order_dirty = True

    def collect(self) -> dict:
        q = dict(self._cur() or {})
        sch: dict = {}
        if self.use_start.isChecked():
            sch["start"] = self.start.time().toString("HH:mm")
        if self.use_end.isChecked():
            sch["end"] = self.end.time().toString("HH:mm")
        days = [i + 1 for i, c in enumerate(self.days) if c.isChecked()]
        if len(days) < 7:
            sch["days"] = days
        name = self.name.text().strip() or q.get("id")
        if q.get("name") and name == queue_label(q):
            name = q["name"]  # the default queue's translated label is not stored as its name
        q.update(
            name=name,
            concurrent=self.conc.value(),
            enabled=self.enabled.isChecked(),
            schedule=sch or None,
            on_done=self.on_done.currentData(),
            on_done_command=shlex.split(self.cmd.text()) if self.on_done.currentData() == "command" else [],
        )
        q.setdefault("manual", False)
        return q

    def order(self) -> list[tuple[str, int]]:
        n = self.files.count()
        return [(self.files.item(i).data(Qt.ItemDataRole.UserRole)["id"], (n - i) * 10) for i in range(n)]

    # ---------------------------------------------------------------- actions
    def apply(self):
        q = self.collect()
        self.queues[self.list.currentRow()] = q
        self.client.request(
            "QueueSet",
            {"queue": {k: q[k] for k in ("id", "name", "concurrent", "schedule", "enabled", "on_done", "on_done_command", "manual")}},
        )
        if getattr(self, "_order_dirty", False):
            for jid, prio in self.order():
                self.client.request("JobSet", {"id": jid, "priority": prio})
            self._order_dirty = False
        self._fill(self.list.currentRow())

    def new_queue(self, name: str | None = None):
        if name is None:
            name, ok = QInputDialog.getText(self, "新建队列", "队列名称：")
            if not ok or not name.strip():
                return
        qid = queue_id_for(name, {q["id"] for q in self.queues})
        q = {
            "id": qid,
            "name": name.strip(),
            "concurrent": 1,
            "enabled": False,
            "schedule": None,
            "on_done": "",
            "on_done_command": [],
            "manual": False,
        }
        self.queues.append(q)
        self.client.request("QueueSet", {"queue": q})
        self._fill(len(self.queues) - 1)

    def delete_queue(self, confirm: bool = True):
        q = self._cur()
        if not q:
            return
        if q["id"] == "main":
            QMessageBox.information(self, tr("删除队列"), tr("主队列不能删除。"))
            return
        if (
            confirm
            and QMessageBox.question(self, tr("删除队列"), tr("删除队列“{name}”？其中的任务会移到主队列。").format(name=queue_label(q)))
            != QMessageBox.StandardButton.Yes
        ):
            return
        self.client.request("QueueDelete", {"id": q["id"]})
        self.queues.remove(q)
        self._fill(0)
