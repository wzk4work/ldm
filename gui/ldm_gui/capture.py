"""Download-info (browser capture), completion and properties dialogs."""

from __future__ import annotations

import os

from PySide6.QtCore import Qt, QUrl
from PySide6.QtGui import QDesktopServices
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QFormLayout,
    QHBoxLayout,
    QLabel,
    QLineEdit,
    QPushButton,
    QSpinBox,
    QVBoxLayout,
)

from ldm_gui.i18n import queue_label, tr
from ldm_gui.model import CATEGORIES, STATE_ZH, human, job_name

SUBDIR = {
    "video": "Video",
    "music": "Music",
    "compressed": "Compressed",
    "programs": "Programs",
    "documents": "Documents",
    "general": "General",
}


def expand(p: str) -> str:
    return os.path.expandvars(os.path.expanduser(p or ""))


def default_download_dir(cfg: dict) -> str:
    d = (cfg.get("paths") or {}).get("download_dir") or "~/Downloads"
    return expand(d)


def dir_for_category(cfg: dict, cat: str) -> str:
    """Mirror of PathsConfig::target_dir for a chosen category (per-category dir, else by_category subdir)."""
    paths = cfg.get("paths") or {}
    over = (paths.get("categories") or {}).get(cat) or {}
    if (over.get("dir") or "").strip():
        return expand(over["dir"].strip())
    base = default_download_dir(cfg)
    if paths.get("by_category", True):
        return os.path.join(base, SUBDIR.get(cat, "General"))
    return base


def _dir_row(edit: QLineEdit, parent) -> QHBoxLayout:
    b = QPushButton("浏览…")

    def pick():
        d = QFileDialog.getExistingDirectory(parent, "选择保存目录", edit.text())
        if d:
            edit.setText(d)

    b.clicked.connect(pick)
    row = QHBoxLayout()
    row.addWidget(edit)
    row.addWidget(b)
    return row


def numbered_name(directory: str, name: str) -> str:
    """First free `stem (n).ext` in directory (same rule as the daemon's rename_numbered)."""
    if not os.path.exists(os.path.join(directory, name)):
        return name
    stem, dot, ext = name.rpartition(".")
    if not dot or not stem:
        stem, ext = name, ""
    for n in range(1, 10000):
        cand = f"{stem} ({n})" + (f".{ext}" if ext else "")
        if not os.path.exists(os.path.join(directory, cand)):
            return cand
    return name


# reason codes of the browser's "main video vs ad" ranking (extension/src/shared/adscore.ts)
AD_REASON_TEXT = {
    "ad-domain": "广告域名",
    "ad-body": "内容是 VAST/VMAP 广告描述，不是视频列表",
    "not-a-playlist": "内容不是播放列表",
    "user-ad-domain": "自定义广告域名",
    "ad-subdomain": "广告子域名",
    "ad-token": "URL 含广告词",
    "progressive-beside-manifest": "页面另有 HLS/DASH 流",
    "other-host": "与主视频流不同主机",
    "short": "时长很短",
    "duration-mismatch": "时长与页面不符",
    "duration-match": "时长与页面一致",
    "early-short": "先于正片出现且很快结束",
    "main-player": "主播放器长时间播放",
    "skip-control": "播放时有“跳过广告”按钮",
    "skip-window": "出现在“跳过广告”期间",
    "ad-request": "紧随 VAST/IMA 广告请求",
    "manifest": "流媒体清单",
    "mse-main": "正在播放的 MSE 播放器使用此流",
    "alt-of": "同一视频的其他码率",
}


def describe_ad_reasons(reasons: list[str]) -> str:
    out = []
    for r in reasons:
        code, _, detail = r.partition(":")
        t = tr(AD_REASON_TEXT.get(code, code))
        if code == "alt-of":  # alt-of:<url of the main entry> → its file name
            detail = next((p for p in reversed(detail.split("?")[0].split("#")[0].split("/")) if p), detail)
        out.append(f"{t}{tr('（')}{detail}{tr('）')}" if detail else t)
    return tr("；").join(out)


