"""Job table model, categories and formatting helpers."""

from __future__ import annotations

from PySide6.QtCore import QAbstractTableModel, QModelIndex, QSortFilterProxyModel, Qt

from ldm_gui.i18n import tr

CATEGORIES = [
    ("all", "全部下载"),
    ("video", "视频"),
    ("music", "音乐"),
    ("compressed", "压缩包"),
    ("programs", "程序"),
    ("documents", "文档"),
    ("general", "其他"),
]
STATE_FILTERS = [
    ("active", "进行中"),
    ("Queued", "排队"),
    ("Paused", "已暂停"),
    ("Completed", "已完成"),
    ("Failed", "失败"),
]
STATE_ZH = {
    "Queued": "排队",
    "Probing": "探测",
    "Running": "下载中",
    "Recording": "录制中",
    "Assembling": "合并中",
    "Paused": "已暂停",
    "Completed": "完成",
    "Failed": "失败",
    "Canceled": "已取消",
}
COLUMNS = ["文件名", "大小", "进度", "速度", "剩余时间", "状态", "类型", "队列", "最后尝试", "描述", "添加时间"]
COL = {name: i for i, name in enumerate(COLUMNS)}
ACTIVE_STATES = ("Probing", "Running", "Recording", "Assembling")
SORT_ROLE = Qt.ItemDataRole.UserRole + 1


def human(n: float | None) -> str:
    if n is None:
        return "-"
    n = max(0.0, float(n))
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if n < 1024 or unit == "TiB":
            return f"{n:.0f} {unit}" if unit == "B" else f"{n:.1f} {unit}"
        n /= 1024
    return "-"


def job_name(j: dict) -> str:
    if j.get("filename"):
        return j["filename"]
    url = j.get("url", "")
    return url.split("?")[0].rstrip("/").split("/")[-1] or url


def eta_seconds(j: dict) -> float | None:
    total, got, spd = j.get("total"), j.get("got") or 0, j.get("speedBps") or 0
    if j.get("state") not in ACTIVE_STATES or is_recording(j) or not total or spd <= 0:
        return None
    return max(0.0, (total - got) / spd)


