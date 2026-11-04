//! `config.toml`.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{LdmError, Result};
use crate::paths;

/// Default capture extension list.
pub const DEFAULT_CAPTURE_EXTS: &[&str] = &[
    // video
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "ts", "m2ts", "mpg", "mpeg", "wmv", "asf", "3gp", "ogv", // audio
    "mp3", "m4a", "aac", "ogg", "opus", "flac", "wav", "wma", // stream manifests
    "m3u8", "mpd", // archives / programs / documents
    "zip", "7z", "rar", "gz", "tar", "xz", "bz2", "tgz", "zst", "iso", "exe", "msi", "deb", "rpm", "appimage", "apk", "dmg", "pdf", "epub",
];

pub const DEFAULT_MEDIA_EXTS: &[&str] =
    &["mp4", "m4v", "webm", "mkv", "mov", "flv", "ts", "mp3", "m4a", "aac", "ogg", "opus", "flac", "wav", "m3u8", "mpd"];

fn s(v: &[&str]) -> Vec<String> {
    v.iter().map(|x| x.to_string()).collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct EngineConfig {
    /// Default connections per job (8).
    pub max_connections_default: u32,
    /// Process-wide connection cap.
    pub max_connections_global: u32,
    /// Per-host overrides (`"example.com" = 4`).
    pub max_connections_per_host: BTreeMap<String, u32>,
    pub min_part_size: u64,
    pub max_retries: u32,
    /// Base for exponential back-off (attempt n waits min(60 s, base·2^n)).
    pub retry_base_ms: u64,
    /// Per-read inactivity timeout.
    pub timeout_secs: u64,
    pub connect_timeout_secs: u64,
    pub user_agent: String,
    pub enable_speed_limit: bool,
    /// Global limit in bytes/second (0 = unlimited).
    pub speed_limit_bps: u64,
    /// Allow splitting the un-fetched tail of an *active* part (work stealing).
    pub split_active: bool,
    /// While a speed limit applies and throughput already reaches 85 % of it, open no extra connections.
    pub limit_climb_gate: bool,
    pub steal_alpha: f64,
    pub steal_grace_secs: u64,
    /// Optional "fake Range" check comparing a part's first bytes to the file head.
    pub fake_range_check: bool,
    pub fake_range_prefix: usize,
    /// `system` (env proxies), `none`, `browser` (follow the extension's ProxyInfo), `pac`
    /// (proxy auto-config from `pac_url`), or a proxy URL (http://, https://, socks5://).
    pub proxy: String,
    /// PAC script for `proxy = "pac"`: http(s):// or file:// URL, or an absolute path.
    pub pac_url: String,
    /// Re-fetch the PAC script this often (it is also re-fetched early after a failure).
    pub pac_refresh_secs: u64,
    pub max_redirects: usize,
    /// Refuse link-local / cloud-metadata addresses (SSRF hardening).
    pub block_link_local: bool,
    /// `rename_numbered` | `overwrite`
    pub conflict_policy: String,
    pub fsync: bool,
    /// Placeholder for P1 "alternative" racing connections (not implemented).
    pub enable_alternative: bool,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_connections_default: 8,
            max_connections_global: 32,
            max_connections_per_host: BTreeMap::new(),
            min_part_size: 262_144,
            max_retries: 8,
            retry_base_ms: 1000,
            timeout_secs: 60,
            connect_timeout_secs: 15,
            user_agent: format!("LDM/{}", env!("CARGO_PKG_VERSION")),
            enable_speed_limit: false,
            speed_limit_bps: 0,
            split_active: true,
            limit_climb_gate: true,
            steal_alpha: 0.25,
            steal_grace_secs: 3,
            fake_range_check: false,
            fake_range_prefix: 2048,
            proxy: "system".into(),
            pac_url: String::new(),
            pac_refresh_secs: 1800,
            max_redirects: 10,
            block_link_local: true,
            conflict_policy: "rename_numbered".into(),
            fsync: true,
            enable_alternative: false,
        }
    }
}

impl EngineConfig {
    pub fn effective_speed_limit(&self) -> u64 {
        if self.enable_speed_limit {
            self.speed_limit_bps
        } else {
            0
        }
    }

