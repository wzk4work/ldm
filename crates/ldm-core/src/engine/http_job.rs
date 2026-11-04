//! Multi-connection HTTP job: probe → plan → scheduled connections with
//! dynamic splitting → merge.
//!
//! Concurrency protocol (original): every part has a `claimed` cursor guarded
//! by the job table mutex. A connection claims bytes *before* writing them, and
//! a split only ever moves `end` of a part down to a point beyond its claimed
//! cursor. Therefore the free tail of an active part can be handed to a new
//! connection without the Range race that the prototype suffered from.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::StatusCode;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::merge::merge_parts;
use super::parts::{choose_split, split_plan, Part, PartState, Span};
use super::state::{self, part_path, JobState};
use super::{resolve_output, verify_sha256, JobEnv, OutputSpec};
use crate::error::{LdmError, Result};
use crate::http::{parse_content_range, Probe, RequestCtx};
use crate::paths::ensure_private_dir;
use crate::util::filename;

#[derive(Debug, Clone, Default)]
pub struct HttpJobSpec {
    pub url: String,
    pub ctx: RequestCtx,
    pub max_conns: u32,
    pub parts_dir: PathBuf,
    pub output: OutputSpec,
    pub filename_hint: Option<String>,
    pub sha256: Option<String>,
    /// Stem from the page's video title; takes precedence over Content-Disposition.
    pub title_stem: Option<String>,
}

#[derive(Debug, Clone)]
pub struct HttpOutcome {
    pub path: PathBuf,
    pub total: u64,
    pub final_url: String,
    pub mime: Option<String>,
    pub range_ok: bool,
    pub splits: u32,
}

/// Back-off delay for `attempt` (1-based): min(60 s, base·2^attempt) + jitter.
pub fn backoff(base_ms: u64, attempt: u32) -> Duration {
    let exp = base_ms.saturating_mul(1u64 << attempt.min(16));
    let jitter = rand::random::<u64>() % (base_ms / 4 + 1);
    Duration::from_millis(exp.min(60_000) + jitter)
}

async fn sleep_or_cancel(d: Duration, c: &CancellationToken) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(d) => true,
        _ = c.cancelled() => false,
    }
}

fn ext_for_mime(m: &str) -> Option<&'static str> {
    Some(match m {
        "video/mp4" => "mp4",
        "video/webm" => "webm",
        "video/x-matroska" => "mkv",
        "video/mp2t" => "ts",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/ogg" => "ogg",
        "application/zip" => "zip",
        "application/pdf" => "pdf",
        "application/gzip" => "gz",
        "application/x-7z-compressed" => "7z",
        _ => return None,
    })
}

/// Pick a file name: explicit override > page title (media candidates) > Content-Disposition > hint > URL.
pub fn choose_name(spec: &HttpJobSpec, probe: &Probe) -> String {
    if spec.output.filename.is_none() {
        if let Some(stem) = spec.title_stem.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            // the title gives the name, the server/URL the type: Content-Disposition ext >
            // URL ext > MIME > mp4 (titles like "Mr. Smith" must not be read as extensions)
            let ext_of = |n: Option<String>| {
                n.map(|n| filename::split_ext(&n).1.to_ascii_lowercase())
                    .filter(|e| !e.is_empty() && e.len() <= 5 && e.chars().all(|c| c.is_ascii_alphanumeric()))
            };
            let ext = ext_of(probe.filename.clone())
                .or_else(|| ext_of(filename::from_url(&probe.final_url)))
                .or_else(|| ext_of(filename::from_url(&spec.url)))
                .or_else(|| probe.mime.as_deref().and_then(ext_for_mime).map(String::from))
                .unwrap_or_else(|| "mp4".into());
            return filename::sanitize_linux(&format!("{}.{ext}", filename::title_stem(stem)));
        }
    }
    let mut name = spec
        .output
        .filename
        .clone()
        .or_else(|| probe.filename.clone())
        .or_else(|| spec.filename_hint.clone())
        .or_else(|| filename::from_url(&probe.final_url))
        .or_else(|| filename::from_url(&spec.url))
        .unwrap_or_else(|| "download".into());
    if filename::split_ext(&name).1.is_empty() {
        if let Some(ext) = probe.mime.as_deref().and_then(ext_for_mime) {
            name = format!("{name}.{ext}");
        }
    }
    filename::sanitize(&name)
}