def human_time(sec: float | None) -> str:
    if sec is None:
        return ""
    sec = int(sec + 0.5)
    if sec < 60:
        return tr("{s} 秒").format(s=sec)
    if sec < 3600:
        return tr("{m} 分 {s:02d} 秒").format(m=sec // 60, s=sec % 60)
    if sec < 86400:
        return tr("{h} 小时 {m:02d} 分").format(h=sec // 3600, m=sec % 3600 // 60)
    return tr("{d} 天 {h} 小时").format(d=sec // 86400, h=sec % 86400 // 3600)


def fmt_ts(ts: int | None) -> str:
    import datetime

    return datetime.datetime.fromtimestamp(ts / 1000).strftime("%Y-%m-%d %H:%M") if ts else ""


def is_recording(j: dict) -> bool:
    """A live stream being recorded: elapsed time + bytes, no percent / ETA."""
    return j.get("state") == "Recording" or (j.get("phase") == "Recording" and j.get("state") in ACTIVE_STATES)


def hms(sec: float | None) -> str:
    s = int(max(0.0, float(sec or 0)))
    return f"{s // 3600:02d}:{s // 60 % 60:02d}:{s % 60:02d}"


def progress(j: dict) -> float | None:
    if is_recording(j):
        return None
    total = j.get("total")
    if not total:
        return 100.0 if j.get("state") == "Completed" else None
    return min(100.0, 100.0 * (j.get("got") or 0) / total)


class JobModel(QAbstractTableModel):
    def __init__(self, parent=None):
        super().__init__(parent)
        self.jobs: list[dict] = []
        self.index_of: dict[str, int] = {}

    def rowCount(self, parent=None):  # noqa: N802
        return 0 if parent is not None and parent.isValid() else len(self.jobs)

    def columnCount(self, parent=None):  # noqa: N802
        return len(COLUMNS)

    def headerData(self, section, orientation, role=Qt.ItemDataRole.DisplayRole):  # noqa: N802
        if orientation == Qt.Orientation.Horizontal and role == Qt.ItemDataRole.DisplayRole:
            return tr("队列（列）") if COLUMNS[section] == "队列" else tr(COLUMNS[section])
        return None

    def data(self, index, role=Qt.ItemDataRole.DisplayRole):
        if not index.isValid():
            return None
        j = self.jobs[index.row()]
        c = index.column()
        if role == Qt.ItemDataRole.UserRole:
            return j
        if role == Qt.ItemDataRole.ToolTipRole:
            return j.get("url")
        if role == SORT_ROLE:
            name = COLUMNS[c]
            num = {
                "大小": lambda: j.get("total") or j.get("got") or 0,
                "进度": lambda: progress(j) or 0,
                "速度": lambda: j.get("speedBps") or 0,
                "剩余时间": lambda: eta_seconds(j) or 0,
                "最后尝试": lambda: j.get("last_try_at") or 0,
                "添加时间": lambda: j.get("created_at") or 0,
            }.get(name)
            return float(num()) if num else self.data(index)
        if role != Qt.ItemDataRole.DisplayRole:
            return None
        name = COLUMNS[c]
        if name == "文件名":
            return job_name(j)
        if name == "大小":
            if is_recording(j):
                return human(j.get("got") or 0)
            return human(j.get("total") or (j.get("got") if j.get("state") == "Completed" else None))
        if name == "进度":
            if is_recording(j):
                return f"⏺ {hms(j.get('recordedSecs'))}"
            p = progress(j)
            return "-" if p is None else f"{p:.1f}%"
        if name == "速度":
            s = j.get("speedBps")
            return f"{human(s)}/s" if s and j.get("state") in ACTIVE_STATES else ""
        if name == "剩余时间":
            return human_time(eta_seconds(j))
        if name == "状态":
            st = tr(STATE_ZH.get(j.get("state", ""), j.get("state", "")))
            if j.get("awaiting_confirm"):
                st = tr("待确认")
            return f"{st} ({j['error_code']})" if j.get("error_code") else st
        if name == "类型":
            if j.get("kind") == "ytdlp":
                src = (j.get("ytdlp") or {}).get("fallback_from")
                return f"{src.upper()} → yt-dlp" if src else "yt-dlp"
            return (j.get("kind") or "http").upper()
        if name == "队列":
            return j.get("queue_id", "")
        if name == "最后尝试":
            return fmt_ts(j.get("last_try_at"))
        if name == "描述":
            if not j.get("description") and j.get("fallbackReason"):
                return tr("via yt-dlp（原因：{r}）").format(r=j["fallbackReason"])
            return j.get("description") or ""
        if name == "添加时间":
            return fmt_ts(j.get("created_at"))
        return None

    def set_jobs(self, jobs: list[dict]) -> None:
        self.beginResetModel()
        self.jobs = list(jobs)
        self.index_of = {j["id"]: i for i, j in enumerate(self.jobs)}
        self.endResetModel()

    def upsert(self, job: dict) -> None:
        jid = job.get("id")
        if not jid:
            return
        if job.get("state") == "Removed":
            if jid in self.index_of:
                row = self.index_of[jid]
                self.beginRemoveRows(QModelIndex(), row, row)
                self.jobs.pop(row)
                self.endRemoveRows()
                self.index_of = {j["id"]: i for i, j in enumerate(self.jobs)}
            return
        if jid in self.index_of:
            row = self.index_of[jid]
            self.jobs[row].update(job)
            self.dataChanged.emit(self.index(row, 0), self.index(row, len(COLUMNS) - 1))
        else:
            self.beginInsertRows(QModelIndex(), len(self.jobs), len(self.jobs))
            self.jobs.append(job)
            self.index_of[jid] = len(self.jobs) - 1
            self.endInsertRows()

    def progress_event(self, p: dict) -> None:
        jid = p.get("jobId")
        if jid in self.index_of:
            row = self.index_of[jid]
            j = self.jobs[row]
            j["got"] = p.get("got", j.get("got"))
            if p.get("total"):
                j["total"] = p["total"]
            j["speedBps"] = p.get("speedBps")
            j["state"] = p.get("state", j.get("state"))
            if p.get("recordedSecs") is not None:
                j["recordedSecs"] = p["recordedSecs"]
            self.dataChanged.emit(self.index(row, 1), self.index(row, COL["状态"]))


class JobFilter(QSortFilterProxyModel):
    """Tree selection → (category, state group, queue) filter (folder tree)."""

    def __init__(self, parent=None):
        super().__init__(parent)
        self.category = "all"
        self.state = None  # None | "active" | "unfinished" | "finished" | concrete state
        self.queue = None
        self.text = ""
        self.setSortRole(SORT_ROLE)

    def set_filter(self, category="all", state=None, text=None, queue=None):
        self.category, self.state, self.queue = category, state, queue
        if text is not None:
            self.text = text.lower()
        self.invalidate()

    def filterAcceptsRow(self, row, parent):  # noqa: N802
        j = self.sourceModel().jobs[row]
        st = j.get("state")
        if self.category != "all" and (j.get("category") or "general") != self.category:
            return False
        if self.queue and j.get("queue_id") != self.queue:
            return False
        if self.state == "active" and st not in ACTIVE_STATES:
            return False
        if self.state == "finished" and st != "Completed":
            return False
        if self.state == "unfinished" and st in ("Completed", "Canceled"):
            return False
        if self.state not in (None, "active", "finished", "unfinished") and st != self.state:
            return False
        if self.text and self.text not in (job_name(j) + " " + j.get("url", "") + " " + (j.get("description") or "")).lower():
            return False
        return True
