//! Download engine.
pub mod http_job;
pub mod merge;
pub mod parts;
pub mod state;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::http::HttpClient;
use crate::util::filename;
use crate::util::ratelimit::TokenBucket;
use crate::util::speed::SpeedMeter;

pub use http_job::{run_http_job, HttpJobSpec};
pub use parts::{Part, PartState};
pub use state::JobState;

const UNKNOWN: u64 = u64::MAX;

/// Live progress shared between a running job and its observers.
#[derive(Debug, Default)]
pub struct Progress {
    got: AtomicU64,
    total: AtomicU64,
    pub connections: AtomicU32,
    pub segments_done: AtomicU64,
    pub segments_total: AtomicU64,
    pub speed: SpeedMeter,
    /// Live recording: finish and save at the next playlist refresh ("停止录制").
    /// Set through [`Progress::request_stop_recording`] so waiting recorders wake up at once.
    pub stop_live: std::sync::atomic::AtomicBool,
    stop_notify: tokio::sync::Notify,
    /// The job is recording a live stream (HLS / dynamic MPD / yt-dlp `is_live`).
    pub live: std::sync::atomic::AtomicBool,
    /// Live: media time recorded so far, in milliseconds.
    recorded_ms: AtomicU64,
    phase: Mutex<String>,
    filename: Mutex<Option<String>>,
    splits: AtomicU32,
    open_conns: AtomicU32,
    peak_conns: AtomicU32,
}

impl Progress {
    pub fn new() -> Arc<Self> {
        let p = Progress::default();
        p.total.store(UNKNOWN, Ordering::Relaxed);
        Arc::new(p)
    }
    pub fn got(&self) -> u64 {
        self.got.load(Ordering::Relaxed)
    }
    pub fn set_got(&self, v: u64) {
        self.got.store(v, Ordering::Relaxed);
        self.speed.record(v);
    }
    pub fn add_got(&self, d: u64) {
        let v = self.got.fetch_add(d, Ordering::Relaxed) + d;
        self.speed.record(v);
    }
    pub fn total(&self) -> Option<u64> {
        match self.total.load(Ordering::Relaxed) {
            UNKNOWN => None,
            v => Some(v),
        }
    }
    pub fn set_total(&self, v: Option<u64>) {
        self.total.store(v.unwrap_or(UNKNOWN), Ordering::Relaxed);
    }
    pub fn phase(&self) -> String {
        self.phase.lock().unwrap().clone()
    }
    pub fn set_phase(&self, s: &str) {
        *self.phase.lock().unwrap() = s.to_string();
    }
    pub fn filename(&self) -> Option<String> {
        self.filename.lock().unwrap().clone()
    }
    pub fn set_filename(&self, s: &str) {
        *self.filename.lock().unwrap() = Some(s.to_string());
    }
    /// "停止录制": finish gracefully (stop fetching, keep and finalise what was recorded).
    pub fn request_stop_recording(&self) {
        self.stop_live.store(true, Ordering::SeqCst);
        self.stop_notify.notify_one();
    }
    pub fn stop_requested(&self) -> bool {
        self.stop_live.load(Ordering::SeqCst)
    }
    /// Resolves once a stop was requested (immediately if it already was).
    pub async fn stop_requested_wait(&self) {
        loop {
            if self.stop_requested() {
                return;
            }
            let n = self.stop_notify.notified();
            if self.stop_requested() {
                return;
            }
            n.await;
        }
    }
    pub fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }
    pub fn set_live(&self, v: bool) {
        self.live.store(v, Ordering::Relaxed);
    }
    pub fn recorded_secs(&self) -> f64 {
        self.recorded_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }
    pub fn set_recorded_secs(&self, s: f64) {
        self.recorded_ms.store((s.max(0.0) * 1000.0) as u64, Ordering::Relaxed);
    }
    pub fn note_split(&self) {
        self.splits.fetch_add(1, Ordering::Relaxed);
    }
    /// Track an HTTP request in flight (client-side connection accounting).
    pub fn conn_opened(&self) {
        let n = self.open_conns.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak_conns.fetch_max(n, Ordering::SeqCst);
    }
    pub fn conn_closed(&self) {
        self.open_conns.fetch_sub(1, Ordering::SeqCst);
    }
    /// Highest number of simultaneously open data connections seen.
    pub fn peak_connections(&self) -> u32 {
        self.peak_conns.load(Ordering::SeqCst)
    }
    pub fn splits(&self) -> u32 {
        self.splits.load(Ordering::Relaxed)
    }
}

/// Callback invoked whenever the resume state is persisted (used to mirror
/// parts into SQLite).
pub type StateSink = Arc<dyn Fn(&JobState) + Send + Sync>;

