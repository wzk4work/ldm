"""Main window: category/queue tree, job list, toolbar + menus, tray, clipboard monitor, dialogs."""

from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys

from PySide6.QtCore import QPoint, QSettings, QSize, Qt, QTimer, QUrl
from PySide6.QtGui import QAction, QColor, QDesktopServices, QGuiApplication, QIcon, QKeySequence, QPainter, QPixmap, QPolygon
from PySide6.QtWidgets import (
    QAbstractItemView,
    QApplication,
    QFileDialog,
    QLabel,
    QLineEdit,
    QMainWindow,
    QMenu,
    QMessageBox,
    QPlainTextEdit,
    QSplitter,
    QSystemTrayIcon,
    QTableView,
    QToolBar,
    QToolButton,
    QTreeWidget,
    QTreeWidgetItem,
    QVBoxLayout,
    QWidget,
)

from ldm_gui import __version__, i18n
from ldm_gui.capture import CompletionDialog, DownloadInfoDialog, PropertiesDialog
from ldm_gui.dialogs import AddDialog, SettingsDialog, urls_in_text
from ldm_gui.i18n import _tr_count, tr
from ldm_gui.ipc import DaemonClient
from ldm_gui.model import ACTIVE_STATES, CATEGORIES, COL, JobFilter, JobModel, human, is_recording, job_name, progress
from ldm_gui.progress import ProgressDialog
from ldm_gui.scheduler import QueueDialog
from ldm_gui.tools import BatchDialog, DropTarget, GrabberDialog

SOUND_FILES = {
    "done": ["/usr/share/sounds/freedesktop/stereo/complete.oga", "/usr/share/sounds/freedesktop/stereo/bell.oga"],
    "fail": ["/usr/share/sounds/freedesktop/stereo/dialog-error.oga", "/usr/share/sounds/freedesktop/stereo/bell.oga"],
}
DEFAULT_SHORTCUTS = {
    "add": ("添加 URL", "Ctrl+N"),
    "batch": ("批量下载", "Ctrl+B"),
    "resume": ("继续", "Ctrl+R"),
    "pause": ("暂停", "Ctrl+P"),
    "remove": ("删除", "Del"),
    "props": ("属性", "Alt+Return"),
    "settings": ("选项", "Ctrl+,"),
    "scheduler": ("计划任务", "Ctrl+Shift+S"),
    "grabber": ("抓取器", "Ctrl+G"),
}
STATE_GROUPS = [("unfinished", "未完成"), ("finished", "已完成")]


def app_icon() -> QIcon:
    pm = QPixmap(64, 64)
    pm.fill(Qt.GlobalColor.transparent)
    p = QPainter(pm)
    p.setRenderHint(QPainter.RenderHint.Antialiasing)
    p.setBrush(QColor("#1a73e8"))
    p.setPen(Qt.PenStyle.NoPen)
    p.drawRoundedRect(4, 4, 56, 56, 10, 10)
    p.setBrush(QColor("white"))
    p.drawRect(27, 12, 10, 22)
    p.drawPolygon(QPolygon([QPoint(16, 32), QPoint(48, 32), QPoint(32, 48)]))
    p.drawRect(16, 50, 32, 5)
    p.end()
    return QIcon(pm)


