"""Offscreen tests for the GUI windows (progress, capture info, completion, properties, scheduler, batch, grabber, options, main)."""

import json
import os

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

import pytest  # noqa: E402
from PySide6.QtCore import QObject, QSettings, Qt, Signal  # noqa: E402
from PySide6.QtWidgets import QApplication  # noqa: E402

from ldm_gui.capture import CompletionDialog, DownloadInfoDialog, PropertiesDialog, dir_for_category  # noqa: E402
from ldm_gui.dialogs import SettingsDialog  # noqa: E402
from ldm_gui.model import COL, JobFilter, JobModel, eta_seconds, human_time  # noqa: E402
from ldm_gui.progress import ProgressDialog, SegmentBar  # noqa: E402
from ldm_gui.scheduler import QueueDialog, queue_id_for  # noqa: E402
from ldm_gui.tools import BatchDialog, GrabberDialog, expand_pattern, expand_wildcard, extract_links, grab  # noqa: E402


@pytest.fixture(scope="module")
def app():
    return QApplication.instance() or QApplication([])


class FakeClient(QObject):
    connected = Signal()
    disconnected = Signal()
    event = Signal(str, dict)

    def __init__(self, replies=None):
        super().__init__()
        self.is_connected = True
        self.sent = []
        self.replies = replies or {}

    def request(self, typ, payload=None, cb=None):
        self.sent.append((typ, payload or {}))
        if cb and typ in self.replies:
            r = self.replies[typ]
            cb({"type": typ, "payload": r(payload) if callable(r) else r})
        return len(self.sent)

    def types(self):
        return [t for t, _ in self.sent]


JOB = {
    "id": "j1",
    "url": "http://h/big.iso",
    "state": "Running",
    "got": 600,
    "total": 1000,
    "speedBps": 100,
    "kind": "http",
    "category": "compressed",
    "etag": '"x"',
    "parts": [
        {"idx": 0, "start": 0, "end": 500, "got": 500, "state": "Done"},
        {"idx": 1, "start": 500, "end": 1000, "got": 100, "state": "Active"},
    ],
}


def test_model_columns_eta_and_tree_filters(app):
    assert eta_seconds(JOB) == 4.0 and human_time(4) == "4 秒" and human_time(3725) == "1 小时 02 分"
    m = JobModel()
    m.set_jobs(
        [
            dict(JOB, description="镜像", last_try_at=1_700_000_000_000, queue_id="main"),
            {"id": "k", "url": "http://h/a.pdf", "state": "Completed", "category": "documents", "queue_id": "night", "total": 5},
            {"id": "w", "url": "http://h/b.pdf", "state": "Paused", "category": "documents", "queue_id": "night", "awaiting_confirm": True},
        ]
    )
    assert m.index(0, COL["剩余时间"]).data() == "4 秒"
    assert m.index(0, COL["描述"]).data() == "镜像"
    assert m.index(0, COL["最后尝试"]).data().startswith("2023-")
    assert m.index(2, COL["状态"]).data() == "待确认"
    f = JobFilter()
    f.setSourceModel(m)
    f.set_filter("documents", "unfinished")
    assert f.rowCount() == 1
    f.set_filter("all", "finished")
    assert f.rowCount() == 1
    f.set_filter("all", None, None, "night")
    assert f.rowCount() == 2
    f.set_filter("all", None, "镜像")
    assert f.rowCount() == 1
    # numeric sort on size
    f.set_filter("all", None, "")
    f.sort(COL["大小"], Qt.SortOrder.DescendingOrder)
    assert f.index(0, 0).data() == "big.iso"


