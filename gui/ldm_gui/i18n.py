"""UI language.

The GUI is written with Chinese source strings. When English is selected, `tr()` maps them and
`Translator` (an application-wide event filter) re-labels every window, menu, button, label,
tab, header and tree item as it is shown, so dialogs need no per-widget code. Strings that only
exist at run time (numbers, file names, server messages) are left as they are.
Language: env LDM_LANG, else QSettings ui/lang ("auto" | "zh" | "en"), auto = system locale.
"""

from __future__ import annotations

import os
import re

from PySide6.QtCore import QEvent, QLocale, QObject, QSettings
from PySide6.QtWidgets import (
    QAbstractButton,
    QComboBox,
    QGroupBox,
    QLabel,
    QLineEdit,
    QMenu,
    QPlainTextEdit,
    QSpinBox,
    QTableWidget,
    QTabWidget,
    QTreeWidget,
    QTreeWidgetItemIterator,
    QWidget,
)

EN: dict[str, str] = {
    # main window / menus / toolbar
    "LDM 下载管理器": "LDM Download Manager",
    "LDM 图形界面": "LDM GUI",
    "任务(&T)": "&Tasks",
    "文件(&F)": "&File",
    "下载(&D)": "&Downloads",
    "查看(&V)": "&View",
    "帮助(&H)": "&Help",
    "添加 URL": "Add URL",
    "添加下载": "Add download",
    "继续": "Resume",
    "暂停": "Pause",
    "停止": "Stop",
    "全部停止": "Stop all",
    "全部继续": "Resume all",
    "删除": "Delete",
    "删除已完成": "Delete completed",
    "选项": "Options",
    "计划任务": "Scheduler",
    "计划任务 / 队列": "Scheduler / Queues",
    "开始队列": "Start queue",
    "停止队列": "Stop queue",
    "抓取器": "Grabber",
    "站点抓取器": "Site grabber",
    "批量下载": "Batch download",
    "导入…": "Import…",
    "导出…": "Export…",
    "导入下载列表": "Import download list",
    "导出下载列表": "Export download list",
    "导出诊断包…": "Export diagnostics…",
    "导出诊断包（已脱敏）": "Export diagnostics (redacted)",
    "关于 LDM": "About LDM",
    "退出": "Exit",
    "显示主窗口": "Show main window",
    "速度限制器": "Speed limiter",
    "监视剪贴板": "Monitor clipboard",
    "分类树": "Categories",
    "详情面板": "Details panel",
    "悬浮投放窗口": "Drop target",
    "进度窗口": "Progress window",
    "属性": "Properties",
    "打开": "Open",
    "打开文件": "Open file",
    "打开文件夹": "Open folder",
    "打开所在文件夹": "Open containing folder",
    "复制 URL": "Copy URL",
    "移动到队列": "Move to queue",
    "删除任务": "Delete task",
    "仅删除任务": "Remove task only",
    "同时删除文件": "Also delete files",
    "搜索文件名、URL 或描述…": "Search file name, URL or description…",
    "已连接 ldmd": "Connected to ldmd",
    "正在连接 ldmd…": "Connecting to ldmd…",
    "ldmd 未连接（自动重试中）": "ldmd not connected (retrying)",
    "  限速中": "  speed limited",
    "添加失败": "Add failed",
    # tree / categories / columns / states
    "全部下载": "All downloads",
    "视频": "Video",
    "音乐": "Music",
    "文档": "Documents",
    "压缩包": "Compressed",
    "程序": "Programs",
    "其他": "General",
    "未完成": "Unfinished",
    "已完成": "Finished",
    "正在下载": "Downloading",
    "失败": "Failed",
    "队列": "Queues",
    "队列（列）": "Queue",
    "主队列": "Main queue",
    "文件名": "File name",
    "大小": "Size",
    "进度": "Progress",
    "速度": "Speed",
    "剩余时间": "Time left",
    "状态": "Status",
    "类型": "Type",
    "最后尝试": "Last try",
    "描述": "Description",
    "添加时间": "Date added",
    "排队": "Queued",
    "探测": "Probing",
    "下载中": "Downloading",
    "合并中": "Assembling",
    "已暂停": "Paused",
    "完成": "Complete",
    "已取消": "Canceled",
    "待确认": "Awaiting confirmation",
    "等待": "Waiting",
    "接收数据": "Receiving data",
    # progress dialog
    "下载状态": "Download status",
    "速度限制": "Speed limit",
    "完成后": "On completion",
    "地址": "URL",
    "文件大小": "File size",
    "已下载": "Downloaded",
    "传输速率": "Transfer rate",
    "断点续传": "Resume capability",
    "连接数": "Connections",
    "是": "Yes",
    "否": "No",
    "未知": "Unknown",
    "是（按分片续传）": "Yes (per segment)",
    "范围": "Range",
    "分段下载进度（每段一条连接）：": "Segment progress (one connection per segment):",
    "限制此任务的最大下载速度": "Limit this download's maximum speed",
    "最大速度：": "Max speed:",
    "提示：全局限速可在托盘菜单或“选项 → 连接”中开关。": "Tip: the global limiter is in the tray menu or Options → Connection.",
    "无操作": "Do nothing",
    "关闭": "Close",
    "关机": "Shut down",
    "运行命令": "Run command",
    "完成后关机": "Shut down when done",
    "此任务完成后将执行关机命令，确定？": "The computer will shut down when this download finishes. Continue?",
    "取消下载": "Cancel",
    "隐藏详情": "Hide details",
    "显示详情": "Show details",
    "停止录制": "Stop recording",
    "直播录制中": "Recording live stream",
    "录制中": "Recording",
    "正在停止录制…": "Stopping recording…",
    "已录制": "recorded",
    "直播：保存已录制的部分并完成": "Live: keep what was recorded and finish",
    # capture / completion / properties
    "下载文件信息": "Download File Info",
    "分类": "Category",
    "保存到": "Save to",
    "队列（稍后下载）": "Queue (download later)",
    "开始下载": "Start download",
    "稍后下载": "Download later",
    "取消": "Cancel",
    "改用编号名": "Use numbered name",
    "可选：备注（显示在列表“描述”列）": "Optional note (shown in the Description column)",
    "浏览…": "Browse…",
    "选择保存目录": "Choose folder",
    "下载完成": "Download complete",
    "以后不再显示此对话框": "Don't show this dialog again",
    "镜像": "Mirrors",
    "备用地址，空格分隔（主地址失败时依次尝试）": "Mirror URLs, space separated (tried when the main URL fails)",
    "保存目录": "Folder",
    "应用": "Apply",
    # scheduler
    "新建队列": "New queue",
    "删除队列": "Delete queue",
    "主队列不能删除。": "The main queue cannot be deleted.",
    "队列名称：": "Queue name:",
    "启用（按计划自动运行）": "Enabled (run on schedule)",
    "开始时间": "Start time",
    "停止时间": "Stop time",
    "星期": "Days",
    "一": "Mon",
    "二": "Tue",
    "三": "Wed",
    "四": "Thu",
    "五": "Fri",
    "六": "Sat",
    "日": "Sun",
    "同时下载数": "Concurrent downloads",
    "队列完成后": "When the queue is done",
    "命令": "Command",
    "例如：notify-send 队列完成": "e.g. notify-send done",
    "队列中的文件（自上而下下载）": "Files in the queue (downloaded top to bottom)",
    "上移": "Move up",
    "下移": "Move down",
    "立即开始": "Start now",
    "计划": "Schedule",
    "名称": "Name",
    "进行中": "Active",
    # options
    "常规": "General",
    "文件类型": "File types",
    "保存位置": "Save to",
    "连接": "Connection",
    "限速与代理": "Speed & proxy",
    "站点登录": "Site logins",
    "不加密（普通 FTP）": "No encryption (plain FTP)",
    "服务器支持时使用 TLS": "Use TLS when the server supports it",
    "必须使用 TLS（显式 FTPS）": "Require TLS (explicit FTPS)",
    "ftp:// 链接": "ftp:// links",
    "强制 FTPS": "Require FTPS",
    "接受无效证书": "Accept invalid certificate",
    "“接受无效证书”会关闭该站点的证书校验，仅在信任该服务器时开启。": (
        '"Accept invalid certificate" turns off certificate checks for that site; enable it only if you trust the server.'
    ),
    "浏览器": "Browser",
    "媒体": "Media",
    "快捷键": "Shortcuts",
    "默认下载目录": "Default download folder",
    "按分类保存到子目录（Video、Music…）": "Save into category sub-folders (Video, Music…)",
    "文件已存在时": "If the file exists",
    "自动编号重命名": "Rename with a number",
    "询问（在下载信息对话框中提示改名）": "Ask (offer a new name in Download File Info)",
    "覆盖": "Overwrite",
    "每个任务连接数": "Connections per download",
    "全局最大连接数": "Global connection limit",
    "同时下载任务数": "Concurrent downloads",
    "最小分段": "Minimum segment",
    "重试次数": "Retries",
    "超时": "Timeout",
    " 秒": " s",
    "按主机限制连接": "Per-host connection limits",
    "每行：主机 连接数，例如 example.com 2": "One per line: host connections, e.g. example.com 2",
    "启用全局限速": "Enable global speed limit",
    "最大速度": "Max speed",
    "代理": "Proxy",
    "PAC 脚本": "PAC script",
    "PAC 刷新间隔": "PAC refresh interval",
    " 分钟": " min",
    "system / none / browser（跟随浏览器）/ pac（自动配置脚本）/ http://host:port / socks5://host:port": (
        "system / none / browser (follow the browser) / pac (auto-config script) / http://host:port / socks5://host:port"
    ),
    "代理为 pac 时使用：http(s)://…/proxy.pac、file:///… 或绝对路径": (
        "Used when the proxy is pac: http(s)://…/proxy.pac, file:///… or an absolute path"
    ),
    "自动接管浏览器中以下类型的下载": "Take over browser downloads of these types",
    "媒体扩展名": "Media extensions",
    "不接管的站点": "Never take over from these sites",
    "每行一个，支持 *.example.com": "One per line, *.example.com allowed",
    "不接管的地址模式": "Never take over these URL patterns",
    "最小文件大小": "Minimum file size",
    "按住此键点击：不接管": "Hold while clicking: leave to the browser",
    "按住此键点击：强制接管": "Hold while clicking: force LDM",
    "浏览器接管时显示“下载文件信息”对话框": "Show Download File Info when the browser hands over",
    "下载完成后显示完成对话框": "Show the completion dialog",
    "开始下载时显示进度对话框": "Show the progress dialog when a download starts",
    "下载完成时显示桌面通知": "Desktop notification on completion",
    "完成/失败时播放提示音": "Play a sound on completion / failure",
    "监视剪贴板中的下载地址": "Watch the clipboard for download links",
    "完成后命令": "Command after completion",
    "关机命令": "Shutdown command",
    "站点（支持 *）": "Site (* allowed)",
    "用户名": "User name",
    "密码": "Password",
    "添加": "Add",
    "默认最高清晰度": "Default maximum height",
    "音轨偏好": "Audio preference",
    "按“主视频 / 疑似广告”排序检测到的媒体（广告置灰放最后，不隐藏）": (
        "Rank detected media main video first; suspected ads greyed at the bottom (never hidden)"
    ),
    "广告域名列表文件": "Ad domain list file",
    "可选：每行一个域名，支持 # 注释和 hosts 格式": "Optional: one domain per line, # comments and hosts format allowed",
    "优先 AAC（默认，兼容最好）": "Prefer AAC (default, best compatibility)",
    "声道数最多（如 5.1）": "Most channels (e.g. 5.1)",
    "优先英语 (language:en)": "Prefer English (language:en)",
    "优先中文 (language:zh)": "Prefer Chinese (language:zh)",
    "输出容器": "Output container",
    "ffmpeg 路径": "ffmpeg path",
    "启用本机 WebSocket 桥": "Enable the local WebSocket bridge",
    "WS 端口": "WS port",
    "日志中记录 Cookie（调试用）": "Log cookies (debugging)",
    "失败时保留已下载分片": "Keep downloaded segments on failure",
    "扩展名": "Extensions",
    "附加扩展名": "Extra extensions",
    "默认：按分类保存到下载目录": "Default: category folder under the download folder",
    "保存目录（空=默认）": "Folder (empty = default)",
    "自动": "Auto",
    "默认": "Default",
    "不使用": "None",
    "界面语言": "Language",
    "自动（跟随系统）": "Auto (system)",
    "界面语言将在重启 LDM 后生效": "The language applies after restarting LDM",
    "活动 {a}  排队 {q}  完成 {c}  速度 {spd}/s{lim}": "Active {a}  Queued {q}  Done {c}  Speed {spd}/s{lim}",
    "LDM - {a} 个下载中，{spd}/s": "LDM - {a} downloading, {spd}/s",
    "下载这些站点的文件时自动附带 HTTP 基本认证（保存在仅本人可读的 config.toml）。": "Send HTTP Basic credentials for these sites (stored in config.toml, readable only by you).",  # noqa: E501
    # tools
    "地址模板": "URL template",
    "预览：": "Preview:",
    "数字": "Numbers",
    "字母": "Letters",
    "从": "From",
    "到": "To",
    "位数": "Digits",
    "不补零": "No padding",
    "通配符 * 类型": "Wildcard * type",
    "起始网页地址": "Start page URL",
    "起始地址": "Start URL",
    "深度": "Depth",
    "只抓取同一站点": "Same site only",
    "最多页面": "Max pages",
    "开始抓取": "Start",
    "抓取中…": "Crawling…",
    "所在页面": "Found on",
    "全选": "Select all",
    "全不选": "Select none",
    "下载选中": "Download selected",
    "加入后暂停（稍后由队列启动）": "Add paused (start later from a queue)",
    "添加后暂停": "Add paused",
    "把链接拖到这里用 LDM 下载（右键关闭）": "Drop links here to download with LDM (right-click to close)",
    "每行一个 URL（http/https，支持 .m3u8 / .mpd）": "One URL per line (http/https, .m3u8 / .mpd supported)",
    "请输入 http(s) 地址": "Enter an http(s) URL",
    "文件": "File",
    "原因": "reason",
    "更新 yt-dlp": "Update yt-dlp",
    "安装 yt-dlp": "Install yt-dlp",
    "工具": "Tools",
    "托管目录": "Managed folder",
    "安装 / 修复": "Install / repair",
    "立即更新": "Update now",
    "检查更新": "Check for updates",
    "回滚 yt-dlp": "Roll back yt-dlp",
    "首次使用": "First use",
    "自动更新": "Auto-update",
    "deno 路径": "deno path",
    "首次需要时询问（默认）": "Ask the first time it is needed (default)",
    "自动下载安装": "Download and install automatically",
    "从不（只用我自己的 yt-dlp）": "Never (only use my own yt-dlp)",
    "每天检查（默认）": "Check daily (default)",
    "每周检查": "Check weekly",
    "LDM 工具": "LDM tools",
    "需要 yt-dlp": "yt-dlp needed",
    "下载并安装": "Download and install",
    "不用了": "No thanks",
    "以后需要时自动安装，不再询问": "Install automatically from now on, don't ask again",
    "yt-dlp 路径": "yt-dlp path",
    "从浏览器读取 Cookie": "Cookies from browser",
    "站点规则": "Site rules",
    "附加参数": "Extra arguments",
    "启用 yt-dlp（YouTube 下载；以及 LDM 失败时的回退）": "Enable yt-dlp (YouTube downloads and fallback when LDM fails)",
    "LDM 下载失败（403/410、非媒体内容、清单解析失败）时自动用 yt-dlp 重试一次": (
        "Retry once with yt-dlp when LDM fails (403/410, not media, manifest errors)"
    ),
    "播放列表链接：把每个视频加入队列（默认只下载当前视频）": "Playlist links: enqueue every video (default: only the current one)",
}

