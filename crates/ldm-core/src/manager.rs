//! Download manager: queue scheduler, job lifecycle, progress events,
//! media-candidate bookkeeping and rule sets.
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{Datelike, Timelike};
use ldm_ipc::messages::{types, DownloadAdd, JobProgress, MediaCandidate, RuleSetPush};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::{broadcast, Notify, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::config::{Config, EngineConfig};
use crate::engine::{run_http_job, HttpJobSpec, JobEnv, OutputSpec, Progress};
use crate::error::{LdmError, Result};
use crate::http::{HttpClient, RequestCtx};
use crate::media::{adscore, probe_media, run_media_job, MediaJobSpec, MediaPlan};
use crate::paths;
use crate::store::{now_ms, state, CandidateRow, JobExtra, JobRow, PartRow, QueueRow, Store, YtdlpJob};
use crate::tools::{self, AutoInstall, Found, Source, Tool};
use crate::util::category::Category;
use crate::util::classify::{self, MediaKind};
use crate::util::filename;
use crate::util::ratelimit::TokenBucket;
use crate::util::security;
use crate::ytdlp;

/// Event broadcast to subscribers (GUI, CLI `--follow`, extensions).
#[derive(Debug, Clone)]
pub struct Event {
    pub kind: &'static str,
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AddResult {
    pub job: Option<JobRow>,
    pub duplicate: bool,
    pub ignored: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    None,
    Pause,
    Window,
    Remove { delete_files: bool },
    Shutdown,
}

struct Running {
    queue_id: String,
    cancel: CancellationToken,
    progress: Arc<Progress>,
    intent: Arc<Mutex<Intent>>,
    job_limiter: Arc<TokenBucket>,
}

/// Keeps a GUI registered while its connection lives (see `gui_attach`).
pub struct GuiGuard(Arc<std::sync::atomic::AtomicUsize>);

impl Drop for GuiGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// (path, mtime, domains) of the last read `media.ad_domains_file`
type AdDomainCache = (String, Option<std::time::SystemTime>, Arc<Vec<String>>);
/// (tab, page) → candidates of that page
type PageGroup = ((i64, Option<String>), Vec<CandidateRow>);

struct Inner {
    cfg: RwLock<Arc<Config>>,
    cfg_path: Option<PathBuf>,
    store: Store,
    client: RwLock<HttpClient>,
    /// Proxy reported by the browser extension (`ProxyInfo`); used when `engine.proxy == "browser"`.
    browser_proxy: RwLock<Option<String>>,
    global_limiter: Arc<TokenBucket>,
    conn_sem: Arc<Semaphore>,
    running: Mutex<HashMap<String, Running>>,
    events: broadcast::Sender<Event>,
    wake: Notify,
    shutdown: CancellationToken,
    recent_manifests: Mutex<HashMap<String, Instant>>,
    /// media.ad_domains_file cache: (path, mtime, domains)
    ad_domains: Mutex<Option<AdDomainCache>>,
    recent_adds: Mutex<HashMap<String, (Instant, String)>>,
    started: Instant,
    rules_dirs: Vec<PathBuf>,
    gui_clients: Arc<std::sync::atomic::AtomicUsize>,
    /// Queues that had work since they last drained (for "队列完成后" actions).
    queue_busy: Mutex<std::collections::HashSet<String>>,
    ytdlp_info: ytdlp::InfoCache,
    /// Managed-tool binaries used by running jobs (never pruned / removed while listed).
    tools_in_use: Mutex<HashMap<String, Vec<PathBuf>>>,
    /// Serialises install / update / rollback / remove inside this daemon (the tools dir
    /// additionally has a file lock against other processes).
    tools_busy: tokio::sync::Mutex<()>,
    /// `--version` of non-managed binaries: path → (mtime, version)
    tool_versions: Mutex<HashMap<PathBuf, ToolVersionEntry>>,
}

type ToolVersionEntry = (Option<std::time::SystemTime>, Option<String>);

#[derive(Clone)]
pub struct Manager {
    inner: Arc<Inner>,
}

/// Is `now` inside a queue schedule window?
/// Schedule format: `{"start":"HH:MM","end":"HH:MM","days":[1..7]}` (1 = Monday).
pub fn window_open(schedule: Option<&Value>, now: chrono::DateTime<chrono::Local>) -> bool {
    let Some(s) = schedule else { return true };
    let parse = |k: &str| -> Option<u32> {
        let v = s.get(k)?.as_str()?;
        let (h, m) = v.split_once(':')?;
        Some(h.trim().parse::<u32>().ok()? * 60 + m.trim().parse::<u32>().ok()?)
    };
    if let Some(days) = s.get("days").and_then(|d| d.as_array()) {
        let today = now.weekday().number_from_monday() as u64;
        if !days.iter().any(|d| d.as_u64() == Some(today)) {
            return false;
        }
    }
    let (Some(a), Some(b)) = (parse("start"), parse("end")) else { return true };
    let t = now.hour() * 60 + now.minute();
    if a <= b {
        t >= a && t < b
    } else {
        t >= a || t < b
    }
}

fn new_id() -> String {
    format!("{:08x}{:04x}", rand::random::<u32>(), (now_ms() / 1000) as u16)
}

fn strip_fragment(u: &str) -> &str {
    u.split('#').next().unwrap_or(u)
}

fn kind_label(k: MediaKind) -> &'static str {
    match k {
        MediaKind::Hls => "hls",
        MediaKind::Dash => "dash",
        _ => "http",
    }
}

impl Manager {
    pub fn new(cfg: Config, cfg_path: Option<PathBuf>, store: Store) -> Result<Manager> {
        let client = match client_for(&cfg.engine, None) {
            // a broken PAC setting must not keep the daemon from starting; browsers go DIRECT too
            Err(e) if cfg.engine.proxy == "pac" => {
                warn!(error = %e, "PAC proxy setting unusable; connecting DIRECT");
                client_for(&EngineConfig { proxy: "none".into(), ..cfg.engine.clone() }, None)?
            }
            r => r?,
        };
        let n = store.recover_interrupted()?;
        if n > 0 {
            info!(recovered = n, "re-queued interrupted jobs");
        }
        let (tx, _) = broadcast::channel(1024);
        let mut rules_dirs = vec![paths::data_dir().join("rules")];
        if let Ok(d) = std::env::var("LDM_RULES_DIR") {
            rules_dirs.insert(0, PathBuf::from(d));
        }
        // rules bundled by install.sh live in <prefix>/lib/ldm/rules (next to bin/)
        if let Some(prefix) = std::env::current_exe().ok().and_then(|e| e.parent().and_then(|b| b.parent()).map(|p| p.to_path_buf())) {
            rules_dirs.push(prefix.join("lib/ldm/rules"));
        }
        for d in ["/usr/lib/ldm/rules", "/usr/local/lib/ldm/rules"] {
            if !rules_dirs.iter().any(|x| x == std::path::Path::new(d)) {
                rules_dirs.push(PathBuf::from(d));
            }
        }
        Ok(Manager {
            inner: Arc::new(Inner {
                global_limiter: Arc::new(TokenBucket::new(cfg.engine.effective_speed_limit())),
                conn_sem: Arc::new(Semaphore::new(cfg.engine.max_connections_global.max(1) as usize)),
                cfg: RwLock::new(Arc::new(cfg)),
                cfg_path,
                store,
                client: RwLock::new(client),
                browser_proxy: RwLock::new(None),
                running: Mutex::new(HashMap::new()),
                events: tx,
                wake: Notify::new(),
                shutdown: CancellationToken::new(),
                recent_manifests: Mutex::new(HashMap::new()),
                ad_domains: Mutex::new(None),
                recent_adds: Mutex::new(HashMap::new()),
                started: Instant::now(),
                rules_dirs,
                gui_clients: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                queue_busy: Mutex::new(Default::default()),
                ytdlp_info: Default::default(),
                tools_in_use: Mutex::new(HashMap::new()),
                tools_busy: tokio::sync::Mutex::new(()),
                tool_versions: Mutex::new(HashMap::new()),
            }),
        })
    }

    pub fn config(&self) -> Arc<Config> {
        self.inner.cfg.read().unwrap().clone()
    }

    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.inner.events.subscribe()
    }

    /// A GUI subscribed to events (it can answer capture confirmations).
    pub fn gui_attach(&self) -> GuiGuard {
        self.inner.gui_clients.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        GuiGuard(self.inner.gui_clients.clone())
    }

    pub fn gui_count(&self) -> usize {
        self.inner.gui_clients.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn emit(&self, kind: &'static str, payload: Value) {
        let _ = self.inner.events.send(Event { kind, payload });
    }

    fn emit_job(&self, id: &str) {
        if let Ok(Some(j)) = self.inner.store.get_job(id) {
            self.emit(types::JOB_STATE_CHANGED, public_job(&j));
        }
    }

    /// Start the scheduler loop (call once inside a tokio runtime).
    pub fn start(&self) -> tokio::task::JoinHandle<()> {
        let me = self.clone();
        tokio::spawn(async move {
            let mut last_prune = Instant::now();
            let mut last_rules = Instant::now();
            let mut rules_fp = me.rules_fingerprint();
            // first managed-tools update check two minutes after start, then every 30 minutes
            let mut last_tools = Instant::now().checked_sub(Duration::from_secs(28 * 60)).unwrap_or_else(Instant::now);
            loop {
                tokio::select! {
                    _ = me.inner.wake.notified() => {}
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                    _ = me.inner.shutdown.cancelled() => break,
                }
                if let Err(e) = me.schedule() {
                    warn!(err = %e, "scheduler error");
                }
                if last_rules.elapsed() >= Duration::from_secs(2) {
                    last_rules = Instant::now();
                    let fp = me.rules_fingerprint();
                    if fp != rules_fp {
                        rules_fp = fp;
                        info!("site rules changed on disk, pushing new rule set");
                        me.bump_ruleset();
                    }
                }
                if last_tools.elapsed() > Duration::from_secs(30 * 60) {
                    last_tools = Instant::now();
                    let m2 = me.clone();
                    tokio::spawn(async move { m2.tools_auto_update().await });
                }
                if last_prune.elapsed() > Duration::from_secs(60) {
                    last_prune = Instant::now();
                    let ttl = me.config().media.candidate_ttl_secs.max(60) as i64 * 1000 * 10;
                    let _ = me.inner.store.prune_candidates(now_ms() - ttl);
                }
            }
        })
    }

    fn schedule(&self) -> Result<()> {
        if self.inner.shutdown.is_cancelled() {
            return Ok(());
        }
        let cfg = self.config();
        let queues: HashMap<String, QueueRow> = self.inner.store.list_queues()?.into_iter().map(|q| (q.id.clone(), q)).collect();
        let now = chrono::Local::now();
        // gentle pause of jobs whose queue window closed
        {
            let running = self.inner.running.lock().unwrap();
            for r in running.values() {
                if let Some(q) = queues.get(&r.queue_id) {
                    if !q.enabled || !window_open(q.schedule.as_ref(), now) {
                        let mut i = r.intent.lock().unwrap();
                        if *i == Intent::None {
                            *i = Intent::Window;
                            r.cancel.cancel();
                        }
                    }
                }
            }
        }
        let queued = self.inner.store.list_jobs(Some(state::QUEUED))?;
        for job in queued {
            let (total, per_queue) = {
                let running = self.inner.running.lock().unwrap();
                (running.len(), running.values().filter(|r| r.queue_id == job.queue_id).count())
            };
            if total >= cfg.queue.global_max_active.max(1) as usize {
                break;
            }
            let Some(q) = queues.get(&job.queue_id).or_else(|| queues.get("main")) else { continue };
            if !q.enabled || !window_open(q.schedule.as_ref(), now) || per_queue >= q.concurrent.max(1) as usize {
                continue;
            }
            if self.inner.running.lock().unwrap().contains_key(&job.id) {
                continue;
            }
            self.start_job(job)?;
        }
        Ok(())
    }

    fn start_job(&self, mut job: JobRow) -> Result<()> {
        let cfg = self.config();
        job.extra.last_try_at = Some(now_ms());
        self.inner.store.set_extra(&job.id, &job.extra)?;
        self.inner.store.set_state(&job.id, state::PROBING, None)?;
        self.inner.queue_busy.lock().unwrap().insert(job.queue_id.clone());
        let job_limiter = Arc::new(TokenBucket::new(job.extra.speed_limit_bps));
        let progress = Progress::new();
        progress.set_got(job.got);
        progress.set_total(job.total);
        let cancel = CancellationToken::new();
        let intent = Arc::new(Mutex::new(Intent::None));
        let store_sink = {
            let me = self.clone();
            let id = job.id.clone();
            let dir = PathBuf::from(job.job_state_path.clone().unwrap_or_default());
            Arc::new(move |st: &crate::engine::state::JobState| {
                let rows: Vec<PartRow> = st
                    .parts
                    .iter()
                    .map(|p| PartRow {
                        idx: p.idx,
                        start: p.start,
                        end: p.end as i64,
                        got: p.got,
                        path: dir.join(format!("part_{:04}.bin", p.idx)).to_string_lossy().into_owned(),
                        state: format!("{:?}", p.state),
                    })
                    .collect();
                let _ = me.inner.store.save_parts(&id, &rows);
            }) as crate::engine::StateSink
        };
        let env = JobEnv {
            cfg: cfg.clone(),
            client: self.inner.client.read().unwrap().clone(),
            global_limiter: self.inner.global_limiter.clone(),
            job_limiter: Some(job_limiter.clone()),
            conn_sem: self.inner.conn_sem.clone(),
            progress: progress.clone(),
            cancel: cancel.clone(),
            sink: Some(store_sink),
        };
        self.inner.running.lock().unwrap().insert(
            job.id.clone(),
            Running {
                queue_id: job.queue_id.clone(),
                cancel: cancel.clone(),
                progress: progress.clone(),
                intent: intent.clone(),
                job_limiter,
            },
        );
        self.emit_job(&job.id);
        let me = self.clone();
        tokio::spawn(async move {
            let done = CancellationToken::new();
            let reporter = tokio::spawn(me.clone().report_loop(job.id.clone(), progress.clone(), done.clone()));
            let res = me.run_job(&env, &job).await;
            done.cancel();
            let _ = reporter.await;
            me.finish_job(&job, res, *intent.lock().unwrap(), &progress);
        });
        Ok(())
    }

    async fn run_job(&self, env: &JobEnv, job: &JobRow) -> Result<(PathBuf, u64, Option<String>, Option<String>)> {
        let mut headers = job.headers.clone();
        if !headers.iter().any(|(n, _)| n.eq_ignore_ascii_case("authorization")) {
            let host = url::Url::parse(&job.url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
            if let Some(l) = env.cfg.sites.login_for(&host) {
                use base64::Engine as _;
                let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", l.user, l.password));
                headers.push(("Authorization".into(), format!("Basic {token}")));
            }
        }
        let ctx = RequestCtx {
            referrer: job.referrer.clone().or_else(|| job.page_url.clone()),
            cookies: job.cookies.clone(),
            user_agent: job.user_agent.clone(),
            headers,
        };
        let work = PathBuf::from(
            job.job_state_path.clone().unwrap_or_else(|| env.cfg.paths.part_dir().join(&job.id).to_string_lossy().into_owned()),
        );
        let output = OutputSpec { dir: job.save_dir.as_ref().map(|d| PathBuf::from(paths::expand(d))), filename: None, exact_path: None };
        match job.kind.as_str() {
            "ytdlp" => self.run_ytdlp(env, job, &work, &output).await,
            "hls" | "dash" | "av" => {
                let spec = MediaJobSpec {
                    audio_url: if job.kind == "av" { job.extra.audio_url.clone() } else { None },
                    url: job.url.clone(),
                    ctx,
                    work_dir: work,
                    output,
                    filename_hint: job.filename.clone(),
                    preferred_height: job.preferred_height,
                };
                let o = run_media_job(env, &spec).await?;
                Ok((o.path, o.bytes, None, Some("video/mp4".into())))
            }
            _ => {
                // main URL first, then mirrors, each with its own resume dir
                let urls: Vec<&String> = std::iter::once(&job.url).chain(job.extra.mirrors.iter()).collect();
                let mut last_err = None;
                for (i, url) in urls.iter().enumerate() {
                    let spec = HttpJobSpec {
                        url: (*url).clone(),
                        ctx: ctx.clone(),
                        max_conns: job.max_connections.max(1),
                        parts_dir: if i == 0 { work.clone() } else { work.join(format!("mirror{i}")) },
                        output: output.clone(),
                        filename_hint: job.filename.clone(),
                        sha256: job.sha256.clone(),
                        title_stem: job.extra.title_name.clone(),
                    };
                    let r = if crate::ftp::is_ftp_url(url) {
                        let mut spec = spec;
                        spec.url = ftp_with_login(&spec.url, &env.cfg);
                        crate::ftp::run_ftp_job(env, &spec).await
                    } else {
                        run_http_job(env, &spec).await
                    };
                    match r {
                        Ok(o) => {
                            if let Err(e) = crate::engine::verify_md5(&o.path, job.extra.md5.as_deref()) {
                                let _ = std::fs::remove_file(&o.path);
                                return Err(e);
                            }
                            // a media capture that turned into a web page (expired link, login wall)
                            let html = o.mime.as_deref().map(|m| m.to_ascii_lowercase().starts_with("text/html")).unwrap_or(false);
                            if html && (job.extra.title_name.is_some() || job.category.as_deref() == Some("video")) {
                                let _ = std::fs::remove_file(&o.path);
                                return Err(LdmError::Format("not media: server returned an HTML page".into()));
                            }
                            // one byte range of a segmented stream (moof/mdat, no moov) is not a playable video
                            let media_capture = job.extra.title_name.is_some() || job.category.as_deref() == Some("video");
                            let ext = filename::extension_of(&o.path.to_string_lossy());
                            if media_capture
                                && matches!(ext.as_str(), "mp4" | "m4v" | "mov" | "m4a")
                                && crate::util::mp4::has_movie_header(&o.path) == Some(false)
                            {
                                let _ = std::fs::remove_file(&o.path);
                                return Err(LdmError::Format(
                                    "not media: MP4 fragment without a movie header (one byte range of a segmented stream)".into(),
                                ));
                            }
                            return Ok((o.path, o.total, Some(o.final_url), o.mime));
                        }
                        Err(e) if matches!(e, LdmError::Canceled | LdmError::Checksum { .. }) || e.is_disk_full() => return Err(e),
                        Err(e) => {
                            if i + 1 < urls.len() {
                                warn!(job = %job.id, err = %security::redact_url(&e.to_string()), next = %security::redact_url(urls[i + 1]), "download failed, trying mirror");
                            }
                            last_err = Some(e);
                        }
                    }
                }
                Err(last_err.unwrap_or(LdmError::Other("no URL".into())))
            }
        }
    }

    async fn report_loop(self, id: String, progress: Arc<Progress>, done: CancellationToken) {
        let mut last_state = String::new();
        let mut last_db = Instant::now();
        loop {
            let stop = tokio::select! {
                _ = tokio::time::sleep(Duration::from_millis(500)) => false,
                _ = done.cancelled() => true,
            };
            let got = progress.got();
            progress.speed.record(got);
            let st = match progress.phase().as_str() {
                "Probing" | "" => state::PROBING,
                "Assembling" => state::ASSEMBLING,
                "Recording" => state::RECORDING,
                _ => state::RUNNING,
            };
            if !stop && st != last_state {
                last_state = st.to_string();
                let _ = self.inner.store.set_state(&id, st, None);
                if let Some(name) = progress.filename() {
                    if let Ok(Some(mut j)) = self.inner.store.get_job(&id) {
                        if j.filename.as_deref() != Some(name.as_str()) && j.filename.is_none() {
                            j.filename = Some(name);
                            let _ = self.inner.store.upsert_job(&j);
                        }
                    }
                }
                self.emit_job(&id);
            }
            if last_db.elapsed() > Duration::from_secs(2) || stop {
                last_db = Instant::now();
                let _ = self.inner.store.update_progress(&id, got, progress.total());
            }
            if !stop {
                let seg_total = progress.segments_total.load(std::sync::atomic::Ordering::Relaxed);
                let p = JobProgress {
                    job_id: id.clone(),
                    got,
                    total: progress.total(),
                    speed_bps: progress.speed.speed(),
                    state: st.to_string(),
                    connections: Some(progress.connections.load(std::sync::atomic::Ordering::Relaxed)),
                    segments_done: (seg_total > 0).then(|| progress.segments_done.load(std::sync::atomic::Ordering::Relaxed)),
                    segments_total: (seg_total > 0).then_some(seg_total),
                    phase: Some(progress.phase()).filter(|p| p == "Recording"),
                    recorded_secs: progress.is_live().then(|| progress.recorded_secs()),
                };
                self.emit(types::JOB_PROGRESS, serde_json::to_value(p).unwrap_or_default());
            }
            if stop {
                break;
            }
        }
    }

    fn finish_job(&self, job: &JobRow, res: Result<(PathBuf, u64, Option<String>, Option<String>)>, intent: Intent, progress: &Progress) {
        let store = &self.inner.store;
        let id = &job.id;
        self.inner.running.lock().unwrap().remove(id);
        match res {
            Ok((path, total, final_url, mime)) => {
                if let Ok(Some(mut j)) = store.get_job(id) {
                    j.final_path = Some(path.to_string_lossy().into_owned());
                    j.filename = path.file_name().map(|n| n.to_string_lossy().into_owned());
                    j.total = Some(total);
                    j.got = total;
                    j.final_url = final_url.or(j.final_url);
                    j.mime = mime.or(j.mime);
                    j.category = Some(
                        self.config().paths.category_of(&filename::extension_of(j.filename.as_deref().unwrap_or(""))).id().to_string(),
                    );
                    j.state = state::COMPLETED.into();
                    j.error = None;
                    j.error_code = None;
                    j.updated_at = now_ms();
                    j.finished_at = Some(now_ms());
                    let _ = store.upsert_job(&j);
                    let _ = store.save_parts(id, &[]);
                    info!(job = %id, path = %path.display(), bytes = total, "download completed");
                    self.emit_job(id);
                    self.emit(types::NOTIFY, json!({"title": "下载完成", "body": j.filename, "jobId": id, "path": j.final_path}));
                    self.emit(types::JOB_COMPLETED, public_job(&j));
                    self.after_complete(&j);
                }
            }
            Err(e) => {
                let _ = store.update_progress(id, progress.got(), progress.total());
                match (intent, &e) {
                    (Intent::Remove { delete_files }, _) => {
                        self.purge(job, delete_files);
                        let _ = store.delete_job(id);
                        self.emit(types::JOB_STATE_CHANGED, json!({"id": id, "state": "Removed"}));
                    }
                    (Intent::Pause, LdmError::Canceled) => {
                        let _ = store.set_state(id, state::PAUSED, None);
                        self.emit_job(id);
                    }
                    (Intent::Window | Intent::Shutdown, LdmError::Canceled) => {
                        let _ = store.set_state(id, state::QUEUED, None);
                        self.emit_job(id);
                    }
                    (_, e) if e.is_disk_full() => {
                        let _ = store.set_state(id, state::PAUSED, Some((e.code(), &e.to_string())));
                        self.emit_job(id);
                        self.emit(types::NOTIFY, json!({"title": "磁盘已满，任务已暂停", "body": job.url, "jobId": id}));
                    }
                    (_, e) => {
                        let cookie_flow = matches!(e, LdmError::HttpStatus(401 | 403, _))
                            && job.source.as_deref() == Some("extension")
                            && !job.extra.cookie_refresh;
                        let fb = self.fallback_reason(job, e);
                        if let (Some(reason), false) = (&fb, cookie_flow) {
                            if self.fallback_to_ytdlp(id, reason, e) {
                                self.check_queue_drained(&job.queue_id);
                                self.inner.wake.notify_one();
                                return;
                            }
                        }
                        warn!(job = %id, code = e.code(), err = %security::redact_url(&e.to_string()), "download failed");
                        let msg = match (e, &job.cookies) {
                            (LdmError::HttpStatus(401, _), _) => format!("{e}（需要登录：可在“选项 → 站点登录”保存账号后重试）"),
                            (LdmError::HttpStatus(403, _), Some(_)) => format!("{e}（Cookie 可能已过期：在浏览器中刷新页面后重试）"),
                            _ if job.kind == "ytdlp" => match job.extra.ytdlp.as_ref().and_then(|y| y.reason.as_deref()) {
                                Some(r) => format!("{e}（via yt-dlp；LDM 原失败原因：{r}）"),
                                None => format!("{e}（via yt-dlp）"),
                            },
                            _ => e.to_string(),
                        };
                        let _ = store.set_state(id, state::FAILED, Some((e.code(), &msg)));
                        // browser captures: ask the extension once for fresh cookies, then retry (see cookies_refreshed)
                        if matches!(e, LdmError::HttpStatus(401 | 403, _))
                            && job.source.as_deref() == Some("extension")
                            && !job.extra.cookie_refresh
                        {
                            let mut x = job.extra.clone();
                            x.cookie_refresh = true;
                            x.fallback_pending = fb.clone();
                            let _ = store.set_extra(id, &x);
                            let page = job.page_url.clone().or(job.referrer.clone()).unwrap_or_else(|| job.url.clone());
                            self.emit(types::GET_COOKIES, json!({"url": job.url, "pageUrl": page, "jobId": id}));
                        }
                        self.emit_job(id);
                        self.emit(
                            types::NOTIFY,
                            json!({"title": "下载失败", "body": format!("{}: {e}", job.url), "jobId": id, "code": e.code()}),
                        );
                    }
                }
            }
        }
        self.check_queue_drained(&job.queue_id);
        self.inner.wake.notify_one();
    }

    /// Run a configured command (no shell); `{path}`/`{url}`/`{id}` substituted.
    fn run_command(cmd: &[String], j: Option<&JobRow>) {
        let Some((prog, args)) = cmd.split_first() else { return };
        let path = j.and_then(|j| j.final_path.clone()).unwrap_or_default();
        let sub = |s: &str| {
            let mut s = s.replace("{path}", &path);
            if let Some(j) = j {
                s = s.replace("{url}", &j.url).replace("{id}", &j.id);
            }
            s
        };
        info!(cmd = %prog, "running post-action command");
        let _ = std::process::Command::new(sub(prog))
            .args(args.iter().map(|a| sub(a)))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }

    /// "队列完成后" action: the queue had work and now has nothing queued or running.
    fn check_queue_drained(&self, queue_id: &str) {
        if !self.inner.queue_busy.lock().unwrap().contains(queue_id) {
            return;
        }
        let Ok(jobs) = self.inner.store.list_jobs(None) else { return };
        let pending = jobs.iter().any(|j| j.queue_id == queue_id && (j.state == state::QUEUED || state::is_active(&j.state)));
        if pending || self.inner.running.lock().unwrap().values().any(|r| r.queue_id == queue_id) {
            return;
        }
        self.inner.queue_busy.lock().unwrap().remove(queue_id);
        let Some(q) = self.inner.store.list_queues().ok().and_then(|qs| qs.into_iter().find(|q| q.id == queue_id)) else { return };
        info!(queue = %queue_id, action = %q.extra.on_done, "queue drained");
        self.emit(types::NOTIFY, json!({"title": "队列已完成", "body": q.name, "queueId": q.id}));
        match q.extra.on_done.as_str() {
            "shutdown" => Self::run_command(&self.config().general.shutdown_command, None),
            "command" => Self::run_command(&q.extra.on_done_command, None),
            _ => {}
        }
    }

    fn after_complete(&self, j: &JobRow) {
        let cfg = self.config();
        let path = j.final_path.clone().unwrap_or_default();
        if cfg.general.notify_on_complete && std::env::var_os("LDM_NO_NOTIFY").is_none() {
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "LDM", "下载完成", j.filename.as_deref().unwrap_or(&path)])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        Self::run_command(&cfg.general.on_complete_command, Some(j));
        match j.extra.after.as_str() {
            "shutdown" => Self::run_command(&cfg.general.shutdown_command, Some(j)),
            // without a GUI the daemon opens the file / folder itself
            "open" if self.gui_count() == 0 => Self::run_command(&["xdg-open".into(), path.clone()], Some(j)),
            "folder" if self.gui_count() == 0 => {
                let dir = std::path::Path::new(&path).parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
                Self::run_command(&["xdg-open".into(), dir], Some(j))
            }
            _ => {}
        }
    }

    fn purge(&self, job: &JobRow, delete_files: bool) {
        if let Some(d) = &job.job_state_path {
            let p = PathBuf::from(d);
            // only ever delete inside our own part directory
            if p.starts_with(self.config().paths.part_dir()) || p.file_name().map(|n| n == job.id.as_str()).unwrap_or(false) {
                let _ = std::fs::remove_dir_all(p);
            }
        }
        if delete_files {
            if let Some(f) = &job.final_path {
                let _ = std::fs::remove_file(f);
            }
        }
    }

    // ---------------------------------------------------------------- API

    pub fn add(&self, req: DownloadAdd) -> Result<AddResult> {
        let cfg = self.config();
        let url = req.url.trim().to_string();
        security::validate_url(&url, cfg.engine.block_link_local)?;
        let dir = req.save_dir.as_deref().map(|d| PathBuf::from(paths::expand(d.trim()))).unwrap_or_else(|| cfg.paths.download_dir());
        if let Some(why) = paths::sandbox_hidden(&dir) {
            return Err(LdmError::Other(why));
        }
        let source = req.source.clone().unwrap_or_else(|| "api".into());
        let from_browser = matches!(source.as_str(), "extension" | "ws");
        let key = strip_fragment(&url).to_string();

        // de-duplicate (active job for the same URL, or a recent browser capture)
        for j in self.inner.store.find_by_url(&url)? {
            if !state::is_terminal(&j.state) {
                return Ok(AddResult { job: Some(j), duplicate: true, ignored: None });
            }
        }
        if from_browser {
            let mut ra = self.inner.recent_adds.lock().unwrap();
            ra.retain(|_, (t, _)| t.elapsed() < Duration::from_secs(cfg.browser.dedupe_secs));
            if let Some((_, id)) = ra.get(&key) {
                if let Some(j) = self.inner.store.get_job(id)? {
                    return Ok(AddResult { job: Some(j), duplicate: true, ignored: None });
                }
            }
        }

        let detected = classify::classify(&url, req.mime.as_deref());
        let audio_url = match req.audio_url.as_deref().map(str::trim).filter(|a| !a.is_empty()) {
            Some(a) => {
                security::validate_url(a, cfg.engine.block_link_local)?;
                Some(a.to_string())
            }
            None => None,
        };
        let ytdlp_rule = ytdlp::site_rule(&cfg.ytdlp, &url);
        let kind = match req.kind.as_deref() {
            Some("ytdlp") => "ytdlp",
            _ if audio_url.is_some() => "av",
            // YouTube / `always` sites go straight to the user's yt-dlp (when it is usable)
            None | Some("auto") | Some("") if ytdlp_rule == ytdlp::SiteRule::Always && ytdlp::available_or_installable(&cfg.ytdlp) => {
                "ytdlp"
            }
            Some("http") => "http",
            Some("hls") => "hls",
            Some("dash") => "dash",
            _ => kind_label(detected),
        };
        let page_key = req.page_url.clone().or(req.referrer.clone()).unwrap_or_default();
        if detected == MediaKind::Segment && from_browser && req.kind.is_none() && audio_url.is_none() {
            let rm = self.inner.recent_manifests.lock().unwrap();
            if let Some(t) = rm.get(&page_key) {
                if t.elapsed() < Duration::from_secs(cfg.media.candidate_ttl_secs) {
                    return Ok(AddResult { job: None, duplicate: false, ignored: Some("segment of an already captured manifest".into()) });
                }
            }
        }
        if !matches!(kind, "http" | "ytdlp") && !page_key.is_empty() {
            self.inner.recent_manifests.lock().unwrap().insert(page_key.clone(), Instant::now());
        }

        let queue_id = req.queue.clone().unwrap_or_else(|| "main".into());
        if !self.inner.store.list_queues()?.iter().any(|q| q.id == queue_id) {
            return Err(LdmError::Other(format!("unknown queue '{queue_id}'")));
        }
        let id = new_id();
        // "下载文件信息" confirmation: only when a GUI can answer
        let awaiting = from_browser && !req.paused && cfg.general.confirm_browser_downloads && self.gui_count() > 0;
        // media candidates are named after the page's video title
        let title_name = req
            .title_hint
            .as_deref()
            .map(|t| filename::title_stem(&filename::clean_title(t, req.page_url.as_deref())))
            .filter(|s| !s.is_empty());
        let mut filename_hint = req.filename_hint.as_deref().map(filename::sanitize).filter(|s| !s.is_empty());
        if let Some(t) = &title_name {
            if kind == "ytdlp" {
                // yt-dlp's own title wins (the page title is the fallback); see run_ytdlp
            } else if kind != "http" {
                filename_hint = Some(filename::sanitize_linux(t));
            } else {
                // provisional display name; the final extension comes from the response
                let e = classify::url_ext(&url);
                let e = if e.is_empty() || e.len() > 5 { "mp4".to_string() } else { e };
                filename_hint = Some(filename::sanitize_linux(&format!("{t}.{e}")));
            }
        }
        let ext = filename_hint.as_deref().map(filename::extension_of).unwrap_or_else(|| classify::url_ext(&url));
        let mut headers: Vec<(String, String)> = req.headers_extra.iter().map(|h| (h.name.clone(), h.value.clone())).collect();
        headers.retain(|(n, _)| !n.is_empty() && !n.contains(['\r', '\n', ':']));
        let host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(|h| h.to_string())).unwrap_or_default();
        let now = now_ms();
        let job = JobRow {
            id: id.clone(),
            url: url.clone(),
            kind: kind.into(),
            filename: filename_hint,
            save_dir: req.save_dir.clone(),
            state: if req.paused || awaiting { state::PAUSED.into() } else { state::QUEUED.into() },
            total: req.bytes_total,
            referrer: req.referrer.clone(),
            page_url: req.page_url.clone(),
            cookies: req.cookies.clone(),
            user_agent: req.user_agent.clone(),
            headers,
            mime: req.mime.clone(),
            category: Some(if kind == "http" { cfg.paths.category_of(&ext) } else { Category::Video }.id().to_string()),
            max_connections: req.connections.unwrap_or_else(|| cfg.engine.connections_for_host(&host)).clamp(1, 32),
            sha256: req.sha256.clone(),
            preferred_height: req.preferred_height,
            source: Some(source),
            job_state_path: Some(cfg.paths.part_dir().join(&id).to_string_lossy().into_owned()),
            queue_id,
            created_at: now,
            updated_at: now,
            extra: JobExtra {
                description: req.description.clone().filter(|d| !d.is_empty()),
                mirrors: req.mirrors.iter().filter(|m| security::validate_url(m, cfg.engine.block_link_local).is_ok()).cloned().collect(),
                md5: req.md5.clone(),
                awaiting_confirm: awaiting,
                title_name: if matches!(kind, "http" | "ytdlp") { title_name } else { None },
                ad_reasons: req.ad_reasons.clone().unwrap_or_default(),
                audio_url,
                ytdlp: (kind == "ytdlp").then(|| YtdlpJob { url: url.clone(), ..Default::default() }),
                ..Default::default()
            },
            ..Default::default()
        };
        self.inner.store.upsert_job(&job)?;
        if awaiting {
            self.emit(types::CAPTURE_CONFIRM, public_job(&job));
        }
        if from_browser {
            self.inner.recent_adds.lock().unwrap().insert(key, (Instant::now(), id.clone()));
        }
        info!(job = %id, kind, url = %security::redact_url(&url), "job added");
        self.emit_job(&id);
        self.inner.wake.notify_one();
        Ok(AddResult { job: Some(job), duplicate: false, ignored: None })
    }

    pub fn list(&self, state_filter: Option<&str>) -> Result<Vec<Value>> {
        let running = self.inner.running.lock().unwrap();
        Ok(self
            .inner
            .store
            .list_jobs(state_filter)?
            .iter()
            .map(|j| {
                let mut v = public_job(j);
                if let Some(r) = running.get(&j.id) {
                    v["got"] = json!(r.progress.got());
                    v["total"] = json!(r.progress.total().or(j.total));
                    v["speedBps"] = json!(r.progress.speed.speed());
                    v["connections"] = json!(r.progress.connections.load(std::sync::atomic::Ordering::Relaxed));
                    if r.progress.is_live() {
                        v["live"] = json!(true);
                        v["total"] = Value::Null;
                        v["recordedSecs"] = json!(r.progress.recorded_secs());
                    }
                }
                v
            })
            .collect())
    }

    pub fn get(&self, id: &str) -> Result<Value> {
        let j = self.find(id)?;
        let mut v = public_job(&j);
        v["parts"] = serde_json::to_value(self.inner.store.load_parts(&j.id)?).unwrap_or_default();
        if let Some(r) = self.inner.running.lock().unwrap().get(&j.id) {
            v["got"] = json!(r.progress.got());
            v["speedBps"] = json!(r.progress.speed.speed());
            v["splits"] = json!(r.progress.splits());
            if r.progress.is_live() {
                v["live"] = json!(true);
                v["recordedSecs"] = json!(r.progress.recorded_secs());
            }
        }
        Ok(v)
    }

    /// Resolve a full id or a unique prefix.
    pub fn find(&self, id: &str) -> Result<JobRow> {
        if let Some(j) = self.inner.store.get_job(id)? {
            return Ok(j);
        }
        let m: Vec<JobRow> = self.inner.store.list_jobs(None)?.into_iter().filter(|j| j.id.starts_with(id)).collect();
        match m.len() {
            1 => Ok(m.into_iter().next().unwrap()),
            0 => Err(LdmError::Other(format!("no such job: {id}"))),
            _ => Err(LdmError::Other(format!("ambiguous job id prefix: {id}"))),
        }
    }

    fn signal(&self, id: &str, intent: Intent) -> bool {
        let running = self.inner.running.lock().unwrap();
        if let Some(r) = running.get(id) {
            *r.intent.lock().unwrap() = intent;
            r.cancel.cancel();
            true
        } else {
            false
        }
    }

    /// "停止录制": a live recording stops fetching and finalises what it has into a playable
    /// file (native HLS / MPD: remux; yt-dlp: SIGINT so it ends ffmpeg with `q` and merges).
    /// Hard cancel / remove keep their kill-all behaviour.
    pub fn stop_recording(&self, id: &str) -> Result<()> {
        let j = self.find(id)?;
        let running = self.inner.running.lock().unwrap();
        match running.get(&j.id) {
            Some(r) if r.progress.is_live() => {
                r.progress.request_stop_recording();
                info!(id = %j.id, "stop recording requested");
                Ok(())
            }
            _ => Err(LdmError::Other(format!("job {} is not recording a live stream", j.id))),
        }
    }

    pub fn pause(&self, id: &str) -> Result<()> {
        let j = self.find(id)?;
        if !self.signal(&j.id, Intent::Pause) && (j.state == state::QUEUED || state::is_active(&j.state)) {
            self.inner.store.set_state(&j.id, state::PAUSED, None)?;
            self.emit_job(&j.id);
        }
        Ok(())
    }

    pub fn resume(&self, id: &str) -> Result<()> {
        let mut j = self.find(id)?;
        if j.extra.awaiting_confirm || j.extra.cookie_refresh {
            j.extra.awaiting_confirm = false;
            j.extra.cookie_refresh = false;
            self.inner.store.set_extra(&j.id, &j.extra)?;
        }
        if matches!(j.state.as_str(), "Paused" | "Failed" | "Canceled") {
            self.inner.store.set_state(&j.id, state::QUEUED, None)?;
            self.emit_job(&j.id);
            self.inner.wake.notify_one();
        }
        Ok(())
    }

    /// `CookiesQueryResult` after a 401/403: store the browser's current cookies and retry once.
    pub fn cookies_refreshed(&self, id: &str, cookies: Option<&str>) -> Result<bool> {
        let mut j = self.find(id)?;
        let fresh = cookies.map(str::trim).filter(|c| !c.is_empty()).filter(|c| j.cookies.as_deref() != Some(*c));
        let Some(c) = fresh.filter(|_| j.state == state::FAILED) else {
            // nothing new to retry with: an eligible failure now gets its yt-dlp attempt
            if j.state == state::FAILED {
                if let Some(reason) = j.extra.fallback_pending.clone() {
                    let e = LdmError::Other(j.error.clone().unwrap_or_default());
                    return Ok(self.fallback_to_ytdlp(&j.id, &reason, &e));
                }
            }
            return Ok(false);
        };
        j.cookies = Some(c.to_string());
        j.updated_at = now_ms();
        self.inner.store.upsert_job(&j)?;
        // keep cookie_refresh=true so a second auth failure does not loop
        self.inner.store.set_state(&j.id, state::QUEUED, None)?;
        info!(job = %j.id, "retrying with refreshed browser cookies");
        self.emit_job(&j.id);
        self.inner.wake.notify_one();
        Ok(true)
    }

    pub fn pause_all(&self) -> Result<usize> {
        let mut n = 0;
        for j in self.inner.store.list_jobs(None)? {
            if j.state == state::QUEUED || state::is_active(&j.state) {
                self.pause(&j.id)?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn resume_all(&self) -> Result<usize> {
        let mut n = 0;
        for j in self.inner.store.list_jobs(Some(state::PAUSED))? {
            if j.extra.awaiting_confirm {
                continue;
            }
            self.resume(&j.id)?;
            n += 1;
        }
        Ok(n)
    }

    pub fn remove(&self, id: &str, delete_files: bool) -> Result<()> {
        let j = self.find(id)?;
        if !self.signal(&j.id, Intent::Remove { delete_files }) {
            self.purge(&j, delete_files);
            self.inner.store.delete_job(&j.id)?;
            self.emit(types::JOB_STATE_CHANGED, json!({"id": j.id, "state": "Removed"}));
        }
        Ok(())
    }

    /// `JobSet`: priority / queue / connections / filename (before start).
    pub fn set(&self, id: &str, patch: &Value) -> Result<Value> {
        let mut j = self.find(id)?;
        if let Some(p) = patch.get("priority").and_then(|v| v.as_i64()) {
            j.priority = p;
        }
        if let Some(q) = patch.get("queue").or(patch.get("queue_id")).and_then(|v| v.as_str()) {
            if !self.inner.store.list_queues()?.iter().any(|x| x.id == q) {
                return Err(LdmError::Other(format!("unknown queue '{q}'")));
            }
            j.queue_id = q.into();
        }
        if let Some(c) = patch.get("connections").or(patch.get("max_connections")).and_then(|v| v.as_u64()) {
            j.max_connections = (c as u32).clamp(1, 32);
        }
        let idle = !state::is_active(&j.state);
        if let Some(f) = patch.get("filename").and_then(|v| v.as_str()) {
            if j.got == 0 && idle && !f.trim().is_empty() {
                j.filename = Some(filename::sanitize(f));
                // an explicit rename beats the page-title name
                j.extra.title_name = None;
            }
        }
        if let Some(d) = patch.get("save_dir").and_then(|v| v.as_str()) {
            if j.got == 0 && idle {
                j.save_dir = Some(d.trim().to_string()).filter(|d| !d.is_empty());
            }
        }
        if let Some(u) = patch.get("url").and_then(|v| v.as_str()) {
            // "更改地址" (e.g. an expired link refreshed from the page) – the
            // resume state is kept, the server validators guard the content
            if idle && u != j.url {
                security::validate_url(u, self.config().engine.block_link_local)?;
                j.url = u.trim().to_string();
            }
        }
        if let Some(r) = patch.get("referrer").and_then(|v| v.as_str()) {
            j.referrer = Some(r.to_string()).filter(|r| !r.is_empty());
        }
        if let Some(c) = patch.get("category").and_then(|v| v.as_str()) {
            if crate::util::category::Category::from_id(c).is_some() {
                j.category = Some(c.into());
            }
        }
        if let Some(d) = patch.get("description").and_then(|v| v.as_str()) {
            j.extra.description = Some(d.to_string()).filter(|d| !d.is_empty());
        }
        if let Some(a) = patch.get("after").and_then(|v| v.as_str()) {
            if matches!(a, "" | "open" | "folder" | "shutdown" | "command") {
                j.extra.after = a.into();
            }
        }
        if let Some(m) = patch.get("mirrors").and_then(|v| v.as_array()) {
            let block = self.config().engine.block_link_local;
            j.extra.mirrors =
                m.iter().filter_map(|x| x.as_str()).filter(|x| security::validate_url(x, block).is_ok()).map(str::to_string).collect();
        }
        if let Some(l) = patch.get("speed_limit_bps").and_then(|v| v.as_u64()) {
            j.extra.speed_limit_bps = l;
            if let Some(r) = self.inner.running.lock().unwrap().get(&j.id) {
                r.job_limiter.set_rate(l);
            }
        }
        if let Some(true) = patch.get("stop_recording").and_then(|v| v.as_bool()) {
            if let Some(r) = self.inner.running.lock().unwrap().get(&j.id) {
                r.progress.request_stop_recording();
            }
        }
        if let Some(false) = patch.get("awaiting_confirm").and_then(|v| v.as_bool()) {
            j.extra.awaiting_confirm = false;
        }
        j.updated_at = now_ms();
        self.inner.store.upsert_job(&j)?;
        self.emit_job(&j.id);
        self.inner.wake.notify_one();
        Ok(public_job(&j))
    }

    pub fn queues(&self) -> Result<Vec<QueueRow>> {
        self.inner.store.list_queues()
    }

    pub fn set_queue(&self, q: QueueRow) -> Result<()> {
        if q.id.is_empty() {
            return Err(LdmError::Other("queue id required".into()));
        }
        self.inner.store.upsert_queue(&q)?;
        self.inner.wake.notify_one();
        Ok(())
    }

    /// "开始队列": enable the queue and resume its paused jobs.
    pub fn queue_start(&self, id: &str) -> Result<usize> {
        let mut q = self.queue(id)?;
        q.enabled = true;
        q.extra.manual = true;
        self.inner.store.upsert_queue(&q)?;
        let mut n = 0;
        for j in self.inner.store.list_jobs(Some(state::PAUSED))? {
            if j.queue_id == q.id && !j.extra.awaiting_confirm {
                self.resume(&j.id)?;
                n += 1;
            }
        }
        self.inner.wake.notify_one();
        Ok(n)
    }

    /// "停止队列": disable the queue; running jobs stop gently and wait in it.
    pub fn queue_stop(&self, id: &str) -> Result<()> {
        let mut q = self.queue(id)?;
        q.enabled = false;
        self.inner.store.upsert_queue(&q)?;
        self.inner.wake.notify_one();
        Ok(())
    }

    pub fn queue_delete(&self, id: &str) -> Result<usize> {
        if id == "main" {
            return Err(LdmError::Other("主队列不能删除".into()));
        }
        self.queue(id)?;
        let moved = self.inner.store.delete_queue(id)?;
        self.inner.wake.notify_one();
        Ok(moved)
    }

    fn queue(&self, id: &str) -> Result<QueueRow> {
        self.inner.store.list_queues()?.into_iter().find(|q| q.id == id).ok_or_else(|| LdmError::Other(format!("unknown queue '{id}'")))
    }

    /// "删除已完成": drop finished entries (files are kept).
    pub fn remove_completed(&self) -> Result<usize> {
        let mut n = 0;
        for j in self.inner.store.list_jobs(Some(state::COMPLETED))? {
            self.remove(&j.id, false)?;
            n += 1;
        }
        Ok(n)
    }

    pub fn config_set(&self, patch: &Value) -> Result<Arc<Config>> {
        let new = self.config().patched(patch)?;
        // build the client first so an invalid proxy / PAC setting is rejected, not saved
        let client = client_for(&new.engine, self.inner.browser_proxy.read().unwrap().as_deref())?;
        if let Some(p) = &self.inner.cfg_path {
            new.save_to(p)?;
        }
        self.inner.global_limiter.set_rate(new.engine.effective_speed_limit());
        *self.inner.client.write().unwrap() = client;
        let arc = Arc::new(new);
        *self.inner.cfg.write().unwrap() = arc.clone();
        self.bump_ruleset();
        self.inner.wake.notify_one();
        Ok(arc)
    }

    /// `ProxyInfo` from the extension: remember it and rebuild the client when the
    /// proxy setting is "browser" (跟随浏览器). PAC / auto-detect fall back to the system proxy.
    pub fn set_browser_proxy(&self, info: &Value) -> Result<Option<String>> {
        let p = match info.get("mode").and_then(|v| v.as_str()) {
            Some("direct") => Some("none".to_string()),
            Some("fixed") => info.get("url").and_then(|v| v.as_str()).filter(|u| url::Url::parse(u).is_ok()).map(str::to_string),
            _ => Some("system".to_string()),
        };
        *self.inner.browser_proxy.write().unwrap() = p.clone();
        let cfg = self.config();
        if cfg.engine.proxy == "browser" {
            *self.inner.client.write().unwrap() = client_for(&cfg.engine, p.as_deref())?;
            info!(proxy = %p.as_deref().map(crate::util::security::redact_url).unwrap_or_default(), "following the browser proxy");
        }
        Ok(p)
    }

    /// Proxy for one URL, for tools that take a single proxy (yt-dlp): resolves `pac` through
    /// the PAC script (`none` = DIRECT), otherwise the same as [`Self::effective_proxy`].
    pub fn proxy_for(&self, url: &str) -> String {
        let p = self.effective_proxy();
        if p != "pac" {
            return p;
        }
        let pac = self.inner.client.read().unwrap().pac().cloned();
        match pac {
            Some(pac) => pac.proxy_string_for(url),
            None => "none".into(),
        }
    }

    /// PAC status (source, loaded, refresh count, last error) when `proxy = "pac"`.
    pub fn pac_status(&self) -> Option<crate::pac::PacStatus> {
        self.inner.client.read().unwrap().pac().map(|p| p.status())
    }

    /// Effective proxy for diagnostics / the GUI.
    pub fn effective_proxy(&self) -> String {
        let cfg = self.config();
        if cfg.engine.proxy == "browser" {
            self.inner.browser_proxy.read().unwrap().clone().unwrap_or_else(|| "system".into())
        } else {
            cfg.engine.proxy.clone()
        }
    }

    /// New rule-set version + push to every connected extension.
    fn bump_ruleset(&self) {
        let v = self.inner.store.get_setting("ruleset_version").ok().flatten().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1) + 1;
        let _ = self.inner.store.set_setting("ruleset_version", &v.to_string());
        self.emit(types::RULESET_PUSH, serde_json::to_value(self.ruleset()).unwrap_or_default());
    }

    /// Cheap change detector for the rules directories (names, sizes, mtimes) (hot reload).
    fn rules_fingerprint(&self) -> Vec<(PathBuf, u64, Option<std::time::SystemTime>)> {
        let mut out = vec![];
        for d in &self.inner.rules_dirs {
            let Ok(rd) = std::fs::read_dir(d) else { continue };
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().map(|e| e == "json").unwrap_or(false) {
                    let m = e.metadata().ok();
                    out.push((p, m.as_ref().map(|m| m.len()).unwrap_or(0), m.and_then(|m| m.modified().ok())));
                }
            }
        }
        out.sort();
        out
    }

    /// Rule set pushed to extensions.
    pub fn ruleset(&self) -> RuleSetPush {
        let cfg = self.config();
        let version = self.inner.store.get_setting("ruleset_version").ok().flatten().and_then(|s| s.parse().ok()).unwrap_or(1);
        let mut adapters = vec![];
        for d in &self.inner.rules_dirs {
            let Ok(rd) = std::fs::read_dir(d) else { continue };
            let mut files: Vec<PathBuf> =
                rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().map(|e| e == "json").unwrap_or(false)).collect();
            files.sort();
            for f in files {
                let list = match std::fs::read_to_string(&f).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok()) {
                    Some(Value::Array(a)) => a,
                    Some(v @ Value::Object(_)) => vec![v],
                    _ => {
                        warn!(file = %f.display(), "ignoring invalid rules file");
                        continue;
                    }
                };
                for a in list {
                    match validate_adapter(&a) {
                        Ok(()) => adapters.push(a),
                        Err(why) => warn!(file = %f.display(), why, "ignoring site adapter"),
                    }
                }
            }
        }
        RuleSetPush {
            version,
            capture_exts: cfg.capture.capture_exts.clone(),
            media_exts: cfg.capture.media_exts.clone(),
            exclude_hosts: cfg.capture.exclude_hosts.clone(),
            exclude_url_patterns: cfg.capture.exclude_url_patterns.clone(),
            min_size: cfg.capture.min_size,
            site_adapters: adapters,
            capture_enabled: cfg.capture.enabled,
            bypass_modifier: cfg.capture.bypass_modifier.clone(),
            force_modifier: cfg.capture.force_modifier.clone(),
            ad_ranking: cfg.media.ad_ranking,
            ad_domains: self.user_ad_domains().to_vec(),
            ytdlp: ytdlp::available_or_installable(&cfg.ytdlp),
            ytdlp_sites: ytdlp::always_domains(&cfg.ytdlp),
            ytdlp_off: ytdlp::off_domains(&cfg.ytdlp),
            ytdlp_auto_fallback: cfg.ytdlp.auto_fallback,
        }
    }

    /// Domains from `media.ad_domains_file` (re-read when the path or the file's mtime changes).
    pub fn user_ad_domains(&self) -> Arc<Vec<String>> {
        let path = self.config().media.ad_domains_file.clone();
        let mtime = (!path.trim().is_empty()).then(|| std::fs::metadata(paths::expand(&path)).and_then(|m| m.modified()).ok()).flatten();
        let mut c = self.inner.ad_domains.lock().unwrap();
        if let Some((p, t, v)) = c.as_ref() {
            if *p == path && *t == mtime {
                return v.clone();
            }
        }
        let v = Arc::new(adscore::load_domain_file(&path));
        *c = Some((path, mtime, v.clone()));
        v
    }

    /// Verdict for one candidate URL: the extension's (all signals) when it sent one, else the
    /// static fallback scored against the other candidates of the same page.
    fn ad_verdict(
        &self,
        mc: &MediaCandidate,
        url: &str,
        kind: &str,
        size: Option<u64>,
        from_extension: bool,
    ) -> Option<adscore::AdVerdict> {
        if !self.config().media.ad_ranking {
            return None;
        }
        if from_extension {
            if let Some(score) = mc.ad_score {
                return Some(adscore::AdVerdict { score, ad: mc.ad_suspect, reasons: mc.ad_reasons.clone() });
            }
        }
        let others = self
            .inner
            .store
            .candidates(Some(mc.tab_id), now_ms() - self.config().media.candidate_ttl_secs as i64 * 1000)
            .unwrap_or_default();
        let mut items: Vec<adscore::StaticCandidate> = others
            .iter()
            .filter(|c| c.page_url == mc.page_url && c.url != strip_fragment(url))
            .map(|c| adscore::StaticCandidate { url: &c.url, kind: &c.kind, size: None })
            .collect();
        items.insert(0, adscore::StaticCandidate { url, kind, size });
        adscore::score_static(&items, &self.user_ad_domains()).into_iter().next()
    }

    /// Record a media candidate; returns the ShowMediaButton payload (if any).
    pub fn media_candidate(&self, mc: &MediaCandidate) -> Result<Option<Value>> {
        let now = now_ms();
        let mut items: Vec<Value> = vec![];
        let mut push = |url: &str, kind: &str, mime: Option<&String>, extra: Value, size: Option<u64>, main: bool| -> Result<()> {
            if url.is_empty() || security::validate_url(url, true).is_err() {
                return Ok(());
            }
            let k = if kind.is_empty() { classify::classify(url, mime.map(|s| s.as_str())).as_str().to_string() } else { kind.to_string() };
            let ad = self.ad_verdict(mc, url, &k, size, main);
            self.inner.store.upsert_candidate(&CandidateRow {
                tab_id: mc.tab_id,
                url: strip_fragment(url).into(),
                kind: k.clone(),
                mime: mime.cloned(),
                page_url: mc.page_url.clone(),
                last_seen: now,
                ad: ad.clone(),
            })?;
            if matches!(k.as_str(), "hls" | "dash") {
                if let Some(p) = &mc.page_url {
                    self.inner.recent_manifests.lock().unwrap().insert(p.clone(), Instant::now());
                }
            }
            // same flat shape as the extension's own items (the page button greys on a boolean `ad`)
            let mut v = json!({
                "url": url, "kind": k, "mime": mime,
                "ad": ad.as_ref().is_some_and(|a| a.ad),
                "adScore": ad.as_ref().map(|a| a.score),
                "adReasons": ad.as_ref().map(|a| a.reasons.clone()).unwrap_or_default(),
            });
            if let (Value::Object(a), Value::Object(b)) = (&mut v, extra) {
                a.extend(b);
            }
            items.push(v);
            Ok(())
        };
        push(
            &mc.url,
            &mc.kind,
            mc.mime.as_ref(),
            json!({"contentLength": mc.content_length, "filenameHint": mc.filename_hint}),
            mc.content_length,
            true,
        )?;
        for m in &mc.media {
            push(&m.url, &m.kind, m.mime.as_ref(), json!({"width": m.width, "height": m.height, "bandwidth": m.bandwidth}), None, false)?;
        }
        if items.is_empty() {
            return Ok(None);
        }
        let all =
            rank_candidates(self.inner.store.candidates(Some(mc.tab_id), now - self.config().media.candidate_ttl_secs as i64 * 1000)?);
        let payload = json!({
            "tabId": mc.tab_id, "frameId": mc.frame_id, "elementId": mc.element_id, "pageUrl": mc.page_url,
            "items": items, "candidates": all,
        });
        self.emit(types::SHOW_MEDIA_BUTTON, payload.clone());
        Ok(Some(payload))
    }

    /// Candidate list (GUI / CLI / IPC): grouped by page (most recent page first), within a page
    /// the likely main video first and suspected ads last. Nothing is dropped.
    pub fn candidates(&self, tab_id: Option<i64>) -> Result<Vec<CandidateRow>> {
        let rows = self.inner.store.candidates(tab_id, now_ms() - self.config().media.candidate_ttl_secs.max(1) as i64 * 1000 * 10)?;
        Ok(if self.config().media.ad_ranking { rank_candidates(rows) } else { rows })
    }

    pub async fn probe(&self, url: &str, ctx: &RequestCtx) -> Result<MediaPlan> {
        let cfg = self.config();
        security::validate_url(url, cfg.engine.block_link_local)?;
        if ytdlp::site_rule(&cfg.ytdlp, url) == ytdlp::SiteRule::Always && classify::classify(url, None) == MediaKind::File {
            return self.probe_ytdlp(url).await;
        }
        let client = self.inner.client.read().unwrap().clone();
        {
            let audio = cfg.media.audio_pref();
            probe_media(&client, url, ctx, cfg.media.hls_max_height, &audio).await
        }
    }

    // ------------------------------------------------------------ yt-dlp

    /// yt-dlp status for `ldm doctor` / the Options tab (runs `yt-dlp --version`).
    pub async fn ytdlp_status(&self) -> ytdlp::Status {
        let c = self.config().ytdlp.clone();
        tokio::task::spawn_blocking(move || ytdlp::status_blocking(&c)).await.unwrap_or_default()
    }

    /// yt-dlp for a job / probe: `ytdlp.path`, managed copy, `$PATH`. When none is found the
    /// managed copy is installed per `ytdlp.auto_install` (`always`), or the job fails with
    /// `E_TOOL_MISSING` and the GUI / CLI ask for consent (`ask`; the job is re-queued after
    /// the install), or the user gets the install hint (`never`).
    async fn ytdlp_found(&self) -> Result<Found> {
        let cfg = self.config();
        if !cfg.ytdlp.enabled {
            return Err(LdmError::Ytdlp("已在设置中关闭（ytdlp.enabled = false）".into()));
        }
        if let Some(f) = ytdlp::find(&cfg.ytdlp) {
            return Ok(f);
        }
        if !cfg.ytdlp.path.trim().is_empty() {
            return Err(LdmError::Ytdlp(format!("ytdlp.path 指向的 yt-dlp 不存在或不可执行：{}", cfg.ytdlp.path)));
        }
        match AutoInstall::parse(&cfg.ytdlp.auto_install) {
            AutoInstall::Always => {
                info!("yt-dlp not found: installing the managed copy (ytdlp.auto_install = always)");
                self.tools_action("install", Some(Tool::YtDlp), None).await?;
                ytdlp::find(&self.config().ytdlp).ok_or_else(|| LdmError::Ytdlp("安装后仍未找到 yt-dlp".into()))
            }
            AutoInstall::Ask => {
                self.emit(types::TOOLS_NEEDED, json!({"tool": "yt-dlp", "reason": "missing", "toolsDir": self.tools_layout().root}));
                self.emit(types::NOTIFY, json!({"title": "需要安装 yt-dlp", "body": TOOL_MISSING_MSG}));
                Err(LdmError::ToolMissing(TOOL_MISSING_MSG.into()))
            }
            AutoInstall::Never => Err(LdmError::Ytdlp(format!("未找到 yt-dlp：{}", ytdlp::install_hint()))),
        }
    }

    /// Version of a yt-dlp / deno binary (managed: from its directory; others: `--version`, cached).
    async fn tool_version(&self, t: Tool, f: &Found) -> Option<String> {
        if f.source == Source::Managed {
            return f.path.parent().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned());
        }
        let mtime = std::fs::metadata(&f.path).and_then(|m| m.modified()).ok();
        if let Some((m, v)) = self.inner.tool_versions.lock().unwrap().get(&f.path) {
            if *m == mtime {
                return v.clone();
            }
        }
        let v = tools::run_version(t, &f.path).await.ok();
        self.inner.tool_versions.lock().unwrap().insert(f.path.clone(), (mtime, v.clone()));
        v
    }

    /// deno for yt-dlp (YouTube's JavaScript challenges). With `auto_install = always` a
    /// missing deno is installed for YouTube URLs; otherwise yt-dlp runs without one.
    async fn js_runtime(&self, ytdlp_version: Option<&str>, url: &str) -> ytdlp::JsRuntime {
        let cfg = self.config();
        let mut deno = ytdlp::find_deno(&cfg.ytdlp);
        let youtube = url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
            .is_some_and(|h| h == "youtu.be" || h == "youtube.com" || h.ends_with(".youtube.com"));
        if deno.is_none()
            && youtube
            && cfg.ytdlp.deno_path.trim().is_empty()
            && AutoInstall::parse(&cfg.ytdlp.auto_install) == AutoInstall::Always
            && self.tools_action("install", Some(Tool::Deno), None).await.is_ok()
        {
            deno = ytdlp::find_deno(&self.config().ytdlp);
        }
        ytdlp::JsRuntime { deno: deno.map(|d| d.path), flag: ytdlp::supports_js_runtimes(ytdlp_version) }
    }

    async fn ytdlp_info(&self, url: &str) -> Result<ytdlp::YtInfo> {
        if let Some(i) = self.inner.ytdlp_info.get(url) {
            return Ok(i);
        }
        let f = self.ytdlp_found().await?;
        let v = self.tool_version(Tool::YtDlp, &f).await;
        let js = self.js_runtime(v.as_deref(), url).await;
        self.ytdlp_info_with(url, &f.path, &js).await
    }

    async fn ytdlp_info_with(&self, url: &str, bin: &std::path::Path, js: &ytdlp::JsRuntime) -> Result<ytdlp::YtInfo> {
        if let Some(i) = self.inner.ytdlp_info.get(url) {
            return Ok(i);
        }
        let info = ytdlp::fetch_info(bin, js, &self.config(), &self.proxy_for(url), url, false).await?;
        self.inner.ytdlp_info.put(url, &info);
        Ok(info)
    }

    /// Quality menu for a yt-dlp page (`yt-dlp -J --no-playlist`, cached for two minutes).
    pub async fn probe_ytdlp(&self, url: &str) -> Result<MediaPlan> {
        let cfg = self.config();
        security::validate_url(url, cfg.engine.block_link_local)?;
        let info = self.ytdlp_info(url).await?;
        Ok(MediaPlan {
            source: "ytdlp".into(),
            duration: info.duration,
            qualities: ytdlp::qualities(&info),
            live: info.is_live,
            ..Default::default()
        })
    }

    /// Enqueue each item of a playlist URL as its own yt-dlp job (`--flat-playlist`).
    pub async fn add_playlist(&self, req: DownloadAdd) -> Result<Vec<JobRow>> {
        security::validate_url(req.url.trim(), self.config().engine.block_link_local)?;
        let f = self.ytdlp_found().await?;
        let v = self.tool_version(Tool::YtDlp, &f).await;
        let js = self.js_runtime(v.as_deref(), req.url.trim()).await;
        let info = ytdlp::fetch_info(&f.path, &js, &self.config(), &self.proxy_for(req.url.trim()), req.url.trim(), true).await?;
        if !info.playlist || info.entries.is_empty() {
            let r = self.add(DownloadAdd { kind: Some("ytdlp".into()), playlist: false, ..req })?;
            return Ok(r.job.into_iter().collect());
        }
        let mut out = vec![];
        for u in &info.entries {
            let r =
                self.add(DownloadAdd { url: u.clone(), kind: Some("ytdlp".into()), playlist: false, title_hint: None, ..req.clone() })?;
            out.extend(r.job);
        }
        info!(items = out.len(), "yt-dlp playlist enqueued");
        Ok(out)
    }

    /// yt-dlp job. A managed yt-dlp that fails like an outdated extractor is updated once and
    /// the job retried once (independent of the LDM → yt-dlp fallback, see `looks_outdated`).
    async fn run_ytdlp(
        &self,
        env: &JobEnv,
        job: &JobRow,
        work: &std::path::Path,
        output: &OutputSpec,
    ) -> Result<(PathBuf, u64, Option<String>, Option<String>)> {
        let mut found = self.ytdlp_found().await?;
        let url = job.extra.ytdlp.as_ref().map(|y| y.url.clone()).unwrap_or_else(|| job.url.clone());
        loop {
            let version = self.tool_version(Tool::YtDlp, &found).await;
            let js = self.js_runtime(version.as_deref(), &url).await;
            self.note_ytdlp_bin(&job.id, &found, version.as_deref(), js.deno.as_deref());
            let mut in_use = vec![found.path.clone()];
            in_use.extend(js.deno.clone());
            self.inner.tools_in_use.lock().unwrap().insert(job.id.clone(), in_use);
            let r = self.run_ytdlp_once(env, job, work, output, &found.path, &js).await;
            self.inner.tools_in_use.lock().unwrap().remove(&job.id);
            match r {
                Err(e) if self.outdated_retry_ok(&job.id, &found, &e, version.as_deref()) => match self.update_for_retry(&job.id, &e).await
                {
                    Some(f) => {
                        env.progress.set_phase("Retrying");
                        found = f;
                    }
                    None => return Err(e),
                },
                r => return r,
            }
        }
    }

    async fn run_ytdlp_once(
        &self,
        env: &JobEnv,
        job: &JobRow,
        work: &std::path::Path,
        output: &OutputSpec,
        bin: &std::path::Path,
        js: &ytdlp::JsRuntime,
    ) -> Result<(PathBuf, u64, Option<String>, Option<String>)> {
        let url = job.extra.ytdlp.as_ref().map(|y| y.url.clone()).unwrap_or_else(|| job.url.clone());

        // name: one already chosen for this job (resume / rename / fallback), else yt-dlp's title.
        // `-J` (cached) also tells whether this is a live stream to record.
        let mut live = false;
        let stem = match job.filename.as_deref() {
            Some(f) => {
                if let Ok(i) = self.ytdlp_info_with(&url, bin, js).await {
                    live = i.is_live;
                }
                let (st, ext) = filename::split_ext(f);
                if matches!(ext.to_ascii_lowercase().as_str(), "mp4" | "mkv" | "m4a" | "webm" | "mp3" | "opus" | "ts") {
                    st.to_string()
                } else {
                    f.to_string()
                }
            }
            None => {
                env.progress.set_phase("Probing");
                let title = match self.ytdlp_info_with(&url, bin, js).await {
                    Ok(i) => {
                        live = i.is_live;
                        i.title
                    }
                    Err(e) => return Err(e),
                };
                title
                    .map(|t| filename::title_stem(&filename::clean_title(&t, Some(&url))))
                    .filter(|s| !s.is_empty())
                    .or_else(|| job.extra.title_name.clone())
                    .unwrap_or_else(|| "video".into())
            }
        };
        if live && !env.cfg.media.live_record {
            return Err(LdmError::Format("live stream: recording is disabled (media.live_record = false)".into()));
        }
        let container = if env.cfg.media.output_container.eq_ignore_ascii_case("mkv") { "mkv" } else { "mp4" };
        let ext = if job.preferred_height == Some(0) { "m4a" } else { container };
        let name = filename::sanitize_linux(&format!("{stem}.{ext}"));
        let mut target = crate::engine::resolve_output(&env.cfg, output, &name);
        if job.filename.is_some() {
            // resume: keep the name picked on the first run
            target = target.with_file_name(&name);
        }
        let fname = target.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or(name);
        env.progress.set_filename(&fname);
        let yt_proxy = self.proxy_for(&url);
        let spec = ytdlp::YtJobSpec {
            url,
            dir: target.parent().map(|p| p.to_path_buf()).unwrap_or_default(),
            stem: filename::split_ext(&fname).0.to_string(),
            work_dir: work.join("ytdlp"),
            height: job.preferred_height,
            rate_limit_bps: env.effective_limit(),
            proxy: yt_proxy,
            live,
            js: js.clone(),
        };
        let (path, size) = ytdlp::run_download(env, bin, &spec).await?;
        let _ = std::fs::remove_dir_all(&spec.work_dir);
        let mime = match filename::extension_of(&path.to_string_lossy()).as_str() {
            "mkv" => "video/x-matroska",
            "webm" => "video/webm",
            "m4a" => "audio/mp4",
            _ => "video/mp4",
        };
        Ok((path, size, None, Some(mime.into())))
    }

    /// Fallback eligibility for a failed job (see `ytdlp::fallback_reason`) plus the settings:
    /// enabled + auto_fallback + binary present, site rule not `off`, a media job, once per job.
    fn fallback_reason(&self, job: &JobRow, e: &LdmError) -> Option<String> {
        let cfg = self.config();
        if job.kind == "ytdlp" || job.extra.ytdlp.is_some() || !cfg.ytdlp.auto_fallback {
            return None;
        }
        let media = job.kind != "http"
            || job.extra.title_name.is_some()
            || job.category.as_deref() == Some("video")
            || job.mime.as_deref().map(|m| m.starts_with("video/") || m.starts_with("audio/")).unwrap_or(false);
        if !media {
            return None;
        }
        let reason = ytdlp::fallback_reason(e)?;
        let page = job.page_url.as_deref().unwrap_or(&job.url);
        if ytdlp::site_rule(&cfg.ytdlp, page) == ytdlp::SiteRule::Off || ytdlp::site_rule(&cfg.ytdlp, &job.url) == ytdlp::SiteRule::Off {
            return None;
        }
        if !ytdlp::available_or_installable(&cfg.ytdlp) {
            return None;
        }
        Some(reason)
    }

    /// Turn the failed job into a yt-dlp job (page URL when known) and re-queue it. One shot:
    /// `extra.ytdlp` stays set, so a failing yt-dlp run is final.
    fn fallback_to_ytdlp(&self, id: &str, reason: &str, e: &LdmError) -> bool {
        let Ok(Some(mut j)) = self.inner.store.get_job(id) else { return false };
        if j.extra.ytdlp.is_some() {
            return false;
        }
        let from = j.kind.clone();
        let target = j.page_url.clone().unwrap_or_else(|| j.url.clone());
        j.extra.ytdlp = Some(YtdlpJob {
            url: target.clone(),
            fallback_from: Some(from.clone()),
            reason: Some(reason.to_string()),
            at: Some(now_ms()),
            ..Default::default()
        });
        j.extra.fallback_pending = None;
        j.extra.history.push(format!(
            "{} LDM ({from}) failed: {} → retrying once via yt-dlp ({reason}) for {}",
            now_ms(),
            security::redact_url(&e.to_string()),
            security::redact_url(&target)
        ));
        j.kind = "ytdlp".into();
        j.state = state::QUEUED.into();
        j.error = None;
        j.error_code = None;
        j.got = 0;
        j.total = None;
        j.updated_at = now_ms();
        if self.inner.store.upsert_job(&j).is_err() {
            return false;
        }
        let _ = self.inner.store.save_parts(id, &[]);
        info!(job = %id, from, reason, "falling back to yt-dlp");
        self.emit_job(id);
        self.emit(
            types::NOTIFY,
            json!({"title": "改用 yt-dlp 重试", "body": format!("{reason}: {}", security::redact_url(&target)), "jobId": id}),
        );
        true
    }

    pub fn status(&self) -> Result<Value> {
        let jobs = self.inner.store.list_jobs(None)?;
        let count = |s: &str| jobs.iter().filter(|j| j.state == s).count();
        let running = self.inner.running.lock().unwrap();
        let speed: f64 = running.values().map(|r| r.progress.speed.speed()).sum();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "pid": std::process::id(),
            "uptimeSecs": self.inner.started.elapsed().as_secs(),
            "active": running.len(),
            "queued": count(state::QUEUED),
            "paused": count(state::PAUSED),
            "completed": count(state::COMPLETED),
            "failed": count(state::FAILED),
            "total": jobs.len(),
            "speedBps": speed,
            "globalMaxActive": self.config().queue.global_max_active,
            "socket": paths::socket_path(),
            "db": paths::db_path(),
        }))
    }

    /// Export the job list (`json` or `csv`).
    pub fn export(&self, format: &str) -> Result<String> {
        let jobs = self.inner.store.list_jobs(None)?;
        if format == "csv" {
            let esc = |s: &str| if s.contains([',', '"', '\n']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
            let mut out = String::from("id,url,state,kind,filename,final_path,total,got,referrer,created_at\n");
            for j in jobs {
                out.push_str(&format!(
                    "{},{},{},{},{},{},{},{},{},{}\n",
                    j.id,
                    esc(&j.url),
                    j.state,
                    j.kind,
                    esc(j.filename.as_deref().unwrap_or("")),
                    esc(j.final_path.as_deref().unwrap_or("")),
                    j.total.map(|t| t.to_string()).unwrap_or_default(),
                    j.got,
                    esc(j.referrer.as_deref().unwrap_or("")),
                    j.created_at
                ));
            }
            Ok(out)
        } else {
            let v: Vec<Value> = jobs.iter().map(public_job).collect();
            Ok(serde_json::to_string_pretty(&json!({"format": "ldm-export", "version": 1, "jobs": v}))?)
        }
    }

    /// Import a previous export, a JSON array of DownloadAdd, a CSV export, or
    /// a plain URL list. Returns the number of jobs added.
    pub fn import(&self, data: &str, paused: bool) -> Result<usize> {
        let mut reqs: Vec<DownloadAdd> = vec![];
        let t = data.trim_start();
        if t.starts_with('{') || t.starts_with('[') {
            let v: Value = serde_json::from_str(t)?;
            let arr = v.get("jobs").cloned().unwrap_or(v);
            for it in arr.as_array().cloned().unwrap_or_default() {
                let url = it.get("url").and_then(|u| u.as_str()).unwrap_or("").to_string();
                if url.is_empty() {
                    continue;
                }
                reqs.push(DownloadAdd {
                    url,
                    referrer: it.get("referrer").and_then(|v| v.as_str()).map(String::from),
                    page_url: it.get("page_url").and_then(|v| v.as_str()).map(String::from),
                    filename_hint: it.get("filename").or(it.get("filename_hint")).and_then(|v| v.as_str()).map(String::from),
                    kind: it.get("kind").and_then(|v| v.as_str()).map(String::from),
                    ..Default::default()
                });
            }
        } else if t.starts_with("id,url,") {
            for line in t.lines().skip(1) {
                if let Some(url) = line.split(',').nth(1) {
                    reqs.push(DownloadAdd { url: url.trim_matches('"').into(), ..Default::default() });
                }
            }
        } else {
            for line in t.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
                reqs.push(DownloadAdd { url: line.into(), ..Default::default() });
            }
        }
        let mut n = 0;
        for mut r in reqs {
            r.paused = paused;
            r.source = Some("import".into());
            match self.add(r) {
                Ok(AddResult { job: Some(_), duplicate: false, .. }) => n += 1,
                Ok(_) => {}
                Err(e) => warn!(err = %e, "import: skipped entry"),
            }
        }
        Ok(n)
    }

    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutdown.is_cancelled()
    }

    pub fn shutdown_token(&self) -> CancellationToken {
        self.inner.shutdown.clone()
    }

    /// Stop scheduling, interrupt running jobs (they persist their state and
    /// return to Queued) and wait up to `timeout` for them to settle.
    pub async fn shutdown(&self, timeout: Duration) {
        self.inner.shutdown.cancel();
        {
            let running = self.inner.running.lock().unwrap();
            for r in running.values() {
                let mut i = r.intent.lock().unwrap();
                if *i == Intent::None {
                    *i = Intent::Shutdown;
                }
                r.cancel.cancel();
            }
        }
        let t0 = Instant::now();
        while !self.inner.running.lock().unwrap().is_empty() && t0.elapsed() < timeout {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    pub fn active_count(&self) -> usize {
        self.inner.running.lock().unwrap().len()
    }
}

/// Job view for clients: cookies are never echoed back.
/// Site-adapter schema check: `id`, `matchHosts` (non-empty
/// strings), optional `detect.urlPatterns` / `detect.elementSelectors` string lists,
/// optional `prefer.maxHeight` number. Unknown keys are allowed for forward compatibility.
pub fn validate_adapter(a: &Value) -> std::result::Result<(), String> {
    let strings = |v: Option<&Value>, name: &str, required: bool| -> std::result::Result<(), String> {
        match v {
            None if !required => Ok(()),
            Some(Value::Array(x)) if x.iter().all(|s| s.as_str().map(|s| !s.trim().is_empty() && s.len() < 512).unwrap_or(false)) => {
                if required && x.is_empty() {
                    Err(format!("{name} is empty"))
                } else {
                    Ok(())
                }
            }
            _ => Err(format!("{name} must be a list of strings")),
        }
    };
    let o = a.as_object().ok_or("adapter must be an object")?;
    match o.get("id").and_then(|v| v.as_str()) {
        Some(id) if !id.is_empty() && id.len() <= 64 => {}
        _ => return Err("missing id".into()),
    }
    strings(o.get("matchHosts"), "matchHosts", true)?;
    if let Some(d) = o.get("detect") {
        let d = d.as_object().ok_or("detect must be an object")?;
        strings(d.get("urlPatterns"), "detect.urlPatterns", false)?;
        strings(d.get("elementSelectors"), "detect.elementSelectors", false)?;
    }
    if let Some(p) = o.get("prefer") {
        if p.get("maxHeight").map(|h| !h.is_u64()).unwrap_or(false) {
            return Err("prefer.maxHeight must be a number".into());
        }
    }
    Ok(())
}

/// Put a saved site login (选项 → 站点登录) into an ftp:// URL that carries none.
fn ftp_with_login(u: &str, cfg: &Config) -> String {
    let Ok(mut url) = url::Url::parse(u) else { return u.to_string() };
    if url.username().is_empty() {
        if let Some(l) = url.host_str().and_then(|h| cfg.sites.login_for(h)).cloned() {
            let _ = url.set_username(&l.user);
            let _ = url.set_password(Some(&l.password));
            return url.to_string();
        }
    }
    u.to_string()
}

fn client_for(engine: &crate::config::EngineConfig, browser_proxy: Option<&str>) -> Result<HttpClient> {
    if engine.proxy == "browser" {
        let mut e = engine.clone();
        e.proxy = browser_proxy.unwrap_or("system").to_string();
        return HttpClient::new(&e);
    }
    HttpClient::new(engine)
}

/// Stable "main first, suspected ads last" order inside each (tab, page) group; groups keep
/// their first-appearance (most recent) order.
pub fn rank_candidates(rows: Vec<CandidateRow>) -> Vec<CandidateRow> {
    let mut groups: Vec<PageGroup> = Vec::new();
    for r in rows {
        let key = (r.tab_id, r.page_url.clone());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, g)) => g.push(r),
            None => groups.push((key, vec![r])),
        }
    }
    let rank = |r: &CandidateRow| r.ad.as_ref().map(|a| (a.ad, a.score)).unwrap_or((false, 0.0));
    groups
        .into_iter()
        .flat_map(|(_, mut g)| {
            g.sort_by(|a, b| {
                let (aa, sa) = rank(a);
                let (ba, sb) = rank(b);
                aa.cmp(&ba).then(sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal))
            });
            group_alternates(g)
        })
        .collect()
}