def play_sound(kind: str) -> None:
    """Short freedesktop sound if a player exists, otherwise the system beep."""
    if os.environ.get("QT_QPA_PLATFORM") == "offscreen":
        return
    player = shutil.which("paplay") or shutil.which("pw-play") or shutil.which("canberra-gtk-play")
    f = next((x for x in SOUND_FILES.get(kind, []) if os.path.exists(x)), None)
    if player and f:
        args = [player, "-f", f] if player.endswith("canberra-gtk-play") else [player, f]
        try:
            subprocess.Popen(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            return
        except OSError:
            pass
    QApplication.beep()


class MainWindow(QMainWindow):
    def __init__(self, client: DaemonClient, settings: QSettings | None = None):
        super().__init__()
        self.client = client
        self.settings = settings or QSettings("LDM", "ldm-gui")
        self.setWindowTitle("LDM 下载管理器")
        self.setWindowIcon(app_icon())
        self.resize(1180, 680)
        self.model = JobModel(self)
        self.proxy = JobFilter(self)
        self.proxy.setSourceModel(self.model)
        self.queues: list[dict] = []
        self.config: dict = {}
        self._last_clip = ""
        self._clip_seen: set[str] = set()
        self._interactive: set[str] = set()  # ids added from dialogs this session → progress window on start
        self._prev_state: dict[str, str] = {}
        self.progress_windows: dict[str, ProgressDialog] = {}
        self.info_dialogs: dict[str, DownloadInfoDialog] = {}
        self.drop_target: DropTarget | None = None

        # left: folder tree
        self.tree = QTreeWidget()
        self.tree.setHeaderHidden(True)
        self.tree.setMaximumWidth(220)
        self._build_tree()
        self.tree.currentItemChanged.connect(self._filter_changed)

        # center: table
        self.table = QTableView()
        self.table.setModel(self.proxy)
        self.table.setSortingEnabled(True)
        self.table.setSelectionBehavior(QAbstractItemView.SelectionBehavior.SelectRows)
        hh = self.table.horizontalHeader()
        # a wide file-name column, compact numeric columns, the last one stretches
        hh.setStretchLastSection(True)
        widths = {"文件名": 240, "大小": 75, "进度": 60, "速度": 85, "剩余时间": 70, "状态": 70, "类型": 60, "队列": 70, "最后尝试": 120}
        for name, wdt in widths.items():
            if name in COL:
                self.table.setColumnWidth(COL[name], wdt)
        hh.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        hh.customContextMenuRequested.connect(self._header_menu)
        self.table.verticalHeader().hide()
        self.table.setContextMenuPolicy(Qt.ContextMenuPolicy.CustomContextMenu)
        self.table.customContextMenuRequested.connect(self._context_menu)
        self.table.doubleClicked.connect(lambda _i: self.activate_selected())
        self.table.selectionModel().selectionChanged.connect(lambda *_: self._show_details())
        for name in self.settings.value("hidden_columns", ["类型"]) or []:
            if name in COL:
                self.table.setColumnHidden(COL[name], True)

        self.search = QLineEdit()
        self.search.setPlaceholderText("搜索文件名、URL 或描述…")
        self.search.textChanged.connect(lambda t: self.proxy.set_filter(self.proxy.category, self.proxy.state, t, self.proxy.queue))
        self.details = QPlainTextEdit()
        self.details.setReadOnly(True)
        self.details.setMaximumHeight(170)

        center = QWidget()
        cl = QVBoxLayout(center)
        cl.setContentsMargins(0, 0, 0, 0)
        cl.addWidget(self.search)
        vsplit = QSplitter(Qt.Orientation.Vertical)
        vsplit.addWidget(self.table)
        vsplit.addWidget(self.details)
        cl.addWidget(vsplit)
        split = QSplitter()
        split.addWidget(self.tree)
        split.addWidget(center)
        split.setStretchFactor(1, 1)
        self.setCentralWidget(split)

        # actions
        A = self._action
        self.act_add = A("添加 URL", self.add_dialog, "add")
        self.act_batch = A("批量下载", self.batch_dialog, "batch")
        self.act_resume = A("继续", lambda: self._each("JobResume"), "resume")
        self.act_pause = A("暂停", lambda: self._each("JobPause"), "pause")
        self.act_remove = A("删除", self.remove_selected, "remove")
        self.act_stop_rec = A("停止录制", self.stop_recording_selected)
        self.act_stop_rec.setToolTip("直播：保存已录制的部分并完成")
        self.act_remove_done = A("删除已完成", self.remove_completed)
        self.act_resume_all = A("全部继续", lambda: self.client.request("JobResume", {"all": True}))
        self.act_pause_all = A("全部停止", lambda: self.client.request("JobPause", {"all": True}))
        self.act_settings = A("选项", self.settings_dialog, "settings")
        self.act_scheduler = A("计划任务", self.scheduler_dialog, "scheduler")
        self.act_grabber = A("抓取器", self.grabber_dialog, "grabber")
        self.act_folder = A("打开文件夹", self.open_folder)
        self.act_open = A("打开", self.open_file)
        self.act_props = A("属性", self.properties_dialog, "props")
        self.act_progress = A("进度窗口", self.progress_selected)
        self.act_import = A("导入…", self.import_list)
        self.act_export = A("导出…", self.export_list)
        self.act_limiter = A("速度限制器", self.toggle_limiter)
        self.act_limiter.setCheckable(True)
        self.act_clip = A("监视剪贴板", self.toggle_clipboard)
        self.act_clip.setCheckable(True)
        self.act_drop = A("悬浮投放窗口", self.toggle_drop_target)
        self.act_drop.setCheckable(True)
        self.act_tree = A("分类树", lambda on: self.tree.setVisible(on))
        self.act_tree.setCheckable(True)
        self.act_tree.setChecked(True)
        self.act_details = A("详情面板", lambda on: self.details.setVisible(on))
        self.act_details.setCheckable(True)
        self.act_details.setChecked(True)
        self.act_quit = A("退出", QApplication.quit)
        self.act_quit.setShortcut(QKeySequence.StandardKey.Quit)
        self.queue_start_menu = QMenu("开始队列", self)
        self.queue_stop_menu = QMenu("停止队列", self)
        self.queue_start_menu.aboutToShow.connect(lambda: self._fill_queue_menu(self.queue_start_menu, "QueueStart"))
        self.queue_stop_menu.aboutToShow.connect(lambda: self._fill_queue_menu(self.queue_stop_menu, "QueueStop"))
        self.apply_shortcuts()

        # menubar (任务/文件/下载/查看/帮助)
        mb = self.menuBar()
        m = mb.addMenu("任务(&T)")
        for a in (self.act_add, self.act_batch, None, self.act_import, self.act_export, None, self.act_quit):
            m.addSeparator() if a is None else m.addAction(a)
        m = mb.addMenu("文件(&F)")
        for a in (self.act_open, self.act_folder, self.act_progress, self.act_props, None, self.act_remove, self.act_remove_done):
            m.addSeparator() if a is None else m.addAction(a)
        m = mb.addMenu("下载(&D)")
        for a in (self.act_resume, self.act_pause, self.act_resume_all, self.act_pause_all, None, self.act_scheduler):
            m.addSeparator() if a is None else m.addAction(a)
        m.addMenu(self.queue_start_menu)
        m.addMenu(self.queue_stop_menu)
        m.addSeparator()
        m.addAction(self.act_limiter)
        m.addAction(self.act_grabber)
        m.addAction(self.act_settings)
        m = mb.addMenu("查看(&V)")
        for a in (self.act_tree, self.act_details, self.act_drop, self.act_clip):
            m.addAction(a)
        m = mb.addMenu("帮助(&H)")
        m.addAction("导出诊断包…", self.export_diag)
        m.addAction("关于 LDM", self.about)

        # toolbar
        tb = QToolBar("main")
        tb.setObjectName("main-toolbar")
        tb.setIconSize(QSize(18, 18))
        tb.setToolButtonStyle(Qt.ToolButtonStyle.ToolButtonTextOnly)
        self.addToolBar(tb)
        for a in (
            self.act_add,
            self.act_resume,
            self.act_pause,
            self.act_pause_all,
            self.act_remove,
            self.act_remove_done,
            None,
            self.act_settings,
            self.act_scheduler,
        ):
            tb.addSeparator() if a is None else tb.addAction(a)
        for menu, text in ((self.queue_start_menu, "开始队列"), (self.queue_stop_menu, "停止队列")):
            btn = QToolButton()
            btn.setText(tr(text))
            btn.setMenu(menu)
            btn.setPopupMode(QToolButton.ToolButtonPopupMode.InstantPopup)
            tb.addWidget(btn)
        tb.addSeparator()
        tb.addAction(self.act_grabber)
        tb.addAction(self.act_batch)
        self.toolbar = tb

        self.status = QLabel("正在连接 ldmd…")
        self.statusBar().addPermanentWidget(self.status)
        self.setAcceptDrops(True)

        # tray
        self.tray = None
        if QSystemTrayIcon.isSystemTrayAvailable():
            self.tray = QSystemTrayIcon(app_icon(), self)
            self.tray.setContextMenu(self.tray_menu())
            self.tray.setToolTip("LDM")
            self.tray.activated.connect(lambda r: self.showNormal() if r == QSystemTrayIcon.ActivationReason.Trigger else None)
            self.tray.show()

        client.connected.connect(self._on_connected)
        client.disconnected.connect(lambda: self.status.setText(tr("ldmd 未连接（自动重试中）")))
        client.event.connect(self._on_event)
        self.poll = QTimer(self)
        self.poll.timeout.connect(self._refresh_status)
        self.poll.start(1000)
        QGuiApplication.clipboard().dataChanged.connect(self._clipboard_changed)

    # ---------------------------------------------------------------- building blocks
    def _action(self, text, slot, key: str | None = None) -> QAction:
        a = QAction(text, self)
        a.triggered.connect(slot)
        if key:
            a.setData(key)
        return a

    def shortcut_map(self) -> dict[str, str]:
        return {k: self.settings.value(f"shortcuts/{k}", d) for k, (_l, d) in DEFAULT_SHORTCUTS.items()}

    def apply_shortcuts(self):
        sc = self.shortcut_map()
        for a in self.findChildren(QAction):
            k = a.data()
            if isinstance(k, str) and k in sc:
                a.setShortcut(QKeySequence(sc[k]))

    def tray_menu(self) -> QMenu:
        m = QMenu(self)
        m.addAction("显示主窗口", self.showNormal)
        m.addAction(self.act_add)
        m.addSeparator()
        m.addAction(self.act_limiter)
        m.addAction(self.act_clip)
        m.addSeparator()
        m.addAction(self.act_pause_all)
        m.addAction(self.act_resume_all)
        m.addAction(self.act_scheduler)
        m.addSeparator()
        m.addAction(self.act_quit)
        return m

    def _build_tree(self):
        cur = self.tree.currentItem().data(0, Qt.ItemDataRole.UserRole) if self.tree.currentItem() else None
        self.tree.blockSignals(True)
        self.tree.clear()

        def node(text, data, parent=None):
            it = QTreeWidgetItem([_tr_count(text)])
            it.setData(0, Qt.ItemDataRole.UserRole, data)
            (parent.addChild(it) if parent else self.tree.addTopLevelItem(it))
            return it

        root = node("全部下载", ("all", None, None))
        for cid, name in CATEGORIES[1:]:
            node(name, (cid, None, None), root)
        for sid, name in STATE_GROUPS:
            grp = node(name, ("all", sid, None))
            for cid, cname in CATEGORIES[1:]:
                node(cname, (cid, sid, None), grp)
        node("正在下载", ("all", "active", None))
        node("失败", ("all", "Failed", None))
        qroot = node("队列", ("all", None, "*"))
        for q in self.queues or [{"id": "main", "name": "主队列"}]:
            node(q.get("name") or q["id"], ("all", None, q["id"]), qroot)
        self.tree.expandToDepth(0)
        self.tree.blockSignals(False)
        target = None
        it = self.tree.invisibleRootItem()
        stack = [it.child(i) for i in range(it.childCount())]
        while stack:
            x = stack.pop()
            if x.data(0, Qt.ItemDataRole.UserRole) == cur:
                target = x
                break
            stack.extend(x.child(i) for i in range(x.childCount()))
        self.tree.setCurrentItem(target or root)

    def tree_items(self) -> list[tuple[str, tuple]]:
        out = []
        stack = [self.tree.topLevelItem(i) for i in range(self.tree.topLevelItemCount())]
        while stack:
            x = stack.pop(0)
            out.append((x.text(0), x.data(0, Qt.ItemDataRole.UserRole)))
            stack.extend(x.child(i) for i in range(x.childCount()))
        return out

    # ---------------------------------------------------------------- daemon
    def _on_connected(self):
        self.status.setText(tr("已连接 ldmd"))
        self.reload()
        self.reload_queues()
        self.client.request("ConfigGet", {}, lambda e: self._set_config(e["payload"].get("config", {})))

    def _set_config(self, cfg: dict):
        self.config = cfg or {}
        self.act_limiter.setChecked(bool(self.config.get("engine", {}).get("enable_speed_limit")))
        self.act_clip.setChecked(bool(self.config.get("general", {}).get("enable_clipboard")))

    def reload(self):
        def got(e):
            jobs = e["payload"].get("jobs", [])
            self.model.set_jobs(jobs)
            self._prev_state = {j["id"]: j.get("state", "") for j in jobs}
            for j in jobs:  # captures that arrived while no GUI was open
                if j.get("awaiting_confirm"):
                    self.show_download_info(j)

        self.client.request("JobList", {}, got)

    def reload_queues(self):
        def got(e):
            self.queues = e["payload"].get("queues", [])
            self._build_tree()

        self.client.request("QueueList", {}, got)

    def _on_event(self, typ: str, payload: dict):
        gen = self.config.get("general", {})
        if typ == "JobProgress":
            self.model.progress_event(payload)
            self._show_details()
        elif typ == "JobStateChanged":
            jid, st = payload.get("id"), payload.get("state")
            prev = self._prev_state.get(jid)
            self._prev_state[jid] = st
            self.model.upsert(payload)
            self._show_details()
            if st == "Failed" and prev != "Failed" and gen.get("sounds"):
                play_sound("fail")
            if st in ACTIVE_STATES and jid in self._interactive and gen.get("show_progress_dialog", True):
                self._interactive.discard(jid)
                self.show_progress(payload)
        elif typ == "CaptureConfirm":
            self.model.upsert(payload)
            self.show_download_info(payload)
        elif typ == "JobCompleted":
            self.on_completed(payload)
        elif typ == "ToolsNeeded":
            self.ask_tools_install(payload)
        elif typ == "ToolsProgress":
            got, total = payload.get("got") or 0, payload.get("total")
            pct = f" {got * 100 / total:.0f}%" if total else ""
            self.statusBar().showMessage(f"{payload.get('tool', '')} {payload.get('phase', '')}{pct}", 3000)
        elif typ == "ToolsChanged":
            res = payload.get("result") or {}
            parts = []
            for t, r in res.items():
                if not isinstance(r, dict):
                    continue
                if r.get("error"):
                    parts.append(f"{t}: 失败（{r['error']}）")
                elif r.get("version") or (r.get("install") or {}).get("version"):
                    parts.append(f"{t} {r.get('version') or r['install']['version']}")
            if parts:
                self.statusBar().showMessage("工具：" + "；".join(parts), 10000)
        elif typ == "Notify" and self.tray and gen.get("notify_on_complete", True):
            self.tray.showMessage(payload.get("title", "LDM"), str(payload.get("body") or ""), app_icon(), 4000)

    def ask_tools_install(self, payload: dict):
        """First-use consent (ytdlp.auto_install = ask): download yt-dlp + deno from GitHub?"""
        if getattr(self, "tools_prompt", None) is not None and self.tools_prompt.isVisible():
            return
        from PySide6.QtWidgets import QCheckBox

        box = QMessageBox(
            QMessageBox.Icon.Question,
            "需要 yt-dlp",
            "此下载需要 yt-dlp（LDM 不内置）。\n\n是否从 GitHub 官方发布页下载 yt-dlp 与 deno 到\n"
            f"{payload.get('toolsDir', '')}\n并校验 SHA-256？完成后等待中的任务会自动重新开始。",
            parent=self,
        )
        yes = box.addButton("下载并安装", QMessageBox.ButtonRole.AcceptRole)
        box.addButton("不用了", QMessageBox.ButtonRole.RejectRole)
        always = QCheckBox("以后需要时自动安装，不再询问")
        box.setCheckBox(always)

        def done(_r):
            if box.clickedButton() is yes:
                self.client.request("ToolsAction", {"action": "install"})
                self.statusBar().showMessage("正在下载 yt-dlp / deno…", 5000)
                if always.isChecked():
                    self.patch_config({"ytdlp": {"auto_install": "always"}})

        box.finished.connect(done)
        box.open()
        self.tools_prompt = box

    def on_completed(self, job: dict):
        gen = self.config.get("general", {})
        if gen.get("sounds"):
            play_sound("done")
        after = job.get("after") or ""
        path = job.get("final_path") or ""
        if after in ("open", "folder") and path:
            target = path if after == "open" else os.path.dirname(path)
            if os.environ.get("QT_QPA_PLATFORM") != "offscreen":
                QDesktopServices.openUrl(QUrl.fromLocalFile(target))
            self.last_after_open = target
            return
        if gen.get("show_complete_dialog", True) and job.get("source") not in ("batch", "import", "grabber"):
            dlg = CompletionDialog(job, self)
            dlg.finished.connect(lambda _r, d=dlg: d.never.isChecked() and self.patch_config({"general": {"show_complete_dialog": False}}))
            dlg.show()
            self.last_completion = dlg

    def show_download_info(self, job: dict):
        jid = job["id"]
        if jid in self.info_dialogs:
            self.info_dialogs[jid].raise_()
            return
        dlg = DownloadInfoDialog(job, self.config, self.queues, self)

        def finished(_r, d=dlg):
            self.info_dialogs.pop(jid, None)
            d.apply(self.client)
            if d.choice == d.START:
                self._interactive.add(jid)

        dlg.finished.connect(finished)
        self.info_dialogs[jid] = dlg
        dlg.show()
        dlg.raise_()
        dlg.activateWindow()

    def show_progress(self, job: dict):
        jid = job["id"]
        w = self.progress_windows.get(jid)
        if w is None:
            full = self.model.jobs[self.model.index_of[jid]] if jid in self.model.index_of else job
            w = ProgressDialog(self.client, full, self)
            w.destroyed.connect(lambda *_: self.progress_windows.pop(jid, None))
            self.progress_windows[jid] = w
        w.show()
        w.raise_()
        return w

    def patch_config(self, patch: dict):
        self.client.request("ConfigSet", {"patch": patch}, lambda e: self._set_config(e["payload"].get("config", self.config)))

    def _refresh_status(self):
        if not self.client.is_connected:
            return

        def show(e):
            s = e["payload"]
            lim = tr("  限速中") if self.act_limiter.isChecked() else ""
            spd = human(s.get("speedBps"))
            self.status.setText(
                tr("活动 {a}  排队 {q}  完成 {c}  速度 {spd}/s{lim}").format(
                    a=s.get("active"), q=s.get("queued"), c=s.get("completed"), spd=spd, lim=lim
                )
            )
            if self.tray:
                self.tray.setToolTip(tr("LDM - {a} 个下载中，{spd}/s").format(a=s.get("active"), spd=spd))

        self.client.request("Status", {}, show)

    # ---------------------------------------------------------------- UI
    def _filter_changed(self, cur, _prev):
        d = cur.data(0, Qt.ItemDataRole.UserRole) if cur else None
        if not d:
            return
        cat, state, queue = d
        self.proxy.set_filter(cat, state, None, None if queue == "*" else queue)

    def _header_menu(self, pos):
        m = QMenu(self)
        for name, c in COL.items():
            a = m.addAction(name)
            a.setCheckable(True)
            a.setChecked(not self.table.isColumnHidden(c))
            a.setEnabled(name != "文件名")
            a.toggled.connect(lambda on, c=c: self._set_column(c, on))
        m.exec(self.table.horizontalHeader().mapToGlobal(pos))

    def _set_column(self, c: int, on: bool):
        self.table.setColumnHidden(c, not on)
        self.settings.setValue("hidden_columns", [n for n, i in COL.items() if self.table.isColumnHidden(i)])

    def selected(self) -> list[dict]:
        rows = {i.row() for i in self.table.selectionModel().selectedRows()}
        return [self.proxy.index(r, 0).data(Qt.ItemDataRole.UserRole) for r in sorted(rows)]

    def _each(self, typ: str):
        for j in self.selected():
            self.client.request(typ, {"id": j["id"]})

    def _fill_queue_menu(self, menu: QMenu, typ: str):
        menu.clear()
        for q in self.queues or [{"id": "main", "name": "主队列"}]:
            menu.addAction(
                q.get("name") or q["id"], lambda qid=q["id"]: self.client.request(typ, {"id": qid}, lambda _e: self.reload_queues())
            )

    def remove_selected(self):
        jobs = self.selected()
        if not jobs:
            return
        box = QMessageBox(QMessageBox.Icon.Question, "删除任务", f"删除 {len(jobs)} 个任务？", parent=self)
        keep = box.addButton("仅删除任务", QMessageBox.ButtonRole.AcceptRole)
        files = box.addButton("同时删除文件", QMessageBox.ButtonRole.DestructiveRole)
        box.addButton(QMessageBox.StandardButton.Cancel)
        box.exec()
        if box.clickedButton() not in (keep, files):
            return
        for j in jobs:
            self.client.request("JobRemove", {"id": j["id"], "deleteFiles": box.clickedButton() is files})

    def remove_completed(self, confirm: bool = True):
        n = sum(1 for j in self.model.jobs if j.get("state") == "Completed")
        if not n:
            return
        if (
            confirm
            and QMessageBox.question(self, "删除已完成", f"从列表中删除 {n} 个已完成的任务（文件保留）？") != QMessageBox.StandardButton.Yes
        ):
            return
        self.client.request("JobRemove", {"completed": True}, lambda _e: self.reload())

    def _show_details(self):
        jobs = self.selected()
        if not jobs:
            self.details.setPlainText("")
            return
        j = jobs[0]
        p = progress(j)
        lines = [
            f"文件: {job_name(j)}",
            f"URL: {j.get('url')}",
            f"状态: {j.get('state')}   进度: {'-' if p is None else f'{p:.1f}%'}   {human(j.get('got'))} / {human(j.get('total'))}",
            f"保存到: {j.get('final_path') or j.get('save_dir') or '(默认目录)'}",
            f"类型: {j.get('kind')}   连接数: {j.get('max_connections')}   队列: {j.get('queue_id')}   来源: {j.get('source')}",
            f"Referer: {j.get('referrer') or '-'}",
        ]
        y = j.get("ytdlp") or {}
        if y.get("bin_path"):
            src = {"user": "用户指定", "managed": "LDM 托管", "system": "系统 PATH"}.get(y.get("bin_source") or "", "?")
            lines.append(
                f"yt-dlp: {y.get('bin_version') or '?'}（{src}）{y['bin_path']}" + ("  已自动更新并重试" if y.get("update_retried") else "")
            )
        if j.get("description"):
            lines.append(f"描述: {j['description']}")
        if j.get("error"):
            lines.append(f"错误: {j.get('error_code')}: {j.get('error')}")
        self.details.setPlainText("\n".join(lines))

    def stop_recording_selected(self):
        """Live jobs: stop gracefully and keep what was recorded (删除 still kills everything)."""
        for j in self.selected():
            if is_recording(j):
                self.client.request("JobStopRecording", {"id": j["id"]})

    def _context_menu(self, pos):
        m = QMenu(self)
        jobs = self.selected()
        done = bool(jobs) and jobs[0].get("state") == "Completed"
        if done:
            m.addAction(self.act_open)
            m.addAction(self.act_folder)
        else:
            m.addAction(self.act_progress)
        m.addSeparator()
        if any(is_recording(j) for j in jobs):
            m.addAction(self.act_stop_rec)
        for a in (self.act_resume, self.act_pause, self.act_remove):
            m.addAction(a)
        mq = m.addMenu("移动到队列")
        for q in self.queues:
            mq.addAction(
                q.get("name") or q["id"],
                lambda qid=q["id"]: [self.client.request("JobSet", {"id": j["id"], "queue": qid}) for j in self.selected()],
            )
        m.addAction("复制 URL", lambda: QGuiApplication.clipboard().setText("\n".join(j["url"] for j in self.selected())))
        m.addSeparator()
        m.addAction(self.act_props)
        m.exec(self.table.viewport().mapToGlobal(pos))

    def activate_selected(self):
        jobs = self.selected()[:1]
        for j in jobs:
            if j.get("state") == "Completed":
                self.open_file()
            else:
                self.show_progress(j)

    def progress_selected(self):
        for j in self.selected()[:1]:
            self.show_progress(j)

    def open_file(self):
        for j in self.selected()[:1]:
            if j.get("final_path"):
                QDesktopServices.openUrl(QUrl.fromLocalFile(j["final_path"]))

    def open_folder(self):
        for j in self.selected()[:1]:
            if j.get("final_path"):
                QDesktopServices.openUrl(QUrl.fromLocalFile(os.path.dirname(j["final_path"])))

    def _send_adds(self, reqs: list[dict], interactive: bool = True):
        def cb(env):
            if env.get("type") == "Error":
                QMessageBox.warning(self, "添加失败", env["payload"].get("error", ""))
                return
            jid = env.get("payload", {}).get("jobId")
            if jid and interactive and not env["payload"].get("duplicate"):
                self._interactive.add(jid)

        for req in reqs:
            self.client.request("DownloadAdd", req, cb)

    def add_dialog(self, urls: list[str] | None = None):
        dlg = AddDialog(self, urls=urls, queues=self.queues)
        if dlg.exec():
            reqs = dlg.requests()
            self._send_adds(reqs, interactive=len(reqs) == 1)

    def batch_dialog(self):
        dlg = BatchDialog(self, queues=self.queues)
        if dlg.exec():
            self._send_adds(dlg.requests(), interactive=False)

    def grabber_dialog(self):
        dlg = GrabberDialog(self, default_exts=self.config.get("capture", {}).get("capture_exts"))
        if dlg.exec():
            self._send_adds(dlg.requests(), interactive=False)

    def properties_dialog(self):
        for j in self.selected()[:1]:
            dlg = PropertiesDialog(j, self)
            if dlg.exec() and dlg.patch():
                self.client.request("JobSet", {"id": j["id"], **dlg.patch()}, self._error_box)

    def _error_box(self, env):
        if env.get("type") == "Error":
            QMessageBox.warning(self, "LDM", env["payload"].get("error", ""))

    def scheduler_dialog(self):
        dlg = QueueDialog(self.client, self.queues, self.model.jobs, self)
        dlg.exec()
        self.reload_queues()

    def settings_dialog(self):
        labels = {k: lbl for k, (lbl, _d) in DEFAULT_SHORTCUTS.items()}
        dlg = SettingsDialog(
            self.config,
            self,
            shortcuts=self.shortcut_map(),
            shortcut_labels=labels,
            lang=str(self.settings.value("ui/lang", "auto") or "auto"),
        )
        if dlg.exec():
            self.patch_config(dlg.patch())
            for k, v in dlg.shortcuts().items():
                self.settings.setValue(f"shortcuts/{k}", v)
            if dlg.language_choice() != (self.settings.value("ui/lang", "auto") or "auto"):
                self.settings.setValue("ui/lang", dlg.language_choice())
                self.statusBar().showMessage(tr("界面语言将在重启 LDM 后生效"), 8000)
            self.apply_shortcuts()

    def toggle_limiter(self, on: bool):
        self.patch_config({"engine": {"enable_speed_limit": bool(on)}})

    def toggle_clipboard(self, on: bool):
        self.patch_config({"general": {"enable_clipboard": bool(on)}})

    def toggle_drop_target(self, on: bool):
        if self.drop_target is None:
            self.drop_target = DropTarget()
            self.drop_target.dropped.connect(self.add_dialog)
            geo = QGuiApplication.primaryScreen().availableGeometry()
            self.drop_target.move(geo.right() - 90, geo.bottom() - 120)
        self.drop_target.setVisible(on)

    def import_list(self, path: str | None = None):
        if path is None:
            path, _ = QFileDialog.getOpenFileName(self, "导入下载列表", "", "列表 (*.txt *.json *.csv);;所有文件 (*)")
        if not path:
            return
        with open(path, encoding="utf-8", errors="replace") as f:
            data = f.read()
        self.client.request("Import", {"data": data, "paused": True}, lambda e: (self._error_box(e), self.reload()))

    def export_list(self, path: str | None = None):
        if path is None:
            path, _ = QFileDialog.getSaveFileName(self, "导出下载列表", "ldm-downloads.json", "JSON (*.json);;纯文本 URL (*.txt)")
        if not path:
            return
        fmt = "txt" if path.endswith(".txt") else "json"

        def save(e):
            with open(path, "w", encoding="utf-8") as f:
                f.write(e["payload"].get("data", ""))

        self.client.request("Export", {"format": fmt}, save)

    def export_diag(self):
        path, _ = QFileDialog.getSaveFileName(self, "导出诊断包（已脱敏）", "ldm-diag.json", "JSON (*.json)")
        if not path:
            return
        exe = shutil.which("ldm") or "ldm"
        try:
            r = subprocess.run([exe, "diag", "-o", path], capture_output=True, text=True, timeout=30)
            msg = r.stdout.strip() or r.stderr.strip()
        except OSError as e:
            msg = f"无法运行 ldm：{e}"
        self.statusBar().showMessage(msg, 8000)

    def about(self):
        QMessageBox.about(self, "关于 LDM", f"LDM {__version__}\nLinux 下载管理器（多连接分段下载、浏览器接管、媒体嗅探）。")

    def _clipboard_changed(self):
        if not self.config.get("general", {}).get("enable_clipboard"):
            return
        text = QGuiApplication.clipboard().text()
        if text == self._last_clip:
            return
        self._last_clip = text
        exts = set(self.config.get("capture", {}).get("capture_exts", []))
        known = {j.get("url") for j in self.model.jobs}
        urls = [
            u
            for u in urls_in_text(text)
            if u.split("?")[0].rsplit(".", 1)[-1].lower() in exts and u not in self._clip_seen and u not in known
        ]
        self._clip_seen.update(urls)
        if urls:
            self.add_dialog(urls)

    # drag & drop URLs
    def dragEnterEvent(self, e):  # noqa: N802
        if e.mimeData().hasUrls() or e.mimeData().hasText():
            e.acceptProposedAction()

    def dropEvent(self, e):  # noqa: N802
        md = e.mimeData()
        urls = [u.toString() for u in md.urls() if u.scheme() in ("http", "https")] or urls_in_text(md.text())
        if urls:
            self.add_dialog(urls)


def ensure_daemon():
    from ldm_gui.ipc import socket_path

    if os.path.exists(socket_path()):
        return
    for exe in ("ldm",):
        try:
            subprocess.run([exe, "daemon", "start"], timeout=15, capture_output=True)
            return
        except (OSError, subprocess.TimeoutExpired):
            pass


def main(argv=None) -> int:
    ap = argparse.ArgumentParser(prog="ldm-gui", description="LDM 图形界面")
    ap.add_argument("--smoke", type=float, metavar="SECONDS", help="启动后运行 N 秒并输出自检 JSON（用于无头测试）")
    ap.add_argument("--screenshot", help="自检结束时保存窗口截图")
    ap.add_argument("--no-autostart", action="store_true", help="不自动启动 ldmd")
    ap.add_argument("--version", action="version", version=__version__)
    args = ap.parse_args(argv)
    app = QApplication.instance() or QApplication(sys.argv[:1])
    app.setApplicationName("LDM")
    i18n.install(app)
    app.setQuitOnLastWindowClosed(not QSystemTrayIcon.isSystemTrayAvailable())
    if not args.no_autostart and not args.smoke:
        ensure_daemon()
    client = DaemonClient()
    win = MainWindow(client)
    try:  # desktop integration: io.github.wzk4work.LDM.Daemon1 on the session bus (optional)
        from ldm_gui import dbus_service

        win._dbus = dbus_service.register(win)
    except Exception:  # no QtDBus / no bus
        win._dbus = None
    win.show()
    client.connect_daemon()
    if args.smoke:

        def finish():
            if args.screenshot:
                win.grab().save(args.screenshot)
            print(
                json.dumps(
                    {
                        "connected": client.is_connected,
                        "jobs": len(win.model.jobs),
                        "status": win.status.text(),
                        "platform": QGuiApplication.platformName(),
                    },
                    ensure_ascii=False,
                ),
                flush=True,
            )
            app.quit()

        QTimer.singleShot(int(args.smoke * 1000), finish)
    return app.exec()


if __name__ == "__main__":
    raise SystemExit(main())