class DownloadInfoDialog(QDialog):
    """Shown when the browser hands a download over: rename, category, location, description, start now / later / cancel."""

    START, LATER, CANCEL = "start", "later", "cancel"

    def __init__(self, job: dict, cfg: dict, queues: list[dict] | None = None, parent=None):
        super().__init__(parent)
        self.job, self.cfg = job, cfg
        self.choice = self.CANCEL
        self.setWindowTitle("下载文件信息")
        self.setWindowFlag(Qt.WindowType.WindowStaysOnTopHint, True)
        self.resize(560, 300)
        self.url = QLineEdit(job.get("url", ""))
        self.url.setReadOnly(True)
        self.cat = QComboBox()
        for cid, name in CATEGORIES[1:]:
            self.cat.addItem(name, cid)
        cur = job.get("category") or "general"
        self.cat.setCurrentIndex(max(0, self.cat.findData(cur)))
        self.dir = QLineEdit(job.get("save_dir") or dir_for_category(cfg, cur))
        self._auto_dir = self.dir.text()
        self.cat.currentIndexChanged.connect(self._cat_changed)
        self.name = QLineEdit(job_name(job))
        self.desc = QLineEdit(job.get("description") or "")
        self.desc.setPlaceholderText("可选：备注（显示在列表“描述”列）")
        self.queue = QComboBox()
        for q in queues or [{"id": "main", "name": "主队列"}]:
            self.queue.addItem(queue_label(q), q["id"])
        self.queue.setCurrentIndex(max(0, self.queue.findData(job.get("queue_id") or "main")))
        # file-exists check (conflict policy "ask")
        self.exists_lbl = QLabel()
        self.exists_lbl.setStyleSheet("color:#b35c00")
        self.btn_rename = QPushButton("改用编号名")
        self.btn_rename.clicked.connect(self.use_numbered)
        self.dir.textChanged.connect(self._check_exists)
        self.name.textChanged.connect(self._check_exists)
        size = job.get("total")
        info = QLabel(
            tr("大小：{size}    来源页：{page}").format(
                size=human(size) if size else tr("未知"), page=job.get("page_url") or job.get("referrer") or "-"
            )
        )
        info.setWordWrap(True)
        # the browser ranked this media as a suspected ad: say so, still downloadable
        self.ad_lbl = QLabel()
        reasons = job.get("ad_reasons") or []
        if reasons:
            self.ad_lbl.setText(tr("⚠ 疑似广告：") + describe_ad_reasons(reasons))
            self.ad_lbl.setStyleSheet("color:#80868b")
            self.ad_lbl.setWordWrap(True)
        self.ad_lbl.setVisible(bool(reasons))
        form = QFormLayout()
        form.addRow("地址", self.url)
        form.addRow("分类", self.cat)
        form.addRow("保存到", _dir_row(self.dir, self))
        form.addRow("文件名", self.name)
        ex = QHBoxLayout()
        ex.addWidget(self.exists_lbl, 1)
        ex.addWidget(self.btn_rename)
        form.addRow("", ex)
        form.addRow("描述", self.desc)
        form.addRow("队列（稍后下载）", self.queue)
        form.addRow("", self.ad_lbl)
        self.btn_start = QPushButton("开始下载")
        self.btn_start.setDefault(True)
        self.btn_later = QPushButton("稍后下载")
        self.btn_cancel = QPushButton("取消")
        self.btn_start.clicked.connect(lambda: self._done(self.START))
        self.btn_later.clicked.connect(lambda: self._done(self.LATER))
        self.btn_cancel.clicked.connect(lambda: self._done(self.CANCEL))
        btns = QHBoxLayout()
        btns.addStretch(1)
        for b in (self.btn_start, self.btn_later, self.btn_cancel):
            btns.addWidget(b)
        lay = QVBoxLayout(self)
        lay.addLayout(form)
        lay.addWidget(info)
        lay.addLayout(btns)
        self._check_exists()

    def _target(self) -> str:
        return os.path.join(os.path.expanduser(self.dir.text().strip()), self.name.text().strip())

    def _check_exists(self):
        exists = bool(self.name.text().strip()) and os.path.exists(self._target())
        policy = (self.cfg.get("engine") or {}).get("conflict_policy", "rename_numbered")
        if exists:
            self.exists_lbl.setText(
                tr("同名文件已存在，") + (tr("将被覆盖") if policy == "overwrite" else tr("下载时会自动编号；也可现在改名"))
            )
        self.exists_lbl.setVisible(exists)
        self.btn_rename.setVisible(exists)

    def use_numbered(self):
        self.name.setText(numbered_name(os.path.expanduser(self.dir.text().strip()), self.name.text().strip()))

    def _cat_changed(self):
        # only follow the category while the user has not typed a custom directory
        if self.dir.text() == self._auto_dir:
            self._auto_dir = dir_for_category(self.cfg, self.cat.currentData())
            self.dir.setText(self._auto_dir)

    def _done(self, choice: str):
        self.choice = choice
        (self.accept if choice != self.CANCEL else self.reject)()

    def patch(self) -> dict:
        p = {
            "category": self.cat.currentData(),
            "save_dir": self.dir.text().strip() or None,
            "description": self.desc.text().strip(),
            "awaiting_confirm": False,
        }
        name = self.name.text().strip()
        if name and name != job_name(self.job):
            p["filename"] = name
        if self.choice == self.LATER:
            p["queue"] = self.queue.currentData()
        return {k: v for k, v in p.items() if v is not None}

    def apply(self, client) -> None:
        """Send the user's choice to the daemon."""
        jid = self.job["id"]
        if self.choice == self.CANCEL:
            client.request("JobRemove", {"id": jid, "deleteFiles": True})
            return
        client.request("JobSet", {"id": jid, **self.patch()})
        if self.choice == self.START:
            client.request("JobResume", {"id": jid})