def test_progress_dialog_segments_limit_after(app):
    bar = SegmentBar()
    bar.set_data(1000, JOB["parts"])
    assert bar.spans() == [(0.0, 0.5, 0.5, "Done"), (0.5, 1.0, 0.6, "Active")]
    bar.grab()  # paints without error
    c = FakeClient({"JobGet": {"job": JOB}})
    d = ProgressDialog(c, {"id": "j1", "url": JOB["url"], "state": "Queued"})
    assert d.lbl["size"].text() == "1000 B" and "60.00%" in d.lbl["got"].text()
    assert d.lbl["resume"].text() == "是" and d.conn_table.rowCount() == 2
    assert d.conn_table.item(1, 3).text() == "接收数据" and d.windowTitle().startswith("60% big.iso")
    d.limit_on.setChecked(True)
    d.limit_kib.setValue(64)
    d.apply_limit()
    assert c.sent[-1] == ("JobSet", {"id": "j1", "speed_limit_bps": 65536})
    d.set_after("open")
    assert c.sent[-1] == ("JobSet", {"id": "j1", "after": "open"})
    d.toggle_pause()
    assert c.sent[-1] == ("JobPause", {"id": "j1"})
    c.event.emit("JobProgress", {"jobId": "j1", "got": 900, "total": 1000, "speedBps": 50, "state": "Running"})
    assert "90.00%" in d.lbl["got"].text()
    assert d.btn_stop_rec.isHidden()
    c.event.emit("JobProgress", {"jobId": "j1", "got": 950, "speedBps": 50, "state": "Running", "phase": "Recording"})
    assert not d.btn_stop_rec.isHidden() and d.lbl["state"].text() == "直播录制中"
    # live recording: elapsed time + bytes, no percent / ETA
    c.event.emit(
        "JobProgress", {"jobId": "j1", "got": 4096, "speedBps": 50, "state": "Recording", "phase": "Recording", "recordedSecs": 65.4}
    )
    assert "00:01:05" in d.lbl["got"].text() and "%" not in d.lbl["got"].text(), d.lbl["got"].text()
    assert d.lbl["eta"].text() == "-" and d.lbl["size"].text() == "未知"
    assert d.windowTitle().startswith("⏺ 00:01:05")
    d.stop_recording()
    assert c.sent[-1] == ("JobStopRecording", {"id": "j1"})
    assert d.lbl["state"].text() == "正在停止录制…" and not d.btn_stop_rec.isEnabled()
    d.close()


def test_download_info_dialog(app, tmp_path):
    cfg = {
        "paths": {"download_dir": str(tmp_path), "by_category": True, "categories": {"video": {"dir": str(tmp_path / "vids"), "exts": []}}}
    }
    assert dir_for_category(cfg, "music") == str(tmp_path / "Music")
    assert dir_for_category(cfg, "video") == str(tmp_path / "vids")
    job = {"id": "c1", "url": "http://h/x.zip", "category": "compressed", "total": 2048, "awaiting_confirm": True}
    queues = [{"id": "main", "name": "主队列"}, {"id": "night", "name": "夜间"}]
    d = DownloadInfoDialog(job, cfg, queues)
    assert d.dir.text() == str(tmp_path / "Compressed")
    d.cat.setCurrentIndex(d.cat.findData("video"))
    assert d.dir.text() == str(tmp_path / "vids"), "directory follows the category"
    d.name.setText("y.zip")
    d.desc.setText("说明")
    c = FakeClient()
    d.btn_start.click()
    d.apply(c)
    assert c.types() == ["JobSet", "JobResume"]
    p = c.sent[0][1]
    assert p["filename"] == "y.zip" and p["category"] == "video" and p["description"] == "说明" and p["awaiting_confirm"] is False
    d2 = DownloadInfoDialog(job, cfg, queues)
    d2.queue.setCurrentIndex(1)
    d2.btn_later.click()
    c2 = FakeClient()
    d2.apply(c2)
    assert c2.types() == ["JobSet"] and c2.sent[0][1]["queue"] == "night"
    # existing file: warning + numbered suggestion
    (tmp_path / "Compressed").mkdir()
    (tmp_path / "Compressed" / "x.zip").write_bytes(b"1")
    (tmp_path / "Compressed" / "x (1).zip").write_bytes(b"1")
    d4 = DownloadInfoDialog(job, cfg, queues)
    assert not d4.btn_rename.isHidden() and "已存在" in d4.exists_lbl.text()
    d4.use_numbered()
    assert d4.name.text() == "x (2).zip" and d4.btn_rename.isHidden()
    d3 = DownloadInfoDialog(job, cfg, queues)
    d3.btn_cancel.click()
    c3 = FakeClient()
    d3.apply(c3)
    assert c3.sent == [("JobRemove", {"id": "c1", "deleteFiles": True})]