async fn probe_with_retry(env: &JobEnv, spec: &HttpJobSpec) -> Result<Probe> {
    let cfg = &env.cfg.engine;
    let mut attempt = 0;
    loop {
        let r = tokio::select! {
            r = env.client.probe(&spec.url, &spec.ctx) => r,
            _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
        };
        match r {
            Ok(p) => return Ok(p),
            Err(e) if e.retryable() && attempt < cfg.max_retries => {
                attempt += 1;
                warn!(attempt, err = %e, "probe failed, retrying");
                if !sleep_or_cancel(backoff(cfg.retry_base_ms, attempt), &env.cancel).await {
                    return Err(LdmError::Canceled);
                }
            }
            Err(e) => return Err(e),
        }
    }
}

/// Run (or resume) an HTTP job to completion.
pub async fn run_http_job(env: &JobEnv, spec: &HttpJobSpec) -> Result<HttpOutcome> {
    ensure_private_dir(&spec.parts_dir)?;
    env.progress.set_phase("Probing");
    let probe = probe_with_retry(env, spec).await?;
    info!(url = %spec.url, total = ?probe.total, range_ok = probe.range_ok, status = probe.status, "probe done");
    let name = choose_name(spec, &probe);
    env.progress.set_filename(&name);
    env.progress.set_total(probe.total);

    let mut st = match state::load(&spec.parts_dir) {
        Some(mut saved) if saved.url == spec.url => {
            let changed = (saved.etag.is_some() && probe.etag.is_some() && saved.etag != probe.etag)
                || (saved.last_modified.is_some() && probe.last_modified.is_some() && saved.last_modified != probe.last_modified)
                || (saved.total.is_some() && probe.total.is_some() && saved.total != probe.total);
            if changed {
                return Err(LdmError::EtagChanged);
            }
            if saved.range_ok && !probe.range_ok {
                warn!("server no longer accepts Range; restarting from zero");
                fresh_state(spec, &probe, &name)
            } else {
                state::reconcile_with_disk(&spec.parts_dir, &mut saved);
                info!(parts = saved.parts.len(), "resuming from saved state");
                saved
            }
        }
        _ => fresh_state(spec, &probe, &name),
    };
    st.final_url = Some(probe.final_url.clone());
    st.mime = probe.mime.clone();

    let multi = st.range_ok && st.total.unwrap_or(0) > 0;
    let result = if multi { run_multi(env, spec, &mut st).await } else { run_single(env, spec, &mut st).await };
    match result {
        Ok(()) => {}
        Err(LdmError::RangeIgnored) => {
            warn!("server ignored Range mid-transfer: falling back to a single connection");
            clear_parts(&spec.parts_dir, &st);
            st.range_ok = false;
            st.parts.clear();
            run_single(env, spec, &mut st).await?;
        }
        Err(e) => return Err(e),
    }

    env.progress.set_phase("Assembling");
    let total = st.total.unwrap_or(0);
    let dest = resolve_output(&env.cfg, &spec.output, &name);
    let parts = st.parts.clone();
    let dir = spec.parts_dir.clone();
    let fsync = env.cfg.engine.fsync;
    let d2 = dest.clone();
    tokio::task::spawn_blocking(move || merge_parts(&parts, &dir, &d2, total, fsync))
        .await
        .map_err(|e| LdmError::Other(e.to_string()))??;
    verify_sha256(&dest, spec.sha256.as_deref())?;
    let _ = std::fs::remove_dir_all(&spec.parts_dir);
    Ok(HttpOutcome {
        path: dest,
        total,
        final_url: probe.final_url,
        mime: probe.mime,
        range_ok: st.range_ok,
        splits: env.progress.splits(),
    })
}