class CompletionDialog(QDialog):
    def __init__(self, job: dict, parent=None):
        super().__init__(parent)
        self.job = job
        self.setWindowTitle("下载完成")
        self.setAttribute(Qt.WidgetAttribute.WA_DeleteOnClose)
        path = job.get("final_path") or ""
        lab = QLabel(
            f"<b>{job_name(job)}</b><br>"
            + tr("大小：{size}<br>保存到：{path}<br>地址：{url}").format(
                size=human(job.get("total") or job.get("got")), path=path, url=job.get("url", "")
            )
        )
        lab.setWordWrap(True)
        lab.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        self.never = QCheckBox("以后不再显示此对话框")
        self.btn_open = QPushButton("打开")
        self.btn_folder = QPushButton("打开文件夹")
        self.btn_close = QPushButton("关闭")
        self.btn_open.clicked.connect(lambda: self._open(path))
        self.btn_folder.clicked.connect(lambda: self._open(os.path.dirname(path)))
        self.btn_close.clicked.connect(self.accept)
        self.btn_open.setEnabled(bool(path))
        self.btn_folder.setEnabled(bool(path))
        row = QHBoxLayout()
        row.addWidget(self.never)
        row.addStretch(1)
        for b in (self.btn_open, self.btn_folder, self.btn_close):
            row.addWidget(b)
        lay = QVBoxLayout(self)
        lay.addWidget(lab)
        lay.addLayout(row)
        self.opened: str | None = None

    def _open(self, p: str):
        self.opened = p
        if os.environ.get("QT_QPA_PLATFORM") != "offscreen":
            QDesktopServices.openUrl(QUrl.fromLocalFile(p))
        self.accept()


class PropertiesDialog(QDialog):
    """Edit an existing job: URL (idle only), file name, directory, description, referrer, connections, mirrors."""

    def __init__(self, job: dict, parent=None):
        super().__init__(parent)
        self.job = job
        self.setWindowTitle(tr("属性 - {name}").format(name=job_name(job)))
        self.resize(600, 340)
        idle = job.get("state") not in ("Probing", "Running", "Recording", "Assembling")
        self.url = QLineEdit(job.get("url", ""))
        self.url.setEnabled(idle and job.get("state") != "Completed")
        self.name = QLineEdit(job.get("filename") or job_name(job))
        self.dir = QLineEdit(job.get("save_dir") or (os.path.dirname(job["final_path"]) if job.get("final_path") else ""))
        self.desc = QLineEdit(job.get("description") or "")
        self.ref = QLineEdit(job.get("referrer") or "")
        self.conns = QSpinBox()
        self.conns.setRange(1, 32)
        self.conns.setValue(int(job.get("max_connections") or 8))
        self.mirrors = QLineEdit(" ".join(job.get("mirrors") or []))
        self.mirrors.setPlaceholderText("备用地址，空格分隔（主地址失败时依次尝试）")
        form = QFormLayout()
        form.addRow("地址", self.url)
        form.addRow("文件名", self.name)
        form.addRow("保存目录", _dir_row(self.dir, self))
        form.addRow("描述", self.desc)
        form.addRow("Referer", self.ref)
        form.addRow("连接数", self.conns)
        form.addRow("镜像", self.mirrors)
        st = job.get("state") or ""
        info = QLabel(
            tr("状态：{st}    大小：{size}    路径：{path}").format(
                st=tr(STATE_ZH.get(st, st)), size=human(job.get("total")), path=job.get("final_path") or "-"
            )
        )
        info.setWordWrap(True)
        bb = QDialogButtonBox(QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel)
        bb.accepted.connect(self.accept)
        bb.rejected.connect(self.reject)
        lay = QVBoxLayout(self)
        lay.addLayout(form)
        lay.addWidget(info)
        lay.addWidget(bb)

    def patch(self) -> dict:
        j, out = self.job, {}

        def diff(key, val, cur):
            if val != (cur or ""):
                out[key] = val

        if self.url.isEnabled():
            diff("url", self.url.text().strip(), j.get("url"))
        diff("filename", self.name.text().strip(), j.get("filename") or job_name(j))
        diff("save_dir", self.dir.text().strip(), j.get("save_dir") or (os.path.dirname(j["final_path"]) if j.get("final_path") else ""))
        diff("description", self.desc.text().strip(), j.get("description"))
        diff("referrer", self.ref.text().strip(), j.get("referrer"))
        if self.conns.value() != int(j.get("max_connections") or 8):
            out["connections"] = self.conns.value()
        m = self.mirrors.text().split()
        if m != (j.get("mirrors") or []):
            out["mirrors"] = m
        return out