def test_completion_and_properties(app):
    d = CompletionDialog({"id": "a", "url": "http://h/f.bin", "final_path": "/tmp/dl/f.bin", "total": 10})
    d.btn_folder.click()
    assert d.opened == "/tmp/dl"
    job = {"id": "a", "url": "http://h/f.bin", "state": "Paused", "filename": "f.bin", "max_connections": 8, "save_dir": "/x"}
    p = PropertiesDialog(job)
    assert p.patch() == {}
    p.url.setText("http://h2/f.bin")
    p.desc.setText("d")
    p.conns.setValue(4)
    p.mirrors.setText("http://m1/f.bin http://m2/f.bin")
    assert p.patch() == {"url": "http://h2/f.bin", "description": "d", "connections": 4, "mirrors": ["http://m1/f.bin", "http://m2/f.bin"]}
    running = PropertiesDialog(dict(job, state="Running"))
    assert not running.url.isEnabled()


def test_queue_dialog(app):
    assert queue_id_for("夜间 下载", {"queue"}) == "queue-2" and queue_id_for("Night", set()) == "night"
    jobs = [
        {"id": "a", "url": "http://h/a", "queue_id": "main", "state": "Queued", "priority": 0, "created_at": 1},
        {"id": "b", "url": "http://h/b", "queue_id": "main", "state": "Queued", "priority": 0, "created_at": 2},
    ]
    c = FakeClient()
    d = QueueDialog(c, [{"id": "main", "name": "主队列", "concurrent": 3, "enabled": True}], jobs)
    assert d.files.count() == 2
    d.files.setCurrentRow(1)
    d.move(-1)
    d.use_start.setChecked(True)
    d.days[5].setChecked(False)
    d.days[6].setChecked(False)
    d.on_done.setCurrentIndex(d.on_done.findData("command"))
    d.cmd.setText("touch '/tmp/a b'")
    d.apply()
    q = [p for t, p in c.sent if t == "QueueSet"][0]["queue"]
    assert q["schedule"] == {"start": "01:00", "days": [1, 2, 3, 4, 5]} and q["on_done_command"] == ["touch", "/tmp/a b"]
    prios = {p["id"]: p["priority"] for t, p in c.sent if t == "JobSet"}
    assert prios["b"] > prios["a"], "moved-up job gets the higher priority"
    d.new_queue("夜间")
    assert d.queues[-1]["id"] == "queue" and c.sent[-1][0] == "QueueSet"
    d.b_start.click()
    assert c.sent[-1] == ("QueueStart", {"id": "queue"})
    d.delete_queue(confirm=False)
    assert c.sent[-1] == ("QueueDelete", {"id": "queue"}) and len(d.queues) == 1


def test_batch_patterns(app):
    assert expand_pattern("http://h/f[08-11].zip") == [f"http://h/f{n}.zip" for n in ("08", "09", "10", "11")]
    assert expand_pattern("http://h/[a-c]/[1-2].jpg") == [
        "http://h/a/1.jpg",
        "http://h/a/2.jpg",
        "http://h/b/1.jpg",
        "http://h/b/2.jpg",
        "http://h/c/1.jpg",
        "http://h/c/2.jpg",
    ]
    assert expand_pattern("http://h/p[0-10:5].png") == ["http://h/p0.png", "http://h/p5.png", "http://h/p10.png"]
    assert expand_wildcard("http://h/pic*.jpg", 1, 3, width=3) == ["http://h/pic001.jpg", "http://h/pic002.jpg", "http://h/pic003.jpg"]
    assert expand_wildcard("http://h/*.bin", "x", "z", letters=True) == ["http://h/x.bin", "http://h/y.bin", "http://h/z.bin"]
    d = BatchDialog()
    d.template.setText("http://h/v*.mp4")
    d.start.setText("9")
    d.end.setText("10")
    assert [r["url"] for r in d.requests()] == ["http://h/v9.mp4", "http://h/v10.mp4"] and "共 2 个" in d.count.text()