_lang: str | None = None


def language() -> str:
    global _lang
    if _lang is None:
        v = os.environ.get("LDM_LANG") or str(QSettings("LDM", "ldm-gui").value("ui/lang", "auto") or "auto")
        if v == "auto":
            v = "zh" if QLocale.system().name().startswith("zh") else "en"
        _lang = "en" if v.startswith("en") else "zh"
    return _lang


def set_language(v: str | None) -> None:
    """Force a language (tests / after the option changes; takes full effect after restart)."""
    global _lang
    _lang = v


_SUFFIX = re.compile(r"^(.*?)([：:…\s]*)$", re.S)


def tr(s: str) -> str:
    if language() != "en" or not s:
        return "队列" if s == "队列（列）" else s
    if s in EN:
        return EN[s]
    m = _SUFFIX.match(s)  # "文件名：" → "File name:"
    if m and m.group(1) in EN:
        return EN[m.group(1)] + m.group(2).replace("：", ":")
    return s


def translate_widget(root: QWidget) -> None:
    if language() != "en":
        return
    widgets = [root, *root.findChildren(QWidget)]
    for w in widgets:
        if w.isWindow() and w.windowTitle():
            w.setWindowTitle(tr(w.windowTitle()))
        for a in w.actions():
            a.setText(tr(a.text()))
            if a.toolTip():
                a.setToolTip(tr(a.toolTip()))
        if isinstance(w, (QAbstractButton, QLabel)):
            w.setText(tr(w.text()))
        if isinstance(w, QMenu):
            w.setTitle(tr(w.title()))
        if isinstance(w, QGroupBox):
            w.setTitle(tr(w.title()))
        if isinstance(w, QSpinBox) and w.suffix():
            w.setSuffix(tr(w.suffix()))
        if isinstance(w, (QLineEdit, QPlainTextEdit)) and w.placeholderText():
            w.setPlaceholderText(tr(w.placeholderText()))
        if w.toolTip():
            w.setToolTip(tr(w.toolTip()))
        if isinstance(w, QTabWidget):
            for i in range(w.count()):
                w.setTabText(i, tr(w.tabText(i)))
        if isinstance(w, QComboBox):
            for i in range(w.count()):
                w.setItemText(i, tr(w.itemText(i)))
        if isinstance(w, QTableWidget):
            for c in range(w.columnCount()):
                it = w.horizontalHeaderItem(c)
                if it:
                    it.setText(tr(it.text()))
        if isinstance(w, QTreeWidget):
            hi = w.headerItem()
            for c in range(hi.columnCount()):
                hi.setText(c, tr(hi.text(c)))
            itr = QTreeWidgetItemIterator(w)
            while itr.value():
                it = itr.value()
                it.setText(0, _tr_count(it.text(0)))
                itr += 1


def _tr_count(s: str) -> str:
    """Tree labels may carry a count: "视频 (3)"."""
    m = re.match(r"^(.*?)(\s*\(\d+\))?$", s)
    return tr(m.group(1)) + (m.group(2) or "") if m else tr(s)


class Translator(QObject):
    """Event filter: translate each window when it is first shown (and menus every time)."""

    def eventFilter(self, obj, ev):  # noqa: N802 (Qt name)
        if ev.type() == QEvent.Type.Show and isinstance(obj, QWidget) and (obj.isWindow() or isinstance(obj, QMenu)):
            translate_widget(obj)
        return False


def install(app) -> Translator | None:
    if language() != "en":
        return None
    t = Translator(app)
    app.installEventFilter(t)
    return t
