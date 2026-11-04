import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

import pytest
from PySide6.QtCore import QSettings
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