/// The main entry a candidate is another bitrate of (`alt-of:<url>` from the extension).
fn alt_of(r: &CandidateRow) -> Option<&str> {
    r.ad.as_ref()?.reasons.iter().find_map(|x| x.strip_prefix("alt-of:"))
}

/// Same video at several bitrates: each alternate goes right after its main entry (the
/// extension already picked the main one: what the player ended up using). Alternates whose main
/// entry is missing keep their place.
fn group_alternates(g: Vec<CandidateRow>) -> Vec<CandidateRow> {
    let has = |u: &str, g: &[CandidateRow]| g.iter().any(|r| r.url == u && alt_of(r).is_none());
    let (alts, mains): (Vec<_>, Vec<_>) = g.into_iter().partition(|r| alt_of(r).is_some());
    let (alts, mut orphans): (Vec<_>, Vec<_>) = alts.into_iter().partition(|r| has(alt_of(r).unwrap_or(""), &mains));
    let mut out = Vec::with_capacity(mains.len() + alts.len() + orphans.len());
    let mut alts = alts;
    for m in mains {
        let url = m.url.clone();
        out.push(m);
        let (mine, rest): (Vec<_>, Vec<_>) = alts.into_iter().partition(|r| alt_of(r) == Some(url.as_str()));
        out.extend(mine);
        alts = rest;
    }
    // ad-suspected orphans stay last, others before them
    let ad_at = out.iter().position(|r| r.ad.as_ref().is_some_and(|a| a.ad)).unwrap_or(out.len());
    let tail = out.split_off(ad_at);
    out.append(&mut orphans);
    out.extend(tail);
    out
}

