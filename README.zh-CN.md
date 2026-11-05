# LDM – Linux 下载管理器

[English](README.md) | **简体中文**

![LDM 主窗口与正在进行的下载](assets/screenshots/zh/hero.png)

LDM 是 Linux 下的下载管理器，重点是检测并保存网页中的视频。它由后台守护进程 `ldmd`、命令行客户端
`ldm`、基于 Qt（PySide6）的桌面图形界面 `ldm-gui`，以及支持 Chromium 系浏览器和 Firefox 的浏览器扩展组成。

## 功能

- **HTTP(S) / FTP / FTPS**：单个文件多连接下载并动态分段，支持断点续传（崩溃后也能继续）、全局和单任务
  限速、镜像、SHA-256/MD5 校验、按站点登录、从浏览器刷新 Cookie、带时间窗口的队列，以及“完成后”操作。
- **流媒体**：HLS（AES-128、fMP4、字节范围、独立音轨、纯音频）和 DASH（SegmentTemplate / Timeline /
  List），用 ffmpeg 合并为 MP4。支持直播录制，可限制时长并干净地“停止录制”。字幕轨保存为 `.vtt`/`.srt`。
  不支持加密（DRM）流。
- **守护进程** `ldmd`：提供 Unix 套接字 IPC 和会话 D-Bus 接口（`AddUrl`、`List`、`Pause`、`Resume`、
  `Status`、工具管理）。
- **浏览器集成**：Manifest V3 扩展（Chromium 与 Firefox），可按文件类型接管下载；在主播放器上显示带清晰度
  菜单的“下载此视频”按钮；对检测到的媒体排序，疑似广告排在最后（但不会隐藏）；并提供“下载全部链接”。扩展
  通过原生消息（native messaging）宿主与守护进程通信，也可选用本地 WebSocket 桥。
- **客户端**：命令行 `ldm` 和图形界面 `ldm-gui`（分类、每个连接的进度、计划任务、批量下载、站点抓取器、
  剪贴板监视、导入/导出；支持英文和简体中文）。
- **yt-dlp 集成**（可选）：用于 YouTube，并作为其他网站的备用方案；yt-dlp 和 deno 在首次使用时由 LDM
  下载并校验。
- **运维**：`ldm doctor`、`ldm diag`（脱敏的诊断包）、`ldm metrics`（Prometheus 文本格式）。日志中的 URL
  和凭据都会脱敏。

## 截图

| | |
|---|---|
| ![主窗口](assets/screenshots/zh/main-window.png) | ![下载进度](assets/screenshots/zh/progress.png) |
| **主窗口**：按分类整理下载，并带详情面板。 | **下载进度**：多连接下载，每个分段的进度实时可见。 |
| ![添加下载](assets/screenshots/zh/add-download.png) | ![选项](assets/screenshots/zh/settings.png) |
| **添加下载**：保存目录、文件名、队列、连接数和登录信息。 | **选项**：连接数、限速、重试和按主机的连接限制。 |
| ![计划任务](assets/screenshots/zh/scheduler.png) | |
| **计划任务**：带时间窗口和星期设置的队列。 | |

## 下载

