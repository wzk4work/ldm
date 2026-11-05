import os
import re

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

import pytest
from PySide6.QtCore import QAbstractListModel, QModelIndex, QSettings, Qt
from PySide6.QtWidgets import QApplication

from ldm_gui import i18n


@pytest.fixture
def english():
    app = QApplication.instance() or QApplication([])
    i18n.set_language("en")
    yield app
    i18n.set_language("zh")


def test_tr_lookup_and_suffix(english):
    assert i18n.tr("全部下载") == "All downloads"
    assert i18n.tr("队列名称：") == "Queue name:"
    assert i18n.tr("最大速度：") == "Max speed:"
    assert i18n._tr_count("视频 (3)") == "Video (3)"
    assert i18n.tr("big.iso") == "big.iso", "unknown text is left alone"
    i18n.set_language("zh")
    assert i18n.tr("全部下载") == "全部下载"


def test_main_window_and_dialogs_in_english(english, tmp_path):
    from test_dialogs import FakeClient

    from ldm_gui.app import MainWindow
    from ldm_gui.capture import DownloadInfoDialog
    from ldm_gui.dialogs import SettingsDialog

    w = MainWindow(FakeClient(), settings=QSettings(str(tmp_path / "s.ini"), QSettings.Format.IniFormat))
    i18n.translate_widget(w)
    menus = [a.text() for a in w.menuBar().actions()]
    assert menus == ["&Tasks", "&File", "&Downloads", "&View", "&Help"]

    from PySide6.QtCore import Qt

    heads = [w.table.model().headerData(i, Qt.Orientation.Horizontal) for i in range(3)]
    assert heads == ["File name", "Size", "Progress"]
    top = w.tree.topLevelItem(0).text(0)
    assert top.startswith("All downloads"), top
    d = DownloadInfoDialog({"id": "x", "url": "http://h/a.zip"}, {"paths": {"download_dir": str(tmp_path)}})
    i18n.translate_widget(d)
    assert d.windowTitle() == "Download File Info" and d.btn_later.text() == "Download later"
    s = SettingsDialog({})
    i18n.translate_widget(s)
    assert [s.tabs.tabText(i) for i in range(3)] == ["General", "File types", "Save to"]
    assert s.lang.itemText(2) == "English"


CJK = re.compile(r"[\u3000-\u303f\u3400-\u9fff\uff00-\uffef]")
ALLOWED = {"简体中文"}  # a language's own name stays in that language


def _texts(root):
    """Every user-visible string of a window: titles, labels, buttons, actions and menus, tabs, combo / list /
    table / tree items, model cells and headers, placeholders, tooltips, spin box affixes and line edit contents."""
    from PySide6.QtWidgets import (
        QAbstractButton,
        QAbstractItemView,
        QAbstractSpinBox,
        QComboBox,
        QGroupBox,
        QHeaderView,
        QLabel,
        QLineEdit,
        QListWidget,
        QMenu,
        QPlainTextEdit,
        QSpinBox,
        QStatusBar,
        QTabWidget,
        QTreeWidget,
        QTreeWidgetItemIterator,
        QWidget,
    )

    out = []

    def add(where, s):
        if s:
            out.append((where, str(s)))

    def actions(where, acts):
        for a in acts:
            add(where + "/action", a.text())
            add(where + "/action-tip", a.toolTip())
            if a.menu():
                add(where + "/menu", a.menu().title())
                actions(where + "/menu", a.menu().actions())

    for w in [root, *root.findChildren(QWidget)]:
        n = type(w).__name__
        if w.isWindow():
            add(n + "/title", w.windowTitle())
        actions(n, w.actions())
        add(n + "/tooltip", w.toolTip())
        if isinstance(w, (QAbstractButton, QLabel)):
            add(n, w.text())
        if isinstance(w, QMenu):
            add(n, w.title())
        if isinstance(w, QGroupBox):
            add(n, w.title())
        if isinstance(w, QAbstractSpinBox):
            add(n + "/special", w.specialValueText())
        if isinstance(w, QSpinBox):
            add(n + "/suffix", w.suffix())
            add(n + "/prefix", w.prefix())
        if isinstance(w, QLineEdit):
            add(n, w.text())
            add(n + "/placeholder", w.placeholderText())
        if isinstance(w, QPlainTextEdit):
            add(n, w.toPlainText())
            add(n + "/placeholder", w.placeholderText())
        if isinstance(w, QStatusBar):
            add(n, w.currentMessage())
        if isinstance(w, QTabWidget):
            for i in range(w.count()):
                add(n + "/tab", w.tabText(i))
        if isinstance(w, QComboBox):
            for i in range(w.count()):
                add(n + "/item", w.itemText(i))
        if isinstance(w, QListWidget):
            for i in range(w.count()):
                add(n + "/item", w.item(i).text())
        if isinstance(w, QTreeWidget):
            for c in range(w.headerItem().columnCount()):
                add(n + "/header", w.headerItem().text(c))
            it = QTreeWidgetItemIterator(w)
            while it.value():
                add(n + "/item", it.value().text(0))
                it += 1
        elif isinstance(w, QAbstractItemView) and not isinstance(w, QHeaderView) and w.model() is not None:
            m = w.model()
            cols = 1 if isinstance(m, QAbstractListModel) else m.columnCount(QModelIndex())
            for c in range(cols):
                add(n + "/header", m.headerData(c, Qt.Orientation.Horizontal))
                for r in range(m.rowCount(QModelIndex())):
                    add(n + "/cell", m.index(r, c).data())
                    add(n + "/cell-tip", m.index(r, c).data(Qt.ItemDataRole.ToolTipRole))
    return out