    pub fn connections_for_host(&self, host: &str) -> u32 {
        self.max_connections_per_host.get(host).copied().unwrap_or(self.max_connections_default).clamp(1, 32)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct PathsConfig {
    pub download_dir: String,
    pub part_dir: String,
    pub by_category: bool,
    /// Keyed by category id (video, music, compressed, programs, documents, general).
    pub categories: BTreeMap<String, CategoryOverride>,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            download_dir: "${XDG_DOWNLOAD_DIR}/LDM".into(),
            part_dir: "${XDG_CACHE_HOME}/ldm/parts".into(),
            by_category: true,
            categories: BTreeMap::new(),
        }
    }
}

impl PathsConfig {
    /// Category of an extension, user overrides first.
    pub fn category_of(&self, ext: &str) -> crate::util::category::Category {
        use crate::util::category::Category;
        let e = ext.to_ascii_lowercase();
        for (id, o) in &self.categories {
            if o.exts.iter().any(|x| x.trim_start_matches('.').eq_ignore_ascii_case(&e)) {
                if let Some(c) = Category::from_id(id) {
                    return c;
                }
            }
        }
        Category::from_ext(&e)
    }

    /// Directory for a finished file named `filename`.
    pub fn target_dir(&self, filename: &str) -> PathBuf {
        let cat = self.category_of(&crate::util::filename::extension_of(filename));
        if let Some(o) = self.categories.get(cat.id()).filter(|o| !o.dir.trim().is_empty()) {
            return PathBuf::from(paths::expand(o.dir.trim()));
        }
        if !self.by_category {
            return self.download_dir();
        }
        self.download_dir().join(cat.subdir())
    }

