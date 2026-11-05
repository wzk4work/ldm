"""Batch download (numbered patterns), site grabber, floating drop target."""

from __future__ import annotations

import itertools
import re
import string
import threading
import urllib.parse
import urllib.request
from html.parser import HTMLParser

from PySide6.QtCore import QObject, QPoint, Qt, Signal
from PySide6.QtGui import QColor, QPainter
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFormLayout,
    QHBoxLayout,
    QHeaderView,
    QLabel,
    QLineEdit,
    QPlainTextEdit,
    QPushButton,
    QSpinBox,
    QTableWidget,
    QTableWidgetItem,
    QVBoxLayout,
    QWidget,
)

from ldm_gui.dialogs import urls_in_text
from ldm_gui.i18n import queue_label, tr

BRACKET = re.compile(r"\[(\d+)-(\d+)(?::(\d+))?\]|\[([a-zA-Z])-([a-zA-Z])\]")
MAX_BATCH = 10000


def _range_values(m: re.Match) -> list[str]:
    if m.group(1) is not None:
        a, b, step = m.group(1), m.group(2), int(m.group(3) or 1)
        width = len(a) if a.startswith("0") and len(a) > 1 else 0
        lo, hi = int(a), int(b)
        rng = range(lo, hi + 1, max(1, step)) if lo <= hi else range(lo, hi - 1, -max(1, step))
        return [str(n).zfill(width) for n in rng]
    a, b = m.group(4), m.group(5)
    alpha = string.ascii_lowercase if a.islower() else string.ascii_uppercase
    i, j = alpha.index(a), alpha.index(b.lower() if a.islower() else b.upper())
    return list(alpha[min(i, j) : max(i, j) + 1])


def expand_pattern(pattern: str, limit: int = MAX_BATCH) -> list[str]:
    """`http://h/img[001-120].jpg`, `…/part[a-f].bin`, several brackets → cartesian product (in order)."""
    pattern = pattern.strip()
    ms = list(BRACKET.finditer(pattern))
    if not ms:
        return [pattern] if pattern else []
    pieces, last = [], 0
    for m in ms:
        pieces.append(pattern[last : m.start()])
        last = m.end()
    tail = pattern[last:]
    out = []
    for combo in itertools.product(*[_range_values(m) for m in ms]):
        out.append("".join(p + v for p, v in zip(pieces, combo, strict=False)) + tail)
        if len(out) >= limit:
            break
    return out


def expand_wildcard(template: str, start, end, width: int = 0, letters: bool = False) -> list[str]:
    """Batch form: one `*` in the address replaced by a numeric (zero padded) or letter range."""
    if "*" not in template:
        return []
    if letters:
        vals = _range_values(BRACKET.fullmatch(f"[{start}-{end}]"))
    else:
        lo, hi = int(start), int(end)
        vals = [str(n).zfill(width) for n in range(lo, hi + 1)][:MAX_BATCH]
    return [template.replace("*", v, 1) for v in vals]


class BatchDialog(QDialog):
    def __init__(self, parent=None, queues: list[dict] | None = None):
        super().__init__(parent)
        self.setWindowTitle("批量下载")
        self.resize(620, 460)
        self.template = QLineEdit()
        self.template.setPlaceholderText("http://example.com/pic*.jpg   或   http://example.com/file[001-100].zip")
        self.mode = QComboBox()
        self.mode.addItems(["数字", "字母"])
        self.start = QLineEdit("1")
        self.end = QLineEdit("10")
        self.width = QSpinBox()
        self.width.setRange(0, 10)
        self.width.setSpecialValueText("不补零")
        self.preview = QPlainTextEdit()
        self.preview.setReadOnly(True)
        self.count = QLabel()
        self.queue = QComboBox()
        for q in queues or [{"id": "main", "name": "主队列"}]:
            self.queue.addItem(queue_label(q), q["id"])
        self.paused = QCheckBox("加入后暂停（稍后由队列启动）")
        for w in (self.template, self.start, self.end):
            w.textChanged.connect(self.refresh)
        self.width.valueChanged.connect(self.refresh)
        self.mode.currentIndexChanged.connect(self.refresh)
        rng = QHBoxLayout()
        for lab, w in (("从", self.start), ("到", self.end), ("位数", self.width)):
            rng.addWidget(QLabel(lab))
            rng.addWidget(w)
        form = QFormLayout()
        form.addRow("地址模板", self.template)
        form.addRow("通配符 * 类型", self.mode)
        form.addRow("范围", rng)
        form.addRow("队列", self.queue)
        form.addRow("", self.paused)
        bb = QDialogButtonBox(QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel)
        bb.accepted.connect(self.accept)
        bb.rejected.connect(self.reject)
        lay = QVBoxLayout(self)
        lay.addLayout(form)
        lay.addWidget(QLabel("预览："))
        lay.addWidget(self.preview)
        lay.addWidget(self.count)
        lay.addWidget(bb)

    def urls(self) -> list[str]:
        t = self.template.text().strip()
        try:
            if "*" in t:
                return expand_wildcard(
                    t, self.start.text().strip(), self.end.text().strip(), self.width.value(), self.mode.currentIndex() == 1
                )
            return expand_pattern(t)
        except (ValueError, AttributeError):
            return []

    def refresh(self):
        u = self.urls()
        self.preview.setPlainText("\n".join(u[:200]) + ("\n…" if len(u) > 200 else ""))
        self.count.setText(tr("共 {n} 个地址").format(n=len(u)))

    def requests(self) -> list[dict]:
        return [{"url": u, "queue": self.queue.currentData(), "paused": self.paused.isChecked(), "source": "batch"} for u in self.urls()]