def test_grabber(app):
    pages = {
        "http://s/": '<a href="a.zip">a</a> <a href="/sub/">sub</a> <img src="i.png"> <a href="http://other/x.zip">x</a>',
        "http://s/sub/": '<a href="../b.zip">b</a><a href="c.pdf#p2">c</a>',
    }
    assert extract_links(pages["http://s/sub/"], "http://s/sub/") == ["http://s/b.zip", "http://s/sub/c.pdf"]
    found = grab("http://s/", {"zip", "pdf"}, depth=1, fetch=lambda u: pages[u])
    assert sorted(f["url"] for f in found) == ["http://s/a.zip", "http://s/b.zip", "http://s/sub/c.pdf"]
    assert [f["url"] for f in grab("http://s/", {"zip"}, depth=0, fetch=lambda u: pages[u])] == ["http://s/a.zip"]
    assert len(grab("http://s/", {"zip"}, depth=0, same_host=False, fetch=lambda u: pages[u])) == 2
    g = GrabberDialog()
    g.show_results(found)
    g.table.item(0, 0).setCheckState(Qt.CheckState.Unchecked)
    reqs = g.requests()
    assert len(reqs) == 2 and reqs[0]["source"] == "grabber" and reqs[0]["referrer"].startswith("http://s/")


def test_options_dialog_tabs_and_patch(app):
    cfg = {
        "engine": {"max_connections_per_host": {"old.example": 2}, "conflict_policy": "rename_numbered"},
        "capture": {"exclude_hosts": ["bank.example"], "bypass_modifier": "alt", "force_modifier": "insert"},
        "sites": {
            "logins": [{"host": "*.corp", "user": "me", "password": "pw"}],
            "tls": [{"host": "ftp.corp", "ftp_explicit_tls": True, "accept_invalid_cert": False}],
        },
        "general": {"on_complete_command": ["notify-send", "完成 了"]},
    }
    s = SettingsDialog(cfg, shortcuts={"add": "Ctrl+N"}, shortcut_labels={"add": "添加 URL"})
    assert [s.tabs.tabText(i) for i in range(s.tabs.count())] == SettingsDialog.TABS
    s.per_host.setPlainText("cdn.example 4")
    s.cat_table.item(0, 1).setText("/data/videos")
    s.cat_table.item(5, 2).setText(".LOG")
    s.bypass.setCurrentIndex(s.bypass.findData("shift"))
    s.sounds.setChecked(True)
    s.proxy.setText("pac")
    s.pac_url.setText(" http://wpad.corp/proxy.pac ")
    s.pac_refresh.setValue(15)
    p = s.patch()
    assert p["engine"]["proxy"] == "pac" and p["engine"]["pac_url"] == "http://wpad.corp/proxy.pac"
    assert p["engine"]["pac_refresh_secs"] == 900
    assert p["engine"]["max_connections_per_host"] == {"cdn.example": 4, "old.example": None}
    assert p["paths"]["categories"]["video"] == {"dir": "/data/videos", "exts": []}
    assert p["paths"]["categories"]["general"] == {"dir": "", "exts": ["log"]} and p["paths"]["categories"]["music"] is None
    assert p["sites"]["logins"] == [{"host": "*.corp", "user": "me", "password": "pw"}]
    assert p["capture"]["bypass_modifier"] == "shift" and p["capture"]["exclude_hosts"] == ["bank.example"]
    assert p["general"]["sounds"] is True and p["general"]["on_complete_command"] == ["notify-send", "完成 了"]
    assert s.shortcuts() == {"add": "Ctrl+N"}
    assert p["ftp"] == {"tls": "off"}
    assert p["sites"]["tls"] == [{"host": "ftp.corp", "ftp_explicit_tls": True, "accept_invalid_cert": False}]
    s._add_tls_row("self-signed.lan", False, True)
    s.ftp_tls.setCurrentIndex(s.ftp_tls.findData("try"))
    p = s.patch()
    assert p["ftp"]["tls"] == "try"
    assert p["sites"]["tls"][1] == {"host": "self-signed.lan", "ftp_explicit_tls": False, "accept_invalid_cert": True}
    assert p["media"]["ad_ranking"] is True and p["media"]["ad_domains_file"] == ""
    s.ad_rank.setChecked(False)
    s.ad_file.setText(" ~/.config/ldm/ad-domains.txt ")
    assert s.patch()["media"]["ad_ranking"] is False and s.patch()["media"]["ad_domains_file"] == "~/.config/ldm/ad-domains.txt"


