//! External yt-dlp integration (original work).
//!
//! LDM never parses YouTube player code, signatures or PO tokens itself: for sites routed to
//! yt-dlp (YouTube by default, `ytdlp.site_rules = always`) or as a one-shot fallback after an
//! eligible LDM failure, the user's own system `yt-dlp` runs as a child process. LDM keeps the
//! job (queue, pause/resume, cancel, categories, filename) and reads machine-friendly progress
//! lines produced by `--progress-template`.
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::process::Command;
use tracing::{info, warn};

use crate::config::{Config, YtdlpConfig};
use crate::engine::JobEnv;
use crate::error::{LdmError, Result};
use crate::media::job::QualityRow;
use crate::media::AudioPreference;
use crate::tools::{self, AutoInstall, Found, Tool};

/// Hosts routed to yt-dlp unless a site rule says otherwise (YouTube, YouTube Music, Shorts, youtu.be).
pub const YOUTUBE_HOSTS: &[&str] = &["youtube.com", "youtu.be", "youtube-nocookie.com"];

/// Machine-readable progress line (`download:` stage of `--progress-template`).
pub const PROGRESS_PREFIX: &str = "LDMP";
pub const PROGRESS_TEMPLATE: &str = "download:LDMP %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s %(progress.speed)s %(progress.eta)s %(progress.status)s %(info.format_id)s";
/// Printed once the final (merged/moved) file exists.
pub const FILE_PREFIX: &str = "LDMF ";
pub const FILE_TEMPLATE: &str = "after_move:LDMF %(filepath)s";
/// A yt-dlp release older than this is reported as outdated (sites change; yt-dlp follows).
pub const OUTDATED_DAYS: i64 = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SiteRule {
    /// fallback allowed (default)
    On,
    /// never use yt-dlp for this site
    Off,
    /// route every download for this site to yt-dlp
    Always,
}

impl SiteRule {
    pub fn parse(s: &str) -> Option<SiteRule> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "fallback" | "auto" => Some(SiteRule::On),
            "off" | "never" | "no" => Some(SiteRule::Off),
            "always" | "yes" => Some(SiteRule::Always),
            _ => None,
        }
    }
}

pub fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.trim_end_matches('.').to_ascii_lowercase())).unwrap_or_default()
}

fn host_matches(host: &str, domain: &str) -> bool {
    let d = domain.trim().trim_start_matches("*.").trim_start_matches('.').trim_end_matches('.').to_ascii_lowercase();
    !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
}

pub fn is_youtube(url: &str) -> bool {
    let h = host_of(url);
    YOUTUBE_HOSTS.iter().any(|d| host_matches(&h, d))
}

/// Effective rule for a URL: the longest matching `site_rules` domain wins, then the
/// built-in YouTube routing, else `On`.
pub fn site_rule(cfg: &YtdlpConfig, url: &str) -> SiteRule {
    let h = host_of(url);
    if h.is_empty() {
        return SiteRule::On;
    }
    let best = cfg
        .site_rules
        .iter()
        .filter(|(d, _)| host_matches(&h, d))
        .filter_map(|(d, r)| SiteRule::parse(r).map(|r| (d.trim_start_matches("*.").trim_start_matches('.').len(), r)))
        .max_by_key(|(n, _)| *n);
    match best {
        Some((_, r)) => r,
        None if is_youtube(url) => SiteRule::Always,
        None => SiteRule::On,
    }
}

/// Domains the extension should treat as "yt-dlp sites" (built-in YouTube + `always` rules,
/// minus `off` rules) – used to show the in-page button without any network media.
pub fn always_domains(cfg: &YtdlpConfig) -> Vec<String> {
    let mut v: Vec<String> = YOUTUBE_HOSTS.iter().map(|s| s.to_string()).collect();
    for (d, r) in &cfg.site_rules {
        let d = d.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase();
        match SiteRule::parse(r) {
            Some(SiteRule::Always) if !v.contains(&d) => v.push(d),
            Some(SiteRule::Off) | Some(SiteRule::On) => v.retain(|x| x != &d),
            _ => {}
        }
    }
    v
}

pub fn off_domains(cfg: &YtdlpConfig) -> Vec<String> {
    cfg.site_rules
        .iter()
        .filter(|(_, r)| SiteRule::parse(r) == Some(SiteRule::Off))
        .map(|(d, _)| d.trim().trim_start_matches("*.").trim_start_matches('.').to_ascii_lowercase())
        .collect()
}

// ---------------------------------------------------------------- binary detection

/// Locate yt-dlp: explicit `ytdlp.path`, else the LDM-managed copy, else `$PATH` /
/// `~/.local/bin/yt-dlp` (see [`crate::tools::resolve`]).
pub fn find(cfg: &YtdlpConfig) -> Option<Found> {
    tools::resolve(Tool::YtDlp, &cfg.path, &tools::Layout::from_config(cfg))
}

pub fn find_binary(cfg: &YtdlpConfig) -> Option<PathBuf> {
    find(cfg).map(|f| f.path)
}

/// deno for yt-dlp's JavaScript challenges (same lookup order, `ytdlp.deno_path`).
pub fn find_deno(cfg: &YtdlpConfig) -> Option<Found> {
    tools::resolve(Tool::Deno, &cfg.deno_path, &tools::Layout::from_config(cfg))
}

/// Running inside a Flatpak sandbox (`FLATPAK_ID` is set by `flatpak run`).
pub fn in_flatpak() -> bool {
    std::env::var_os("FLATPAK_ID").is_some_and(|v| !v.is_empty())
}

/// How to make yt-dlp available.
pub fn install_hint() -> String {
    "运行 `ldm tools install`（或在“选项 → 工具”中点“安装”）下载官方 yt-dlp 到 LDM 数据目录，或在“选项 → yt-dlp”中设置路径".into()
}

pub fn search_path(name: &str) -> Option<PathBuf> {
    tools::search_path(name)
}

/// `enabled` and a binary was found.
pub fn usable(cfg: &YtdlpConfig) -> Option<PathBuf> {
    if !cfg.enabled {
        return None;
    }
    find_binary(cfg)
}

/// yt-dlp jobs can run now or after a (consented / automatic) install of the managed copy.
pub fn available_or_installable(cfg: &YtdlpConfig) -> bool {
    cfg.enabled && (find(cfg).is_some() || (cfg.path.trim().is_empty() && AutoInstall::parse(&cfg.auto_install) != AutoInstall::Never))
}