每个 [GitHub Release](https://github.com/wzk4work/ldm/releases/latest) 都附带 **x86_64** 和
**aarch64（arm64）** 两种架构的预编译包：

| 安装包 | 安装方法 |
|---|---|
| `ldm_<版本>_amd64.deb` / `_arm64.deb` | `sudo apt install ./ldm_<版本>_<架构>.deb`（图形界面需要 Debian 13 或 Ubuntu 24.10 及以上） |
| `ldm-<版本>-1.x86_64.rpm` / `.aarch64.rpm` | `sudo dnf install ./ldm-<版本>-1.<架构>.rpm`（Fedora；openSUSE 用 `zypper install`） |
| `LDM-<版本>-x86_64.AppImage` / `-aarch64.AppImage` | 先 `chmod +x`，再运行 `./LDM-….AppImage --install-integration`（需要 glibc 2.39+ 和 FUSE） |
| `LDM-<版本>-x86_64.flatpak` / `-aarch64.flatpak` | `flatpak install --user ./LDM-….flatpak`（运行时从 Flathub 获取） |
| `ldm-extension-chrome-<版本>.zip`、`ldm-extension-firefox-<版本>.zip` | 未签名的浏览器扩展，用法见[安装](#安装) |

下载后可用 `sha256sum -c --ignore-missing SHA256SUMS` 校验。装好 deb 或 rpm 后，如果希望登录时自动启动守护进程，
可以运行 `systemctl --user enable --now ldmd`（可选；图形界面和浏览器宿主会在需要时自动启动它）。

## 仓库结构

```
crates/ldm-core        引擎：HTTP/FTP、HLS/DASH、直播录制、合并、SQLite 存储、
                       调度器、PAC、工具管理
crates/ldm-ipc         JSON 信封协议 {v,id,type,payload} 及分帧
crates/ldm-daemon      ldmd（Unix 套接字，单实例）
crates/ldm-native-host 浏览器原生消息宿主（需要时启动 ldmd）
crates/ldm-ws-bridge   可选的 WebSocket 桥，监听 127.0.0.1
crates/ldm-cli         ldm 命令行客户端
tools/ldm-testsrv      测试使用的 HTTP/HLS/DASH 测试服务器
extension/             TypeScript MV3 扩展（用 esbuild 构建）
gui/ldm_gui/           PySide6 图形界面和 D-Bus 服务
packaging/             安装脚本、deb、rpm、AppImage、Flatpak、systemd、桌面文件
rules/bundled/         示例站点规则
tests/e2e/             端到端测试（pytest）
scripts/check.sh       运行全部检查
```

## 构建

需要：Rust 1.85+、Node.js 20+（扩展）、带 PySide6 的 Python 3.10+（图形界面），以及 ffmpeg（推荐；
没有 ffmpeg 时 HLS/DASH 会保存为原始的 `.ts`/fMP4）。

```sh
cargo build --release                                     # ldmd、ldm、ldm-native-host
scripts/build-release.sh                                  # 同上，用于打包：去除本地构建路径
(cd extension && npm ci && npm run build)                 # extension/dist/{chromium,firefox}
python3 -m venv .venv && .venv/bin/pip install -r requirements-dev.txt   # 图形界面和测试
```

### 扩展的商店版构建

```sh
(cd extension && npm run build:store)     # 或：LDM_STORE_BUILD=1 npm run build
```

输出到 `extension/dist-store/{chromium,firefox}`。商店版**不含 yt-dlp 集成**：没有 yt-dlp 菜单项和页面
规则，在 YouTube 页面上也不提供任何下载。它还去掉了 Chromium 清单中固定的 `"key"`，因为商店会分配自己的
扩展 ID。为从商店安装的扩展安装 LDM 时，请把该 ID 加入原生消息清单（`allowed_origins`）。普通构建
（`npm run build`）不受影响。

## 安装

```sh
packaging/install.sh                 # 为当前用户安装到 ~/.local
sudo packaging/install.sh --system   # 系统级安装到 /usr
packaging/build-deb.sh               # dist/ldm_<version>_<arch>.deb
packaging/build-rpm.sh               # dist/ldm-<version>-1.<arch>.rpm (needs rpmbuild)
packaging/uninstall.sh [--purge]     # --purge 同时删除用户数据
```

`install.sh` 会安装 `ldmd`、`ldm`、`ldm-gui`、`ldm-dbus` 和原生消息宿主，以及规则、图形界面和扩展，
Chrome、Chromium、Edge、Brave 和 Firefox 的原生消息清单，systemd 用户服务 `ldmd.service` 和桌面启动项。
`build-deb.sh` 从 `LDM_MAINTAINER="Name <email>"` 读取维护者信息。

加载扩展：

- Chrome / Chromium：打开 `chrome://extensions`，开启开发者模式，选择“加载已解压的扩展程序”，然后选择
  `~/.local/lib/ldm/extension/chromium`。清单中的 `key` 把扩展 ID 固定为 `extension/EXTENSION_ID` 中的值，
  原生消息清单允许该 ID。
- Firefox：打开 `about:debugging`，选择“临时载入附加组件”，然后选择 `extension/dist/firefox/manifest.json`。

安装后运行 `ldm doctor`，检查 ffmpeg、套接字、守护进程和浏览器清单。

### AppImage

```sh
packaging/appimage/build-appimage.sh      # dist/LDM-<version>-<arch>.AppImage
```

镜像包含 ldmd、命令行、原生消息宿主和图形界面，并自带 CPython 和精简过的 PySide6（不含 yt-dlp）。直接运行
即打开图形界面；用 `LDM-….AppImage ldm …` 运行命令行；加 `--install-integration` 可为当前用户添加桌面启动项
和浏览器原生消息清单（`--remove-integration` 将其移除）。运行需要 FUSE，且 glibc 版本不低于构建机器。

构建需要 `mksquashfs`（squashfs-tools）和带 PySide6 的 Python（`PYTHON`，默认 `.venv/bin/python`）；
缺少 release 二进制文件或扩展时会先构建它们。AppImage type-2 运行时只下载一次，存放在
`~/.cache/ldm-appimage`；可用 `RUNTIME=…` 指定本地文件，或用 `RUNTIME_SHA256=…` 固定其校验值。许可证文本
（LDM、第三方声明、Qt/PySide6 LGPL、CPython）安装在镜像的 `usr/lib/ldm/` 下。

### Flatpak

清单为 `packaging/flatpak/io.github.wzk4work.LDM.yml`（org.kde.Platform 6.11 和 io.qt.PySide.BaseApp；
ffmpeg 作为模块从源码构建；Rust crate 已 vendor，可离线构建）。

```sh
python3 packaging/flatpak/gen-cargo-sources.py     # 每次修改 Cargo.lock 后运行
flatpak install --user flathub org.kde.Sdk//6.11 org.kde.Platform//6.11 \
    io.qt.PySide.BaseApp//6.11 org.freedesktop.Sdk.Extension.rust-stable//25.08
flatpak-builder --user --force-clean --repo=repo build-dir packaging/flatpak/io.github.wzk4work.LDM.yml
flatpak build-bundle --runtime-repo=https://dl.flathub.org/repo/flathub.flatpakrepo \
    repo dist/LDM-0.9.0.flatpak io.github.wzk4work.LDM
flatpak install --user dist/LDM-0.9.0.flatpak
flatpak run io.github.wzk4work.LDM                         # 图形界面
flatpak run --command=ldm io.github.wzk4work.LDM --help    # 命令行
packaging/flatpak/host-integration.sh install               # 在宿主机上运行，供扩展使用
packaging/flatpak/smoke.sh                                  # 可选：对已安装应用做冒烟测试
```

- 应用可以写入 `~/Downloads`（xdg-download）。其他目录需先运行
  `flatpak override --user --filesystem=<dir> io.github.wzk4work.LDM`，否则守护进程会拒绝该目录并给出提示。
- 许可证文本（LDM、第三方声明、FFmpeg）安装在 `/app/share/licenses/io.github.wzk4work.LDM/`。该包含有
  FFmpeg（LGPL-2.1+）。如果分发此包，请同时提供对应的源代码，例如用 `flatpak-builder --bundle-sources`
  构建并发布生成的源码运行时。

## 使用

```sh
ldm daemon start                     # 通常由 systemd 或原生消息宿主启动
ldm add URL [-d DIR] [-n NAME] [-c CONNS] [--referrer R] [--cookie-file cookies.txt] [-w]
ldm add URL --kind hls --height 720
ldm list [--json] | ldm info ID | ldm watch [ID]
ldm pause ID|--all | ldm resume ID|--all | ldm rm ID [--delete-files]
ldm stop-recording ID                # 停止直播录制并保留可播放的文件
ldm set ID --priority 5 --queue main --conns 8
ldm probe URL.m3u8                   # 列出 HLS/DASH 变体
ldm queue list | ldm queue set main --concurrent 2 --window 01:00-07:00
ldm config get | ldm config set engine.speed_limit_bps 1048576 | ldm config path
ldm export --format csv -o jobs.csv | ldm import jobs.json
ldm get URL [-d DIR|-o FILE] | ldm hls URL | ldm dash URL   # 一次性下载，不经过守护进程
ldm add ftpes://user:pass@host/file.iso   # 显式 FTPS；ftps:// 为隐式（端口 990）
ldm diag [-o FILE] | ldm metrics | ldm doctor
ldm-gui                              # 图形界面（LDM_LANG=en 为英文）
ldm-dbus                             # 仅 D-Bus 服务
```

ID 可以缩写为唯一的前缀。守护进程未运行时，`ldm` 以退出码 3 退出。

### 配置

配置文件 `~/.config/ldm/config.toml`（可用 `LDM_CONFIG` 覆盖）；数据库 `~/.local/share/ldm/downloads.db`
（`LDM_DB`）；套接字 `$XDG_RUNTIME_DIR/ldm.sock`（`LDM_SOCKET`）；默认下载到 `XDG_DOWNLOAD_DIR`。常用配置项：

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `engine.proxy` | `system` | `none`、`system`、`browser`（跟随扩展）、`pac` 或代理 URL |
| `engine.pac_url` / `engine.pac_refresh_secs` | `""` / `1800` | PAC 脚本（`http(s)://`、`file://` 或路径）及刷新间隔 |
| `engine.conflict_policy` | `rename_numbered` | `rename_numbered`、`ask` 或 `overwrite` |
| `ftp.tls` | `off` | `ftp://` 使用 `off`、`try` 或 `require` |
| `media.live_record` / `media.live_max_duration` | `true` / `0` | 直播录制及其时长上限（秒，0 = 不限） |
| `media.subtitles` | `true` | 保存字幕轨 |
| `media.ad_ranking` / `media.ad_domains_file` | `true` / `""` | 疑似广告排在最后；额外的广告域名列表 |
| `media.audio_preference` | `aac` | `aac`、`most_channels` 或 `language:<code>` |
| `browser.ws_bridge_enabled` / `browser.ws_bridge_token` | `false` / `""` | 可选的 WebSocket 桥，见 [SECURITY.md](SECURITY.md) |

PAC 脚本在一个小型沙箱解释器中运行，它实现了常用的 PAC 子集和辅助函数；遇到不支持的语法时脚本加载失败（会
记录日志），LDM 随后像浏览器一样直接连接。TLS 使用 rustls 和内置的 webpki 根证书；对使用自签名证书的 FTPS
服务器，可以按主机设置 `[[sites.tls]] accept_invalid_cert = true`。

### WebSocket 桥（默认关闭）

使用 `ldmd --ws [--ws-port 17001]` 或 `browser.ws_bridge_enabled = true` 开启。它只监听 127.0.0.1，以
`WS_PORT n` 输出实际端口，使用 `ldm.bridge.v1` 子协议（JSON 信封），拒绝来自网页的 origin，并可要求令牌。
见 [SECURITY.md](SECURITY.md)。

### 可选的页面钩子（默认关闭）

扩展弹窗中的“页面钩子”复选框会在页面中安装只读的观察器：`fetch`/`XMLHttpRequest` 的 URL、Media Source
Extensions 的编码格式，以及 `appendBuffer` 调用的次数和数据量。它只做观察：每个钩子都调用原函数并原样返回其
结果，媒体数据从不被读取、复制或保存。它有助于把实际正在播放的播放器与对应的流匹配起来，并在菜单中显示编码
格式。不勾选时（默认），页面自身的 JavaScript 上下文中不会运行任何 LDM 代码。

## yt-dlp 集成

LDM 不附带 yt-dlp，自身也不实现 YouTube 签名或令牌处理。首次需要 yt-dlp 时，LDM 会先征求同意（图形界面
对话框、命令行提示或 `ldm tools install`），然后从 GitHub 下载 yt-dlp 和 deno 的官方发布版本到
`$XDG_DATA_HOME/ldm/tools/`。下载的文件会经过校验（SHA-256；有 `gpg` 时还会校验 yt-dlp 的 GPG 签名），
以原子方式安装，并保留上一版本以便回滚。在 `ytdlp.path` 中指定的 yt-dlp 或 `PATH` 中的 yt-dlp 也可以使用。

```sh
ldm tools status
ldm tools install [yt-dlp|deno] [--version X]
ldm tools update | check | rollback | remove [yt-dlp|deno]
ldm add --ytdlp URL
```

- YouTube 页面（youtube.com、youtu.be、music 和 shorts）交给 yt-dlp 处理。默认格式为最高 1080p、非 HDR、
  优先 H.264，配 AAC 音频，合并为 MP4。
- 对其他网站，yt-dlp 只作为备用方案：页面上找不到媒体时，或 LDM 的媒体下载因 yt-dlp 可能解决的原因失败时
  （403/410、签名 URL 过期、不是媒体、清单解析错误）。每个下载最多尝试一次。
- 除非设置了 `ytdlp.cookies_from_browser`，否则不会传递任何 Cookie。LDM 不做自动登录，也不会尝试绕过
  “确认你不是机器人”之类的页面；这类失败会如实报告。

| 配置项 | 默认值 | 说明 |
|---|---|---|
| `ytdlp.enabled` | `true` | 总开关 |
| `ytdlp.path` / `ytdlp.deno_path` | `""` | 显式路径（默认：先用 LDM 管理的版本，再找 `PATH`） |
| `ytdlp.auto_install` | `ask` | `ask`、`always` 或 `never` |
| `ytdlp.auto_update` | `daily` | `daily`、`weekly` 或 `off` |
| `ytdlp.auto_fallback` | `true` | 允许上文所述的备用方案 |
| `ytdlp.site_rules` | `{}` | 按域名设置 `on`、`off` 或 `always` |
| `ytdlp.extra_args` | `[]` | 额外参数 |
| `ytdlp.cookies_from_browser` | `""` | 如 `firefox`；留空 = 不传 Cookie |
| `ytdlp.playlist` | `false` | 播放列表中每一项单独下载 |
| `ytdlp.max_height` | `1080` | 默认的高度上限 |

## 标识符

公开的标识符列在 [`packaging/identifiers.env`](packaging/identifiers.env) 中：

| | |
|---|---|
| 应用 ID（Flatpak、桌面文件、AppStream） | `io.github.wzk4work.LDM` |
| D-Bus 名称 / 路径 | `io.github.wzk4work.LDM.Daemon1` / `/io/github/wzk4work/LDM/Daemon1` |
| 原生消息宿主 | `io.github.wzk4work.ldm.browser_host` |
| Firefox 附加组件 ID | `ldm@wzk4work.github.io` |

`tests/e2e/test_identifiers.py` 会检查所有文件使用的值是否一致。

## 开发

```sh
scripts/check.sh    # fmt、clippy -D warnings、cargo test、扩展检查与构建、ruff、图形界面和端到端测试
```

参与开发见 [CONTRIBUTING.md](CONTRIBUTING.md)。安全问题报告见 [SECURITY.md](SECURITY.md)。

## 许可

LDM 以 [MIT 许可证](LICENSE) 发布。第三方组件及其许可证列在 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)
中（用 `scripts/gen_third_party_notices.py` 重新生成）。Flatpak 中还包含 FFmpeg（LGPL-2.1+），AppImage 中
包含 Qt/PySide6（LGPL-3.0）和 CPython（PSF 许可证）。