def test_download_info_dialog_marks_suspected_ads(app, tmp_path):
    cfg = {"paths": {"download_dir": str(tmp_path)}}
    d = DownloadInfoDialog({"id": "a1", "url": "http://h/pre.mp4", "category": "video", "ad_reasons": ["skip-control", "short:20s"]}, cfg)
    assert not d.ad_lbl.isHidden()
    assert d.ad_lbl.text() == "⚠ 疑似广告：播放时有“跳过广告”按钮；时长很短（20s）"
    assert d.btn_start.isEnabled(), "still downloadable"
    plain = DownloadInfoDialog({"id": "a2", "url": "http://h/v.mp4", "category": "video"}, cfg)
    assert plain.ad_lbl.isHidden()


def test_main_window_flows(app, tmp_path):
    from ldm_gui.app import MainWindow

    jobs = [
        {"id": "d1", "url": "http://h/done.zip", "state": "Completed", "category": "compressed", "queue_id": "main"},
        {"id": "w1", "url": "http://h/wait.zip", "state": "Paused", "awaiting_confirm": True, "category": "compressed", "queue_id": "main"},
    ]
    c = FakeClient(
        {
            "JobList": {"jobs": jobs},
            "QueueList": {"queues": [{"id": "main", "name": "主队列"}, {"id": "night", "name": "夜间"}]},
            "ConfigGet": {
                "config": {"general": {"show_complete_dialog": True, "enable_clipboard": True}, "capture": {"capture_exts": ["zip"]}}
            },
            "ConfigSet": lambda p: {"config": {"engine": p["patch"].get("engine", {}), "general": {"enable_clipboard": True}}},
            "DownloadAdd": lambda p: {"action": "take", "jobId": "n1"},
            "Export": {"format": "json", "data": "[1]"},
            "Status": {"active": 0, "queued": 0, "completed": 1, "speedBps": 0},
        }
    )
    settings = QSettings(str(tmp_path / "gui.ini"), QSettings.Format.IniFormat)
    w = MainWindow(c, settings)
    c.connected.emit()
    texts = [t for t, _ in w.tree_items()]
    for t in ("全部下载", "未完成", "已完成", "队列", "夜间", "视频"):
        assert t in texts
    assert [a.text() for a in w.menuBar().actions()] == ["任务(&T)", "文件(&F)", "下载(&D)", "查看(&V)", "帮助(&H)"]
    tb = [a.text() for a in w.toolbar.actions() if a.text()]
    for t in ("删除已完成", "计划任务", "抓取器", "选项"):
        assert t in tb
    assert "w1" in w.info_dialogs, "pending capture shown after connect"
    # capture event while running
    c.event.emit("CaptureConfirm", {"id": "c9", "url": "http://h/new.zip", "state": "Paused", "awaiting_confirm": True})
    dlg = w.info_dialogs["c9"]
    dlg.btn_start.click()
    assert c.sent[-2][0] == "JobSet" and c.sent[-1] == ("JobResume", {"id": "c9"})
    # becomes active → progress window (user pressed 开始下载)
    c.event.emit("JobStateChanged", {"id": "c9", "url": "http://h/new.zip", "state": "Running"})
    assert "c9" in w.progress_windows
    # completion dialog, "不再显示" writes config
    c.event.emit(
        "JobCompleted",
        {"id": "c9", "url": "http://h/new.zip", "state": "Completed", "final_path": str(tmp_path / "new.zip"), "source": "extension"},
    )
    w.last_completion.never.setChecked(True)
    w.last_completion.btn_close.click()
    assert ("ConfigSet", {"patch": {"general": {"show_complete_dialog": False}}}) in c.sent
    # per-job "after: folder" is handled by the GUI
    c.event.emit("JobCompleted", {"id": "x", "url": "http://h/x", "after": "folder", "final_path": "/tmp/q/x.bin"})
    assert w.last_after_open == "/tmp/q"
    # tray toggles
    w.act_limiter.trigger()
    assert c.sent[-1] == ("ConfigSet", {"patch": {"engine": {"enable_speed_limit": True}}})
    assert w.act_limiter.isChecked()
    # remove completed
    w.remove_completed(confirm=False)
    assert ("JobRemove", {"completed": True}) in c.sent
    # clipboard dedupe: known job URL and repeated URL are ignored
    w.config = {"general": {"enable_clipboard": True}, "capture": {"capture_exts": ["zip"]}}
    opened = []
    w.add_dialog = lambda urls=None: opened.append(urls)
    clip = QApplication.clipboard()
    clip.setText("http://h/done.zip http://h/fresh.zip")
    w._clipboard_changed()
    clip.setText("again http://h/fresh.zip")
    w._clipboard_changed()
    assert opened == [["http://h/fresh.zip"]]
    # shortcuts come from QSettings
    settings.setValue("shortcuts/grabber", "Ctrl+Alt+G")
    w.apply_shortcuts()
    assert w.act_grabber.shortcut().toString() == "Ctrl+Alt+G"
    # export
    out = tmp_path / "list.json"
    w.export_list(str(out))
    assert out.read_text() == "[1]"
    # hidden columns persist
    w._set_column(COL["描述"], False)
    assert "描述" in settings.value("hidden_columns")
    for d in list(w.progress_windows.values()):
        d.close()
    w.close()