pub fn public_job(j: &JobRow) -> Value {
    let mut v = serde_json::to_value(j).unwrap_or_default();
    if let Value::Object(m) = &mut v {
        m.remove("cookies");
        m.insert("hasCookies".into(), json!(j.cookies.is_some()));
        if j.kind == "ytdlp" {
            m.insert("via".into(), json!("yt-dlp"));
            if let Some(r) = j.extra.ytdlp.as_ref().and_then(|y| y.reason.as_ref()) {
                m.insert("fallbackReason".into(), json!(r));
            }
        }
    }
    v
}

/// Message of `E_TOOL_MISSING` (yt-dlp needed, `ytdlp.auto_install = ask`).
pub const TOOL_MISSING_MSG: &str =
    "需要 yt-dlp：LDM 可从 GitHub 官方发布页下载并校验（SHA-256）到数据目录。请在弹出的对话框中确认，或运行 `ldm tools install`；安装后任务会自动重新排队";

// ------------------------------------------------------------ managed tools (yt-dlp / deno)

impl Manager {
    pub fn tools_layout(&self) -> tools::Layout {
        tools::Layout::from_config(&self.config().ytdlp)
    }

    /// Installer using LDM's HTTP client (proxy / PAC settings apply).
    fn tools_installer(&self, t: Tool, action: &str) -> tools::Installer {
        let cfg = self.config();
        let client = self.inner.client.read().unwrap().clone();
        let mut ins = tools::Installer::new(client, &cfg.ytdlp);
        let me = self.clone();
        let action = action.to_string();
        ins.progress = Some(Arc::new(move |phase: &str, got: u64, total: Option<u64>| {
            me.emit(types::TOOLS_PROGRESS, json!({"tool": t.name(), "action": action, "phase": phase, "got": got, "total": total}));
        }));
        ins
    }

