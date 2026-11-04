//! Typed payloads for the envelope types.
//!
//! Field naming follows the JSON envelope schemas
//! (snake_case for DownloadAdd / MediaCandidate, camelCase for HelloAck,
//! InterceptDecision, JobProgress). Aliases accept the other spelling so that
//! hand-written clients are not punished for a casing mismatch.
use serde::{Deserialize, Serialize};

/// Envelope type names.
pub mod types {
    // extension / client -> daemon
    pub const HELLO: &str = "Hello";
    pub const PING: &str = "Ping";
    pub const DOWNLOAD_ADD: &str = "DownloadAdd";
    pub const DOWNLOAD_BATCH: &str = "DownloadBatch";
    pub const MEDIA_CANDIDATE: &str = "MediaCandidate";
    pub const MEDIA_GEOMETRY: &str = "MediaGeometry";
    pub const MEDIA_PROBE: &str = "MediaProbe";
    pub const CANDIDATE_LIST: &str = "CandidateList";
    pub const TAB_STATE: &str = "TabState";
    pub const PROXY_INFO: &str = "ProxyInfo";
    pub const COOKIES_QUERY_RESULT: &str = "CookiesQueryResult";
    pub const SUBSCRIBE: &str = "Subscribe";
    pub const JOB_LIST: &str = "JobList";
    pub const JOB_GET: &str = "JobGet";
    pub const JOB_PAUSE: &str = "JobPause";
    pub const JOB_RESUME: &str = "JobResume";
    pub const JOB_REMOVE: &str = "JobRemove";
    pub const JOB_SET: &str = "JobSet";
    /// `{id}`: finish a live recording gracefully (keep what was recorded).
    pub const JOB_STOP_RECORDING: &str = "JobStopRecording";
    pub const CONFIG_GET: &str = "ConfigGet";
    pub const CONFIG_SET: &str = "ConfigSet";
    pub const QUEUE_LIST: &str = "QueueList";
    pub const QUEUE_SET: &str = "QueueSet";
    pub const EXPORT: &str = "Export";
    pub const IMPORT: &str = "Import";
    pub const STATUS: &str = "Status";
    /// yt-dlp detection / version (local clients).
    pub const YTDLP_STATUS: &str = "YtdlpStatus";
    /// Managed yt-dlp / deno: versions, sources, last update check (local clients).
    pub const TOOLS_STATUS: &str = "ToolsStatus";
    /// `{action: install|update|check|rollback|remove, tool?: "yt-dlp"|"deno", version?}` (local clients).
    pub const TOOLS_ACTION: &str = "ToolsAction";
    pub const SHUTDOWN: &str = "Shutdown";
    // daemon -> extension / client
    pub const HELLO_ACK: &str = "HelloAck";
    pub const PONG: &str = "Pong";
    pub const RULESET_PUSH: &str = "RuleSetPush";
    pub const SITE_RULES: &str = "SiteRules";
    pub const SHOW_MEDIA_BUTTON: &str = "ShowMediaButton";
    pub const HIDE_MEDIA_BUTTON: &str = "HideMediaButton";
    pub const INTERCEPT_DECISION: &str = "InterceptDecision";
    pub const NOTIFY: &str = "Notify";
    pub const GET_COOKIES: &str = "GetCookies";
    pub const JOB_PROGRESS: &str = "JobProgress";
    pub const JOB_STATE_CHANGED: &str = "JobStateChanged";
    pub const MEDIA_VARIANTS: &str = "MediaVariants";
    /// Daemon → GUI: a browser capture waits for the "下载文件信息" dialog.
    pub const CAPTURE_CONFIRM: &str = "CaptureConfirm";
    /// Daemon → GUI: a download finished (drives the completion dialog).
    pub const JOB_COMPLETED: &str = "JobCompleted";
    /// Daemon → GUI/CLI: a job needs yt-dlp and `ytdlp.auto_install = ask` (show the consent dialog).
    pub const TOOLS_NEEDED: &str = "ToolsNeeded";
    /// Daemon → clients: install/update progress `{tool, action, phase, got, total}`.
    pub const TOOLS_PROGRESS: &str = "ToolsProgress";
    /// Daemon → clients: a tools action finished `{action, result, requeued}`.
    pub const TOOLS_CHANGED: &str = "ToolsChanged";
    pub const QUEUE_START: &str = "QueueStart";
    pub const QUEUE_STOP: &str = "QueueStop";
    pub const QUEUE_DELETE: &str = "QueueDelete";
    pub const ACK: &str = "Ack";
    pub const ERROR: &str = "Error";
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Hello {
    #[serde(default)]
    pub browser: String,
    #[serde(default)]
    pub version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extension_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct HelloAck {
    pub protocol_version: u32,
    pub features: Vec<String>,
    #[serde(default)]
    pub rule_set_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ws_port: Option<u16>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HeaderKv {
    pub name: String,
    pub value: String,
}

/// `DownloadAdd`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadAdd {
    pub url: String,
    #[serde(default, alias = "referer")]
    pub referrer: Option<String>,
    #[serde(default, alias = "pageUrl")]
    pub page_url: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default, alias = "userAgent")]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default, alias = "filenameHint")]
    pub filename_hint: Option<String>,
    #[serde(default, alias = "preferredHeight")]
    pub preferred_height: Option<u32>,
    /// Page / video title of a media candidate. When set, the file is named after the
    /// (cleaned) title, ahead of Content-Disposition; plain intercepted
    /// downloads leave it empty and keep the server-provided name.
    #[serde(default, alias = "titleHint")]
    pub title_hint: Option<String>,
    /// Set when the user downloads a candidate the ranking flagged as a suspected ad (reasons).
    #[serde(default, alias = "adReasons")]
    pub ad_reasons: Option<Vec<String>>,
    #[serde(default, alias = "headersExtra", alias = "headers")]
    pub headers_extra: Vec<HeaderKv>,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default, alias = "bodyB64")]
    pub body_b64: Option<String>,
    #[serde(default, alias = "bytesTotal", alias = "content_length")]
    pub bytes_total: Option<u64>,
    #[serde(default)]
    pub flags: u32,
    /// Separate audio file for `url` (sites that stream video and audio as two whole files,
    /// e.g. per-track fMP4 fetched with Range requests): both are downloaded and merged.
    #[serde(default, alias = "audioUrl")]
    pub audio_url: Option<String>,
    /// Kind `ytdlp` with a playlist URL: enqueue one job per item (needs `ytdlp.playlist` or this flag).
    #[serde(default)]
    pub playlist: bool,
    /// Explicit job kind: `http` | `hls` | `dash` | `av` | `auto` (default auto).
    #[serde(default)]
    pub kind: Option<String>,
    /// Target directory override.
    #[serde(default, alias = "saveDir")]
    pub save_dir: Option<String>,
    #[serde(default)]
    pub connections: Option<u32>,
    #[serde(default)]
    pub queue: Option<String>,
    #[serde(default)]
    pub sha256: Option<String>,
    /// Browser request id (extension download id) for InterceptDecision correlation.
    #[serde(default, alias = "requestId")]
    pub request_id: Option<String>,
    /// Origin of the request: `extension`, `cli`, `gui`, `ws`...
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default, alias = "tabId")]
    pub tab_id: Option<i64>,
    /// Start paused instead of queueing immediately.
    #[serde(default)]
    pub paused: bool,
    /// Free-text description ("描述" column).
    #[serde(default)]
    pub description: Option<String>,
    /// Alternative URLs for the same file (tried when the main one fails).
    #[serde(default)]
    pub mirrors: Vec<String>,
    /// Expected MD5 (hex) of the finished file.
    #[serde(default)]
    pub md5: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct DownloadBatch {
    pub items: Vec<DownloadAdd>,
}