FAKE_YTDLP = os.path.join(os.path.dirname(__file__), "../../crates/ldm-core/tests/fixtures/fake-yt-dlp")


def test_ytdlp_tab_status_rules_and_update(app, tmp_path):
    from ldm_gui import ytdlp

    cfg = {"ytdlp": {"path": FAKE_YTDLP, "site_rules": {"old.example": "off"}, "extra_args": ["--embed-metadata"]}}
    s = SettingsDialog(cfg)
    assert "yt-dlp" in SettingsDialog.TABS
    assert "2026.10.10" in s.yt_status.text()
    s.yt_rules.setPlainText("vimeo.com always\nbad line\nexample.org OFF")
    s.yt_cookies.setCurrentIndex(s.yt_cookies.findData("firefox"))
    s.yt_fallback.setChecked(False)
    p = s.patch()["ytdlp"]
    assert p["site_rules"] == {"vimeo.com": "always", "example.org": "off", "old.example": None}
    assert p["cookies_from_browser"] == "firefox" and p["auto_fallback"] is False and p["enabled"] is True
    assert p["extra_args"] == ["--embed-metadata"] and p["path"] == FAKE_YTDLP and p["max_height"] == 1080
    # missing binary → red warning
    s.yt_path.setText(str(tmp_path / "nope"))
    s.refresh_ytdlp()
    assert s.yt_status.text().startswith("未找到 yt-dlp")
    # outdated
    st = ytdlp.Status("/x/yt-dlp", "2025.01.01", ytdlp.version_age_days("2025.01.01"), "binary")
    assert st.outdated and "未更新" in st.text()[0]
    # update command: a release binary updates itself; a pip console script via pip
    big = tmp_path / "yt-dlp"
    big.write_bytes(b"#!/usr/bin/env python3\n" + b"PK" * 20000)
    big.chmod(0o755)
    assert ytdlp.update_command(str(big)) == [str(big), "-U"]
    small = tmp_path / "bin" / "yt-dlp"
    small.parent.mkdir()
    small.write_text("#!/usr/bin/python3\nfrom yt_dlp import main\nmain()\n")
    small.chmod(0o755)
    assert ytdlp.update_command(str(small))[:4] == ["python3", "-m", "pip", "install"]
    assert ytdlp.find_binary(str(small)) == str(small)