/// `--js-runtimes` exists since yt-dlp 2025.11.12 (older releases get deno via `$PATH` only).
pub fn supports_js_runtimes(version: Option<&str>) -> bool {
    match version {
        Some(v) => !tools::version_newer("2025.11.12", v),
        None => false,
    }
}

/// Days a yt-dlp release must be old before "Requested format is not available" counts
/// as a sign of an outdated extractor (a fresh release saying so means the format is gone).
pub const FORMAT_HINT_DAYS: i64 = 14;

/// The error looks like an extractor that broke because the site changed (a newer yt-dlp
/// probably fixes it). `age_days` is the age of the yt-dlp release that produced it.
pub fn looks_outdated(msg: &str, age_days: Option<i64>) -> bool {
    let low = msg.to_ascii_lowercase();
    let strong = [
        "unable to extract",
        "signature extraction failed",
        "nsig extraction failed",
        "n challenge",
        "unable to decode n",
        "please update yt-dlp",
        "update to the latest version",
        "yt-dlp -u",
        "--update",
        "is out of date",
        "older than 90 days",
    ];
    if low.contains("not a bot") || low.contains("sign in to confirm") || low.contains("drm") || low.contains("private video") {
        return false;
    }
    if strong.iter().any(|k| low.contains(k)) || (low.contains("nsig") && (low.contains("fail") || low.contains("unable"))) {
        return true;
    }
    low.contains("requested format is not available") && age_days.is_some_and(|d| d > FORMAT_HINT_DAYS)
}

/// Days since a `YYYY.MM.DD[.N]` release (`None` when the version is not date-shaped).
pub fn version_age_days(version: &str, today: chrono::NaiveDate) -> Option<i64> {
    let mut it = version.trim().split('.');
    let y = it.next()?.parse().ok()?;
    let m = it.next()?.parse().ok()?;
    let d = it.next()?.parse().ok()?;
    let rel = chrono::NaiveDate::from_ymd_opt(y, m, d)?;
    Some((today - rel).num_days())
}

/// How the binary was installed – decides the "Update yt-dlp" command.
pub fn install_kind(bin: &Path) -> &'static str {
    let managed_root = tools::Layout::default_root();
    if bin.starts_with(&managed_root) {
        return "managed";
    }
    let real = std::fs::canonicalize(bin).unwrap_or_else(|_| bin.to_path_buf());
    let s = real.to_string_lossy();
    if s.contains("/site-packages/") || s.contains("/dist-packages/") || s.contains("/pipx/") {
        return "pip";
    }
    // a pip console-script is a tiny Python text file importing yt_dlp; the release
    // binaries (zipapp / PyInstaller) are megabytes and can update themselves with -U
    match std::fs::metadata(&real) {
        Ok(m) if m.len() < 16 * 1024 => {
            let head = std::fs::read(&real).unwrap_or_default();
            if String::from_utf8_lossy(&head).contains("yt_dlp") {
                "pip"
            } else {
                "unknown"
            }
        }
        Ok(_) => "binary",
        Err(_) => "unknown",
    }
}