# ------------------------------------------------------------------ grabber
class _Links(HTMLParser):
    def __init__(self):
        super().__init__()
        self.links: list[str] = []

    def handle_starttag(self, tag, attrs):
        for k, v in attrs:
            if v and k in ("href", "src", "data-src"):
                self.links.append(v)


def extract_links(html: str, base: str) -> list[str]:
    p = _Links()
    try:
        p.feed(html)
    except Exception:  # noqa: BLE001 - malformed pages still yield what was parsed
        pass
    out, seen = [], set()
    for raw in p.links + urls_in_text(html):
        u = urllib.parse.urljoin(base, raw.strip())
        u = urllib.parse.urldefrag(u)[0]
        if u.startswith(("http://", "https://")) and u not in seen:
            seen.add(u)
            out.append(u)
    return out


def ext_of(url: str) -> str:
    path = urllib.parse.urlparse(url).path
    name = path.rsplit("/", 1)[-1]
    return name.rsplit(".", 1)[-1].lower() if "." in name else ""


def grab(start: str, exts: set[str], depth: int = 1, same_host: bool = True, max_pages: int = 50, fetch=None, stop=None) -> list[dict]:
    """Breadth-first crawl from `start`; returns files whose extension is in `exts` with the page they were found on."""
    fetch = fetch or _fetch
    host = urllib.parse.urlparse(start).hostname
    seen_pages, found, frontier = set(), {}, [(start, 0)]
    pages = 0
    while frontier and pages < max_pages:
        if stop and stop():
            break
        page, d = frontier.pop(0)
        if page in seen_pages:
            continue
        seen_pages.add(page)
        try:
            html = fetch(page)
        except Exception:  # noqa: BLE001 - unreachable pages are skipped
            continue
        pages += 1
        for u in extract_links(html, page):
            e = ext_of(u)
            if same_host and urllib.parse.urlparse(u).hostname != host:
                continue
            if e in exts:
                found.setdefault(u, {"url": u, "page": page, "ext": e})
            elif d < depth and e in ("", "html", "htm", "php", "asp", "aspx", "shtml"):
                frontier.append((u, d + 1))
    return list(found.values())


def _fetch(url: str) -> str:
    req = urllib.request.Request(url, headers={"User-Agent": "LDM-Grabber/1.0"})
    with urllib.request.urlopen(req, timeout=15) as r:  # noqa: S310 - user supplied http(s) URL
        if "html" not in (r.headers.get("Content-Type") or "text/html"):
            return ""
        return r.read(4 << 20).decode(r.headers.get_content_charset() or "utf-8", "replace")


class _GrabSignals(QObject):
    done = Signal(list)