    fn tools_in_use(&self) -> Vec<PathBuf> {
        self.inner.tools_in_use.lock().unwrap().values().flatten().cloned().collect()
    }

    /// Versions / sources / update state of yt-dlp and deno (GUI "工具" page, `ldm tools status`).
    pub async fn tools_status(&self) -> Value {
        let cfg = self.config();
        let layout = tools::Layout::from_config(&cfg.ytdlp);
        let yt = tools::status(Tool::YtDlp, &cfg.ytdlp.path, &layout).await;
        let deno = tools::status(Tool::Deno, &cfg.ytdlp.deno_path, &layout).await;
        json!({
            "yt-dlp": yt,
            "deno": deno,
            "enabled": cfg.ytdlp.enabled,
            "autoInstall": cfg.ytdlp.auto_install,
            "autoUpdate": cfg.ytdlp.auto_update,
            "busy": self.inner.tools_busy.try_lock().is_err(),
            "inUse": self.tools_in_use().len(),
            "flatpak": ytdlp::in_flatpak(),
            "toolsDir": layout.root,
        })
    }

    /// `install | update | check | rollback | remove` for one tool, or (`tool = None`) for
    /// yt-dlp + deno (install) / every managed tool (the rest). Binaries used by running jobs
    /// are never removed; switching versions only moves the `current` link.
    pub async fn tools_action(&self, action: &str, tool: Option<Tool>, version: Option<&str>) -> Result<Value> {
        if !matches!(action, "install" | "update" | "check" | "rollback" | "remove") {
            return Err(LdmError::Other(format!("unknown tools action: {action}")));
        }
        if version.is_some() && (tool.is_none() || action != "install") {
            return Err(LdmError::Other("--version needs `install` and a single tool".into()));
        }
        let _busy = self.inner.tools_busy.lock().await;
        let layout = self.tools_layout();
        let list: Vec<Tool> = match tool {
            Some(t) => vec![t],
            None if action == "install" => vec![Tool::YtDlp, Tool::Deno],
            None => [Tool::YtDlp, Tool::Deno].into_iter().filter(|t| layout.current_version(*t).is_some()).collect(),
        };
        let mut out = serde_json::Map::new();
        let mut first_err = None;
        for t in list {
            let ins = self.tools_installer(t, action);
            let in_use = self.tools_in_use();
            info!(tool = %t, action, version = version.unwrap_or("latest"), "managed tools");
            let r: Result<Value> = match action {
                "install" => ins.install(t, version, &in_use).await.map(|r| json!(r)),
                "update" => ins.update(t, &in_use).await.map(|r| json!(r)),
                "check" => ins.check(t).await.map(|(c, l)| {
                    let newer = c.as_deref().is_none_or(|c| tools::version_newer(&l, c));
                    json!({"current": c, "latest": l, "updateAvailable": newer})
                }),
                "rollback" => ins.rollback(t).await.map(|(from, to)| json!({"from": from, "to": to})),
                _ => ins.remove(t, &in_use).await.map(|v| json!({"removed": v})),
            };
            match r {
                Ok(v) => {
                    out.insert(t.name().into(), v);
                }
                Err(e) => {
                    warn!(tool = %t, action, err = %e, "managed tools action failed");
                    out.insert(t.name().into(), json!({"error": e.to_string(), "code": e.code()}));
                    // deno is optional when installing both
                    if first_err.is_none() && !(tool.is_none() && t == Tool::Deno) {
                        first_err = Some(e);
                    }
                }
            }
        }
        self.inner.tool_versions.lock().unwrap().clear();
        let requeued = if matches!(action, "install" | "update" | "rollback") { self.requeue_tool_missing() } else { 0 };
        out.insert("requeued".into(), json!(requeued));
        self.emit(types::TOOLS_CHANGED, json!({"action": action, "result": out, "ok": first_err.is_none()}));
        match first_err {
            Some(e) => Err(e),
            None => Ok(Value::Object(out)),
        }
    }