def test_ytdlp_update_dialog_streams_output(app):
    from PySide6.QtCore import QEventLoop, QTimer

    from ldm_gui import ytdlp

    d = ytdlp.UpdateDialog([FAKE_YTDLP, "-U"])
    loop = QEventLoop()
    d.proc.finished.connect(lambda *_: QTimer.singleShot(50, loop.quit))
    QTimer.singleShot(10000, loop.quit)
    d.start()
    loop.exec()
    assert d.exit_code == 0
    assert "up to date" in d.out.toPlainText()
    assert d.state.text().startswith("完成")


def test_job_list_shows_via_ytdlp(app):
    m = JobModel()
    m.set_jobs(
        [
            {"id": "a", "url": "https://youtu.be/x", "kind": "ytdlp", "state": "Running", "via": "yt-dlp"},
            {
                "id": "b",
                "url": "http://h/v.mp4",
                "kind": "ytdlp",
                "state": "Completed",
                "via": "yt-dlp",
                "ytdlp": {"fallback_from": "hls"},
                "fallbackReason": "HTTP 403",
            },
        ]
    )
    assert m.data(m.index(0, COL["类型"])) == "yt-dlp"
    assert m.data(m.index(1, COL["类型"])) == "HLS → yt-dlp"
    assert m.data(m.index(1, COL["描述"])) == "via yt-dlp（原因：HTTP 403）"


def _managed(root, tool, versions, current, previous=None):
    for v in versions:
        d = root / tool / v
        d.mkdir(parents=True)
        b = d / tool
        b.write_text(f"#!/bin/sh\necho {v}\n")
        b.chmod(0o755)
        (d / "meta.json").write_text(json.dumps({"tool": tool, "version": v, "checksum_source": "SHA2-256SUMS", "gpg": "verified"}))
    (root / tool / "current").symlink_to(current)
    if previous:
        (root / tool / "previous").symlink_to(previous)


