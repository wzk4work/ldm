"""Add-download and settings dialogs."""

from __future__ import annotations

import re
import shlex

from PySide6.QtCore import Qt
from PySide6.QtGui import QKeySequence
from PySide6.QtWidgets import (
    QCheckBox,
    QComboBox,
    QDialog,
    QDialogButtonBox,
    QFileDialog,
    QFormLayout,
    QHBoxLayout,
    QHeaderView,
    QKeySequenceEdit,
    QLabel,
    QLineEdit,
    QPlainTextEdit,
    QPushButton,
    QSpinBox,
    QTableWidget,
    QTableWidgetItem,
    QTabWidget,
    QVBoxLayout,
    QWidget,
)

from ldm_gui.i18n import queue_label, tr

URL_RE = re.compile(r"https?://[^\s\"'<>]+", re.I)


def urls_in_text(text: str) -> list[str]:
    seen, out = set(), []
    for m in URL_RE.findall(text or ""):
        u = m.rstrip("),.;")
        if u not in seen:
            seen.add(u)
            out.append(u)
    return out


def _split(cmd: str) -> list[str]:
    try:
        return shlex.split(cmd)
    except ValueError:
        return cmd.split()


class AddDialog(QDialog):
    def __init__(self, parent=None, urls: list[str] | None = None, queues: list[dict] | None = None):
        super().__init__(parent)
        self.setWindowTitle("添加下载")
        self.resize(560, 360)
        self.urls = QPlainTextEdit("\n".join(urls or []))
        self.urls.setPlaceholderText("每行一个 URL（http/https，支持 .m3u8 / .mpd）")
        self.dir = QLineEdit()
        self.dir.setPlaceholderText("默认：按分类保存到下载目录")
        browse = QPushButton("浏览…")
        browse.clicked.connect(self._browse)
        drow = QHBoxLayout()
        drow.addWidget(self.dir)
        drow.addWidget(browse)
        self.name = QLineEdit()
        self.name.setPlaceholderText("自动")
        self.queue = QComboBox()
        for q in queues or [{"id": "main", "name": "主队列"}]:
            self.queue.addItem(f"{queue_label(q)} ({q['id']})", q["id"])
        self.conns = QSpinBox()
        self.conns.setRange(0, 32)
        self.conns.setSpecialValueText("默认")
        self.referrer = QLineEdit()
        self.cookies = QLineEdit()
        self.cookies.setPlaceholderText("a=1; b=2（登录站点可填）")
        self.user = QLineEdit()
        self.user.setPlaceholderText("HTTP 基本认证 用户:密码（可选）")
        self.paused = QCheckBox("添加后暂停")
        form = QFormLayout()
        form.addRow("URL", self.urls)
        form.addRow("保存目录", drow)
        form.addRow("文件名", self.name)
        form.addRow("队列", self.queue)
        form.addRow("连接数", self.conns)
        form.addRow("Referer", self.referrer)
        form.addRow("Cookie", self.cookies)
        form.addRow("登录", self.user)
        form.addRow("", self.paused)
        bb = QDialogButtonBox(QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel)
        bb.accepted.connect(self.accept)
        bb.rejected.connect(self.reject)
        lay = QVBoxLayout(self)
        lay.addLayout(form)
        lay.addWidget(bb)

    def _browse(self):
        d = QFileDialog.getExistingDirectory(self, "选择保存目录")
        if d:
            self.dir.setText(d)

    def requests(self) -> list[dict]:
        out = []
        headers = []
        if self.user.text().strip():
            import base64

            headers.append({"name": "Authorization", "value": "Basic " + base64.b64encode(self.user.text().strip().encode()).decode()})
        urls = urls_in_text(self.urls.toPlainText())
        for u in urls:
            out.append(
                {
                    "url": u,
                    "save_dir": self.dir.text().strip() or None,
                    "filename_hint": (self.name.text().strip() or None) if len(urls) == 1 else None,
                    "queue": self.queue.currentData(),
                    "connections": self.conns.value() or None,
                    "referrer": self.referrer.text().strip() or None,
                    "cookies": self.cookies.text().strip() or None,
                    "headers_extra": headers,
                    "paused": self.paused.isChecked(),
                    "source": "gui",
                }
            )
        return out