pub fn update_command(bin: &Path) -> Vec<String> {
    match install_kind(bin) {
        "managed" => ["ldm", "tools", "update", "yt-dlp"].iter().map(|s| s.to_string()).collect(),
        "binary" => vec![bin.to_string_lossy().into_owned(), "-U".into()],
        _ => ["python3", "-m", "pip", "install", "-U", "--user", "yt-dlp"].iter().map(|s| s.to_string()).collect(),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Status {
    pub enabled: bool,
    pub path: Option<String>,
    /// `user` / `managed` / `system`
    pub source: Option<String>,
    pub deno_path: Option<String>,
    pub deno_source: Option<String>,
    pub auto_install: String,
    pub auto_update: String,
    pub version: Option<String>,
    pub age_days: Option<i64>,
    pub outdated: bool,
    pub install: Option<String>,
    pub usable: bool,
    pub auto_fallback: bool,
    pub cookies_from_browser: String,
}

/// Probe the binary (runs `yt-dlp --version`, a few hundred ms).
pub fn status_blocking(cfg: &YtdlpConfig) -> Status {
    let found = find(cfg);
    let deno = find_deno(cfg);
    let bin = found.as_ref().map(|f| f.path.clone());
    let version = bin.as_ref().and_then(|b| {
        std::process::Command::new(b)
            .arg("--version")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|v| !v.is_empty())
    });
    let age = version.as_deref().and_then(|v| version_age_days(v, chrono::Local::now().date_naive()));
    Status {
        enabled: cfg.enabled,
        usable: cfg.enabled && version.is_some(),
        path: bin.as_ref().map(|b| b.to_string_lossy().into_owned()),
        source: found.as_ref().map(|f| f.source.as_str().to_string()),
        deno_path: deno.as_ref().map(|f| f.path.to_string_lossy().into_owned()),
        deno_source: deno.as_ref().map(|f| f.source.as_str().to_string()),
        auto_install: cfg.auto_install.clone(),
        auto_update: cfg.auto_update.clone(),
        install: bin.as_ref().map(|b| install_kind(b).to_string()),
        outdated: age.map(|a| a > OUTDATED_DAYS).unwrap_or(false),
        age_days: age,
        version,
        auto_fallback: cfg.auto_fallback,
        cookies_from_browser: cfg.cookies_from_browser.clone(),
    }
}

// ---------------------------------------------------------------- formats (-J)

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct YtFormat {
    pub id: String,
    pub ext: String,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub vcodec: Option<String>,
    pub acodec: Option<String>,
    pub tbr: Option<f64>,
    pub filesize: Option<u64>,
    pub hdr: bool,
    pub language: Option<String>,
}

impl YtFormat {
    pub fn has_video(&self) -> bool {
        self.vcodec.as_deref().map(|c| c != "none").unwrap_or(self.height.is_some())
    }
    pub fn has_audio(&self) -> bool {
        self.acodec.as_deref().map(|c| c != "none").unwrap_or(false)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct YtInfo {
    pub title: Option<String>,
    pub duration: Option<f64>,
    pub webpage_url: Option<String>,
    pub extractor: Option<String>,
    pub is_live: bool,
    pub formats: Vec<YtFormat>,
    /// `_type == playlist`: item URLs (flat).
    pub playlist: bool,
    pub entries: Vec<String>,
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k).and_then(|x| x.as_str()).map(str::to_string).filter(|x| !x.is_empty())
}
fn f(v: &Value, k: &str) -> Option<f64> {
    v.get(k).and_then(|x| x.as_f64())
}

/// Parse `yt-dlp -J` output.
pub fn parse_info(v: &Value) -> YtInfo {
    let formats = v
        .get("formats")
        .and_then(|x| x.as_array())
        .map(|a| {
            a.iter()
                .map(|fm| YtFormat {
                    id: s(fm, "format_id").unwrap_or_default(),
                    ext: s(fm, "ext").unwrap_or_default(),
                    height: fm.get("height").and_then(|x| x.as_u64()).map(|h| h as u32),
                    fps: f(fm, "fps"),
                    vcodec: s(fm, "vcodec"),
                    acodec: s(fm, "acodec"),
                    tbr: f(fm, "tbr"),
                    filesize: fm.get("filesize").or(fm.get("filesize_approx")).and_then(|x| x.as_f64()).map(|x| x as u64),
                    hdr: s(fm, "dynamic_range").map(|d| d.to_ascii_uppercase().starts_with("HDR")).unwrap_or(false),
                    language: s(fm, "language"),
                })
                .collect()
        })
        .unwrap_or_default();
    let playlist = v.get("_type").and_then(|t| t.as_str()) == Some("playlist");
    let entries = v
        .get("entries")
        .and_then(|e| e.as_array())
        .map(|a| a.iter().filter_map(|e| s(e, "url").or_else(|| s(e, "webpage_url"))).collect())
        .unwrap_or_default();
    YtInfo {
        title: s(v, "title").or_else(|| s(v, "fulltitle")),
        duration: f(v, "duration"),
        webpage_url: s(v, "webpage_url"),
        extractor: s(v, "extractor_key").or_else(|| s(v, "extractor")),
        // `live_status` (newer yt-dlp) is authoritative; `is_live` for older releases
        is_live: match v.get("live_status").and_then(|x| x.as_str()) {
            Some(st) => st == "is_live",
            None => v.get("is_live").and_then(|x| x.as_bool()).unwrap_or(false),
        },
        formats,
        playlist,
        entries,
    }
}

fn short_codec(c: &str) -> String {
    c.split('.').next().unwrap_or(c).to_string()
}

/// Quality rows for the in-page menu: one per video height (best SDR representative),
/// highest first, plus one audio-only row (`height = 0`, kind `ytdlp-audio`; the UI prefixes "audio only").
pub fn qualities(info: &YtInfo) -> Vec<QualityRow> {
    let mut by_h: BTreeMap<u32, &YtFormat> = BTreeMap::new();
    for fm in info.formats.iter().filter(|f| f.has_video() && f.height.is_some()) {
        let h = fm.height.unwrap_or(0);
        let better = |a: &YtFormat, b: &YtFormat| {
            // SDR first, then higher fps, then AVC (most compatible), then bitrate
            let k = |x: &YtFormat| {
                (
                    !x.hdr,
                    (x.fps.unwrap_or(0.0) * 10.0) as i64,
                    x.vcodec.as_deref().map(|c| c.starts_with("avc")).unwrap_or(false),
                    (x.tbr.unwrap_or(0.0) * 10.0) as i64,
                )
            };
            k(a) > k(b)
        };
        match by_h.get(&h) {
            Some(cur) if !better(fm, cur) => {}
            _ => {
                by_h.insert(h, fm);
            }
        }
    }
    let mut rows: Vec<QualityRow> = by_h
        .into_iter()
        .rev()
        .map(|(h, fm)| {
            let fps = fm.fps.filter(|f| *f > 30.5).map(|f| format!("{}", f.round() as u32)).unwrap_or_default();
            let mut label = format!("{h}p{fps}");
            if let Some(c) = fm.vcodec.as_deref() {
                label.push_str(&format!(" · {}", short_codec(c)));
            }
            if fm.hdr {
                label.push_str(" · HDR");
            }
            QualityRow { label, height: Some(h), bandwidth: (fm.tbr.unwrap_or(0.0) * 1000.0) as u64, kind: "ytdlp".into() }
        })
        .collect();
    if let Some(a) = info.formats.iter().filter(|f| f.has_audio() && !f.has_video()).max_by_key(|f| (f.tbr.unwrap_or(0.0) * 10.0) as i64) {
        let codec = a.acodec.as_deref().map(short_codec).unwrap_or_default();
        rows.push(QualityRow {
            label: format!("{codec} {}k", a.tbr.unwrap_or(0.0).round() as u64),
            height: Some(0),
            bandwidth: (a.tbr.unwrap_or(0.0) * 1000.0) as u64,
            kind: "ytdlp-audio".into(),
        });
    }
    rows
}

/// `-f` selector: best video ≤ `height` (default `max_height`), SDR preferred, plus audio
/// chosen by `media.audio_preference` (AAC/m4a first by default). `Some(0)` = audio only.
pub fn format_selector(height: Option<u32>, max_height: u32, audio: &AudioPreference) -> String {
    let audios: Vec<String> = match audio {
        AudioPreference::Aac => vec!["ba[acodec^=mp4a]".into(), "ba[ext=m4a]".into(), "ba".into()],
        AudioPreference::MostChannels => vec!["ba".into()],
        AudioPreference::Language(l) => {
            vec![format!("ba[language^={l}][acodec^=mp4a]"), format!("ba[language^={l}]"), "ba[acodec^=mp4a]".into(), "ba".into()]
        }
    };
    if height == Some(0) {
        return audios.join("/");
    }
    let h = height.filter(|h| *h > 0).unwrap_or(max_height.max(144));
    let mut alts = vec![];
    for v in [format!("bv*[height<={h}][dynamic_range=SDR]"), format!("bv*[height<={h}]")] {
        for a in &audios {
            alts.push(format!("{v}+{a}"));
        }
    }
    alts.push(format!("b[height<={h}]"));
    alts.push("bv*+ba".into());
    alts.push("b".into());
    alts.join("/")
}

// ---------------------------------------------------------------- progress

#[derive(Debug, Clone, Default, PartialEq)]
pub struct YtProgress {
    pub downloaded: Option<u64>,
    pub total: Option<u64>,
    pub speed: Option<f64>,
    pub eta: Option<u64>,
    pub status: String,
    pub format_id: String,
}

fn num(t: &str) -> Option<f64> {
    match t {
        "NA" | "None" | "none" | "" => None,
        x => x.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0),
    }
}

/// Parse one `LDMP …` line (see [`PROGRESS_TEMPLATE`]).
pub fn parse_progress(line: &str) -> Option<YtProgress> {
    let rest = line.trim().strip_prefix(PROGRESS_PREFIX)?.trim_start();
    let t: Vec<&str> = rest.split_whitespace().collect();
    if t.len() < 6 {
        return None;
    }
    let total = num(t[1]).or_else(|| num(t[2]));
    Some(YtProgress {
        downloaded: num(t[0]).map(|v| v as u64),
        total: total.map(|v| v as u64),
        speed: num(t[3]),
        eta: num(t[4]).map(|v| v as u64),
        status: t[5].to_string(),
        format_id: t.get(6).map(|s| s.to_string()).unwrap_or_default(),
    })
}

/// Sums per-format progress (video and audio are downloaded one after the other).
#[derive(Debug, Default)]
pub struct ProgressAcc {
    per: Vec<(String, u64, Option<u64>)>,
}

impl ProgressAcc {
    pub fn update(&mut self, p: &YtProgress) -> (u64, Option<u64>) {
        let got = p.downloaded.unwrap_or(0);
        match self.per.iter_mut().find(|(id, ..)| *id == p.format_id) {
            Some(e) => {
                e.1 = got.max(if p.status == "finished" { e.2.unwrap_or(got) } else { 0 });
                e.2 = p.total.or(e.2);
            }
            None => self.per.push((p.format_id.clone(), got, p.total)),
        }
        let got = self.per.iter().map(|e| e.1).sum();
        let total = self.per.iter().map(|e| e.2).sum::<Option<u64>>();
        (got, total)
    }
}

// ---------------------------------------------------------------- errors / fallback

/// Turn yt-dlp's stderr into an error; never retried by LDM (yt-dlp retries itself).
pub fn error_from_output(stderr_tail: &[String], code: Option<i32>) -> LdmError {
    let msg = stderr_tail
        .iter()
        .rev()
        .find(|l| l.starts_with("ERROR:"))
        .map(|l| l.trim_start_matches("ERROR:").trim().to_string())
        .or_else(|| stderr_tail.last().cloned())
        .unwrap_or_else(|| format!("exited with status {}", code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into())));
    let low = msg.to_ascii_lowercase();
    if low.contains("drm") {
        return LdmError::UnsupportedKey(format!("yt-dlp: {msg}"));
    }
    if low.contains("not a bot") || low.contains("sign in to confirm") {
        return LdmError::Ytdlp(format!("{msg}（站点要求登录/人机验证；LDM 不会绕过。可在“选项 → yt-dlp”启用从浏览器读取 Cookie 后重试）"));
    }
    LdmError::Ytdlp(msg)
}