class GrabberDialog(QDialog):
    def __init__(self, parent=None, default_exts: list[str] | None = None):
        super().__init__(parent)
        self.setWindowTitle("站点抓取器")
        self.resize(760, 520)
        self.url = QLineEdit()
        self.url.setPlaceholderText("起始网页地址")
        self.exts = QLineEdit(" ".join(default_exts or ["zip", "rar", "7z", "pdf", "mp3", "mp4", "jpg", "png", "iso", "exe"]))
        self.depth = QSpinBox()
        self.depth.setRange(0, 5)
        self.depth.setValue(1)
        self.max_pages = QSpinBox()
        self.max_pages.setRange(1, 2000)
        self.max_pages.setValue(50)
        self.same = QCheckBox("只抓取同一站点")
        self.same.setChecked(True)
        self.go = QPushButton("开始抓取")
        self.go.clicked.connect(self.start)
        self.table = QTableWidget(0, 3)
        self.table.setHorizontalHeaderLabels(["文件", "类型", "所在页面"])
        self.table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        self.table.verticalHeader().hide()
        self.status = QLabel()
        sel_all = QPushButton("全选")
        sel_all.clicked.connect(lambda: self._check_all(True))
        sel_none = QPushButton("全不选")
        sel_none.clicked.connect(lambda: self._check_all(False))
        form = QFormLayout()
        form.addRow("起始地址", self.url)
        form.addRow("文件类型", self.exts)
        opts = QHBoxLayout()
        opts.addWidget(QLabel("深度"))
        opts.addWidget(self.depth)
        opts.addWidget(QLabel("最多页面"))
        opts.addWidget(self.max_pages)
        opts.addWidget(self.same)
        opts.addStretch(1)
        opts.addWidget(self.go)
        form.addRow(opts)
        bb = QDialogButtonBox()
        self.btn_add = bb.addButton("下载选中", QDialogButtonBox.ButtonRole.AcceptRole)
        bb.addButton(QDialogButtonBox.StandardButton.Close)
        bb.accepted.connect(self.accept)
        bb.rejected.connect(self.reject)
        row = QHBoxLayout()
        row.addWidget(sel_all)
        row.addWidget(sel_none)
        row.addWidget(self.status, 1)
        lay = QVBoxLayout(self)
        lay.addLayout(form)
        lay.addWidget(self.table)
        lay.addLayout(row)
        lay.addWidget(bb)
        self.sig = _GrabSignals()
        self.sig.done.connect(self.show_results)
        self._stop = False

    def start(self):
        url = self.url.text().strip()
        if not url.startswith(("http://", "https://")):
            self.status.setText(tr("请输入 http(s) 地址"))
            return
        exts = {e.strip(".").lower() for e in self.exts.text().split() if e.strip(".")}
        self.go.setEnabled(False)
        self.status.setText(tr("抓取中…"))
        args = (url, exts, self.depth.value(), self.same.isChecked(), self.max_pages.value())
        threading.Thread(target=lambda: self.sig.done.emit(grab(*args, stop=lambda: self._stop)), daemon=True).start()

    def show_results(self, items: list):
        self.go.setEnabled(True)
        self.table.setRowCount(len(items))
        for r, it in enumerate(items):
            c0 = QTableWidgetItem(it["url"])
            c0.setFlags(c0.flags() | Qt.ItemFlag.ItemIsUserCheckable)
            c0.setCheckState(Qt.CheckState.Checked)
            self.table.setItem(r, 0, c0)
            self.table.setItem(r, 1, QTableWidgetItem(it["ext"]))
            self.table.setItem(r, 2, QTableWidgetItem(it["page"]))
        self.status.setText(tr("找到 {n} 个文件").format(n=len(items)))

    def _check_all(self, on: bool):
        for r in range(self.table.rowCount()):
            self.table.item(r, 0).setCheckState(Qt.CheckState.Checked if on else Qt.CheckState.Unchecked)

    def requests(self) -> list[dict]:
        out = []
        for r in range(self.table.rowCount()):
            if self.table.item(r, 0).checkState() == Qt.CheckState.Checked:
                out.append({"url": self.table.item(r, 0).text(), "referrer": self.table.item(r, 2).text(), "source": "grabber"})
        return out

    def done(self, r):  # noqa: D401 - QDialog override
        self._stop = True
        super().done(r)


# ------------------------------------------------------------------ drop target
class DropTarget(QWidget):
    """Small always-on-top square; drop links or text with URLs on it to add downloads."""

    dropped = Signal(list)

    def __init__(self):
        super().__init__(None, Qt.WindowType.FramelessWindowHint | Qt.WindowType.WindowStaysOnTopHint | Qt.WindowType.Tool)
        self.setAcceptDrops(True)
        self.setFixedSize(56, 56)
        self.setToolTip("把链接拖到这里用 LDM 下载（右键关闭）")
        self._drag: QPoint | None = None
        self._hover = False

    def paintEvent(self, _e):  # noqa: N802
        p = QPainter(self)
        p.setRenderHint(QPainter.RenderHint.Antialiasing)
        p.setBrush(QColor("#0b57d0" if self._hover else "#1a73e8"))
        p.setPen(Qt.PenStyle.NoPen)
        p.drawRoundedRect(self.rect().adjusted(2, 2, -2, -2), 10, 10)
        p.setPen(QColor("white"))
        f = p.font()
        f.setBold(True)
        f.setPointSize(16)
        p.setFont(f)
        p.drawText(self.rect(), Qt.AlignmentFlag.AlignCenter, "↓")
        p.end()

    def dragEnterEvent(self, e):  # noqa: N802
        if e.mimeData().hasUrls() or e.mimeData().hasText():
            self._hover = True
            self.update()
            e.acceptProposedAction()

    def dragLeaveEvent(self, _e):  # noqa: N802
        self._hover = False
        self.update()

    def dropEvent(self, e):  # noqa: N802
        self._hover = False
        self.update()
        md = e.mimeData()
        urls = [u.toString() for u in md.urls() if u.scheme() in ("http", "https")] or urls_in_text(md.text())
        if urls:
            self.dropped.emit(urls)

    def mousePressEvent(self, e):  # noqa: N802
        if e.button() == Qt.MouseButton.RightButton:
            self.hide()
        else:
            self._drag = e.globalPosition().toPoint() - self.frameGeometry().topLeft()

    def mouseMoveEvent(self, e):  # noqa: N802
        if self._drag is not None:
            self.move(e.globalPosition().toPoint() - self._drag)

    def mouseReleaseEvent(self, _e):  # noqa: N802
        self._drag = None