    /// Re-queue jobs that failed with `E_TOOL_MISSING` once yt-dlp is available.
    fn requeue_tool_missing(&self) -> usize {
        if ytdlp::find(&self.config().ytdlp).is_none() {
            return 0;
        }
        let Ok(failed) = self.inner.store.list_jobs(Some(state::FAILED)) else { return 0 };
        let mut n = 0;
        for j in failed.iter().filter(|j| j.error_code.as_deref() == Some("E_TOOL_MISSING")) {
            if self.inner.store.set_state(&j.id, state::QUEUED, None).is_ok() {
                n += 1;
                self.emit_job(&j.id);
            }
        }
        if n > 0 {
            info!(jobs = n, "yt-dlp installed: re-queued waiting jobs");
            self.inner.wake.notify_one();
        }
        n
    }

    /// Record which yt-dlp (and deno) a job runs with (job details / `ldm info`).
    fn note_ytdlp_bin(&self, id: &str, f: &Found, version: Option<&str>, deno: Option<&std::path::Path>) {
        let Ok(Some(j)) = self.inner.store.get_job(id) else { return };
        let mut x = j.extra.clone();
        let y = x.ytdlp.get_or_insert_with(|| YtdlpJob { url: j.url.clone(), ..Default::default() });
        let path = f.path.to_string_lossy().into_owned();
        if y.bin_path.as_deref() == Some(path.as_str()) && y.bin_version.as_deref() == version {
            return;
        }
        y.bin_source = Some(f.source.as_str().into());
        y.bin_version = version.map(str::to_string);
        y.bin_path = Some(path.clone());
        x.history.push(format!(
            "{} yt-dlp {} ({}) {path}{}",
            now_ms(),
            version.unwrap_or("?"),
            f.source.label(),
            deno.map(|d| format!("; deno {}", d.display())).unwrap_or_default()
        ));
        let _ = self.inner.store.set_extra(id, &x);
        self.emit_job(id);
    }