/// Shared resources a job runs with.
#[derive(Clone)]
pub struct JobEnv {
    pub cfg: Arc<Config>,
    pub client: HttpClient,
    pub global_limiter: Arc<TokenBucket>,
    pub job_limiter: Option<Arc<TokenBucket>>,
    pub conn_sem: Arc<Semaphore>,
    pub progress: Arc<Progress>,
    pub cancel: CancellationToken,
    pub sink: Option<StateSink>,
}

impl JobEnv {
    /// Effective byte rate cap for this job (0 = none).
    pub fn effective_limit(&self) -> u64 {
        let g = self.global_limiter.rate();
        let j = self.job_limiter.as_ref().map(|l| l.rate()).unwrap_or(0);
        match (g, j) {
            (0, x) | (x, 0) => x,
            (a, b) => a.min(b),
        }
    }

    /// Climb gate: with a limit on and ≥85 % of it already reached,
    /// extra connections would only add server load.
    pub fn climb_gated(&self, active: u32) -> bool {
        let lim = self.effective_limit();
        self.cfg.engine.limit_climb_gate && lim > 0 && active >= 1 && self.progress.speed.sampled_rate() >= 0.85 * lim as f64
    }

    /// Standalone environment (CLI `ldm get`, tests).
    pub fn standalone(cfg: Config) -> crate::error::Result<Self> {
        let client = HttpClient::new(&cfg.engine)?;
        let limit = cfg.engine.effective_speed_limit();
        let global = cfg.engine.max_connections_global.max(1) as usize;
        Ok(Self {
            cfg: Arc::new(cfg),
            client,
            global_limiter: Arc::new(TokenBucket::new(limit)),
            job_limiter: None,
            conn_sem: Arc::new(Semaphore::new(global)),
            progress: Progress::new(),
            cancel: CancellationToken::new(),
            sink: None,
        })
    }

    pub async fn throttle(&self, n: usize) {
        self.global_limiter.acquire(n).await;
        if let Some(l) = &self.job_limiter {
            l.acquire(n).await;
        }
    }
}

/// Where the finished file goes.
#[derive(Debug, Clone, Default)]
pub struct OutputSpec {
    /// Directory override; otherwise download_dir (+ category sub-directory).
    pub dir: Option<PathBuf>,
    /// File name override (sanitized).
    pub filename: Option<String>,
    /// Use this exact path (CLI `-o`).
    pub exact_path: Option<PathBuf>,
}

/// Decide the final path for `name`, honouring category sub-directories and
/// the conflict policy.
pub fn resolve_output(cfg: &Config, out: &OutputSpec, name: &str) -> PathBuf {
    if let Some(p) = &out.exact_path {
        return p.clone();
    }
    // `name` was already chosen and sanitised by the job (title-based names keep `:` `?` …
    // under Linux rules); only an explicit override gets the portable sanitiser here
    let name = match out.filename.as_deref() {
        Some(f) => filename::sanitize(f),
        None => filename::sanitize_linux(name),
    };
    let dir = match &out.dir {
        Some(d) => d.clone(),
        None => cfg.paths.target_dir(&name),
    };
    let _ = std::fs::create_dir_all(&dir);
    filename::unique_path(&dir, &name, &cfg.engine.conflict_policy)
}

/// Verify an optional SHA-256 of the finished file.
pub fn verify_sha256(path: &Path, expected: Option<&str>) -> crate::error::Result<()> {
    let Some(exp) = expected.filter(|s| !s.is_empty()) else { return Ok(()) };
    let actual = sha256_file(path)?;
    if !actual.eq_ignore_ascii_case(exp.trim()) {
        return Err(crate::error::LdmError::Checksum { expected: exp.to_string(), actual });
    }
    Ok(())
}

/// Verify an optional MD5 given as hex or as base64 (Content-MD5 style).
pub fn verify_md5(path: &Path, expected: Option<&str>) -> crate::error::Result<()> {
    use base64::Engine as _;
    use md5::{Digest, Md5};
    use std::io::Read;
    let Some(exp) = expected.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(()) };
    let mut f = std::fs::File::open(path)?;
    let mut h = Md5::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    let d = h.finalize();
    let hex = hex::encode(d);
    let b64 = base64::engine::general_purpose::STANDARD.encode(d);
    if exp.eq_ignore_ascii_case(&hex) || exp == b64 {
        Ok(())
    } else {
        Err(crate::error::LdmError::Checksum { expected: exp.to_string(), actual: hex })
    }
}

pub fn sha256_file(path: &Path) -> crate::error::Result<String> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut f = std::fs::File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