fn fresh_state(spec: &HttpJobSpec, probe: &Probe, name: &str) -> JobState {
    clear_dir_files(&spec.parts_dir);
    JobState {
        url: spec.url.clone(),
        final_url: Some(probe.final_url.clone()),
        total: probe.total,
        range_ok: probe.range_ok,
        etag: probe.etag.clone(),
        last_modified: probe.last_modified.clone(),
        filename: Some(name.to_string()),
        mime: probe.mime.clone(),
        parts: vec![],
    }
}

fn clear_dir_files(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

fn clear_parts(dir: &Path, st: &JobState) {
    for p in &st.parts {
        let _ = std::fs::remove_file(part_path(dir, p.idx));
    }
}

fn persist(env: &JobEnv, dir: &Path, st: &JobState) {
    if let Err(e) = state::save(dir, st) {
        warn!(err = %e, "failed to persist job state");
    }
    if let Some(s) = &env.sink {
        s(st);
    }
}

// ---------------------------------------------------------------- single

async fn open_part(path: &Path, keep: u64) -> Result<tokio::fs::File> {
    let mut f = tokio::fs::OpenOptions::new().create(true).write(true).truncate(false).mode(0o600).open(path).await?;
    f.set_len(keep).await?;
    f.seek(std::io::SeekFrom::Start(keep)).await?;
    Ok(f)
}

/// Single connection: used when Range is unsupported or the size is unknown.
async fn run_single(env: &JobEnv, spec: &HttpJobSpec, st: &mut JobState) -> Result<()> {
    let cfg = &env.cfg.engine;
    env.progress.set_phase("Running");
    env.progress.connections.store(1, std::sync::atomic::Ordering::Relaxed);
    let path = part_path(&spec.parts_dir, 0);
    let mut attempts = 0u32;
    let read_to = Duration::from_secs(cfg.timeout_secs.max(1));
    loop {
        let on_disk = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut from = if st.range_ok { on_disk } else { 0 };
        if let Some(t) = st.total {
            if from >= t && t > 0 {
                break;
            }
        }
        st.parts = vec![Part { idx: 0, start: 0, end: st.total.unwrap_or(0), got: from, state: PartState::Active }];
        persist(env, &spec.parts_dir, st);
        let resp = tokio::select! {
            r = env.client.get_range(&spec.url, &spec.ctx, from, None) => r,
            _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
        };
        let mut progressed = false;
        let res: Result<bool> = async {
            let resp = resp?;
            if from > 0 && resp.status() == StatusCode::OK {
                warn!("resume request answered with 200: restarting from zero");
                from = 0;
                st.range_ok = false;
            }
            if st.total.is_none() && from == 0 {
                st.total = resp.content_length();
                env.progress.set_total(st.total);
            }
            let mut f = tokio::io::BufWriter::with_capacity(256 * 1024, open_part(&path, from).await?);
            env.progress.set_got(from);
            let mut got = from;
            let mut stream = resp.bytes_stream();
            loop {
                let next = tokio::select! {
                    n = tokio::time::timeout(read_to, stream.next()) => n,
                    _ = env.cancel.cancelled() => { f.flush().await?; return Err(LdmError::Canceled) }
                };
                let chunk = match next {
                    Err(_) => {
                        f.flush().await?;
                        return Err(LdmError::Timeout("read timed out".into()));
                    }
                    Ok(None) => break,
                    Ok(Some(Err(e))) => {
                        f.flush().await?;
                        return Err(e.into());
                    }
                    Ok(Some(Ok(b))) => b,
                };
                tokio::select! {
                    _ = env.throttle(chunk.len()) => {},
                    _ = env.cancel.cancelled() => { f.flush().await?; return Err(LdmError::Canceled) }
                }
                f.write_all(&chunk).await?;
                got += chunk.len() as u64;
                progressed = true;
                env.progress.add_got(chunk.len() as u64);
            }
            f.flush().await?;
            if cfg.fsync {
                f.get_ref().sync_all().await?;
            }
            Ok(match st.total {
                Some(t) => got >= t,
                None => true,
            })
        }
        .await;
        match res {
            Ok(true) => break,
            Ok(false) => {
                if !progressed {
                    attempts += 1;
                }
                warn!("connection closed early");
            }
            Err(LdmError::Canceled) => {
                persist(env, &spec.parts_dir, st);
                return Err(LdmError::Canceled);
            }
            Err(e) if e.retryable() => {
                attempts += 1;
                warn!(attempts, err = %e, "single connection retry");
            }
            Err(e) => return Err(e),
        }
        if attempts > cfg.max_retries {
            return Err(LdmError::RetryExhausted(format!("{} attempts", attempts)));
        }
        if attempts > 0 && !sleep_or_cancel(backoff(cfg.retry_base_ms, attempts), &env.cancel).await {
            return Err(LdmError::Canceled);
        }
    }
    let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    if st.total.is_none() {
        st.total = Some(len);
    }
    if !path.exists() {
        // zero-length resource
        open_part(&path, 0).await?;
    }
    st.parts = vec![Part { idx: 0, start: 0, end: len, got: len, state: PartState::Done }];
    persist(env, &spec.parts_dir, st);
    Ok(())
}

// ----------------------------------------------------------------- multi

#[derive(Debug)]
struct Slot {
    part: Part,
    /// bytes reserved by the connection (>= part.got)
    claimed: u64,
    active: bool,
    last_got: u64,
    speed: f64,
    slow_for: f64,
}

#[derive(Debug, Default)]
struct Table {
    slots: Vec<Slot>,
}

impl Table {
    fn slot(&mut self, idx: u32) -> &mut Slot {
        self.slots.iter_mut().find(|s| s.part.idx == idx).expect("slot exists")
    }
    fn snapshot(&self) -> Vec<Part> {
        let mut v: Vec<Part> = self.slots.iter().map(|s| s.part.clone()).collect();
        v.sort_by_key(|p| p.start);
        v
    }
}

#[derive(Debug)]
enum WorkerResult {
    Done,
    Stopped,
    RangeIgnored,
    Failed(LdmError),
}

enum StreamEnd {
    Complete,
    Short { progressed: bool },
    Stopped,
}

async fn run_multi(env: &JobEnv, spec: &HttpJobSpec, st: &mut JobState) -> Result<()> {
    let cfg = env.cfg.engine.clone();
    let total = st.total.unwrap_or(0);
    let max_conns = spec.max_conns.clamp(1, 32);
    let min_part = cfg.min_part_size.max(1);
    if st.parts.is_empty() {
        st.parts = split_plan(total, max_conns, min_part);
    }
    env.progress.set_phase("Running");
    let initial_got: u64 = st.parts.iter().map(|p| p.got).sum();
    env.progress.set_got(initial_got);

    let table = Arc::new(Mutex::new(Table {
        slots: st
            .parts
            .iter()
            .map(|p| {
                let mut part = p.clone();
                part.state = if part.remaining() == 0 { PartState::Done } else { PartState::Pending };
                Slot { claimed: part.got, last_got: part.got, part, active: false, speed: 0.0, slow_for: 0.0 }
            })
            .collect(),
    }));
    let wcancel = env.cancel.child_token();
    let url: Arc<str> = Arc::from(spec.url.as_str());
    let ctx = Arc::new(spec.ctx.clone());
    let mut set: JoinSet<(u32, WorkerResult)> = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_millis(500));
    let mut last_tick = Instant::now();
    let mut last_persist = Instant::now();
    let mut prefer: Option<u32> = None;
    let mut fatal: Option<LdmError> = None;

    let spawn = |set: &mut JoinSet<(u32, WorkerResult)>, idx: u32| {
        let env = env.clone();
        let url = url.clone();
        let ctx = ctx.clone();
        let table = table.clone();
        let dir = spec.parts_dir.clone();
        let c = wcancel.clone();
        set.spawn(async move {
            let r = part_worker(idx, &env, &url, &ctx, &table, &dir, &c).await;
            (idx, r)
        });
    };

    loop {
        if fatal.is_none() && !env.cancel.is_cancelled() {
            let mut t = table.lock().unwrap();
            loop {
                let active = t.slots.iter().filter(|s| s.active).count() as u32;
                if active >= max_conns {
                    break;
                }
                if let Some(s) = t.slots.iter_mut().find(|s| !s.active && s.part.state == PartState::Pending) {
                    s.active = true;
                    s.part.state = PartState::Active;
                    s.claimed = s.part.got;
                    let idx = s.part.idx;
                    spawn(&mut set, idx);
                    continue;
                }
                if env.climb_gated(active) {
                    break;
                }
                let spans: Vec<Span> = t
                    .slots
                    .iter()
                    .map(|s| Span { idx: s.part.idx, state: s.part.state, cursor: s.part.start + s.claimed, end: s.part.end })
                    .collect();
                match choose_split(&spans, min_part, max_conns, cfg.split_active, prefer.take()) {
                    Some((idx, mid)) => {
                        let next = t.slots.iter().map(|s| s.part.idx).max().unwrap_or(0) + 1;
                        let donor = t.slot(idx);
                        let old_end = donor.part.end;
                        donor.part.end = mid;
                        debug!(donor = idx, new = next, mid, "split");
                        t.slots.push(Slot {
                            part: Part { idx: next, start: mid, end: old_end, got: 0, state: PartState::Pending },
                            claimed: 0,
                            active: false,
                            last_got: 0,
                            speed: 0.0,
                            slow_for: 0.0,
                        });
                        env.progress.note_split();
                    }
                    None => break,
                }
            }
            let n = t.slots.iter().filter(|s| s.active).count() as u32;
            env.progress.connections.store(n, std::sync::atomic::Ordering::Relaxed);
            if t.slots.iter().all(|s| s.part.state == PartState::Done) && set.is_empty() {
                break;
            }
        }
        if set.is_empty() {
            break;
        }
        tokio::select! {
            joined = set.join_next() => {
                let Some(joined) = joined else { continue };
                let (idx, res) = joined.map_err(|e| LdmError::Other(format!("worker panicked: {e}")))?;
                let mut t = table.lock().unwrap();
                let s = t.slot(idx);
                s.active = false;
                match res {
                    WorkerResult::Done => {
                        s.part.state = PartState::Done;
                        s.part.got = s.part.len();
                    }
                    WorkerResult::Stopped => {
                        s.part.state = if s.part.remaining() == 0 { PartState::Done } else { PartState::Pending };
                    }
                    WorkerResult::RangeIgnored => {
                        s.part.state = PartState::Pending;
                        if fatal.is_none() { fatal = Some(LdmError::RangeIgnored); }
                        wcancel.cancel();
                    }
                    WorkerResult::Failed(e) => {
                        s.part.state = PartState::Failed;
                        warn!(part = idx, err = %e, "part failed");
                        if fatal.is_none() { fatal = Some(e); }
                        wcancel.cancel();
                    }
                }
                drop(t);
                st.parts = table.lock().unwrap().snapshot();
                persist(env, &spec.parts_dir, st);
                last_persist = Instant::now();
            }
            _ = tick.tick() => {
                let dt = last_tick.elapsed().as_secs_f64().max(0.001);
                last_tick = Instant::now();
                let mut t = table.lock().unwrap();
                let mut sum = 0.0;
                let mut n = 0.0;
                for s in t.slots.iter_mut().filter(|s| s.active) {
                    s.speed = s.part.got.saturating_sub(s.last_got) as f64 / dt;
                    s.last_got = s.part.got;
                    sum += s.speed;
                    n += 1.0;
                }
                if n >= 2.0 {
                    let avg = sum / n;
                    for s in t.slots.iter_mut().filter(|s| s.active) {
                        let rem = s.part.end.saturating_sub(s.part.start + s.claimed);
                        if s.speed < avg * cfg.steal_alpha && rem >= 2 * min_part {
                            s.slow_for += dt;
                            if s.slow_for >= cfg.steal_grace_secs as f64 {
                                prefer = Some(s.part.idx);
                                s.slow_for = 0.0;
                            }
                        } else {
                            s.slow_for = 0.0;
                        }
                    }
                }
                if last_persist.elapsed() >= Duration::from_secs(1) {
                    st.parts = t.snapshot();
                    drop(t);
                    persist(env, &spec.parts_dir, st);
                    last_persist = Instant::now();
                }
            }
        }
    }

    st.parts = table.lock().unwrap().snapshot();
    persist(env, &spec.parts_dir, st);
    if let Some(e) = fatal {
        return Err(e);
    }
    if env.cancel.is_cancelled() {
        return Err(LdmError::Canceled);
    }
    if !st.parts.iter().all(|p| p.state == PartState::Done) {
        return Err(LdmError::Other("scheduler stopped with unfinished parts".into()));
    }
    Ok(())
}