def _assert_english(root):
    bad = [(where, s) for where, s in _texts(root) if CJK.search(s) and s not in ALLOWED]
    assert not bad, "untranslated in English mode:\n" + "\n".join(f"  {w}: {s!r}" for w, s in bad)


RUNNING = {
    "id": "j1",
    "url": "http://example.com/big.iso",
    "state": "Running",
    "got": 600_000,
    "total": 1_000_000,
    "speedBps": 2000,
    "kind": "http",
    "category": "compressed",
    "queue_id": "main",
    "max_connections": 2,
    "source": "browser",
    "etag": '"x"',
    "description": "via yt-dlp",
    "parts": [
        {"idx": 0, "start": 0, "end": 500_000, "got": 500_000, "state": "Done"},
        {"idx": 1, "start": 500_000, "end": 1_000_000, "got": 100_000, "state": "Active"},
        {"idx": 2, "start": 900_000, "end": 1_000_000, "got": 0, "state": "Pending"},
    ],
}


def test_every_window_is_fully_english(english, tmp_path, monkeypatch):
    """Instantiate each window and dialog in English, show it (so the Translator runs), let dynamic updates
    happen, then scan every visible text for Chinese characters."""
    from test_dialogs import FakeClient

    from ldm_gui.app import DEFAULT_SHORTCUTS, MainWindow
    from ldm_gui.capture import CompletionDialog, DownloadInfoDialog, PropertiesDialog
    from ldm_gui.dialogs import AddDialog, SettingsDialog
    from ldm_gui.progress import ProgressDialog
    from ldm_gui.scheduler import QueueDialog
    from ldm_gui.tools import BatchDialog, GrabberDialog

    monkeypatch.setenv("XDG_DATA_HOME", str(tmp_path / "data"))
    monkeypatch.setenv("PATH", str(tmp_path / "nobin"))  # no system yt-dlp / deno: the "not found" texts
    translator = i18n.Translator(english)
    english.installEventFilter(translator)
    queues = [{"id": "main", "name": "主队列", "concurrent": 3, "enabled": True}, {"id": "night", "name": "Night"}]
    jobs = [
        RUNNING,
        {"id": "d1", "url": "http://example.com/done.zip", "state": "Completed", "category": "compressed", "queue_id": "main"},
        {"id": "q1", "url": "http://example.com/a.mp4", "state": "Queued", "category": "video", "queue_id": "night"},
        {"id": "y1", "url": "http://example.com/v", "state": "Failed", "error_code": "E_HTTP", "error": "403", "queue_id": "main"},
        {"id": "e1", "url": "http://example.com/long.bin", "state": "Running", "got": 1, "total": 10**12, "speedBps": 1},
    ]
    cfg = {"paths": {"download_dir": str(tmp_path)}, "engine": {"conflict_policy": "rename_numbered"}}
    c = FakeClient(
        {
            "JobList": {"jobs": jobs},
            "JobGet": {"job": RUNNING},
            "QueueList": {"queues": queues},
            "ConfigGet": {"config": cfg},
            "Status": {"active": 2, "queued": 1, "completed": 1, "speedBps": 2001},
        }
    )
    try:
        w = MainWindow(c, QSettings(str(tmp_path / "gui.ini"), QSettings.Format.IniFormat))
        w.show()
        c.connected.emit()
        w.table.selectRow(0)
        english.processEvents()
        _assert_english(w)
        for a in w.menuBar().actions():  # menus are translated whenever they open
            a.menu().popup(w.pos())
            a.menu().hide()
        _assert_english(w)

        p = ProgressDialog(c, dict(RUNNING))
        p.show()
        c.event.emit("JobProgress", {"jobId": "j1", "got": 700_000, "total": 1_000_000, "speedBps": 3000, "state": "Running"})
        p.toggle_details()
        p.toggle_details()
        _assert_english(p)
        p.toggle_pause()
        assert c.sent[-1] == ("JobPause", {"id": "j1"})
        c.event.emit("JobProgress", {"jobId": "j1", "got": 700_000, "state": "Paused"})
        assert p.btn_pause.text() == "Resume"
        p.toggle_pause()
        assert c.sent[-1] == ("JobResume", {"id": "j1"}), "pause / resume does not depend on the button's language"
        c.event.emit("JobProgress", {"jobId": "j1", "got": 4096, "state": "Recording", "phase": "Recording", "recordedSecs": 65})
        _assert_english(p)
        p.close()

        add = AddDialog(urls=["http://example.com/a.zip"], queues=queues)
        add.show()
        _assert_english(add)
        assert add.queue.itemText(0) == "Main queue (main)"

        s = SettingsDialog(cfg, shortcuts={"add": "Ctrl+N"}, shortcut_labels={k: v[0] for k, v in DEFAULT_SHORTCUTS.items()})
        s.show()
        s.refresh_ytdlp()
        for i in range(s.tabs.count()):
            s.tabs.setCurrentIndex(i)
        _assert_english(s)
        assert "Speed && proxy" in [s.tabs.tabText(i) for i in range(s.tabs.count())], "a literal & is not a mnemonic"

        q = QueueDialog(c, queues, jobs)
        q.show()
        _assert_english(q)
        assert q.name.text() == "Main queue" and [d.text() for d in q.days][:2] == ["Mon", "Tue"]
        assert q.collect()["name"] == "主队列", "the translated label is not saved as the default queue's name"

        info = DownloadInfoDialog(
            {
                "id": "c1",
                "url": "http://example.com/pre.mp4",
                "category": "video",
                "total": 2048,
                "ad_reasons": ["skip-control", "short:20s"],
            },
            cfg,
            queues,
        )
        info.show()
        (tmp_path / "pre.mp4").write_bytes(b"x")
        info.dir.setText(str(tmp_path))
        _assert_english(info)

        others = [
            CompletionDialog({"id": "a", "url": "http://example.com/f.bin", "final_path": str(tmp_path / "f.bin"), "total": 10}),
            PropertiesDialog(dict(RUNNING)),
            BatchDialog(queues=queues),
            GrabberDialog(),
        ]
        for d in others:
            d.show()
            _assert_english(d)
        others[2].template.setText("http://example.com/file[1-3].zip")
        others[3].start()
        others[3].show_results([])
        for d in others:
            _assert_english(d)

        # human-readable times and the other dynamic helpers
        from ldm_gui.model import human_time

        assert [human_time(5), human_time(305), human_time(3725), human_time(90000)] == ["5 s", "5 min 05 s", "1 h 02 min", "1 d 1 h"]
    finally:
        english.removeEventFilter(translator)