/// Why an LDM failure may be retried once through yt-dlp (`None` = not eligible).
///
/// Eligible: HTTP 403 / 410 (expired or bound signed URL), a body that is not media
/// (HTML instead of a video, VAST instead of a playlist), manifest parse errors and "no
/// media found". Not eligible: network / DNS / timeouts, disk full / permissions, user
/// cancel, DRM (yt-dlp cannot either), checksum, security rejections.
pub fn fallback_reason(e: &LdmError) -> Option<String> {
    match e {
        LdmError::HttpStatus(c @ (403 | 410), _) => Some(format!("HTTP {c}")),
        LdmError::HttpStatus(c, m) if m.to_ascii_lowercase().contains("expired") => Some(format!("HTTP {c} (expired URL)")),
        LdmError::Format(m) => {
            let low = m.to_ascii_lowercase();
            if low.contains("drm") || low.contains("encrypt") || low.contains("disk") {
                None
            } else if low.starts_with("not media") || low.starts_with("no media") {
                Some(m.clone())
            } else if low.contains("not media") || low.contains("html") {
                Some(format!("not media: {m}"))
            } else if low.contains("no media") || low.contains("no variant") || low.contains("no candidate") {
                Some(format!("no media: {m}"))
            } else {
                Some(format!("manifest: {m}"))
            }
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- process

/// JavaScript runtime handed to yt-dlp (YouTube's challenges need one since 2025.11).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct JsRuntime {
    pub deno: Option<PathBuf>,
    /// the yt-dlp in use understands `--js-runtimes`
    pub flag: bool,
}

impl JsRuntime {
    fn args(&self) -> Vec<String> {
        match (&self.deno, self.flag) {
            (Some(d), true) => vec!["--js-runtimes".into(), format!("deno:{}", d.display())],
            _ => vec![],
        }
    }

    /// `$PATH` for the child with deno's directory first (older yt-dlp finds it there).
    pub fn child_path(&self) -> Option<std::ffi::OsString> {
        let dir = self.deno.as_ref()?.parent()?.to_path_buf();
        let mut v = vec![dir];
        if let Some(p) = std::env::var_os("PATH") {
            v.extend(std::env::split_paths(&p));
        }
        std::env::join_paths(v).ok()
    }

    fn apply(&self, cmd: &mut Command) {
        if let Some(p) = self.child_path() {
            cmd.env("PATH", p);
        }
    }
}

/// Common arguments: config isolation, cookies, proxy, JS runtime.
fn base_args(cfg: &Config, proxy: &str, js: &JsRuntime) -> Vec<String> {
    let mut a: Vec<String> = vec!["--ignore-config".into(), "--no-warnings".into()];
    a.extend(js.args());
    let browser = cfg.ytdlp.cookies_from_browser.trim();
    if !browser.is_empty() && browser != "off" && browser != "none" {
        a.push("--cookies-from-browser".into());
        a.push(browser.into());
    }
    match proxy {
        "" | "system" => {}
        "none" => {
            a.push("--proxy".into());
            a.push(String::new());
        }
        p => {
            a.push("--proxy".into());
            a.push(p.into());
        }
    }
    a
}

/// Short-lived cache of `-J` results (the menu probes, then the job starts within seconds).
#[derive(Default)]
pub struct InfoCache {
    map: Mutex<HashMap<String, (Instant, YtInfo)>>,
}

pub const INFO_TTL: Duration = Duration::from_secs(120);

impl InfoCache {
    pub fn get(&self, url: &str) -> Option<YtInfo> {
        let mut m = self.map.lock().unwrap();
        m.retain(|_, (t, _)| t.elapsed() < INFO_TTL);
        m.get(url).map(|(_, i)| i.clone())
    }
    pub fn put(&self, url: &str, info: &YtInfo) {
        self.map.lock().unwrap().insert(url.to_string(), (Instant::now(), info.clone()));
    }
}

/// `yt-dlp -J --no-playlist <url>` (or `--flat-playlist --yes-playlist` for `playlist`).
pub async fn fetch_info(bin: &Path, js: &JsRuntime, cfg: &Config, proxy: &str, url: &str, playlist: bool) -> Result<YtInfo> {
    let mut args = base_args(cfg, proxy, js);
    args.push("-J".into());
    if playlist {
        args.extend(["--flat-playlist".into(), "--yes-playlist".into()]);
    } else {
        args.push("--no-playlist".into());
    }
    args.extend(cfg.ytdlp.extra_args.iter().cloned());
    args.push("--".into());
    args.push(url.into());
    let mut cmd = Command::new(bin);
    js.apply(&mut cmd);
    let child = cmd.args(&args).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).output();
    let out = match tokio::time::timeout(Duration::from_secs(90), child).await {
        Ok(r) => r.map_err(|e| LdmError::Ytdlp(format!("cannot run {}: {e}", bin.display())))?,
        Err(_) => return Err(LdmError::Timeout("yt-dlp -J took longer than 90 s".into())),
    };
    if !out.status.success() {
        let tail: Vec<String> = String::from_utf8_lossy(&out.stderr).lines().map(str::to_string).collect();
        return Err(error_from_output(&tail, out.status.code()));
    }
    let v: Value = serde_json::from_slice(&out.stdout).map_err(|e| LdmError::Ytdlp(format!("unreadable -J output: {e}")))?;
    Ok(parse_info(&v))
}

/// Everything a yt-dlp download needs.
#[derive(Debug, Clone, Default)]
pub struct YtJobSpec {
    pub url: String,
    pub dir: PathBuf,
    /// File stem (already sanitised); yt-dlp appends `.%(ext)s`.
    pub stem: String,
    pub work_dir: PathBuf,
    pub height: Option<u32>,
    pub rate_limit_bps: u64,
    pub proxy: String,
    /// `-J` said the stream is live: record (elapsed time + bytes, "停止录制" = SIGINT).
    pub live: bool,
    pub js: JsRuntime,
}

/// Arguments for the download run (exposed for tests).
pub fn download_args(cfg: &Config, spec: &YtJobSpec) -> Vec<String> {
    let mut a = base_args(cfg, &spec.proxy, &spec.js);
    let container = if cfg.media.output_container.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" };
    let fmt = format_selector(spec.height, cfg.ytdlp.max_height, &cfg.media.audio_pref());
    a.extend(
        [
            "--no-playlist",
            "--newline",
            "--progress",
            "--no-simulate",
            "--continue",
            "--no-mtime",
            "--progress-template",
            PROGRESS_TEMPLATE,
            "--print",
            FILE_TEMPLATE,
            "-f",
            &fmt,
            "--merge-output-format",
            container,
            "--retries",
            &cfg.engine.max_retries.max(1).to_string(),
            "--fragment-retries",
            &cfg.engine.max_retries.max(1).to_string(),
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    // within the chosen height/fps prefer H.264 (+AAC): what the menu label shows, plays everywhere
    a.push("-S".into());
    a.push(if cfg.media.audio_pref() == AudioPreference::Aac { "res,fps,vcodec:h264,acodec:aac" } else { "res,fps,vcodec:h264" }.into());
    a.push("-P".into());
    a.push(format!("home:{}", spec.dir.display()));
    a.push("-P".into());
    a.push(format!("temp:{}", spec.work_dir.display()));
    // `%` in a title would be read as a template field
    a.push("-o".into());
    a.push(format!("{}.%(ext)s", spec.stem.replace('%', "%%")));
    if spec.rate_limit_bps > 0 {
        a.push("--limit-rate".into());
        a.push(spec.rate_limit_bps.to_string());
    }
    if cfg.media.ffmpeg_path != "ffmpeg" && !cfg.media.ffmpeg_path.is_empty() {
        a.push("--ffmpeg-location".into());
        a.push(cfg.media.ffmpeg_path.clone());
    }
    a.extend(cfg.ytdlp.extra_args.iter().cloned());
    a.push("--".into());
    a.push(spec.url.clone());
    a
}

/// Signal yt-dlp's whole process group. yt-dlp is spawned as a group leader, and the
/// helpers it starts (ffmpeg for live streams / HLS, aria2c, …) stay in that group, so
/// they stop too instead of being orphaned and writing on forever.
fn signal_group(child: &tokio::process::Child, sig: i32) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), sig);
        }
    }
}

