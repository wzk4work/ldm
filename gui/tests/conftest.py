import gc
import os

import pytest

# the tests assert on the Chinese source strings; English is covered in test_i18n.py
os.environ["LDM_LANG"] = "zh"
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")


@pytest.fixture(autouse=True)
def _dispose_windows():
    """Delete every top-level window after each test, so no Python-owned widget outlives
    QApplication at interpreter exit (PySide6 can crash in that teardown order)."""
    yield
    from PySide6.QtWidgets import QApplication

    app = QApplication.instance()
    if app is None:
        return
    for w in app.topLevelWidgets():
        w.close()
        w.deleteLater()
    app.processEvents()
    gc.collect()