/// `MediaCandidate`. The richer form with a `media`
/// array is accepted as well.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MediaCandidate {
    #[serde(default, alias = "tabId")]
    pub tab_id: i64,
    #[serde(default, alias = "frameId")]
    pub frame_id: Option<i64>,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub kind: String,
    #[serde(default, alias = "pageUrl")]
    pub page_url: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    #[serde(default, alias = "contentLength")]
    pub content_length: Option<u64>,
    #[serde(default, alias = "filenameHint")]
    pub filename_hint: Option<String>,
    #[serde(default, alias = "elementId")]
    pub element_id: Option<String>,
    #[serde(default)]
    pub media: Vec<MediaItem>,
    /// Ad ranking from the extension (all signals); None → the daemon's static fallback.
    #[serde(default, alias = "adScore")]
    pub ad_score: Option<f64>,
    #[serde(default, alias = "adSuspect")]
    pub ad_suspect: bool,
    #[serde(default, alias = "adReasons")]
    pub ad_reasons: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct MediaItem {
    pub url: String,
    #[serde(default)]
    pub mime: Option<String>,
    #[serde(default)]
    pub ext: Option<String>,
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    #[serde(default)]
    pub bandwidth: Option<u64>,
    #[serde(default)]
    pub kind: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct InterceptDecision {
    /// `take` | `pass`
    pub action: String,
    #[serde(default)]
    pub save_path: Option<String>,
    #[serde(default)]
    pub job_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuleSetPush {
    pub version: u32,
    pub capture_exts: Vec<String>,
    pub media_exts: Vec<String>,
    pub exclude_hosts: Vec<String>,
    pub exclude_url_patterns: Vec<String>,
    pub min_size: u64,
    #[serde(default)]
    pub site_adapters: Vec<serde_json::Value>,
    #[serde(default)]
    pub capture_enabled: bool,
    /// Modifier keys for link clicks (see CaptureConfig).
    #[serde(default)]
    pub bypass_modifier: String,
    #[serde(default)]
    pub force_modifier: String,
    /// media.ad_ranking
    #[serde(default = "yes")]
    pub ad_ranking: bool,
    /// Domains from media.ad_domains_file (the extension has the built-in list).
    #[serde(default)]
    pub ad_domains: Vec<String>,
    /// yt-dlp usable (enabled and the binary was found).
    #[serde(default)]
    pub ytdlp: bool,
    /// Sites routed to yt-dlp (YouTube + `always` rules): the extension offers the page itself.
    #[serde(default)]
    pub ytdlp_sites: Vec<String>,
    /// Sites where yt-dlp must not be offered (`off` rules).
    #[serde(default)]
    pub ytdlp_off: Vec<String>,
    /// `ytdlp.auto_fallback`
    #[serde(default)]
    pub ytdlp_auto_fallback: bool,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct JobProgress {
    pub job_id: String,
    pub got: u64,
    pub total: Option<u64>,
    pub speed_bps: f64,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connections: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segments_done: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub segments_total: Option<u64>,
    /// Engine phase when it matters to the UI (e.g. "Recording" for live streams).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Live recording: media time recorded so far (seconds). Live jobs have no percent / ETA.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recorded_secs: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ErrorPayload {
    pub error: String,
    pub code: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_add_accepts_both_casings() {
        let a: DownloadAdd = serde_json::from_str(
            r#"{"url":"http://h/a","pageUrl":"http://h/","filename_hint":"a.bin","headers_extra":[{"name":"X","value":"1"}]}"#,
        )
        .unwrap();
        assert_eq!(a.page_url.as_deref(), Some("http://h/"));
        assert_eq!(a.filename_hint.as_deref(), Some("a.bin"));
        assert_eq!(a.headers_extra.len(), 1);
    }

    #[test]
    fn hello_ack_camel() {
        let s = serde_json::to_string(&HelloAck { protocol_version: 1, features: vec!["http".into()], ..Default::default() }).unwrap();
        assert!(s.contains("protocolVersion"));
    }
}