CATEGORY_NAMES = [
    ("video", "视频"),
    ("music", "音乐"),
    ("compressed", "压缩包"),
    ("programs", "程序"),
    ("documents", "文档"),
    ("general", "其他"),
]
MODIFIERS = [("alt", "Alt"), ("shift", "Shift"), ("ctrl", "Ctrl"), ("insert", "Insert"), ("none", "不使用")]


class SettingsDialog(QDialog):
    """Options dialog. Each tab maps to a config.toml section; `patch()` returns a JSON merge-patch for ConfigSet."""

    TABS = ["常规", "文件类型", "保存位置", "连接", "限速与代理", "站点登录", "浏览器", "媒体", "yt-dlp", "工具", "快捷键"]

    def __init__(
        self,
        cfg: dict,
        parent=None,
        shortcuts: dict[str, str] | None = None,
        shortcut_labels: dict[str, str] | None = None,
        lang: str = "auto",
    ):
        super().__init__(parent)
        self.setWindowTitle("选项")
        self.resize(720, 520)
        self.cfg = cfg
        e, p, g, m, b, c, q, s = (cfg.get(k, {}) for k in ("engine", "paths", "general", "media", "browser", "capture", "queue", "sites"))
        self.tabs = QTabWidget()

        def spin(v, lo, hi, suffix=""):
            x = QSpinBox()
            x.setRange(lo, hi)
            x.setValue(int(v))
            if suffix:
                x.setSuffix(suffix)
            return x

        def check(v, text=""):
            x = QCheckBox(text)
            x.setChecked(bool(v))
            return x

        def combo(items, cur):
            x = QComboBox()
            for val, text in items:
                x.addItem(text, val)
            x.setCurrentIndex(max(0, x.findData(cur)))
            return x

        def add_tab(title, rows, extra=None):
            w = QWidget()
            lay = QVBoxLayout(w)
            f = QFormLayout()
            for label, widget in rows:
                f.addRow(label, widget)
            lay.addLayout(f)
            if extra is not None:
                lay.addWidget(extra)
            lay.addStretch(0 if extra is not None else 1)
            self.tabs.addTab(w, title)

        # 常规
        self.notify = check(g.get("notify_on_complete", True), "下载完成时显示桌面通知")
        self.clipboard = check(g.get("enable_clipboard", False), "监视剪贴板中的下载地址")
        self.confirm = check(g.get("confirm_browser_downloads", True), "浏览器接管时显示“下载文件信息”对话框")
        self.show_progress = check(g.get("show_progress_dialog", True), "开始下载时显示进度对话框")
        self.show_complete = check(g.get("show_complete_dialog", True), "下载完成后显示完成对话框")
        self.sounds = check(g.get("sounds", False), "完成/失败时播放提示音")
        self.log_cookies = check(g.get("log_cookies", False), "日志中记录 Cookie（调试用）")
        self.done_cmd = QLineEdit(shlex.join(g.get("on_complete_command", [])))
        self.done_cmd.setPlaceholderText("每个任务完成后运行（可用 {path}）")
        self.shutdown_cmd = QLineEdit(shlex.join(g.get("shutdown_command", ["systemctl", "poweroff"])))
        self.lang = QComboBox()
        for code, name in (("auto", "自动（跟随系统）"), ("zh", "简体中文"), ("en", "English")):
            self.lang.addItem(name, code)
        self.lang.setCurrentIndex(max(0, self.lang.findData(lang)))
        add_tab(
            "常规",
            [
                ("界面语言", self.lang),
                ("", self.notify),
                ("", self.clipboard),
                ("", self.confirm),
                ("", self.show_progress),
                ("", self.show_complete),
                ("", self.sounds),
                ("", self.log_cookies),
                ("完成后命令", self.done_cmd),
                ("关机命令", self.shutdown_cmd),
            ],
        )

        # 文件类型
        self.capture = check(c.get("enabled", True), "自动接管浏览器中以下类型的下载")
        self.exts = QPlainTextEdit(" ".join(c.get("capture_exts", [])))
        self.exts.setMaximumHeight(90)
        self.media_exts = QLineEdit(" ".join(c.get("media_exts", [])))
        self.min_size = spin(c.get("min_size", 0) // 1024, 0, 1 << 20, " KiB")
        self.exclude_hosts = QPlainTextEdit("\n".join(c.get("exclude_hosts", [])))
        self.exclude_hosts.setPlaceholderText("每行一个，支持 *.example.com")
        self.exclude_hosts.setMaximumHeight(80)
        self.exclude_urls = QPlainTextEdit("\n".join(c.get("exclude_url_patterns", [])))
        self.exclude_urls.setMaximumHeight(60)
        self.bypass = combo(MODIFIERS, c.get("bypass_modifier", "alt"))
        self.force = combo(MODIFIERS, c.get("force_modifier", "insert"))
        add_tab(
            "文件类型",
            [
                ("", self.capture),
                ("扩展名", self.exts),
                ("媒体扩展名", self.media_exts),
                ("最小文件大小", self.min_size),
                ("不接管的站点", self.exclude_hosts),
                ("不接管的地址模式", self.exclude_urls),
                ("按住此键点击：不接管", self.bypass),
                ("按住此键点击：强制接管", self.force),
            ],
        )

        # 保存位置
        self.dl_dir = QLineEdit(p.get("download_dir", ""))
        self.by_cat = check(p.get("by_category", True), "按分类保存到子目录（Video、Music…）")
        self.conflict = combo(
            [("rename_numbered", "自动编号重命名"), ("ask", "询问（在下载信息对话框中提示改名）"), ("overwrite", "覆盖")],
            e.get("conflict_policy", "rename_numbered"),
        )
        self.cat_table = QTableWidget(len(CATEGORY_NAMES), 3)
        self.cat_table.setHorizontalHeaderLabels(["分类", "保存目录（空=默认）", "附加扩展名"])
        self.cat_table.horizontalHeader().setSectionResizeMode(1, QHeaderView.ResizeMode.Stretch)
        self.cat_table.verticalHeader().hide()
        cats = p.get("categories") or {}
        for r, (cid, name) in enumerate(CATEGORY_NAMES):
            it = QTableWidgetItem(tr(name))
            it.setData(Qt.ItemDataRole.UserRole, cid)
            it.setFlags(it.flags() & ~Qt.ItemFlag.ItemIsEditable)
            self.cat_table.setItem(r, 0, it)
            o = cats.get(cid) or {}
            self.cat_table.setItem(r, 1, QTableWidgetItem(o.get("dir", "")))
            self.cat_table.setItem(r, 2, QTableWidgetItem(" ".join(o.get("exts", []))))
        add_tab("保存位置", [("默认下载目录", self.dl_dir), ("", self.by_cat), ("文件已存在时", self.conflict)], self.cat_table)

        # 连接
        self.conns = spin(e.get("max_connections_default", 8), 1, 32)
        self.global_conns = spin(e.get("max_connections_global", 32), 1, 256)
        self.min_part = spin(e.get("min_part_size", 262144) // 1024, 16, 1 << 20, " KiB")
        self.retries = spin(e.get("max_retries", 8), 0, 100)
        self.timeout = spin(e.get("timeout_secs", 30), 5, 600, " 秒")
        self.active = spin(q.get("global_max_active", 3), 1, 32)
        self.per_host = QPlainTextEdit("\n".join(f"{h} {n}" for h, n in (e.get("max_connections_per_host") or {}).items()))
        self.per_host.setPlaceholderText("每行：主机 连接数，例如 example.com 2")
        self.per_host.setMaximumHeight(90)
        add_tab(
            "连接",
            [
                ("每个任务连接数", self.conns),
                ("全局最大连接数", self.global_conns),
                ("最小分段", self.min_part),
                ("重试次数", self.retries),
                ("超时", self.timeout),
                ("同时下载任务数", self.active),
                ("按主机限制连接", self.per_host),
            ],
        )

        # 限速与代理
        self.limit_on = check(e.get("enable_speed_limit", False), "启用全局限速")
        self.limit = spin(e.get("speed_limit_bps", 0) // 1024, 0, 10_000_000, " KiB/s")
        self.proxy = QLineEdit(e.get("proxy", "system"))
        self.proxy.setPlaceholderText("system / none / browser（跟随浏览器）/ pac（自动配置脚本）/ http://host:port / socks5://host:port")
        self.pac_url = QLineEdit(e.get("pac_url", ""))
        self.pac_url.setPlaceholderText("代理为 pac 时使用：http(s)://…/proxy.pac、file:///… 或绝对路径")
        self.pac_refresh = spin(max(1, e.get("pac_refresh_secs", 1800) // 60), 1, 10080, " 分钟")
        self.ua = QLineEdit(e.get("user_agent", ""))
        add_tab(
            "限速与代理",
            [
                ("", self.limit_on),
                ("最大速度", self.limit),
                ("代理", self.proxy),
                ("PAC 脚本", self.pac_url),
                ("PAC 刷新间隔", self.pac_refresh),
                ("User-Agent", self.ua),
            ],
        )

        # 站点登录
        logins = s.get("logins") or []
        self.login_table = QTableWidget(len(logins), 3)
        self.login_table.setHorizontalHeaderLabels(["站点（支持 *）", "用户名", "密码"])
        self.login_table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        for r, lg in enumerate(logins):
            for col, key in enumerate(("host", "user", "password")):
                self.login_table.setItem(r, col, QTableWidgetItem(lg.get(key, "")))
        w = QWidget()
        lay = QVBoxLayout(w)
        lay.addWidget(QLabel("下载这些站点的文件时自动附带 HTTP 基本认证（保存在仅本人可读的 config.toml）。"))
        lay.addWidget(self.login_table)
        row = QHBoxLayout()
        b_add, b_del = QPushButton("添加"), QPushButton("删除")
        b_add.clicked.connect(lambda: self.login_table.insertRow(self.login_table.rowCount()))
        b_del.clicked.connect(lambda: self.login_table.removeRow(self.login_table.currentRow()))
        row.addWidget(b_add)
        row.addWidget(b_del)
        row.addStretch(1)
        lay.addLayout(row)
        # FTPS: global mode for ftp:// and per-site TLS options (certificate checks stay on unless opted in)
        self.ftp_tls = combo(
            [("off", "不加密（普通 FTP）"), ("try", "服务器支持时使用 TLS"), ("require", "必须使用 TLS（显式 FTPS）")],
            (cfg.get("ftp") or {}).get("tls", "off"),
        )
        f = QFormLayout()
        f.addRow("ftp:// 链接", self.ftp_tls)
        lay.addLayout(f)
        tls = s.get("tls") or []
        self.tls_table = QTableWidget(0, 3)
        self.tls_table.setHorizontalHeaderLabels(["站点（支持 *）", "强制 FTPS", "接受无效证书"])
        self.tls_table.horizontalHeader().setSectionResizeMode(0, QHeaderView.ResizeMode.Stretch)
        for t in tls:
            self._add_tls_row(t.get("host", ""), t.get("ftp_explicit_tls", False), t.get("accept_invalid_cert", False))
        lay.addWidget(QLabel("“接受无效证书”会关闭该站点的证书校验，仅在信任该服务器时开启。"))
        lay.addWidget(self.tls_table)
        row = QHBoxLayout()
        t_add, t_del = QPushButton("添加"), QPushButton("删除")
        t_add.clicked.connect(lambda: self._add_tls_row("", False, False))
        t_del.clicked.connect(lambda: self.tls_table.removeRow(self.tls_table.currentRow()))
        row.addWidget(t_add)
        row.addWidget(t_del)
        row.addStretch(1)
        lay.addLayout(row)
        self.tabs.addTab(w, "站点登录")

        # 浏览器
        self.ws = check(b.get("ws_bridge_enabled", False), "启用本机 WebSocket 桥")
        self.ws_port = spin(b.get("ws_bridge_port", 17001), 1024, 65535)
        add_tab("浏览器", [("", self.ws), ("WS 端口", self.ws_port)])

        # 媒体
        self.height = spin(m.get("hls_max_height", 1080), 144, 4320, " p")
        self.seg_par = spin(m.get("segment_parallel", 4), 1, 16)
        self.keep = check(m.get("keep_segments_on_failure", True), "失败时保留已下载分片")
        self.ffmpeg = QLineEdit(m.get("ffmpeg_path", "ffmpeg"))
        self.container = combo([("mp4", "MP4"), ("mkv", "MKV"), ("ts", "TS")], m.get("output_container", "mp4"))
        # aac (default) | most_channels | language:xx typed as free text via "自定义…"
        ap = m.get("audio_preference", "aac") or "aac"
        self.audio_pref = combo(
            [
                ("aac", "优先 AAC（默认，兼容最好）"),
                ("most_channels", "声道数最多（如 5.1）"),
                ("language:en", "优先英语 (language:en)"),
                ("language:zh", "优先中文 (language:zh)"),
            ],
            ap if ap in ("aac", "most_channels", "language:en", "language:zh") else "aac",
        )
        if ap not in ("aac", "most_channels", "language:en", "language:zh"):
            # keep custom value (e.g. language:ja) editable via a line next to the combo
            self.audio_pref.addItem(ap, ap)
            self.audio_pref.setCurrentIndex(self.audio_pref.count() - 1)
        self.ad_rank = check(m.get("ad_ranking", True), "按“主视频 / 疑似广告”排序检测到的媒体（广告置灰放最后，不隐藏）")
        self.ad_file = QLineEdit(m.get("ad_domains_file", "") or "")
        self.ad_file.setPlaceholderText("可选：每行一个域名，支持 # 注释和 hosts 格式")
        add_tab(
            "媒体",
            [
                ("默认最高清晰度", self.height),
                ("", self.ad_rank),
                ("广告域名列表文件", self.ad_file),
                ("音轨偏好", self.audio_pref),
                ("分片并发", self.seg_par),
                ("", self.keep),
                ("ffmpeg 路径", self.ffmpeg),
                ("输出容器", self.container),
            ],
        )

        # yt-dlp (never bundled: your own copy, LDM's managed download, or the system one)
        y = cfg.get("ytdlp", {})
        self.yt_enabled = check(y.get("enabled", True), "启用 yt-dlp（YouTube 下载；以及 LDM 失败时的回退）")
        self.yt_path = QLineEdit(y.get("path", "") or "")
        self.yt_path.setPlaceholderText("留空 = 自动查找（LDM 托管版本 → PATH → ~/.local/bin/yt-dlp）")
        self.yt_fallback = check(y.get("auto_fallback", True), "LDM 下载失败（403/410、非媒体内容、清单解析失败）时自动用 yt-dlp 重试一次")
        self.yt_cookies = combo(
            [
                ("", "不使用（默认）"),
                ("firefox", "Firefox"),
                ("chrome", "Chrome"),
                ("chromium", "Chromium"),
                ("brave", "Brave"),
                ("edge", "Edge"),
            ],
            y.get("cookies_from_browser", "") or "",
        )
        self.yt_playlist = check(y.get("playlist", False), "播放列表链接：把每个视频加入队列（默认只下载当前视频）")
        self.yt_height = spin(y.get("max_height", 1080), 144, 4320, " p")
        self.yt_rules = QPlainTextEdit("\n".join(f"{d} {r}" for d, r in (y.get("site_rules") or {}).items()))
        self.yt_rules.setPlaceholderText("每行：域名 on|off|always\n例如：vimeo.com always\nexample.org off")
        self.yt_rules.setMaximumHeight(90)
        self.yt_args = QLineEdit(shlex.join(y.get("extra_args", []) or []))
        self.yt_args.setPlaceholderText("附加参数，例如 --embed-metadata")
        self.yt_status = QLabel()
        self.yt_status.setWordWrap(True)
        self.yt_update = QPushButton("更新 yt-dlp")
        self.yt_update.clicked.connect(self._update_ytdlp)
        self.yt_path.editingFinished.connect(self.refresh_ytdlp)
        add_tab(
            "yt-dlp",
            [
                ("状态", self.yt_status),
                ("", self.yt_update),
                ("", self.yt_enabled),
                ("yt-dlp 路径", self.yt_path),
                ("", self.yt_fallback),
                ("默认最高清晰度", self.yt_height),
                ("从浏览器读取 Cookie", self.yt_cookies),
                ("", self.yt_playlist),
                ("站点规则", self.yt_rules),
                ("附加参数", self.yt_args),
            ],
        )
        # 工具: LDM-managed yt-dlp / deno (official GitHub releases, SHA-256 verified, data dir)
        from . import ytdlp as _yt

        self.tools_dir = y.get("tools_dir", "") or ""
        self.tl_root = QLabel(_yt.tools_root(self.tools_dir))
        self.tl_root.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
        self.tl_auto_install = combo(
            [("ask", "首次需要时询问（默认）"), ("always", "自动下载安装"), ("never", "从不（只用我自己的 yt-dlp）")],
            y.get("auto_install", "ask") or "ask",
        )
        self.tl_auto_update = combo(
            [("daily", "每天检查（默认）"), ("weekly", "每周检查"), ("off", "关闭")], y.get("auto_update", "daily") or "daily"
        )
        self.deno_path = QLineEdit(y.get("deno_path", "") or "")
        self.deno_path.setPlaceholderText("留空 = 自动查找（LDM 托管版本 → PATH → ~/.deno/bin/deno）")
        self.tl_labels: dict[str, QLabel] = {}
        rows = [("托管目录", self.tl_root)]
        for t in _yt.TOOLS:
            lbl = QLabel()
            lbl.setWordWrap(True)
            lbl.setTextInteractionFlags(Qt.TextInteractionFlag.TextSelectableByMouse)
            self.tl_labels[t] = lbl
            rows.append((t, lbl))
        btns = QWidget()
        bl = QHBoxLayout(btns)
        bl.setContentsMargins(0, 0, 0, 0)
        self.tl_install = QPushButton("安装 / 修复")
        self.tl_update = QPushButton("立即更新")
        self.tl_check = QPushButton("检查更新")
        self.tl_rollback = QPushButton("回滚 yt-dlp")
        for b_, args in (
            (self.tl_install, ["install"]),
            (self.tl_update, ["update"]),
            (self.tl_check, ["check"]),
            (self.tl_rollback, ["rollback", "yt-dlp"]),
        ):
            b_.clicked.connect(lambda _c=False, a=args: self.run_tools(a))
            bl.addWidget(b_)
        bl.addStretch(1)
        rows += [
            ("", btns),
            ("首次使用", self.tl_auto_install),
            ("自动更新", self.tl_auto_update),
            ("deno 路径", self.deno_path),
        ]
        note = QLabel(
            "LDM 不内置 yt-dlp / deno：需要时从 GitHub 官方发布页下载到上面的目录，"
            "yt-dlp 用 SHA2-256SUMS 校验（装有 gpg 时再验证发布签名），deno 用发布页的 .sha256sum 校验；"
            "旧版本保留一份以便回滚，正在下载中的任务不会被替换。查找顺序：自己填写的路径 → LDM 托管 → 系统 PATH。"
        )
        note.setWordWrap(True)
        add_tab("工具", rows, note)
        self.refresh_ytdlp()

        # 快捷键 (GUI-local, stored by the main window in QSettings)
        self.shortcut_edits: dict[str, QKeySequenceEdit] = {}
        sk = QWidget()
        sf = QFormLayout(sk)
        for key, label in (shortcut_labels or {}).items():
            ed = QKeySequenceEdit(QKeySequence((shortcuts or {}).get(key, "")))
            self.shortcut_edits[key] = ed
            sf.addRow(label, ed)
        self.tabs.addTab(sk, "快捷键")

        bb = QDialogButtonBox(QDialogButtonBox.StandardButton.Ok | QDialogButtonBox.StandardButton.Cancel)
        bb.accepted.connect(self.accept)
        bb.rejected.connect(self.reject)
        lay = QVBoxLayout(self)
        lay.addWidget(self.tabs)
        lay.addWidget(bb)

    def refresh_ytdlp(self) -> None:
        from . import ytdlp

        st = ytdlp.status(self.yt_path.text(), tools_dir=self.tools_dir)
        text, ok = st.text(self.yt_enabled.isChecked())
        self.yt_status.setText(text)
        self.yt_status.setStyleSheet("" if ok else "color: #c5221f;")
        self.yt_update.setText(tr("更新 yt-dlp") if st.path else tr("安装 yt-dlp"))
        self.yt_state = st
        self.refresh_tools()

    def refresh_tools(self) -> None:
        from . import ytdlp

        for t, lbl in self.tl_labels.items():
            i = ytdlp.tools_info(t, self.yt_path.text() if t == "yt-dlp" else self.deno_path.text(), self.tools_dir)
            if i.path:
                line = f"{i.path}{tr('（')}{tr(ytdlp.SOURCE_LABELS.get(i.source or '', '?'))}{tr('）')}"
            else:
                line = tr("未找到") + (tr("（YouTube 需要）") if t == "yt-dlp" else tr("（YouTube 的 JS 验证需要，可选）"))
            if i.versions:
                line += "\n" + tr("托管版本：{vs}；当前 {cur}，可回滚到 {prev}").format(
                    vs=", ".join(i.versions), cur=i.current or "-", prev=i.previous or "-"
                )
            if i.meta:
                line += "\n" + tr("校验：SHA-256（{src}）；GPG：{gpg}").format(
                    src=i.meta.get("checksum_source", "?"), gpg=i.meta.get("gpg", "-")
                )
            if i.latest:
                line += "\n" + tr("最新发布：{v}").format(v=i.latest) + (tr("  ⬆ 可更新") if ytdlp.update_available(i) else "")
            if i.last_error:
                line += "\n" + tr("上次检查失败：{e}").format(e=i.last_error)
            lbl.setText(line)
            if t == "yt-dlp":
                self.tl_rollback.setEnabled(bool(i.previous))

    def run_tools(self, args: list[str]) -> None:
        """`ldm tools …` with live output (goes through ldmd when it runs)."""
        from . import ytdlp

        d = ytdlp.UpdateDialog([ytdlp.ldm_cli(), "tools", *args], self, title="LDM 工具")
        d.start()
        d.exec()
        self.refresh_ytdlp()

    def _update_ytdlp(self) -> None:
        from . import ytdlp

        d = ytdlp.UpdateDialog(ytdlp.update_command(self.yt_state.path, self.tools_dir), self)
        d.start()
        d.exec()
        self.refresh_ytdlp()

    def ytdlp_rules(self) -> dict:
        rules: dict = {}
        for ln in self._lines(self.yt_rules):
            parts = ln.split()
            if len(parts) == 2 and parts[1].lower() in ("on", "off", "always"):
                rules[parts[0].lower()] = parts[1].lower()
        old = (self.cfg.get("ytdlp", {}).get("site_rules") or {}).keys()
        rules.update({d: None for d in old if d not in rules})  # merge-patch null deletes
        return rules

    @staticmethod
    def _lines(w: QPlainTextEdit) -> list[str]:
        return [ln.strip() for ln in w.toPlainText().splitlines() if ln.strip()]

    def language_choice(self) -> str:
        return self.lang.currentData()

    def shortcuts(self) -> dict[str, str]:
        return {k: ed.keySequence().toString() for k, ed in self.shortcut_edits.items()}

    def _add_tls_row(self, host: str, explicit: bool, insecure: bool) -> None:
        r = self.tls_table.rowCount()
        self.tls_table.insertRow(r)
        self.tls_table.setItem(r, 0, QTableWidgetItem(host))
        for col, on in ((1, explicit), (2, insecure)):
            it = QTableWidgetItem()
            it.setFlags(Qt.ItemFlag.ItemIsUserCheckable | Qt.ItemFlag.ItemIsEnabled | Qt.ItemFlag.ItemIsSelectable)
            it.setCheckState(Qt.CheckState.Checked if on else Qt.CheckState.Unchecked)
            self.tls_table.setItem(r, col, it)

    def patch(self) -> dict:
        old_hosts = (self.cfg.get("engine", {}).get("max_connections_per_host") or {}).keys()
        per_host: dict = {}
        for ln in self._lines(self.per_host):
            parts = ln.split()
            if len(parts) == 2 and parts[1].isdigit():
                per_host[parts[0]] = int(parts[1])
        per_host.update({h: None for h in old_hosts if h not in per_host})  # merge-patch null deletes
        cats = {}
        for r in range(self.cat_table.rowCount()):
            cid = self.cat_table.item(r, 0).data(Qt.ItemDataRole.UserRole)
            d = (self.cat_table.item(r, 1).text() if self.cat_table.item(r, 1) else "").strip()
            ex = (self.cat_table.item(r, 2).text() if self.cat_table.item(r, 2) else "").split()
            cats[cid] = {"dir": d, "exts": [x.lstrip(".").lower() for x in ex]} if d or ex else None
        logins = []
        for r in range(self.login_table.rowCount()):
            vals = [(self.login_table.item(r, c).text().strip() if self.login_table.item(r, c) else "") for c in range(3)]
            if vals[0] and vals[1]:
                logins.append({"host": vals[0], "user": vals[1], "password": vals[2]})
        tls = []
        for r in range(self.tls_table.rowCount()):
            host = self.tls_table.item(r, 0).text().strip() if self.tls_table.item(r, 0) else ""
            if host:
                flags = [self.tls_table.item(r, c).checkState() == Qt.CheckState.Checked for c in (1, 2)]
                tls.append({"host": host, "ftp_explicit_tls": flags[0], "accept_invalid_cert": flags[1]})
        return {
            "ftp": {"tls": self.ftp_tls.currentData()},
            "general": {
                "notify_on_complete": self.notify.isChecked(),
                "enable_clipboard": self.clipboard.isChecked(),
                "confirm_browser_downloads": self.confirm.isChecked(),
                "show_progress_dialog": self.show_progress.isChecked(),
                "show_complete_dialog": self.show_complete.isChecked(),
                "sounds": self.sounds.isChecked(),
                "log_cookies": self.log_cookies.isChecked(),
                "on_complete_command": _split(self.done_cmd.text()),
                "shutdown_command": _split(self.shutdown_cmd.text()) or ["systemctl", "poweroff"],
            },
            "sites": {"logins": logins, "tls": tls},
            "engine": {
                "max_connections_default": self.conns.value(),
                "max_connections_global": self.global_conns.value(),
                "max_connections_per_host": per_host,
                "min_part_size": self.min_part.value() * 1024,
                "max_retries": self.retries.value(),
                "timeout_secs": self.timeout.value(),
                "enable_speed_limit": self.limit_on.isChecked(),
                "speed_limit_bps": self.limit.value() * 1024,
                "proxy": self.proxy.text().strip() or "system",
                "pac_url": self.pac_url.text().strip(),
                "pac_refresh_secs": self.pac_refresh.value() * 60,
                "user_agent": self.ua.text().strip() or self.cfg.get("engine", {}).get("user_agent", ""),
                "conflict_policy": self.conflict.currentData(),
            },
            "queue": {"global_max_active": self.active.value()},
            "paths": {"download_dir": self.dl_dir.text().strip(), "by_category": self.by_cat.isChecked(), "categories": cats},
            "capture": {
                "enabled": self.capture.isChecked(),
                "capture_exts": self.exts.toPlainText().split(),
                "media_exts": self.media_exts.text().split(),
                "min_size": self.min_size.value() * 1024,
                "exclude_hosts": self._lines(self.exclude_hosts),
                "exclude_url_patterns": self._lines(self.exclude_urls),
                "bypass_modifier": self.bypass.currentData(),
                "force_modifier": self.force.currentData(),
            },
            "media": {
                "hls_max_height": self.height.value(),
                "segment_parallel": self.seg_par.value(),
                "keep_segments_on_failure": self.keep.isChecked(),
                "ffmpeg_path": self.ffmpeg.text().strip(),
                "output_container": self.container.currentData(),
                "audio_preference": self.audio_pref.currentData() or "aac",
                "ad_ranking": self.ad_rank.isChecked(),
                "ad_domains_file": self.ad_file.text().strip(),
            },
            "ytdlp": {
                "enabled": self.yt_enabled.isChecked(),
                "path": self.yt_path.text().strip(),
                "auto_fallback": self.yt_fallback.isChecked(),
                "cookies_from_browser": self.yt_cookies.currentData() or "",
                "playlist": self.yt_playlist.isChecked(),
                "max_height": self.yt_height.value(),
                "site_rules": self.ytdlp_rules(),
                "extra_args": _split(self.yt_args.text()),
                "auto_install": self.tl_auto_install.currentData() or "ask",
                "auto_update": self.tl_auto_update.currentData() or "daily",
                "deno_path": self.deno_path.text().strip(),
            },
            "browser": {
                "ws_bridge_enabled": self.ws.isChecked(),
                "ws_bridge_port": self.ws_port.value(),
            },
        }