    /// Outdated-extractor retry allowed: managed copy, auto-update on, not yet used by this job.
    fn outdated_retry_ok(&self, id: &str, f: &Found, e: &LdmError, version: Option<&str>) -> bool {
        let cfg = self.config();
        if f.source != Source::Managed || tools::update_interval(&cfg.ytdlp.auto_update).is_none() || matches!(e, LdmError::Canceled) {
            return false;
        }
        let Ok(Some(j)) = self.inner.store.get_job(id) else { return false };
        if j.extra.ytdlp.as_ref().is_some_and(|y| y.update_retried) {
            return false;
        }
        let age = version.and_then(|v| ytdlp::version_age_days(v, chrono::Local::now().date_naive()));
        ytdlp::looks_outdated(&e.to_string(), age)
    }

    /// Update the managed yt-dlp for one retry; `Some(new binary)` when a newer one is installed.
    async fn update_for_retry(&self, id: &str, e: &LdmError) -> Option<Found> {
        let history = |msg: String| {
            if let Ok(Some(j)) = self.inner.store.get_job(id) {
                let mut x = j.extra.clone();
                let y = x.ytdlp.get_or_insert_with(|| YtdlpJob { url: j.url.clone(), ..Default::default() });
                y.update_retried = true;
                x.history.push(format!("{} {msg}", now_ms()));
                let _ = self.inner.store.set_extra(id, &x);
            }
        };
        let err: String = security::redact_url(&e.to_string()).chars().take(160).collect();
        history(format!("yt-dlp 疑似过旧（{err}）→ 更新 yt-dlp 后重试一次"));
        info!(job = %id, "yt-dlp extractor looks outdated: updating the managed copy and retrying once");
        let r = self.tools_action("update", Some(Tool::YtDlp), None).await;
        match r {
            Ok(v) if v["yt-dlp"]["updated"].as_bool() == Some(true) => {
                let f = ytdlp::find(&self.config().ytdlp)?;
                history(format!(
                    "yt-dlp 已更新 {} → {}",
                    v["yt-dlp"]["from"].as_str().unwrap_or("?"),
                    v["yt-dlp"]["latest"].as_str().unwrap_or("?")
                ));
                self.emit(types::NOTIFY, json!({"title": "yt-dlp 已更新，正在重试", "body": v["yt-dlp"]["latest"], "jobId": id}));
                Some(f)
            }
            Ok(_) => {
                history("yt-dlp 已是最新版本，不再重试".into());
                None
            }
            Err(e) => {
                history(format!("yt-dlp 更新失败：{e}"));
                None
            }
        }
    }