/// Signal yt-dlp itself only (not its helpers).
fn signal_pid(child: &tokio::process::Child, sig: i32) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

/// `time=HH:MM:SS.xx` from an ffmpeg stats line (yt-dlp lets ffmpeg write to its stderr).
pub fn ffmpeg_stats_time(line: &str) -> Option<f64> {
    let i = line.rfind("time=")?;
    let t = line[i + 5..].split_whitespace().next()?;
    let mut secs = 0.0;
    for (n, p) in t.split(':').enumerate() {
        if n > 2 {
            return None;
        }
        secs = secs * 60.0 + p.parse::<f64>().ok().filter(|v| v.is_finite() && *v >= 0.0)?;
    }
    Some(secs)
}

/// Sum of file sizes below `dir` (yt-dlp's `.part` / fragment files while recording).
fn tree_size(dir: &Path) -> u64 {
    let mut n = 0;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            match e.file_type() {
                Ok(t) if t.is_dir() => n += tree_size(&e.path()),
                Ok(_) => n += e.metadata().map(|m| m.len()).unwrap_or(0),
                Err(_) => {}
            }
        }
    }
    n
}

/// After a stop where yt-dlp did not report a final file: the largest recorded media file
/// (`<stem>….part`, `.ts`, `.mp4` …) in the work / destination dirs.
fn recorded_leftover(spec: &YtJobSpec) -> Option<PathBuf> {
    let mut best: Option<(u64, PathBuf)> = None;
    for d in [&spec.work_dir, &spec.dir] {
        let Ok(rd) = std::fs::read_dir(d) else { continue };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.starts_with(&spec.stem) || name.ends_with(".ytdl") || name.ends_with(".json") || name.ends_with(".vtt") {
                continue;
            }
            let len = e.metadata().map(|m| m.len()).unwrap_or(0);
            if len > 0 && best.as_ref().map(|b| len > b.0).unwrap_or(true) {
                best = Some((len, e.path()));
            }
        }
    }
    best.map(|b| b.1)
}

/// Read a child stream as lines split on `\n` *and* `\r` (ffmpeg stats lines end in `\r`).
async fn pump_lines(
    mut r: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    is_err: bool,
    tx: tokio::sync::mpsc::UnboundedSender<(bool, String)>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = vec![0u8; 8192];
    let mut cur: Vec<u8> = vec![];
    loop {
        let n = match r.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        for &b in &buf[..n] {
            if b == b'\n' || b == b'\r' {
                if !cur.is_empty() && tx.send((is_err, String::from_utf8_lossy(&cur).into_owned())).is_err() {
                    return;
                }
                cur.clear();
            } else if cur.len() < 64 * 1024 {
                cur.push(b);
            }
        }
    }
    if !cur.is_empty() {
        let _ = tx.send((is_err, String::from_utf8_lossy(&cur).into_owned()));
    }
}

