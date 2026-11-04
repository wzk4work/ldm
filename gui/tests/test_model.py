"""Offscreen unit tests for the GUI model and dialogs."""

import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

import pytest  # noqa: E402
from PySide6.QtWidgets import QApplication  # noqa: E402

from ldm_gui.dialogs import AddDialog, SettingsDialog, urls_in_text  # noqa: E402
from ldm_gui.model import ACTIVE_STATES, JobFilter, JobModel, eta_seconds, human, is_recording, job_name, progress  # noqa: E402


@pytest.fixture(scope="module")
def app():
    return QApplication.instance() or QApplication([])


def jobs():
    return [
        {"id": "a1", "url": "http://h/v.mp4", "state": "Running", "got": 50, "total": 200, "category": "video", "kind": "http"},
        {"id": "b2", "url": "http://h/x.zip", "filename": "x.zip", "state": "Completed", "got": 10, "total": 10, "category": "compressed"},
        {"id": "c3", "url": "http://h/m.m3u8", "state": "Paused", "got": 0, "total": None, "category": "video", "kind": "hls"},
    ]


def test_helpers():
    assert human(0) == "0 B" and human(1536) == "1.5 KiB" and human(None) == "-"
    assert job_name({"url": "http://h/a/b.bin?x=1"}) == "b.bin"
    assert progress({"got": 50, "total": 200}) == 25.0
    assert progress({"state": "Completed", "total": None}) == 100.0
    assert urls_in_text("x https://a/b.zip, (http://c/d.mp4) https://a/b.zip") == ["https://a/b.zip", "http://c/d.mp4"]


def test_live_recording_row(app):
    m = JobModel()
    live = {"id": "r1", "url": "http://h/live.m3u8", "state": "Recording", "got": 0, "total": None, "kind": "hls", "speedBps": 9}
    m.set_jobs([live])
    assert "Recording" in ACTIVE_STATES and is_recording(live)
    m.progress_event({"jobId": "r1", "got": 3 * 1048576, "speedBps": 4096, "state": "Recording", "recordedSecs": 3725})
    assert m.index(0, 2).data() == "⏺ 01:02:05", "elapsed instead of percent"
    assert m.index(0, 1).data() == "3.0 MiB"
    assert progress(live) is None and eta_seconds({**live, "total": 10}) is None
    assert m.index(0, 5).data() == "录制中"


def test_model_updates_and_filter(app):
    m = JobModel()
    m.set_jobs(jobs())
    assert m.rowCount() == 3
    assert m.index(0, 2).data() == "25.0%"
    m.progress_event({"jobId": "a1", "got": 100, "total": 200, "speedBps": 2048, "state": "Running"})
    assert m.index(0, 2).data() == "50.0%" and m.index(0, 3).data() == "2.0 KiB/s"
    m.upsert({"id": "d4", "url": "http://h/n.pdf", "state": "Queued", "category": "documents"})
    assert m.rowCount() == 4
    m.upsert({"id": "b2", "state": "Removed"})
    assert m.rowCount() == 3 and "b2" not in m.index_of
    f = JobFilter()
    f.setSourceModel(m)
    f.set_filter("video")
    assert f.rowCount() == 2
    f.set_filter("all", "active")
    assert f.rowCount() == 1
    f.set_filter("all", None, "n.pdf")
    assert f.rowCount() == 1


def test_dialogs(app):
    d = AddDialog(urls=["http://h/a.zip", "http://h/b.zip"], queues=[{"id": "main", "name": "主队列"}])
    d.cookies.setText("s=1")
    reqs = d.requests()
    assert [r["url"] for r in reqs] == ["http://h/a.zip", "http://h/b.zip"]
    assert reqs[0]["cookies"] == "s=1" and reqs[0]["queue"] == "main" and reqs[0]["source"] == "gui"
    s = SettingsDialog({"engine": {"max_connections_default": 4, "speed_limit_bps": 102400}, "queue": {"global_max_active": 2}})
    p = s.patch()
    assert p["engine"]["max_connections_default"] == 4 and p["engine"]["speed_limit_bps"] == 102400
    assert p["queue"]["global_max_active"] == 2
