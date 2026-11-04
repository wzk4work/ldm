//! Error type with stable machine-readable error codes.
use std::io;

#[derive(Debug, thiserror::Error)]
pub enum LdmError {
    #[error("server ignored the Range request")]
    RangeIgnored,
    #[error("HTTP status {0}: {1}")]
    HttpStatus(u16, String),
    #[error("timeout: {0}")]
    Timeout(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("retries exhausted: {0}")]
    RetryExhausted(String),
    #[error("format error: {0}")]
    Format(String),
    #[error("key / decryption error: {0}")]
    Key(String),
    #[error("unsupported encryption: {0}")]
    UnsupportedKey(String),
    #[error("ffmpeg failed: {0}")]
    Ffmpeg(String),
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("part coverage gap: {0}")]
    Gap(String),
    #[error("canceled")]
    Canceled,
    #[error("remote resource changed (ETag/Last-Modified/size)")]
    EtagChanged,
    #[error("checksum mismatch: expected {expected}, got {actual}")]
    Checksum { expected: String, actual: String },
    #[error("rejected URL: {0}")]
    Security(String),
    #[error("database error: {0}")]
    Db(String),
    #[error("yt-dlp: {0}")]
    Ytdlp(String),
    /// A helper tool (yt-dlp) is not installed and may not be installed automatically.
    #[error("{0}")]
    ToolMissing(String),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, LdmError>;

impl LdmError {
    /// Stable machine-readable code.
    pub fn code(&self) -> &'static str {
        match self {
            LdmError::RangeIgnored => "E_RANGE_IGNORED",
            LdmError::HttpStatus(..) => "E_HTTP_STATUS",
            LdmError::Timeout(_) => "E_TIMEOUT",
            LdmError::Network(_) => "E_NETWORK",
            LdmError::RetryExhausted(_) => "E_RETRY_EXHAUSTED",
            LdmError::Format(_) => "E_FORMAT",
            LdmError::Key(_) => "E_KEY",
            LdmError::UnsupportedKey(_) => "E_UNSUPPORTED_KEY",
            LdmError::Ffmpeg(_) => "E_FFMPEG",
            LdmError::Io(e) if is_disk_full(e) => "E_DISK_FULL",
            LdmError::Io(_) => "E_IO",
            LdmError::Gap(_) => "E_GAP",
            LdmError::Canceled => "E_CANCELED",
            LdmError::EtagChanged => "E_ETAG_CHANGED",
            LdmError::Checksum { .. } => "E_CHECKSUM",
            LdmError::Security(_) => "E_SECURITY",
            LdmError::Db(_) => "E_DB",
            LdmError::Ytdlp(_) => "E_YTDLP",
            LdmError::ToolMissing(_) => "E_TOOL_MISSING",
            LdmError::Other(_) => "E_OTHER",
        }
    }

    /// Whether a connection-level retry makes sense.
    pub fn retryable(&self) -> bool {
        match self {
            LdmError::Timeout(_) | LdmError::Network(_) => true,
            LdmError::HttpStatus(s, _) => *s >= 500 || *s == 408 || *s == 429,
            LdmError::Io(e) => !is_disk_full(e) && e.kind() != io::ErrorKind::PermissionDenied,
            _ => false,
        }
    }

    pub fn is_disk_full(&self) -> bool {
        matches!(self, LdmError::Io(e) if is_disk_full(e))
    }
}

fn is_disk_full(e: &io::Error) -> bool {
    e.raw_os_error() == Some(libc::ENOSPC) || e.raw_os_error() == Some(libc::EDQUOT)
}

impl From<reqwest::Error> for LdmError {
    fn from(e: reqwest::Error) -> Self {
        if e.is_timeout() {
            LdmError::Timeout(e.to_string())
        } else if let Some(s) = e.status() {
            LdmError::HttpStatus(s.as_u16(), e.to_string())
        } else if e.is_redirect() {
            LdmError::Security(format!("redirect rejected: {e}"))
        } else {
            LdmError::Network(format!("{e:#}"))
        }
    }
}

impl From<rusqlite::Error> for LdmError {
    fn from(e: rusqlite::Error) -> Self {
        LdmError::Db(e.to_string())
    }
}

impl From<serde_json::Error> for LdmError {
    fn from(e: serde_json::Error) -> Self {
        LdmError::Format(e.to_string())
    }
}