async fn part_worker(
    idx: u32,
    env: &JobEnv,
    url: &str,
    ctx: &RequestCtx,
    table: &Arc<Mutex<Table>>,
    dir: &Path,
    cancel: &CancellationToken,
) -> WorkerResult {
    let cfg = &env.cfg.engine;
    let mut attempts = 0u32;
    loop {
        let (start, from, end) = {
            let mut t = table.lock().unwrap();
            let s = t.slot(idx);
            s.claimed = s.part.got;
            (s.part.start, s.part.start + s.part.got, s.part.end)
        };
        if from >= end {
            return WorkerResult::Done;
        }
        let permit = tokio::select! {
            p = env.conn_sem.clone().acquire_owned() => match p { Ok(p) => p, Err(_) => return WorkerResult::Stopped },
            _ = cancel.cancelled() => return WorkerResult::Stopped,
        };
        env.progress.conn_opened();
        let out = tokio::select! {
            r = env.client.get_range(url, ctx, from, Some(end - 1)) => match r {
                Ok(r) => stream_part(idx, env, r, table, dir, start, from, cancel).await,
                Err(e) => Err(e),
            },
            _ = cancel.cancelled() => Ok(StreamEnd::Stopped),
        };
        env.progress.conn_closed();
        drop(permit);
        match out {
            Ok(StreamEnd::Complete) => return WorkerResult::Done,
            Ok(StreamEnd::Stopped) => return WorkerResult::Stopped,
            Ok(StreamEnd::Short { progressed }) => {
                if progressed {
                    attempts = 0;
                    continue;
                }
                attempts += 1;
            }
            Err(LdmError::RangeIgnored) => return WorkerResult::RangeIgnored,
            Err(e) if e.retryable() => {
                attempts += 1;
                warn!(part = idx, attempts, err = %e, "connection error, will retry");
            }
            Err(e) => return WorkerResult::Failed(e),
        }
        if attempts > cfg.max_retries {
            return WorkerResult::Failed(LdmError::RetryExhausted(format!("part {idx}: {attempts} attempts")));
        }
        if !sleep_or_cancel(backoff(cfg.retry_base_ms, attempts), cancel).await {
            return WorkerResult::Stopped;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stream_part(
    idx: u32,
    env: &JobEnv,
    resp: reqwest::Response,
    table: &Arc<Mutex<Table>>,
    dir: &Path,
    start: u64,
    from: u64,
    cancel: &CancellationToken,
) -> Result<StreamEnd> {
    let cfg = &env.cfg.engine;
    match resp.status() {
        StatusCode::PARTIAL_CONTENT => {
            let cr = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).and_then(parse_content_range);
            match cr {
                Some((a, _, _)) if a == from => {}
                _ => return Err(LdmError::RangeIgnored),
            }
        }
        StatusCode::OK if from == 0 => {}
        StatusCode::OK => return Err(LdmError::RangeIgnored),
        s => return Err(LdmError::HttpStatus(s.as_u16(), "unexpected status".into())),
    }
    let head_check = if cfg.fake_range_check && start > 0 && from == start { read_head(dir, cfg.fake_range_prefix) } else { None };
    let file = open_part(&part_path(dir, idx), from - start).await?;
    let mut w = tokio::io::BufWriter::with_capacity(256 * 1024, file);
    let mut stream = resp.bytes_stream();
    let read_to = Duration::from_secs(cfg.timeout_secs.max(1));
    let mut progressed = false;
    let mut first = true;
    loop {
        let next = tokio::select! {
            n = tokio::time::timeout(read_to, stream.next()) => n,
            _ = cancel.cancelled() => { w.flush().await?; return Ok(StreamEnd::Stopped) }
        };
        let chunk = match next {
            Err(_) => {
                w.flush().await?;
                return Err(LdmError::Timeout(format!("part {idx}: read timed out")));
            }
            Ok(None) => {
                w.flush().await?;
                return Ok(StreamEnd::Short { progressed });
            }
            Ok(Some(Err(e))) => {
                w.flush().await?;
                return Err(e.into());
            }
            Ok(Some(Ok(b))) => b,
        };
        if first {
            first = false;
            if let Some(head) = &head_check {
                let n = head.len().min(chunk.len());
                if n >= 64 && head[..n] == chunk[..n] && head[..n].iter().any(|b| *b != head[0]) {
                    warn!(part = idx, "part data equals file head: Range is fake");
                    return Err(LdmError::RangeIgnored);
                }
            }
        }
        tokio::select! {
            _ = env.throttle(chunk.len()) => {},
            _ = cancel.cancelled() => { w.flush().await?; return Ok(StreamEnd::Stopped) }
        }
        let (allowed, reached) = {
            let mut t = table.lock().unwrap();
            let s = t.slot(idx);
            let avail = s.part.end.saturating_sub(s.part.start + s.claimed);
            let a = avail.min(chunk.len() as u64);
            s.claimed += a;
            (a as usize, s.part.start + s.claimed >= s.part.end)
        };
        if allowed > 0 {
            w.write_all(&chunk[..allowed]).await?;
            table.lock().unwrap().slot(idx).part.got += allowed as u64;
            env.progress.add_got(allowed as u64);
            progressed = true;
        }
        if reached {
            w.flush().await?;
            if cfg.fsync {
                w.get_ref().sync_all().await?;
            }
            return Ok(StreamEnd::Complete);
        }
    }
}

fn read_head(dir: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(part_path(dir, 0)).ok()?;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        match f.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(k) => got += k,
            Err(_) => return None,
        }
    }
    if got < 64 {
        return None;
    }
    buf.truncate(got);
    Some(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(cd: Option<&str>, mime: Option<&str>, url: &str) -> Probe {
        Probe { filename: cd.map(String::from), mime: mime.map(String::from), final_url: url.into(), ..Default::default() }
    }

    #[test]
    fn title_name_beats_disposition_but_keeps_its_type() {
        let mut s = HttpJobSpec { url: "https://cdn.example.com/v/1080P_4000K_123.mp4?t=1".into(), ..Default::default() };
        let p = probe(Some("server-name.mp4"), Some("video/mp4"), &s.url);
        // plain download: Content-Disposition wins as before
        assert_eq!(choose_name(&s, &p), "server-name.mp4");
        // media candidate: page title + extension from the response
        s.title_stem = Some("Mr. Smith goes 中文/日本語".into());
        assert_eq!(choose_name(&s, &p), "Mr. Smith goes 中文_日本語.mp4");
        // no CD: URL extension; nothing at all: MIME, then mp4
        assert_eq!(choose_name(&s, &probe(None, None, "https://cdn.example.com/a/clip.webm")), "Mr. Smith goes 中文_日本語.webm");
        let mut bare = s.clone();
        bare.url = "https://cdn.example.com/stream".into();
        assert_eq!(
            choose_name(&bare, &probe(None, Some("video/webm"), "https://cdn.example.com/stream")),
            "Mr. Smith goes 中文_日本語.webm"
        );
        assert_eq!(choose_name(&bare, &probe(None, None, "https://cdn.example.com/stream")), "Mr. Smith goes 中文_日本語.mp4");
        // an explicit output name still wins
        s.output.filename = Some("mine.mp4".into());
        assert_eq!(choose_name(&s, &p), "mine.mp4");
        // empty title → old order
        s.output.filename = None;
        s.title_stem = Some("  ".into());
        assert_eq!(choose_name(&s, &p), "server-name.mp4");
    }
}