    /// Periodic update of the managed tools (`ytdlp.auto_update`), skipped while yt-dlp jobs run.
    async fn tools_auto_update(&self) {
        let cfg = self.config();
        if !cfg.ytdlp.enabled || tools::update_interval(&cfg.ytdlp.auto_update).is_none() {
            return;
        }
        let layout = tools::Layout::from_config(&cfg.ytdlp);
        let state = layout.load_state();
        for t in [Tool::YtDlp, Tool::Deno] {
            if layout.current_version(t).is_none() || !tools::update_due(&cfg.ytdlp.auto_update, state.get(t).last_check_ms, now_ms()) {
                continue;
            }
            if !self.inner.tools_in_use.lock().unwrap().is_empty() || self.inner.tools_busy.try_lock().is_err() {
                return;
            }
            match self.tools_action("update", Some(t), None).await {
                Ok(v) => info!(tool = %t, updated = v[t.name()]["updated"].as_bool().unwrap_or(false), "managed tools: update check"),
                Err(e) => warn!(tool = %t, err = %e, "managed tools: update check failed"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn adapter_schema() {
        assert!(validate_adapter(
            &json!({"id":"a","matchHosts":["x.com"],"detect":{"urlPatterns":["**/p.m3u8"]},"prefer":{"maxHeight":720}})
        )
        .is_ok());
        assert!(validate_adapter(&json!({"matchHosts":["x.com"]})).is_err());
        assert!(validate_adapter(&json!({"id":"a","matchHosts":[]})).is_err());
        assert!(validate_adapter(&json!({"id":"a","matchHosts":["x"],"detect":{"elementSelectors":[1]}})).is_err());
        assert!(validate_adapter(&json!({"id":"a","matchHosts":["x"],"prefer":{"maxHeight":"hd"}})).is_err());
        assert!(validate_adapter(&json!([1])).is_err());
    }

    #[test]
    fn browser_proxy_is_followed_only_when_selected() {
        let mut cfg = Config::default();
        cfg.engine.proxy = "browser".into();
        let m = Manager::new(cfg, None, Store::open_memory().unwrap()).unwrap();
        assert_eq!(m.effective_proxy(), "system");
        m.set_browser_proxy(&json!({"mode":"fixed","url":"socks5://127.0.0.1:9050"})).unwrap();
        assert_eq!(m.effective_proxy(), "socks5://127.0.0.1:9050");
        m.set_browser_proxy(&json!({"mode":"direct"})).unwrap();
        assert_eq!(m.effective_proxy(), "none");
        m.set_browser_proxy(&json!({"mode":"pac"})).unwrap();
        assert_eq!(m.effective_proxy(), "system");
        let m2 = Manager::new(Config::default(), None, Store::open_memory().unwrap()).unwrap();
        m2.set_browser_proxy(&json!({"mode":"fixed","url":"http://p:1"})).unwrap();
        assert_eq!(m2.effective_proxy(), "system", "manual setting wins");
    }

    #[tokio::test]
    async fn rules_hot_reload_pushes_new_ruleset() {
        let dir = std::env::temp_dir().join(format!("ldm-rules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = Manager::new(Config::default(), None, Store::open_memory().unwrap()).unwrap();
        Arc::get_mut(&mut m.inner).unwrap().rules_dirs = vec![dir.clone()];
        let mut ev = m.subscribe();
        let h = m.start();
        tokio::time::sleep(Duration::from_millis(300)).await;
        let v0 = m.ruleset().version;
        std::fs::write(dir.join("site.json"), r#"[{"id":"demo","matchHosts":["video.example"]}]"#).unwrap();
        let push = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let e = ev.recv().await.unwrap();
                if e.kind == types::RULESET_PUSH {
                    break e.payload;
                }
            }
        })
        .await
        .expect("rule set pushed after the file changed");
        assert!(push["version"].as_u64().unwrap() > v0 as u64);
        assert_eq!(push["siteAdapters"][0]["id"], "demo", "{push}");
        m.shutdown_token().cancel();
        h.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn schedule_windows() {
        let at = |h, m| chrono::Local.with_ymd_and_hms(2026, 10, 19, h, m, 0).unwrap(); // Monday
        let w = json!({"start":"01:00","end":"07:00"});
        assert!(window_open(Some(&w), at(3, 0)));
        assert!(!window_open(Some(&w), at(8, 0)));
        let wrap = json!({"start":"23:00","end":"02:00"});
        assert!(window_open(Some(&wrap), at(23, 30)));
        assert!(window_open(Some(&wrap), at(1, 0)));
        assert!(!window_open(Some(&wrap), at(12, 0)));
        let days = json!({"days":[6,7]});
        assert!(!window_open(Some(&days), at(12, 0)));
        assert!(window_open(None, at(12, 0)));
    }

    #[test]
    fn rank_candidates_keeps_quality_alternates_under_their_main_entry() {
        let row = |url: &str, score: f64, ad: bool, reasons: &[&str]| CandidateRow {
            tab_id: 1,
            url: url.into(),
            kind: "progressive".into(),
            mime: None,
            page_url: Some("https://site.example/v".into()),
            last_seen: 0,
            ad: Some(crate::media::adscore::AdVerdict { score, ad, reasons: reasons.iter().map(|s| s.to_string()).collect() }),
        };
        let rows = vec![
            row("https://a.example/lo", -6.5, false, &["mse-main", "alt-of:https://a.example/hi"]),
            row("https://a.example/ad.mp4", 3.5, true, &["short:2s"]),
            row("https://a.example/other", -2.0, false, &[]),
            row("https://a.example/hi", -6.5, false, &["mse-main"]),
            row("https://a.example/orphan", -1.0, false, &["alt-of:https://gone.example/x"]),
        ];
        let urls: Vec<String> = rank_candidates(rows).into_iter().map(|r| r.url).collect();
        assert_eq!(
            urls,
            [
                "https://a.example/hi",
                "https://a.example/lo",
                "https://a.example/other",
                "https://a.example/orphan",
                "https://a.example/ad.mp4"
            ]
        );
    }
}