fn terminate(child: &tokio::process::Child) {
    // SIGTERM lets yt-dlp flush its .part file; the next run continues it (--continue)
    signal_group(child, libc::SIGTERM);
}

/// Run the download; returns the final file and its size. Cancel (pause/stop) terminates
/// the process and leaves the `.part` files in `work_dir` for resuming.
pub async fn run_download(env: &JobEnv, bin: &Path, spec: &YtJobSpec) -> Result<(PathBuf, u64)> {
    std::fs::create_dir_all(&spec.work_dir)?;
    std::fs::create_dir_all(&spec.dir)?;
    let args = download_args(&env.cfg, spec);
    let mut cmd = Command::new(bin);
    spec.js.apply(&mut cmd);
    let mut child = cmd
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0)
        .spawn()
        .map_err(|e| LdmError::Ytdlp(format!("cannot run {}: {e}", bin.display())))?;
    let progress = env.progress.clone();
    progress.set_phase("Probing");
    let live = spec.live;
    if live {
        progress.set_live(true);
        progress.set_total(None);
        progress.set_recorded_secs(0.0);
        progress.set_phase("Recording");
    }
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(bool, String)>();
    for (is_err, r) in [
        (false, child.stdout.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)),
        (true, child.stderr.take().map(|o| Box::new(o) as Box<dyn tokio::io::AsyncRead + Unpin + Send>)),
    ] {
        if let Some(r) = r {
            tokio::spawn(pump_lines(r, is_err, tx.clone()));
        }
    }
    drop(tx);
    let mut acc = ProgressAcc::default();
    let mut final_path: Option<PathBuf> = None;
    let mut tail: Vec<String> = vec![];
    let mut canceled = false;
    let mut kill_at: Option<tokio::time::Instant> = None;
    // live recording
    let started = Instant::now();
    let cap = env.cfg.media.live_max_duration as f64;
    let mut ff_time: Option<f64> = None;
    let mut stopping = false;
    // graceful stop escalation: SIGINT yt-dlp → (60 s) SIGINT the group → (15 s) SIGKILL
    let mut stop_stage = 0u8;
    let mut stop_at: Option<tokio::time::Instant> = None;
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        let deadline = kill_at.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        let stop_deadline = stop_at.unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(3600));
        let mut graceful = false;
        tokio::select! {
            l = rx.recv() => {
                let Some((is_err, line)) = l else { break };
                if let Some(p) = parse_progress(&line) {
                    let (got, total) = acc.update(&p);
                    if live {
                        progress.set_got(got.max(progress.got()));
                    } else {
                        if progress.phase() != "Downloading" {
                            progress.set_phase("Downloading");
                        }
                        progress.set_got(got);
                        progress.set_total(total);
                    }
                } else if let Some(fp) = line.strip_prefix(FILE_PREFIX) {
                    final_path = Some(PathBuf::from(fp.trim()));
                } else if line.starts_with("[Merger]") || line.starts_with("[VideoConvertor]") || line.starts_with("[ExtractAudio]") || line.starts_with("[FixupM3u8]") {
                    progress.set_phase("Assembling");
                } else if let (true, Some(t)) = (live, ffmpeg_stats_time(&line)) {
                    ff_time = Some(t);
                } else if is_err || line.starts_with("ERROR") {
                    tail.push(line);
                    if tail.len() > 20 {
                        tail.remove(0);
                    }
                }
            }
            _ = tick.tick(), if live && !canceled && !stopping => {
                let size = tree_size(&spec.work_dir);
                if size > progress.got() {
                    progress.set_got(size);
                }
                let rec = ff_time.unwrap_or_else(|| started.elapsed().as_secs_f64());
                progress.set_recorded_secs(rec);
                if cap > 0.0 && rec >= cap {
                    info!(secs = rec, cap, "live recording reached media.live_max_duration");
                    graceful = true;
                }
            }
            _ = progress.stop_requested_wait(), if live && !canceled && !stopping => {
                graceful = true;
            }
            _ = tokio::time::sleep_until(stop_deadline), if stop_at.is_some() && !canceled => {
                stop_stage += 1;
                if stop_stage == 1 {
                    warn!("yt-dlp did not finish 60 s after SIGINT; interrupting its helpers too");
                    signal_group(&child, libc::SIGINT);
                    stop_at = Some(tokio::time::Instant::now() + Duration::from_secs(15));
                } else {
                    warn!("yt-dlp still running after stop; killing it");
                    signal_group(&child, libc::SIGKILL);
                    let _ = child.start_kill();
                    break;
                }
            }
            _ = env.cancel.cancelled(), if !canceled => {
                canceled = true;
                terminate(&child);
                kill_at = Some(tokio::time::Instant::now() + Duration::from_secs(5));
            }
            _ = tokio::time::sleep_until(deadline), if kill_at.is_some() => {
                signal_group(&child, libc::SIGKILL);
                let _ = child.start_kill();
                // a helper outside the group may still hold the pipes open: stop
                // reading rather than wait for an EOF that never comes
                break;
            }
        }
        if graceful && !stopping {
            // like Ctrl-C: yt-dlp asks its ffmpeg to finish ('q'), treats an interrupted live
            // download as complete and runs its post-processing (merge / fixup)
            stopping = true;
            progress.request_stop_recording();
            progress.set_recorded_secs(ff_time.unwrap_or_else(|| started.elapsed().as_secs_f64()));
            info!(secs = progress.recorded_secs(), "stopping live recording (SIGINT to yt-dlp)");
            signal_pid(&child, libc::SIGINT);
            stop_at = Some(tokio::time::Instant::now() + Duration::from_secs(60));
        }
    }
    let status = child.wait().await?;
    if canceled {
        return Err(LdmError::Canceled);
    }
    if stopping {
        // any helper left behind (it should have exited with yt-dlp) must not keep writing
        signal_group(&child, libc::SIGKILL);
        let path = match final_path.filter(|p| p.exists()) {
            Some(p) => p,
            None => finalize_leftover(env, spec).await.map_err(|e| {
                if status.success() {
                    e
                } else {
                    LdmError::Ytdlp(format!("stopped recording, but no usable file: {e}"))
                }
            })?,
        };
        let size = std::fs::metadata(&path).map_err(|e| LdmError::Ytdlp(format!("output {} missing: {e}", path.display())))?.len();
        progress.set_got(size);
        progress.set_total(Some(size));
        return Ok((path, size));
    }
    if !status.success() {
        return Err(error_from_output(&tail, status.code()));
    }
    let path = final_path.ok_or_else(|| LdmError::Ytdlp("yt-dlp finished without reporting the output file".into()))?;
    let size = std::fs::metadata(&path).map_err(|e| LdmError::Ytdlp(format!("output {} missing: {e}", path.display())))?.len();
    progress.set_got(size);
    progress.set_total(Some(size));
    Ok((path, size))
}

