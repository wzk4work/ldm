"""Per-download progress window: status, segment map, connection table, speed limit, after-completion action.

Original implementation (progress dialog, per-job limiter);
layout and wording are LDM's own.
"""

from __future__ import annotations

from PySide6.QtCore import QRectF, Qt, QTimer
from PySide6.QtGui import QColor, QPainter, QPen
from PySide6.QtWidgets import (
    QButtonGroup,
    QCheckBox,
    QDialog,
    QFormLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QMessageBox,
    QPushButton,
    QRadioButton,
    QSpinBox,
    QTableWidget,
    QTableWidgetItem,
    QTabWidget,
    QVBoxLayout,
    QWidget,
)

from ldm_gui.model import ACTIVE_STATES, STATE_ZH, eta_seconds, hms, human, human_time, is_recording, job_name, progress

AFTER_CHOICES = [("", "无操作"), ("open", "打开文件"), ("folder", "打开所在文件夹"), ("shutdown", "关机")]


def part_end(p: dict, total: int | None) -> int:
    e = p.get("end", -1)
    if e is None or e < 0:
        return total or (p.get("start", 0) + p.get("got", 0))
    return e


class SegmentBar(QWidget):
    """Draws each part as a lane slice: downloaded share filled, active parts highlighted."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.setMinimumHeight(22)
        self.total: int | None = None
        self.parts: list[dict] = []
        self.fraction: float | None = None  # used for media jobs (segments) and unknown sizes

    def set_data(self, total: int | None, parts: list[dict], fraction: float | None = None) -> None:
        self.total, self.parts, self.fraction = total, parts or [], fraction
        self.update()

    def spans(self) -> list[tuple[float, float, float, str]]:
        """(x0, x1, fill_x1, state) in 0..1 units; exposed for tests."""
        out = []
        if self.total and self.parts:
            for p in self.parts:
                s = p.get("start", 0) / self.total
                e = min(1.0, part_end(p, self.total) / self.total)
                f = min(e, (p.get("start", 0) + p.get("got", 0)) / self.total)
                out.append((s, e, f, p.get("state", "")))
        elif self.fraction is not None:
            out.append((0.0, 1.0, max(0.0, min(1.0, self.fraction)), "Active"))
        return out

    def paintEvent(self, _e):  # noqa: N802
        p = QPainter(self)
        r = QRectF(self.rect()).adjusted(0.5, 0.5, -0.5, -0.5)
        p.fillRect(r, QColor("#e8eaed"))
        w = r.width()
        for s, e, f, st in self.spans():
            done = st in ("Done", "Completed")
            p.fillRect(QRectF(r.x() + s * w, r.y(), max(1.0, (f - s) * w), r.height()), QColor("#1e8e3e" if done else "#1a73e8"))
            if e > s:
                p.setPen(QPen(QColor("#5f6368"), 1))
                p.drawLine(int(r.x() + e * w), int(r.y()), int(r.x() + e * w), int(r.bottom()))
        p.setPen(QPen(QColor("#9aa0a6"), 1))
        p.drawRect(r)
        p.end()


class ProgressDialog(QDialog):
    def __init__(self, client, job: dict, parent=None):
        super().__init__(parent)
        self.client = client
        self.job = dict(job)
        self.setWindowTitle(self._title())
        self.resize(560, 470)
        self.setAttribute(Qt.WidgetAttribute.WA_DeleteOnClose)

        tabs = QTabWidget()
        # --- status page
        st = QWidget()
        form = QFormLayout()
        self.lbl = {}
        for key, text in [
            ("url", "地址"),
            ("state", "状态"),
            ("size", "文件大小"),
            ("got", "已下载"),
            ("speed", "传输速率"),
            ("eta", "剩余时间"),
            ("resume", "断点续传"),
            ("conns", "连接数"),
        ]:
            lab = QLabel()
            lab.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
            if key == "url":
                lab.setWordWrap(True)
            self.lbl[key] = lab
            form.addRow(text + "：", lab)
        self.bar = SegmentBar()
        self.conn_table = QTableWidget(0, 4)
        self.conn_table.setHorizontalHeaderLabels(["#", "范围", "已下载", "状态"])
        self.conn_table.horizontalHeader().setSectionResizeMode(1, QHeaderView.ResizeMode.Stretch)
        self.conn_table.verticalHeader().hide()
        self.conn_table.setEditTriggers(QTableWidget.EditTrigger.NoEditTriggers)
        sl = QVBoxLayout(st)
        sl.addLayout(form)
        sl.addWidget(QLabel("分段下载进度（每段一条连接）："))
        sl.addWidget(self.bar)
        sl.addWidget(self.conn_table)
        tabs.addTab(st, "下载状态")

        # --- speed limit page
        lim = QWidget()
        self.limit_on = QCheckBox("限制此任务的最大下载速度")
        self.limit_kib = QSpinBox()
        self.limit_kib.setRange(1, 10_000_000)
        self.limit_kib.setSuffix(" KiB/s")
        cur = int(self.job.get("speed_limit_bps") or 0)
        self.limit_on.setChecked(cur > 0)
        self.limit_kib.setValue(max(1, cur // 1024) if cur else 512)
        self.limit_kib.setEnabled(cur > 0)
        self.limit_on.toggled.connect(self.limit_kib.setEnabled)
        apply_lim = QPushButton("应用")
        apply_lim.clicked.connect(self.apply_limit)
        ll = QVBoxLayout(lim)
        ll.addWidget(self.limit_on)
        row = QHBoxLayout()
        row.addWidget(QLabel("最大速度："))
        row.addWidget(self.limit_kib)
        row.addWidget(apply_lim)
        row.addStretch(1)
        ll.addLayout(row)
        ll.addWidget(QLabel("提示：全局限速可在托盘菜单或“选项 → 连接”中开关。"))
        ll.addStretch(1)
        tabs.addTab(lim, "速度限制")

        # --- after completion page
        aft = QWidget()
        al = QVBoxLayout(aft)
        self.after_group = QButtonGroup(self)
        cur_after = self.job.get("after") or ""
        for i, (val, text) in enumerate(AFTER_CHOICES):
            rb = QRadioButton(text)
            rb.setProperty("after", val)
            rb.setChecked(val == cur_after)
            self.after_group.addButton(rb, i)
            al.addWidget(rb)
        self.after_group.buttonClicked.connect(lambda b: self.set_after(b.property("after")))
        al.addStretch(1)
        tabs.addTab(aft, "完成后")
        self.tabs = tabs

        # --- buttons
        self.btn_pause = QPushButton("暂停")
        self.btn_pause.clicked.connect(self.toggle_pause)
        self.btn_cancel = QPushButton("取消下载")
        self.btn_cancel.clicked.connect(self.cancel)
        self.btn_stop_rec = QPushButton("停止录制")
        self.btn_stop_rec.setToolTip("直播：保存已录制的部分并完成")
        self.btn_stop_rec.clicked.connect(self.stop_recording)
        self.btn_stop_rec.hide()
        self.btn_hide = QPushButton("隐藏详情")
        self.btn_hide.clicked.connect(self.toggle_details)
        btns = QHBoxLayout()
        btns.addWidget(self.btn_hide)
        btns.addStretch(1)
        btns.addWidget(self.btn_stop_rec)
        btns.addWidget(self.btn_pause)
        btns.addWidget(self.btn_cancel)
        lay = QVBoxLayout(self)
        lay.addWidget(tabs)
        lay.addLayout(btns)

        self.timer = QTimer(self)
        self.timer.timeout.connect(self.refresh)
        self.timer.start(1000)
        client.event.connect(self._on_event)
        self.render()
        self.refresh()

    def _title(self) -> str:
        if is_recording(self.job):
            return f"⏺ {hms(self.job.get('recordedSecs'))} {job_name(self.job)}"
        p = progress(self.job)
        pct = "" if p is None else f"{p:.0f}% "
        return f"{pct}{job_name(self.job)}"

    # ---------------------------------------------------------------- data
    def refresh(self):
        if self.client.is_connected:
            self.client.request("JobGet", {"id": self.job["id"]}, self._got)

    def _got(self, env):
        j = env.get("payload", {}).get("job")
        if j:
            self.update_job(j)

    def _on_event(self, typ, p):
        if typ == "JobProgress" and p.get("jobId") == self.job.get("id"):
            self.job.update(
                {k: v for k, v in (("got", p.get("got")), ("speedBps", p.get("speedBps")), ("state", p.get("state"))) if v is not None}
            )
            if p.get("total"):
                self.job["total"] = p["total"]
            self.job["phase"] = p.get("phase")
            for k in ("connections", "segments_done", "segments_total", "recordedSecs"):
                if p.get(k) is not None:
                    self.job[k] = p[k]
            self.render()
        elif typ == "JobStateChanged" and p.get("id") == self.job.get("id"):
            if p.get("state") == "Removed":
                self.close()
                return
            self.update_job(p)

    def update_job(self, j: dict):
        self.job.update(j)
        self.render()
        if self.job.get("state") in ("Completed", "Canceled"):
            self.timer.stop()
            if self.job.get("state") == "Completed":
                self.close()

    def render(self):
        j = self.job
        total, got = j.get("total"), j.get("got") or 0
        p = progress(j)
        state = j.get("state", "")
        self.setWindowTitle(self._title())
        self.lbl["url"].setText(j.get("url", ""))
        st = STATE_ZH.get(state, state)
        recording = is_recording(j)
        if recording:
            st = "直播录制中"
            if j.get("stopRequested"):
                st = "正在停止录制…"
        if j.get("error"):
            st += f"：{j.get('error_code')} {j.get('error')}"
        self.lbl["state"].setText(st)
        self.lbl["size"].setText(human(total) if total and not recording else "未知")
        if recording:
            self.lbl["got"].setText(f"{human(got)}（已录制 {hms(j.get('recordedSecs'))}）")
        else:
            self.lbl["got"].setText(f"{human(got)}" + ("" if p is None else f"（{p:.2f}%）"))
        spd = j.get("speedBps") or 0
        self.lbl["speed"].setText(f"{human(spd)}/s" if state in ACTIVE_STATES else "-")
        self.lbl["eta"].setText(human_time(eta_seconds(j)) or "-")
        parts = j.get("parts") or []
        if j.get("kind") in ("hls", "dash"):
            resumable = "是（按分片续传）"
        elif len(parts) > 1 or (total and (j.get("etag") or j.get("last_modified"))):
            resumable = "是"
        elif total:
            resumable = "未知"
        else:
            resumable = "否"
        self.lbl["resume"].setText(resumable)
        self.lbl["conns"].setText(
            str(j.get("connections") if j.get("connections") is not None else len([x for x in parts if x.get("state") == "Active"]))
        )
        frac = None
        if j.get("segments_total"):
            frac = (j.get("segments_done") or 0) / j["segments_total"]
        elif p is not None and not parts:
            frac = p / 100
        self.bar.set_data(total, parts, frac)
        self.conn_table.setRowCount(len(parts))
        for r, part in enumerate(parts):
            end = part_end(part, total)
            size = max(0, end - part.get("start", 0))
            pct = f"{100 * part.get('got', 0) / size:.0f}%" if size else ""
            vals = [
                str(part.get("idx", r) + 1),
                f"{human(part.get('start', 0))} – {human(end)}",
                f"{human(part.get('got', 0))} {pct}",
                {"Active": "接收数据", "Done": "完成", "Pending": "等待"}.get(part.get("state", ""), part.get("state", "")),
            ]
            for c, v in enumerate(vals):
                self.conn_table.setItem(r, c, QTableWidgetItem(v))
        paused = state in ("Paused", "Failed", "Queued")
        self.btn_pause.setText("继续" if paused else "暂停")
        self.btn_pause.setEnabled(state not in ("Completed", "Canceled"))
        self.btn_stop_rec.setVisible(recording)

    # ---------------------------------------------------------------- actions
    def toggle_pause(self):
        typ = "JobResume" if self.btn_pause.text() == "继续" else "JobPause"
        self.client.request(typ, {"id": self.job["id"]})

    def stop_recording(self):
        """Graceful: the recording is finalised into a playable file (unlike 取消 / 删除)."""
        self.client.request("JobStopRecording", {"id": self.job["id"]})
        self.job["stopRequested"] = True
        self.btn_stop_rec.setEnabled(False)
        self.render()

    def cancel(self):
        if self.job.get("state") in ACTIVE_STATES or self.job.get("state") == "Queued":
            self.client.request("JobPause", {"id": self.job["id"]})
        self.close()

    def toggle_details(self):
        vis = not self.tabs.isVisible()
        self.tabs.setVisible(vis)
        self.btn_hide.setText("隐藏详情" if vis else "显示详情")
        self.adjustSize()

    def apply_limit(self):
        bps = self.limit_kib.value() * 1024 if self.limit_on.isChecked() else 0
        self.client.request("JobSet", {"id": self.job["id"], "speed_limit_bps": bps})
        self.job["speed_limit_bps"] = bps

    def set_after(self, val: str):
        if val == "shutdown":
            ok = QMessageBox.question(self, "完成后关机", "此任务完成后将执行关机命令，确定？") == QMessageBox.StandardButton.Yes
            if not ok:
                self.after_group.button(0).setChecked(True)
                val = ""
        self.client.request("JobSet", {"id": self.job["id"], "after": val})
        self.job["after"] = val

    def closeEvent(self, e):  # noqa: N802
        self.timer.stop()
        try:
            self.client.event.disconnect(self._on_event)
        except (RuntimeError, TypeError):
            pass
        super().closeEvent(e)