    pub fn download_dir(&self) -> PathBuf {
        PathBuf::from(paths::expand(&self.download_dir))
    }
    pub fn part_dir(&self) -> PathBuf {
        PathBuf::from(paths::expand(&self.part_dir))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct BrowserConfig {
    pub native_host_name: String,
    /// Extension origins allowed to use the native host (empty = rely on the manifest).
    pub allowed_origins: Vec<String>,
    pub ws_bridge_enabled: bool,
    pub ws_bridge_host: String,
    pub ws_bridge_port: u16,
    /// Optional token required in `Hello` on the JSON sub-protocol.
    pub ws_bridge_token: String,
    /// Identical URLs added within this window reuse the existing job.
    pub dedupe_secs: u64,
}

impl Default for BrowserConfig {
    fn default() -> Self {
        Self {
            native_host_name: ldm_ipc::NATIVE_HOST_NAME.into(),
            allowed_origins: vec![],
            ws_bridge_enabled: false,
            ws_bridge_host: "127.0.0.1".into(),
            ws_bridge_port: 17001,
            ws_bridge_token: String::new(),
            dedupe_secs: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QueueConfig {
    /// Files downloading at the same time (all queues together).
    pub global_max_active: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self { global_max_active: 3 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MediaConfig {
    pub hls_max_height: u32,
    pub segment_parallel: u32,
    pub ffmpeg_path: String,
    pub keep_segments_on_failure: bool,
    pub candidate_ttl_secs: u64,
    /// `mp4` or `mkv`
    pub output_container: String,
    /// Save subtitle renditions (HLS SUBTITLES / DASH text) as `.vtt` next to the video.
    pub subtitles: bool,
    /// Record live HLS (no EXT-X-ENDLIST) instead of rejecting it.
    pub live_record: bool,
    /// Recording cap in seconds of recorded media (0 = unlimited: until the stream ends or the
    /// user stops it). Reaching it stops the recording gracefully, like "停止录制".
    /// (`live_max_secs` is accepted as the old name.)
    #[serde(alias = "live_max_secs")]
    pub live_max_duration: u64,
    /// Live HLS: `false` (default) starts at the live edge (last ~3 target durations, RFC 8216 §6.3.3),
    /// `true` also saves the DVR window the playlist still offers.
    pub live_from_start: bool,
    /// Audio track preference when a master / MPD offers several options for the same video:
    /// `aac` (default; then ec-3, then ac-3), `most_channels`, or `language:<code>` (e.g. `language:en`).
    /// Prefers AAC while still auto-merging video+audio into one file.
    pub audio_preference: String,
    /// Rank detected media "main video first, suspected ads last (greyed)". Never hides anything.
    pub ad_ranking: bool,
    /// Optional extra ad domains: one per line, `#` comments, hosts-file lines accepted.
    pub ad_domains_file: String,
}

impl Default for MediaConfig {
    fn default() -> Self {
        Self {
            hls_max_height: 1080,
            segment_parallel: 4,
            ffmpeg_path: "ffmpeg".into(),
            keep_segments_on_failure: true,
            candidate_ttl_secs: 120,
            output_container: "mp4".into(),
            subtitles: true,
            live_record: true,
            live_max_duration: 0,
            live_from_start: false,
            audio_preference: "aac".into(),
            ad_ranking: true,
            ad_domains_file: String::new(),
        }
    }
}

impl MediaConfig {
    pub fn audio_pref(&self) -> crate::media::AudioPreference {
        crate::media::AudioPreference::parse(&self.audio_preference)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CaptureConfig {
    pub enabled: bool,
    pub capture_exts: Vec<String>,
    pub media_exts: Vec<String>,
    pub exclude_hosts: Vec<String>,
    pub exclude_url_patterns: Vec<String>,
    pub min_size: u64,
    /// Key held while clicking a link to let the browser download it
    /// ("alt" | "shift" | "ctrl" | "" = off).
    pub bypass_modifier: String,
    /// Key held while clicking a link to force LDM to take it even when the
    /// type rules would not ("insert" | "alt" | "shift" | "ctrl" | "").
    pub force_modifier: String,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            capture_exts: s(DEFAULT_CAPTURE_EXTS),
            media_exts: s(DEFAULT_MEDIA_EXTS),
            exclude_hosts: vec![],
            exclude_url_patterns: vec![],
            min_size: 10_240,
            bypass_modifier: "alt".into(),
            force_modifier: "insert".into(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct GeneralConfig {
    pub notify_on_complete: bool,
    pub enable_clipboard: bool,
    /// Optional command run after completion; `{path}` is substituted. No shell.
    pub on_complete_command: Vec<String>,
    pub log_cookies: bool,
    /// Browser captures wait for the GUI "下载文件信息" dialog when a GUI is
    /// connected (confirmation dialog); without a GUI they start at once.
    pub confirm_browser_downloads: bool,
    /// GUI: open the per-download progress window when a download starts.
    pub show_progress_dialog: bool,
    /// GUI: show the "下载完成" dialog.
    pub show_complete_dialog: bool,
    /// GUI: play a sound on completion / failure.
    pub sounds: bool,
    /// Command used for "完成后关机" (job or queue post-action). No shell.
    pub shutdown_command: Vec<String>,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            notify_on_complete: true,
            enable_clipboard: false,
            on_complete_command: vec![],
            log_cookies: false,
            confirm_browser_downloads: true,
            show_progress_dialog: true,
            show_complete_dialog: true,
            sounds: false,
            shutdown_command: vec!["systemctl".into(), "poweroff".into()],
        }
    }
}

/// Saved credentials for sites that need HTTP authentication.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SiteLogin {
    /// Host glob, e.g. `*.example.com` or `files.example.org`.
    pub host: String,
    pub user: String,
    pub password: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SitesConfig {
    pub logins: Vec<SiteLogin>,
    /// Per-site TLS options (FTPS).
    pub tls: Vec<SiteTls>,
}

/// Per-site TLS settings (host glob like [`SiteLogin::host`]).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SiteTls {
    pub host: String,
    /// `ftp://` URLs to this host must upgrade with AUTH TLS (explicit FTPS).
    pub ftp_explicit_tls: bool,
    /// Accept an invalid / self-signed / mismatched certificate for this host (off by default;
    /// the handshake signatures are still checked, the certificate chain and name are not).
    pub accept_invalid_cert: bool,
}

/// FTP / FTPS options. `ftps://` is implicit TLS (default port 990), `ftpes://` is explicit
/// TLS (AUTH TLS, required); `tls` decides what plain `ftp://` URLs do.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct FtpConfig {
    /// `ftp://` URLs: "off" (plain FTP), "try" (AUTH TLS when the server offers it, else plain),
    /// "require" (AUTH TLS or fail).
    pub tls: String,
}

impl Default for FtpConfig {
    fn default() -> Self {
        FtpConfig { tls: "off".into() }
    }
}

impl SitesConfig {
    /// First login whose host glob matches `host`.
    pub fn login_for(&self, host: &str) -> Option<&SiteLogin> {
        let h = host.to_ascii_lowercase();
        self.logins.iter().find(|l| host_glob_match(&l.host.to_ascii_lowercase(), &h))
    }

    /// First TLS rule whose host glob matches `host`.
    pub fn tls_for(&self, host: &str) -> Option<&SiteTls> {
        let h = host.to_ascii_lowercase();
        self.tls.iter().find(|t| host_glob_match(&t.host.to_ascii_lowercase(), &h))
    }
}

/// `*` matches any run of characters (including dots); everything else literal.
pub fn host_glob_match(pat: &str, host: &str) -> bool {
    fn rec(p: &[u8], h: &[u8]) -> bool {
        match p.first() {
            None => h.is_empty(),
            Some(b'*') => (0..=h.len()).any(|i| rec(&p[1..], &h[i..])),
            Some(c) => h.first() == Some(c) && rec(&p[1..], &h[1..]),
        }
    }
    rec(pat.as_bytes(), host.as_bytes())
}

/// Per-category overrides ("保存位置" per category): own directory and
/// extra extensions that map to the category.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct CategoryOverride {
    pub dir: String,
    pub exts: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Config {
    pub general: GeneralConfig,
    pub sites: SitesConfig,
    pub engine: EngineConfig,
    pub paths: PathsConfig,
    pub browser: BrowserConfig,
    pub queue: QueueConfig,
    pub media: MediaConfig,
    pub capture: CaptureConfig,
    pub ytdlp: YtdlpConfig,
    pub ftp: FtpConfig,
}

/// External yt-dlp integration. LDM never bundles yt-dlp: it uses the user's copy
/// (`path`), else the LDM-managed copy in `<data>/tools` (installed on first use with
/// consent, see `tools.rs`), else one on `$PATH`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct YtdlpConfig {
    /// Use yt-dlp at all (effective only when the binary is found).
    pub enabled: bool,
    /// Explicit binary; empty = auto-detect (managed copy, then `$PATH` / `~/.local/bin/yt-dlp`).
    pub path: String,
    /// Install the managed yt-dlp when a job needs it and none is found:
    /// `ask` (default: GUI dialog / CLI prompt / job waits for consent), `always`, `never`.
    pub auto_install: String,
    /// Check for a newer managed yt-dlp/deno: `daily` (default), `weekly`, `off`.
    pub auto_update: String,
    /// Explicit deno binary (JavaScript runtime for yt-dlp's YouTube challenges); empty = auto.
    pub deno_path: String,
    /// Managed tools directory; empty = `<data dir>/tools`.
    pub tools_dir: String,
    /// Tests only: fake release server (loopback `http://` URLs only; empty = GitHub).
    pub release_base_url: String,
    /// Retry eligible failed media jobs (403/410, not-media body, manifest errors) once via yt-dlp.
    pub auto_fallback: bool,
    /// Per-domain rule: `on` (fallback allowed, default), `off` (never use yt-dlp), `always`
    /// (route every download of that site to yt-dlp; built-in for YouTube).
    pub site_rules: BTreeMap<String, String>,
    /// Extra command-line arguments appended before the URL (e.g. `["--sponsorblock-remove", "all"]`).
    pub extra_args: Vec<String>,
    /// Browser for `--cookies-from-browser` (`firefox`, `chrome`, …); empty = never pass cookies.
    pub cookies_from_browser: String,
    /// A playlist URL enqueues one job per item (otherwise `--no-playlist`: only the video itself).
    pub playlist: bool,
    /// Default quality cap (height) when the user did not pick one.
    pub max_height: u32,
}

impl Default for YtdlpConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            path: String::new(),
            auto_install: "ask".into(),
            auto_update: "daily".into(),
            deno_path: String::new(),
            tools_dir: String::new(),
            release_base_url: String::new(),
            auto_fallback: true,
            site_rules: BTreeMap::new(),
            extra_args: vec![],
            cookies_from_browser: String::new(),
            playlist: false,
            max_height: 1080,
        }
    }
}

impl Config {
    pub fn load_from(path: &Path) -> Result<Config> {
        match std::fs::read_to_string(path) {
            Ok(t) => toml::from_str(&t).map_err(|e| LdmError::Format(format!("{}: {e}", path.display()))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn load() -> Result<Config> {
        Self::load_from(&paths::config_file())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let t = toml::to_string_pretty(self).map_err(|e| LdmError::Format(e.to_string()))?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, t)?;
        // may hold site passwords and the WS bridge token
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }

    /// Apply a JSON merge-patch style update (`{"engine":{"max_connections_default":4}}`).
    pub fn patched(&self, patch: &serde_json::Value) -> Result<Config> {
        let mut base = serde_json::to_value(self)?;
        merge(&mut base, patch);
        let cfg: Config = serde_json::from_value(base)?;
        // serde silently drops unknown struct fields: a key that does not survive the round trip
        // is a typo (`ldm config set genral.x …`) and must be reported, not ignored
        let after = serde_json::to_value(&cfg)?;
        let mut missing = vec![];
        unknown_paths(&after, patch, String::new(), &mut missing);
        if !missing.is_empty() {
            return Err(crate::LdmError::Other(format!("unknown config key: {}", missing.join(", "))));
        }
        Ok(cfg)
    }
}

fn unknown_paths(have: &serde_json::Value, patch: &serde_json::Value, prefix: String, out: &mut Vec<String>) {
    if let serde_json::Value::Object(p) = patch {
        for (k, v) in p {
            let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            if v.is_null() {
                continue; // removal of a map entry
            }
            match have.get(k) {
                None => out.push(path),
                Some(h) if h.is_object() => unknown_paths(h, v, path, out),
                Some(_) => {}
            }
        }
    }
}

fn merge(a: &mut serde_json::Value, b: &serde_json::Value) {
    match (a, b) {
        (serde_json::Value::Object(a), serde_json::Value::Object(b)) => {
            for (k, v) in b {
                // JSON merge-patch: null removes a map entry (e.g. a per-host limit)
                if v.is_null() {
                    a.remove(k);
                } else {
                    merge(a.entry(k.clone()).or_insert(serde_json::Value::Null), v);
                }
            }
        }
        (a, b) => *a = b.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_are_rejected() {
        let c = Config::default();
        let e = c.patched(&serde_json::json!({"general": {"nosuch": 1}})).unwrap_err().to_string();
        assert!(e.contains("general.nosuch"), "{e}");
        assert!(c.patched(&serde_json::json!({"nosuch": {"x": 1}})).is_err());
        // real keys and free-form maps still work
        let n = c.patched(&serde_json::json!({"engine": {"speed_limit_bps": 5, "max_connections_per_host": {"a.com": 2}}})).unwrap();
        assert_eq!(n.engine.speed_limit_bps, 5);
        assert_eq!(n.engine.max_connections_per_host.get("a.com"), Some(&2));
        assert!(n.patched(&serde_json::json!({"engine": {"max_connections_per_host": {"a.com": null}}})).is_ok());
    }

    #[test]
    fn defaults_match_design() {
        let c = Config::default();
        assert_eq!(c.engine.max_connections_default, 8);
        assert_eq!(c.engine.min_part_size, 262144);
        assert_eq!(c.engine.max_retries, 8);
        assert_eq!(c.queue.global_max_active, 3);
        assert_eq!(c.media.hls_max_height, 1080);
        assert_eq!(c.media.audio_preference, "aac");
        assert!(!c.browser.ws_bridge_enabled);
        assert_eq!(c.capture.min_size, 10240);
        assert_eq!(c.ytdlp.auto_install, "ask");
        assert_eq!(c.ytdlp.auto_update, "daily");
        assert!(c.ytdlp.release_base_url.is_empty() && c.ytdlp.tools_dir.is_empty());
    }

    #[test]
    fn partial_toml_and_patch() {
        let c: Config = toml::from_str("[engine]\nmax_connections_default = 4\n").unwrap();
        assert_eq!(c.engine.max_connections_default, 4);
        assert_eq!(c.engine.max_retries, 8);
        let p = c.patched(&serde_json::json!({"queue": {"global_max_active": 1}})).unwrap();
        assert_eq!(p.queue.global_max_active, 1);
        assert_eq!(p.engine.max_connections_default, 4);
        let t = toml::to_string_pretty(&p).unwrap();
        let back: Config = toml::from_str(&t).unwrap();
        assert_eq!(back, p);
    }
}