/// yt-dlp exited after "停止录制" without reporting a file: remux what it recorded.
async fn finalize_leftover(env: &JobEnv, spec: &YtJobSpec) -> Result<PathBuf> {
    let src = recorded_leftover(spec).ok_or_else(|| LdmError::Ytdlp("nothing was recorded".into()))?;
    let ffmpeg = &env.cfg.media.ffmpeg_path;
    if !crate::media::mux::ffmpeg_available(ffmpeg).await {
        return Err(LdmError::Ffmpeg("ffmpeg is needed to finalise the recording".into()));
    }
    let container = if env.cfg.media.output_container.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" };
    let out = spec.dir.join(format!("{}.{container}", spec.stem));
    let tmp = spec.work_dir.join(format!("finalize.{container}"));
    crate::media::mux::remux(ffmpeg, std::slice::from_ref(&src), &tmp).await?;
    std::fs::rename(&tmp, &out).or_else(|_| std::fs::copy(&tmp, &out).map(|_| ()))?;
    let _ = std::fs::remove_file(&src);
    info!(file = %out.display(), "live recording finalised from yt-dlp's partial file");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_rules_and_youtube_routing() {
        let mut c = YtdlpConfig::default();
        assert_eq!(site_rule(&c, "https://www.youtube.com/watch?v=x"), SiteRule::Always);
        assert_eq!(site_rule(&c, "https://youtu.be/x"), SiteRule::Always);
        assert_eq!(site_rule(&c, "https://music.youtube.com/watch?v=x"), SiteRule::Always);
        assert_eq!(site_rule(&c, "https://m.youtube.com/shorts/x"), SiteRule::Always);
        assert_eq!(site_rule(&c, "https://notyoutube.com/x"), SiteRule::On);
        assert_eq!(site_rule(&c, "https://vimeo.com/1"), SiteRule::On);
        c.site_rules.insert("vimeo.com".into(), "always".into());
        c.site_rules.insert("youtube.com".into(), "off".into());
        c.site_rules.insert("music.youtube.com".into(), "always".into());
        c.site_rules.insert("example.org".into(), "off".into());
        assert_eq!(site_rule(&c, "https://player.vimeo.com/1"), SiteRule::Always);
        assert_eq!(site_rule(&c, "https://www.youtube.com/watch?v=x"), SiteRule::Off);
        // longest domain wins
        assert_eq!(site_rule(&c, "https://music.youtube.com/watch?v=x"), SiteRule::Always);
        assert_eq!(site_rule(&c, "https://cdn.example.org/a.mp4"), SiteRule::Off);
        let al = always_domains(&c);
        assert!(al.contains(&"vimeo.com".to_string()) && !al.contains(&"youtube.com".to_string()));
        assert!(al.contains(&"youtu.be".to_string()));
        assert_eq!(off_domains(&c), vec!["example.org".to_string(), "youtube.com".to_string()]);
    }

    #[test]
    fn progress_lines() {
        let p = parse_progress("LDMP 1048576 4194304 NA 524288.5 6 downloading 137").unwrap();
        assert_eq!(p.downloaded, Some(1048576));
        assert_eq!(p.total, Some(4194304));
        assert_eq!(p.speed, Some(524288.5));
        assert_eq!(p.eta, Some(6));
        assert_eq!(p.format_id, "137");
        // estimate used when the exact size is unknown; NA everywhere is fine
        let p = parse_progress("LDMP 10 NA 100.0 NA NA downloading 140").unwrap();
        assert_eq!((p.downloaded, p.total, p.speed, p.eta), (Some(10), Some(100), None, None));
        assert!(parse_progress("[download]  10.0% of 4.00MiB").is_none());
        assert!(parse_progress("LDMP x").is_none());
        // video then audio: sums
        let mut acc = ProgressAcc::default();
        assert_eq!(acc.update(&parse_progress("LDMP 50 100 NA 1 1 downloading 137").unwrap()), (50, Some(100)));
        assert_eq!(acc.update(&parse_progress("LDMP 100 100 NA 1 0 finished 137").unwrap()), (100, Some(100)));
        assert_eq!(acc.update(&parse_progress("LDMP 5 20 NA 1 1 downloading 140").unwrap()), (105, Some(120)));
    }

    fn info() -> YtInfo {
        let v: Value = serde_json::from_str(include_str!("../tests/fixtures/ytdlp-info.json")).unwrap();
        parse_info(&v)
    }

    #[test]
    fn format_listing() {
        let i = info();
        assert_eq!(i.title.as_deref(), Some("Test clip: Big Buck Bunny (trailer) | 100% free"));
        let q = qualities(&i);
        let labels: Vec<&str> = q.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(labels, vec!["2160p60 · vp09 · HDR", "1080p60 · avc1", "720p · avc1", "360p · avc1", "mp4a 129k"]);
        assert_eq!(q.last().unwrap().height, Some(0));
    }

    #[test]
    fn selector_respects_height_and_audio_preference() {
        let s = format_selector(None, 1080, &AudioPreference::Aac);
        assert!(s.starts_with("bv*[height<=1080][dynamic_range=SDR]+ba[acodec^=mp4a]/"), "{s}");
        assert!(s.ends_with("/b"));
        let s = format_selector(Some(360), 1080, &AudioPreference::MostChannels);
        assert!(s.starts_with("bv*[height<=360][dynamic_range=SDR]+ba/bv*[height<=360]+ba/b[height<=360]"), "{s}");
        assert_eq!(format_selector(Some(0), 1080, &AudioPreference::Aac), "ba[acodec^=mp4a]/ba[ext=m4a]/ba");
        assert!(format_selector(None, 720, &AudioPreference::Language("de".into()))
            .starts_with("bv*[height<=720][dynamic_range=SDR]+ba[language^=de][acodec^=mp4a]/"));
    }

    #[test]
    fn fallback_eligibility_matrix() {
        use std::io;
        let yes = [
            LdmError::HttpStatus(403, "Forbidden".into()),
            LdmError::HttpStatus(410, "Gone".into()),
            LdmError::HttpStatus(400, "signature expired".into()),
            LdmError::Format("not media: server returned an HTML page".into()),
            LdmError::Format("not an M3U8 playlist".into()),
            LdmError::Format("MPD parse error: missing Period".into()),
            LdmError::Format("no media candidates".into()),
        ];
        for e in &yes {
            assert!(fallback_reason(e).is_some(), "{e} should be eligible");
        }
        assert_eq!(
            fallback_reason(&LdmError::Format("not media: server returned an HTML page".into())).unwrap(),
            "not media: server returned an HTML page"
        );
        let no = [
            LdmError::Network("dns error: failed to lookup address".into()),
            LdmError::Timeout("read".into()),
            LdmError::HttpStatus(404, "Not Found".into()),
            LdmError::HttpStatus(401, "Unauthorized".into()),
            LdmError::HttpStatus(503, "busy".into()),
            LdmError::Io(io::Error::from_raw_os_error(libc::ENOSPC)),
            LdmError::Io(io::Error::from(io::ErrorKind::PermissionDenied)),
            LdmError::Canceled,
            LdmError::UnsupportedKey("DRM ContentProtection present".into()),
            LdmError::Key("bad key".into()),
            LdmError::Format("DRM-protected stream".into()),
            LdmError::Checksum { expected: "a".into(), actual: "b".into() },
            LdmError::Security("blocked".into()),
            LdmError::Ytdlp("already via yt-dlp".into()),
        ];
        for e in &no {
            assert!(fallback_reason(e).is_none(), "{e} must not fall back");
        }
    }

    #[test]
    fn errors_from_stderr() {
        let e = error_from_output(
            &["[youtube] x: Downloading webpage".into(), "ERROR: [youtube] x: Sign in to confirm you’re not a bot".into()],
            Some(1),
        );
        assert!(matches!(&e, LdmError::Ytdlp(m) if m.contains("不会绕过")), "{e}");
        let e = error_from_output(&["ERROR: [generic] This video is DRM protected".into()], Some(1));
        assert!(matches!(e, LdmError::UnsupportedKey(_)));
        assert!(fallback_reason(&e).is_none());
        let e = error_from_output(&[], None);
        assert_eq!(e.to_string(), "yt-dlp: exited with status signal");
    }

    #[test]
    fn outdated_error_detection() {
        assert!(looks_outdated("[youtube] abc: Unable to extract uploader id; please report this issue", Some(1)));
        assert!(looks_outdated("Signature extraction failed: Some formats may be missing", None));
        assert!(looks_outdated("[youtube] x: nsig extraction failed: You may experience throttling", Some(3)));
        assert!(looks_outdated("Your yt-dlp version is older than 90 days; please update yt-dlp", None));
        // format errors only count for an old release
        assert!(looks_outdated("Requested format is not available. Use --list-formats", Some(40)));
        assert!(!looks_outdated("Requested format is not available. Use --list-formats", Some(3)));
        assert!(!looks_outdated("Requested format is not available", None));
        // not extractor problems
        assert!(!looks_outdated("Sign in to confirm you're not a bot. Unable to extract", Some(100)));
        assert!(!looks_outdated("This video is DRM protected", Some(100)));
        assert!(!looks_outdated("HTTP Error 404: Not Found", Some(100)));
        assert!(!looks_outdated("Private video. Sign in if you've been granted access", Some(100)));
    }

    #[test]
    fn js_runtime_args() {
        assert!(supports_js_runtimes(Some("2025.11.12")));
        assert!(supports_js_runtimes(Some("2026.10.21")));
        assert!(!supports_js_runtimes(Some("2025.10.22")));
        assert!(!supports_js_runtimes(None));
        let cfg = Config::default();
        let js = JsRuntime { deno: Some("/t/deno/2.9.8/deno".into()), flag: true };
        let spec = YtJobSpec { url: "https://youtu.be/x".into(), js: js.clone(), ..Default::default() };
        let j = download_args(&cfg, &spec).join(" ");
        assert!(j.contains("--js-runtimes deno:/t/deno/2.9.8/deno"), "{j}");
        let p = js.child_path().unwrap();
        assert!(p.to_string_lossy().starts_with("/t/deno/2.9.8:") || p.to_string_lossy() == "/t/deno/2.9.8");
        // old yt-dlp: no flag (deno only via PATH)
        let spec = YtJobSpec { js: JsRuntime { flag: false, ..js }, ..spec };
        assert!(!download_args(&cfg, &spec).join(" ").contains("--js-runtimes"));
        assert!(!download_args(&cfg, &YtJobSpec::default()).join(" ").contains("--js-runtimes"));
    }

    #[test]
    fn version_age() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 10, 18).unwrap();
        assert_eq!(version_age_days("2026.10.10", d), Some(8));
        assert_eq!(version_age_days("2025.12.31.1", d), Some(291));
        assert_eq!(version_age_days("git-abc", d), None);
    }

    #[test]
    fn download_arguments() {
        let mut cfg = Config::default();
        let spec = YtJobSpec {
            url: "https://youtu.be/x".into(),
            dir: "/d".into(),
            stem: "100% fun".into(),
            work_dir: "/w".into(),
            ..Default::default()
        };
        let a = download_args(&cfg, &spec);
        let j = a.join(" ");
        assert!(j.contains("--no-playlist") && j.contains("--continue") && j.contains("--merge-output-format mp4"), "{j}");
        assert!(j.contains("-P home:/d -P temp:/w -o 100%% fun.%(ext)s"), "{j}");
        assert!(!j.contains("cookies"), "cookies are off by default");
        assert!(j.contains("-S res,fps,vcodec:h264,acodec:aac"), "{j}");
        assert_eq!(a.last().unwrap(), "https://youtu.be/x");
        assert_eq!(a[a.len() - 2], "--");
        cfg.ytdlp.cookies_from_browser = "firefox".into();
        cfg.ytdlp.extra_args = vec!["--embed-metadata".into()];
        cfg.media.output_container = "mkv".into();
        let j = download_args(&cfg, &YtJobSpec { rate_limit_bps: 1000, proxy: "socks5://127.0.0.1:1080".into(), ..spec }).join(" ");
        assert!(
            j.contains("--cookies-from-browser firefox") && j.contains("--embed-metadata --") && j.contains("--merge-output-format mkv"),
            "{j}"
        );
        assert!(j.contains("--limit-rate 1000") && j.contains("--proxy socks5://127.0.0.1:1080"), "{j}");
    }
}