def test_tools_lookup_order_and_tab(app, tmp_path, monkeypatch):
    from ldm_gui import ytdlp

    root = tmp_path / "tools"
    _managed(root, "yt-dlp", ["2026.09.01", "2026.10.10"], "2026.10.10", "2026.09.01")
    (root / "state.json").write_text(json.dumps({"tools": {"yt-dlp": {"latest": "2026.10.21", "last_check_ms": 1}}}))
    sysbin = tmp_path / "sys"
    sysbin.mkdir()
    for t in ("yt-dlp", "deno"):
        (sysbin / t).write_text("#!/bin/sh\n")
        (sysbin / t).chmod(0o755)
    monkeypatch.setenv("PATH", str(sysbin))
    monkeypatch.setenv("XDG_DATA_HOME", str(tmp_path / "xdg"))
    # default root follows $XDG_DATA_HOME (Flatpak: ~/.var/app/io.github.wzk4work.LDM/data)
    assert ytdlp.tools_root() == str(tmp_path / "xdg" / "ldm" / "tools")
    # user path → managed → PATH
    assert ytdlp.find("yt-dlp", FAKE_YTDLP, str(root)) == (FAKE_YTDLP, "user")
    managed = str(root / "yt-dlp" / "2026.10.10" / "yt-dlp")
    assert ytdlp.find("yt-dlp", "", str(root)) == (managed, "managed")
    assert ytdlp.find("deno", "", str(root)) == (str(sysbin / "deno"), "system")
    assert ytdlp.find("yt-dlp", str(tmp_path / "nope"), str(root)) == (None, None)
    assert ytdlp.install_kind(managed, str(root)) == "managed"
    monkeypatch.setenv("LDM_CLI", "/opt/ldm/ldm")
    assert ytdlp.update_command(managed, str(root)) == ["/opt/ldm/ldm", "tools", "update", "yt-dlp"]
    assert ytdlp.update_command(None, str(root)) == ["/opt/ldm/ldm", "tools", "install", "yt-dlp"]
    i = ytdlp.tools_info("yt-dlp", "", str(root))
    assert (i.current, i.previous, i.versions, i.latest) == ("2026.10.10", "2026.09.01", ["2026.10.10", "2026.09.01"], "2026.10.21")
    assert ytdlp.update_available(i) and i.meta["gpg"] == "verified"

    s = SettingsDialog({"ytdlp": {"tools_dir": str(root), "auto_update": "weekly"}})
    assert "工具" in SettingsDialog.TABS
    assert "LDM 托管" in s.yt_status.text() and "2026.10.10" in s.yt_status.text()
    t = s.tl_labels["yt-dlp"].text()
    assert managed in t and "可回滚到 2026.09.01" in t and "最新发布：2026.10.21" in t and "可更新" in t
    assert "系统 PATH" in s.tl_labels["deno"].text()
    assert s.tl_rollback.isEnabled()
    s.tl_auto_install.setCurrentIndex(s.tl_auto_install.findData("always"))
    p = s.patch()["ytdlp"]
    assert (p["auto_install"], p["auto_update"], p["deno_path"]) == ("always", "weekly", "")
    # nothing installed anywhere → "安装 yt-dlp"
    monkeypatch.setenv("PATH", str(tmp_path / "empty"))
    monkeypatch.setenv("HOME", str(tmp_path / "home"))
    s2 = SettingsDialog({"ytdlp": {"tools_dir": str(tmp_path / "none")}})
    assert s2.yt_update.text() == "安装 yt-dlp" and not s2.tl_rollback.isEnabled()
    assert "未找到" in s2.tl_labels["yt-dlp"].text()


def test_tools_consent_and_events(app, tmp_path):
    from ldm_gui.app import MainWindow

    c = FakeClient(
        {
            "JobList": {"jobs": []},
            "QueueList": {"queues": []},
            "ConfigGet": {"config": {}},
            "ConfigSet": lambda p: {"config": p["patch"]},
            "Status": {"active": 0, "queued": 0, "completed": 0, "speedBps": 0},
        }
    )
    w = MainWindow(c, QSettings(str(tmp_path / "gui.ini"), QSettings.Format.IniFormat))
    c.connected.emit()
    c.event.emit("ToolsNeeded", {"tool": "yt-dlp", "reason": "missing", "toolsDir": "/data/ldm/tools"})
    box = w.tools_prompt
    assert box.isVisible() and "/data/ldm/tools" in box.text()
    c.event.emit("ToolsNeeded", {"tool": "yt-dlp", "reason": "missing", "toolsDir": "/data/ldm/tools"})
    assert w.tools_prompt is box, "one prompt at a time"
    box.checkBox().setChecked(True)
    [b for b in box.buttons() if b.text() == "下载并安装"][0].click()
    assert ("ToolsAction", {"action": "install"}) in c.sent
    assert ("ConfigSet", {"patch": {"ytdlp": {"auto_install": "always"}}}) in c.sent
    # declining sends nothing
    n = len(c.sent)
    c.event.emit("ToolsNeeded", {"tool": "yt-dlp", "reason": "missing", "toolsDir": "/x"})
    [b for b in w.tools_prompt.buttons() if b.text() == "不用了"][0].click()
    assert all(t != "ToolsAction" for t, _ in c.sent[n:])
    c.event.emit("ToolsProgress", {"tool": "yt-dlp", "phase": "download", "got": 50, "total": 100})
    assert "yt-dlp download 50%" in w.statusBar().currentMessage()
    c.event.emit(
        "ToolsChanged", {"action": "install", "ok": True, "result": {"yt-dlp": {"version": "2026.10.21"}, "deno": {"error": "offline"}}}
    )
    m = w.statusBar().currentMessage()
    assert "yt-dlp 2026.10.21" in m and "deno: 失败（offline）" in m
    w.close()
