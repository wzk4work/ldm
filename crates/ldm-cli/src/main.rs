//! ldm – command-line client plus standalone `get/hls/dash`.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use ldm_core::engine::{run_http_job, HttpJobSpec, JobEnv, OutputSpec};
use ldm_core::http::RequestCtx;
use ldm_core::media::{run_media_job, MediaJobSpec};
use ldm_core::{paths, Config};
use ldm_ipc::client::Client;
use ldm_ipc::messages::types;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "ldm", version, about = "LDM – Linux 下载管理器命令行客户端")]
struct Cli {
    /// Output JSON instead of tables
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(ClapArgs, Clone, Default)]
struct ReqOpts {
    /// Referer header
    #[arg(long, alias = "referer")]
    referrer: Option<String>,
    /// Cookie header value ("a=1; b=2")
    #[arg(long)]
    cookie: Option<String>,
    /// Netscape cookies.txt (or a file with a raw Cookie header)
    #[arg(long)]
    cookie_file: Option<PathBuf>,
    /// User-Agent
    #[arg(long)]
    user_agent: Option<String>,
    /// Extra header "Name: value" (repeatable)
    #[arg(long = "header", short = 'H')]
    headers: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Add a download to the daemon queue
    Add {
        url: Vec<String>,
        #[command(flatten)]
        req: ReqOpts,
        /// Connections per file (1-32)
        #[arg(long, short = 'c')]
        conns: Option<u32>,
        /// Target directory
        #[arg(long, short = 'd')]
        dir: Option<String>,
        /// File name
        #[arg(long, short = 'n')]
        name: Option<String>,
        #[arg(long, short = 'q')]
        queue: Option<String>,
        /// Force job kind: http | hls | dash | ytdlp
        #[arg(long)]
        kind: Option<String>,
        /// Download with the external yt-dlp (same as --kind ytdlp)
        #[arg(long)]
        ytdlp: bool,
        /// yt-dlp playlist URL: enqueue every item as its own job
        #[arg(long)]
        playlist: bool,
        /// Preferred video height for HLS/DASH
        #[arg(long)]
        height: Option<u32>,
        #[arg(long)]
        sha256: Option<String>,
        /// Add paused
        #[arg(long)]
        paused: bool,
        /// Wait until finished and show progress
        #[arg(long, short = 'w')]
        wait: bool,
    },
    /// List jobs
    List {
        #[arg(long, short = 's')]
        state: Option<String>,
    },
    /// Show one job
    Info { id: String },
    /// Pause a job (or --all)
    Pause {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Resume a job (or --all)
    Resume {
        id: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Finish a live recording gracefully (keeps a playable file of what was recorded)
    StopRecording { id: String },
    /// Remove a job
    Rm {
        id: String,
        /// Also delete the downloaded file
        #[arg(long)]
        delete_files: bool,
    },
    /// Change priority / queue / connections
    Set {
        id: String,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long)]
        queue: Option<String>,
        #[arg(long)]
        conns: Option<u32>,
    },
    /// Follow progress (all jobs, or one until it finishes)
    Watch { id: Option<String> },
    /// Probe an HLS/DASH URL and list qualities
    Probe {
        url: String,
        #[command(flatten)]
        req: ReqOpts,
    },
    /// Daemon control
    Daemon {
        #[command(subcommand)]
        action: DaemonCmd,
    },
    /// Queue management
    Queue {
        #[command(subcommand)]
        action: QueueCmd,
    },
    /// Configuration
    Config {
        #[command(subcommand)]
        action: ConfigCmd,
    },
    /// Export the job list
    Export {
        #[arg(long, default_value = "json")]
        format: String,
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// Import jobs (LDM export JSON/CSV or a URL list)
    Import {
        file: PathBuf,
        #[arg(long)]
        paused: bool,
    },
    /// Check the environment (ffmpeg, socket, manifests …)
    Doctor,
    /// LDM-managed yt-dlp / deno (official GitHub releases, SHA-256 verified, in the data dir)
    Tools {
        #[command(subcommand)]
        cmd: ToolsCmd,
    },
    /// Print daemon metrics in Prometheus text format (for node_exporter's textfile collector)
    Metrics,
    /// Write a redacted diagnostics bundle (config, job summary, log tail) as JSON
    Diag {
        #[arg(long, short = 'o')]
        output: Option<PathBuf>,
    },
    /// Standalone HTTP download without the daemon
    Get {
        url: String,
        /// Output file, or an existing directory (`-d DIR`, like `ldm add`)
        #[arg(long, short = 'o', visible_alias = "dir", visible_short_alias = 'd')]
        output: Option<PathBuf>,
        #[arg(long, short = 'c', default_value_t = 8)]
        conns: u32,
        #[command(flatten)]
        req: ReqOpts,
        #[arg(long)]
        sha256: Option<String>,
        /// Speed limit, bytes/s
        #[arg(long)]
        limit: Option<u64>,
    },
    /// Standalone HLS download without the daemon
    Hls {
        url: String,
        /// Output file, or an existing directory (`-d DIR`, like `ldm add`)
        #[arg(long, short = 'o', visible_alias = "dir", visible_short_alias = 'd')]
        output: Option<PathBuf>,
        #[arg(long)]
        height: Option<u32>,
        #[command(flatten)]
        req: ReqOpts,
    },
    /// Standalone DASH download without the daemon
    Dash {
        url: String,
        /// Output file, or an existing directory (`-d DIR`, like `ldm add`)
        #[arg(long, short = 'o', visible_alias = "dir", visible_short_alias = 'd')]
        output: Option<PathBuf>,
        #[arg(long)]
        height: Option<u32>,
        #[command(flatten)]
        req: ReqOpts,
    },
}

#[derive(Subcommand)]
enum ToolsCmd {
    /// Versions, sources (user / managed / system) and the last update check
    Status,
    /// Download + verify + activate (default: yt-dlp and deno, latest release)
    Install {
        /// yt-dlp | deno
        tool: Option<String>,
        /// A specific release, e.g. 2025.11.12 (yt-dlp) or 2.9.8 (deno)
        #[arg(long)]
        version: Option<String>,
    },
    /// Install the latest release when newer (keeps the previous one for rollback)
    Update { tool: Option<String> },
    /// Only look for a newer release
    Check { tool: Option<String> },
    /// Switch back to the previously active version
    Rollback { tool: Option<String> },
    /// Delete the managed copies (not your own yt-dlp)
    Remove { tool: Option<String> },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Start ldmd in the background
    Start {
        /// Enable the WebSocket bridge
        #[arg(long)]
        ws: bool,
    },
    /// Stop ldmd
    Stop,
    /// Show daemon status
    Status,
}

#[derive(Subcommand)]
enum QueueCmd {
    List,
    Set {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        concurrent: Option<u32>,
        /// Time window "HH:MM-HH:MM"
        #[arg(long)]
        window: Option<String>,
        /// Clear the time window
        #[arg(long)]
        no_window: bool,
        #[arg(long)]
        disable: bool,
        #[arg(long)]
        enable: bool,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Print config (optionally one dotted key)
    Get { key: Option<String> },
    /// Set a dotted key, e.g. `engine.max_connections_default 4`
    Set { key: String, value: String },
    /// Print the config file path
    Path,
}

const EXIT_NO_DAEMON: i32 = 3;

fn main() {
    let cli = Cli::parse();
    if std::env::var_os("LDM_LOG").is_some() {
        tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_env("LDM_LOG")).with_writer(std::io::stderr).init();
    }
    match run(cli) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            eprintln!("ldm: 错误: {e:#}");
            let code = if e.downcast_ref::<NoDaemon>().is_some() { EXIT_NO_DAEMON } else { 1 };
            std::process::exit(code);
        }
    }
}

#[derive(Debug)]
struct NoDaemon(PathBuf);
impl std::fmt::Display for NoDaemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ldmd 未运行（{}）。请先执行 `ldm daemon start`", self.0.display())
    }
}
impl std::error::Error for NoDaemon {}

fn connect() -> Result<Client> {
    let p = paths::socket_path();
    Client::connect(&p).map_err(|_| anyhow!(NoDaemon(p)))
}

fn call(c: &mut Client, kind: &str, payload: Value) -> Result<Value> {
    let r = c.request(kind, payload)?;
    if r.kind == types::ERROR {
        bail!("{} ({})", r.payload["error"].as_str().unwrap_or("?"), r.payload["code"].as_str().unwrap_or("?"));
    }
    Ok(r.payload)
}

fn read_cookie_file(p: &Path, url: &str) -> Result<String> {
    let text = std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?;
    let host = url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_string())).unwrap_or_default();
    let mut pairs = vec![];
    let mut netscape = false;
    for line in text.lines() {
        let line = line.strip_prefix("#HttpOnly_").unwrap_or(line);
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let cols: Vec<&str> = line.split('\t').collect();
        if cols.len() >= 7 {
            netscape = true;
            let domain = cols[0].trim_start_matches('.');
            if host == domain || host.ends_with(&format!(".{domain}")) {
                pairs.push(format!("{}={}", cols[5], cols[6]));
            }
        }
    }
    if netscape {
        Ok(pairs.join("; "))
    } else {
        Ok(text.trim().trim_start_matches("Cookie:").trim().to_string())
    }
}

fn req_ctx(r: &ReqOpts, url: &str) -> Result<RequestCtx> {
    let cookies = match (&r.cookie, &r.cookie_file) {
        (Some(c), _) => Some(c.clone()),
        (None, Some(f)) => Some(read_cookie_file(f, url)?).filter(|s| !s.is_empty()),
        _ => None,
    };
    let mut headers = vec![];
    for h in &r.headers {
        let (n, v) = h.split_once(':').ok_or_else(|| anyhow!("bad header '{h}', expected 'Name: value'"))?;
        headers.push((n.trim().to_string(), v.trim().to_string()));
    }
    Ok(RequestCtx { referrer: r.referrer.clone(), cookies, user_agent: r.user_agent.clone(), headers })
}

fn human(n: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n.max(0.0);
    let mut i = 0;
    while v >= 1024.0 && i < units.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{v:.0} {}", units[i])
    } else {
        format!("{v:.1} {}", units[i])
    }
}

/// `HH:MM:SS` of recorded live time.
fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn print_jobs(jobs: &[Value]) {
    println!("{:<12}  {:<10}  {:>6}  {:>10}  {:>10}  文件/URL", "ID", "状态", "进度", "大小", "速度");
    for j in jobs {
        let got = j["got"].as_f64().unwrap_or(0.0);
        let total = j["total"].as_f64();
        let live = j["recordedSecs"].as_f64();
        // live recordings: elapsed recorded time instead of a percentage
        let pct = match live {
            Some(r) => hms(r),
            None => total.filter(|t| *t > 0.0).map(|t| format!("{:.0}%", got * 100.0 / t)).unwrap_or_else(|| "-".into()),
        };
        let size = if live.is_some() { human(got) } else { total.map(human).unwrap_or_else(|| human(got)) };
        let speed = j["speedBps"].as_f64().map(|s| format!("{}/s", human(s))).unwrap_or_default();
        let name = j["filename"].as_str().map(String::from).unwrap_or_else(|| j["url"].as_str().unwrap_or("").to_string());
        println!(
            "{:<12}  {:<10}  {:>6}  {:>10}  {:>10}  {}",
            j["id"].as_str().unwrap_or(""),
            j["state"].as_str().unwrap_or(""),
            pct,
            size,
            speed,
            name
        );
    }
}

fn wait_job(c: &mut Client, id: &str) -> Result<i32> {
    call(c, types::SUBSCRIBE, json!({"enabled": true}))?;
    let mut last = Instant::now() - Duration::from_secs(1);
    c.set_timeout(Some(Duration::from_secs(2)))?;
    loop {
        let ev = match c.next_event() {
            Ok(Some(e)) => Some(e),
            Ok(None) => bail!("daemon closed the connection"),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => None,
            Err(e) => return Err(e.into()),
        };
        let poll = ev.is_none();
        if let Some(ev) = &ev {
            if ev.kind == types::JOB_PROGRESS && ev.payload["jobId"] == id && last.elapsed() > Duration::from_millis(300) {
                last = Instant::now();
                let got = ev.payload["got"].as_f64().unwrap_or(0.0);
                let tot = ev.payload["total"].as_f64();
                let pct = match ev.payload["recordedSecs"].as_f64() {
                    Some(r) => format!("录制 {}", hms(r)),
                    None => tot.filter(|t| *t > 0.0).map(|t| format!("{:5.1}%", got * 100.0 / t)).unwrap_or_else(|| "  -  ".into()),
                };
                eprint!(
                    "\r{pct}  {:>10}  {:>10}/s  {:<10}",
                    human(got),
                    human(ev.payload["speedBps"].as_f64().unwrap_or(0.0)),
                    ev.payload["state"].as_str().unwrap_or("")
                );
                let _ = std::io::stderr().flush();
            }
        }
        let state_changed = ev.as_ref().map(|e| e.kind == types::JOB_STATE_CHANGED && e.payload["id"] == id).unwrap_or(false);
        if state_changed || poll {
            let j = call(c, types::JOB_GET, json!({"id": id}))?;
            let st = j["job"]["state"].as_str().unwrap_or("").to_string();
            match st.as_str() {
                "Completed" => {
                    eprintln!();
                    println!("完成: {}", j["job"]["final_path"].as_str().unwrap_or(""));
                    return Ok(0);
                }
                "Failed" | "Canceled" => {
                    eprintln!();
                    eprintln!("失败: {} ({})", j["job"]["error"].as_str().unwrap_or(""), j["job"]["error_code"].as_str().unwrap_or(""));
                    return Ok(1);
                }
                "Paused" => {
                    eprintln!();
                    eprintln!("已暂停");
                    return Ok(1);
                }
                _ => {}
            }
        }
    }
}

fn ldmd_path() -> PathBuf {
    if let Ok(p) = std::env::var("LDM_LDMD") {
        return PathBuf::from(p);
    }
    std::env::current_exe().ok().map(|p| p.with_file_name("ldmd")).filter(|p| p.exists()).unwrap_or_else(|| PathBuf::from("ldmd"))
}

fn set_dotted(root: &mut Value, key: &str, val: Value) {
    let mut cur = root;
    let parts: Vec<&str> = key.split('.').collect();
    for (i, p) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            cur[*p] = val;
            return;
        }
        if !cur[*p].is_object() {
            cur[*p] = json!({});
        }
        cur = &mut cur[*p];
    }
}

fn standalone_env(limit: Option<u64>) -> Result<JobEnv> {
    let mut cfg = Config::load().unwrap_or_default();
    if let Some(l) = limit {
        cfg.engine.enable_speed_limit = true;
        cfg.engine.speed_limit_bps = l;
    }
    Ok(JobEnv::standalone(cfg)?)
}

fn progress_task(env: &JobEnv) -> tokio::task::JoinHandle<()> {
    let p = env.progress.clone();
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let got = p.got();
            p.speed.record(got);
            let pct = if p.is_live() {
                format!("录制 {}", hms(p.recorded_secs()))
            } else {
                p.total().filter(|t| *t > 0).map(|t| format!("{:5.1}%", got as f64 * 100.0 / t as f64)).unwrap_or_else(|| "  -  ".into())
            };
            let segs = p.segments_total.load(std::sync::atomic::Ordering::Relaxed);
            let seg = if segs > 0 {
                format!(" 分片 {}/{}", p.segments_done.load(std::sync::atomic::Ordering::Relaxed), segs)
            } else {
                String::new()
            };
            eprint!(
                "\r{pct} {:>10} {:>10}/s 连接 {}{seg} {}   ",
                human(got as f64),
                human(p.speed.speed()),
                p.connections.load(std::sync::atomic::Ordering::Relaxed),
                p.phase()
            );
            let _ = std::io::stderr().flush();
        }
    })
}

fn out_spec(o: &Option<PathBuf>) -> OutputSpec {
    match o {
        Some(p) if p.is_dir() => OutputSpec { dir: Some(p.clone()), ..Default::default() },
        Some(p) => OutputSpec { exact_path: Some(p.clone()), ..Default::default() },
        None => OutputSpec { dir: Some(std::env::current_dir().unwrap_or_default()), ..Default::default() },
    }
}

fn work_dir(url: &str) -> PathBuf {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut h);
    paths::cache_dir().join("standalone").join(format!("{:016x}", h.finish()))
}

fn run(cli: Cli) -> Result<i32> {
    let json_out = cli.json;
    match cli.cmd {
        Cmd::Add { url, req, conns, dir, name, queue, kind, ytdlp, playlist, height, sha256, paused, wait } => {
            let kind = if ytdlp || playlist { Some("ytdlp".to_string()) } else { kind };
            if url.is_empty() {
                bail!("至少需要一个 URL");
            }
            let mut c = connect()?;
            let mut last_id = None;
            for u in &url {
                let ctx = req_ctx(&req, u)?;
                let p = json!({
                    "url": u, "referrer": ctx.referrer, "cookies": ctx.cookies, "user_agent": ctx.user_agent,
                    "headers_extra": ctx.headers.iter().map(|(n, v)| json!({"name": n, "value": v})).collect::<Vec<_>>(),
                    "connections": conns, "save_dir": dir.as_ref().map(|d| std::fs::canonicalize(d).map(|p| p.to_string_lossy().into_owned()).unwrap_or_else(|_| d.clone())),
                    "filename_hint": name, "queue": queue, "kind": kind, "preferred_height": height, "sha256": sha256, "paused": paused, "source": "cli",
                    "playlist": playlist,
                });
                let mut r = c.request(types::DOWNLOAD_ADD, p.clone())?;
                // playlist URLs need yt-dlp before anything is queued: ask, install, retry once
                if r.kind == types::ERROR && r.payload["code"] == "E_TOOL_MISSING" && !json_out && ask_install_ytdlp(&mut c)? {
                    r = c.request(types::DOWNLOAD_ADD, p)?;
                }
                if r.kind == types::ERROR {
                    bail!("{} ({})", r.payload["error"].as_str().unwrap_or("?"), r.payload["code"].as_str().unwrap_or("?"));
                }
                let r = r.payload;
                if r["toolsNeeded"].is_string() && !json_out {
                    ask_install_ytdlp(&mut c)?;
                }
                if json_out {
                    println!("{r}");
                } else if let Some(ids) = r["jobIds"].as_array().filter(|a| a.len() > 1) {
                    for id in ids {
                        println!("{}  (playlist) {u}", id.as_str().unwrap_or("?"));
                    }
                } else if let Some(id) = r["jobId"].as_str() {
                    let dup = if r["duplicate"].as_bool().unwrap_or(false) { "（已存在）" } else { "" };
                    println!("{id}{dup}  {u}");
                } else {
                    println!("未添加: {}", r["reason"].as_str().unwrap_or("?"));
                }
                last_id = r["jobId"].as_str().map(String::from);
            }
            if wait {
                if let Some(id) = last_id {
                    return wait_job(&mut c, &id);
                }
            }
            Ok(0)
        }
        Cmd::List { state } => {
            let mut c = connect()?;
            let r = call(&mut c, types::JOB_LIST, json!({"state": state}))?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&r["jobs"])?);
            } else {
                print_jobs(r["jobs"].as_array().map(|v| v.as_slice()).unwrap_or(&[]));
            }
            Ok(0)
        }
        Cmd::Info { id } => {
            let mut c = connect()?;
            let r = call(&mut c, types::JOB_GET, json!({"id": id}))?;
            println!("{}", serde_json::to_string_pretty(&r["job"])?);
            Ok(0)
        }
        Cmd::Pause { id, all } | Cmd::Resume { id, all } if id.is_none() && !all => bail!("需要任务 ID 或 --all"),
        Cmd::Pause { id, all } => {
            let mut c = connect()?;
            call(&mut c, types::JOB_PAUSE, json!({"id": id, "all": all}))?;
            Ok(0)
        }
        Cmd::Resume { id, all } => {
            let mut c = connect()?;
            call(&mut c, types::JOB_RESUME, json!({"id": id, "all": all}))?;
            Ok(0)
        }
        Cmd::StopRecording { id } => {
            let mut c = connect()?;
            call(&mut c, types::JOB_STOP_RECORDING, json!({"id": id}))?;
            println!("已请求停止录制: {id}");
            Ok(0)
        }
        Cmd::Rm { id, delete_files } => {
            let mut c = connect()?;
            call(&mut c, types::JOB_REMOVE, json!({"id": id, "deleteFiles": delete_files}))?;
            Ok(0)
        }
        Cmd::Set { id, priority, queue, conns } => {
            let mut c = connect()?;
            let r = call(&mut c, types::JOB_SET, json!({"id": id, "priority": priority, "queue": queue, "connections": conns}))?;
            println!("{}", serde_json::to_string_pretty(&r["job"])?);
            Ok(0)
        }
        Cmd::Watch { id } => {
            let mut c = connect()?;
            if let Some(id) = id {
                let full = call(&mut c, types::JOB_GET, json!({"id": id}))?["job"]["id"].as_str().unwrap_or("").to_string();
                return wait_job(&mut c, &full);
            }
            call(&mut c, types::SUBSCRIBE, json!({"enabled": true}))?;
            while let Some(ev) = c.next_event()? {
                println!("{} {}", ev.kind, ev.payload);
            }
            Ok(0)
        }
        Cmd::Probe { url, req } => {
            let mut c = connect()?;
            let ctx = req_ctx(&req, &url)?;
            let r = call(&mut c, types::MEDIA_PROBE, json!({"url": url, "referrer": ctx.referrer, "cookies": ctx.cookies}))?;
            if json_out {
                println!("{}", serde_json::to_string_pretty(&r)?);
            } else {
                println!(
                    "类型: {}  时长: {}  直播: {}",
                    r["source"].as_str().unwrap_or("?"),
                    r["duration"].as_f64().map(|d| format!("{d:.1}s")).unwrap_or("-".into()),
                    r["live"]
                );
                for q in r["qualities"].as_array().cloned().unwrap_or_default() {
                    println!(
                        "  {:<6} {:<12} {:>8} kbps",
                        q["kind"].as_str().unwrap_or(""),
                        q["label"].as_str().unwrap_or(""),
                        q["bandwidth"].as_u64().unwrap_or(0) / 1000
                    );
                }
            }
            Ok(0)
        }
        Cmd::Daemon { action } => match action {
            DaemonCmd::Start { ws } => {
                if connect().is_ok() {
                    println!("ldmd 已在运行");
                    return Ok(0);
                }
                let mut cmd = Command::new(ldmd_path());
                if ws {
                    cmd.arg("--ws");
                }
                let log = paths::cache_dir().join("ldmd.log");
                std::fs::create_dir_all(paths::cache_dir()).ok();
                let f = std::fs::OpenOptions::new().create(true).append(true).open(&log)?;
                cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(f);
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
                cmd.spawn().context("无法启动 ldmd")?;
                let t0 = Instant::now();
                while t0.elapsed() < Duration::from_secs(8) {
                    if connect().is_ok() {
                        println!("ldmd 已启动（日志 {}）", log.display());
                        return Ok(0);
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
                bail!("ldmd 启动超时，见 {}", log.display())
            }
            DaemonCmd::Stop => {
                let Ok(mut c) = connect() else {
                    println!("ldmd 未运行");
                    return Ok(0);
                };
                call(&mut c, types::SHUTDOWN, json!({}))?;
                let t0 = Instant::now();
                while paths::socket_path().exists() && t0.elapsed() < Duration::from_secs(15) {
                    std::thread::sleep(Duration::from_millis(100));
                }
                println!("ldmd 已停止");
                Ok(0)
            }
            DaemonCmd::Status => {
                let mut c = connect()?;
                let s = call(&mut c, types::STATUS, json!({}))?;
                if json_out {
                    println!("{}", serde_json::to_string_pretty(&s)?);
                } else {
                    println!("ldmd {} 运行中 pid={} 已运行 {}s", s["version"].as_str().unwrap_or(""), s["pid"], s["uptimeSecs"]);
                    println!(
                        "活动 {} / 排队 {} / 暂停 {} / 完成 {} / 失败 {}  总速度 {}/s",
                        s["active"],
                        s["queued"],
                        s["paused"],
                        s["completed"],
                        s["failed"],
                        human(s["speedBps"].as_f64().unwrap_or(0.0))
                    );
                    println!("WebSocket 桥: {}", s["wsPort"].as_u64().map(|p| format!("127.0.0.1:{p}")).unwrap_or_else(|| "关闭".into()));
                }
                Ok(0)
            }
        },
        Cmd::Queue { action } => {
            let mut c = connect()?;
            match action {
                QueueCmd::List => {
                    let r = call(&mut c, types::QUEUE_LIST, json!({}))?;
                    if json_out {
                        println!("{}", serde_json::to_string_pretty(&r["queues"])?);
                    } else {
                        for q in r["queues"].as_array().cloned().unwrap_or_default() {
                            println!(
                                "{:<10} {:<12} 并发 {}  {}  {}",
                                q["id"].as_str().unwrap_or(""),
                                q["name"].as_str().unwrap_or(""),
                                q["concurrent"],
                                if q["enabled"].as_bool().unwrap_or(true) { "启用" } else { "停用" },
                                if q["schedule"].is_null() { String::new() } else { q["schedule"].to_string() }
                            );
                        }
                    }
                }
                QueueCmd::Set { id, name, concurrent, window, no_window, disable, enable } => {
                    let r = call(&mut c, types::QUEUE_LIST, json!({}))?;
                    let cur = r["queues"].as_array().and_then(|a| a.iter().find(|q| q["id"] == id.as_str()).cloned());
                    let mut q = cur.unwrap_or_else(|| json!({"id": id, "name": id, "concurrent": 3, "schedule": null, "enabled": true}));
                    if let Some(n) = name {
                        q["name"] = json!(n);
                    }
                    if let Some(n) = concurrent {
                        q["concurrent"] = json!(n);
                    }
                    if let Some(w) = window {
                        let (a, b) = w.split_once('-').ok_or_else(|| anyhow!("窗口格式应为 HH:MM-HH:MM"))?;
                        q["schedule"] = json!({"start": a, "end": b});
                    }
                    if no_window {
                        q["schedule"] = Value::Null;
                    }
                    if disable {
                        q["enabled"] = json!(false);
                    }
                    if enable {
                        q["enabled"] = json!(true);
                    }
                    call(&mut c, types::QUEUE_SET, json!({"queue": q}))?;
                }
            }
            Ok(0)
        }
        Cmd::Config { action } => match action {
            ConfigCmd::Path => {
                println!("{}", paths::config_file().display());
                Ok(0)
            }
            ConfigCmd::Get { key } => {
                let cfg = match connect() {
                    Ok(mut c) => call(&mut c, types::CONFIG_GET, json!({}))?["config"].clone(),
                    Err(_) => serde_json::to_value(Config::load()?)?,
                };
                let v = match &key {
                    Some(k) => k.split('.').fold(&cfg, |acc, p| &acc[p]).clone(),
                    None => cfg,
                };
                println!("{}", serde_json::to_string_pretty(&v)?);
                Ok(0)
            }
            ConfigCmd::Set { key, value } => {
                let val: Value = serde_json::from_str(&value).unwrap_or(Value::String(value.clone()));
                let mut patch = json!({});
                set_dotted(&mut patch, &key, val);
                match connect() {
                    Ok(mut c) => {
                        call(&mut c, types::CONFIG_SET, json!({"patch": patch}))?;
                    }
                    Err(_) => {
                        let cfg = Config::load()?.patched(&patch)?;
                        cfg.save_to(&paths::config_file())?;
                    }
                }
                Ok(0)
            }
        },
        Cmd::Export { format, output } => {
            let mut c = connect()?;
            let r = call(&mut c, types::EXPORT, json!({"format": format}))?;
            let data = r["data"].as_str().unwrap_or("");
            match output {
                Some(p) => std::fs::write(p, data)?,
                None => print!("{data}"),
            }
            Ok(0)
        }
        Cmd::Import { file, paused } => {
            let data = std::fs::read_to_string(&file)?;
            let mut c = connect()?;
            let r = call(&mut c, types::IMPORT, json!({"data": data, "paused": paused}))?;
            println!("已导入 {} 个任务", r["added"]);
            Ok(0)
        }
        Cmd::Doctor => doctor(),
        Cmd::Tools { cmd } => tools_cmd(cmd, json_out),
        Cmd::Metrics => {
            let s = match connect() {
                Ok(mut c) => ldm_core::util::metrics::prometheus(&call(&mut c, types::STATUS, json!({}))?),
                Err(_) => "# HELP ldm_up 1 when the daemon answered\n# TYPE ldm_up gauge\nldm_up 0\n".to_string(),
            };
            print!("{s}");
            Ok(0)
        }
        Cmd::Diag { output } => {
            let (cfg, jobs, running) = match connect() {
                Ok(mut c) => {
                    let cfg = call(&mut c, types::CONFIG_GET, json!({}))?["config"].clone();
                    let jobs = call(&mut c, types::JOB_LIST, json!({}))?["jobs"].as_array().cloned().unwrap_or_default();
                    (cfg, jobs, true)
                }
                Err(_) => (serde_json::to_value(Config::load()?)?, vec![], false),
            };
            let log = std::fs::read_to_string(paths::cache_dir().join("ldmd.log")).unwrap_or_default();
            let tail: Vec<&str> = log.lines().rev().take(500).collect::<Vec<_>>().into_iter().rev().collect();
            let b = ldm_core::util::diag::bundle(&cfg, &jobs, &tail.join("\n"), running);
            let out = output.unwrap_or_else(|| {
                PathBuf::from(format!(
                    "ldm-diag-{}.json",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
                ))
            });
            std::fs::write(&out, serde_json::to_vec_pretty(&b)?)?;
            println!("诊断包已写入 {}（已去除密码、Cookie、命令与 URL 查询参数）", out.display());
            Ok(0)
        }
        Cmd::Get { url, output, conns, req, sha256, limit } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env = standalone_env(limit)?;
                let pt = progress_task(&env);
                let spec = HttpJobSpec {
                    url: url.clone(),
                    ctx: req_ctx(&req, &url)?,
                    max_conns: conns.clamp(1, 32),
                    parts_dir: work_dir(&url),
                    output: out_spec(&output),
                    filename_hint: None,
                    sha256,
                    title_stem: None,
                };
                let r = run_http_job(&env, &spec).await;
                pt.abort();
                eprintln!();
                let o = r?;
                println!("{}", o.path.display());
                Ok::<i32, anyhow::Error>(0)
            })
        }
        Cmd::Hls { url, output, height, req } | Cmd::Dash { url, output, height, req } => {
            let rt = tokio::runtime::Runtime::new()?;
            rt.block_on(async {
                let env = standalone_env(None)?;
                let pt = progress_task(&env);
                // Ctrl-C while recording a live stream = "停止录制" (keep what was recorded);
                // otherwise, or a second Ctrl-C, aborts as usual
                let prog = env.progress.clone();
                let sig = tokio::spawn(async move {
                    while tokio::signal::ctrl_c().await.is_ok() {
                        if prog.is_live() && !prog.stop_requested() {
                            eprintln!("\n正在停止录制…（再按 Ctrl-C 放弃）");
                            prog.request_stop_recording();
                        } else {
                            std::process::exit(130);
                        }
                    }
                });
                let spec = MediaJobSpec {
                    url: url.clone(),
                    ctx: req_ctx(&req, &url)?,
                    work_dir: work_dir(&url),
                    output: out_spec(&output),
                    filename_hint: None,
                    preferred_height: height,
                    audio_url: None,
                };
                let r = run_media_job(&env, &spec).await;
                pt.abort();
                sig.abort();
                eprintln!();
                let o = r?;
                if !o.remuxed {
                    eprintln!("注意: 未找到 ffmpeg，输出为未封装的拼接流");
                }
                println!("{}", o.path.display());
                Ok::<i32, anyhow::Error>(0)
            })
        }
    }
}

fn doctor() -> Result<i32> {
    let mut bad = 0;
    let mut check = |name: &str, ok: bool, detail: String| {
        println!("[{}] {name}: {detail}", if ok { " OK " } else { "WARN" });
        if !ok {
            bad += 1;
        }
    };
    let cfg = Config::load();
    check(
        "配置文件",
        cfg.is_ok(),
        format!("{}{}", paths::config_file().display(), cfg.as_ref().err().map(|e| format!(" ({e})")).unwrap_or_default()),
    );
    let cfg = cfg.unwrap_or_default();
    let ff = ldm_core::media::mux::ffmpeg_available_blocking(&cfg.media.ffmpeg_path);
    check("ffmpeg", ff, if ff { cfg.media.ffmpeg_path.clone() } else { "未找到：HLS/DASH 将输出未封装的流".into() });
    let yt = ldm_core::ytdlp::status_blocking(&cfg.ytdlp);
    let yt_detail = match (&yt.path, &yt.version) {
        _ if !cfg.ytdlp.enabled => "已关闭（ytdlp.enabled = false）".to_string(),
        (Some(p), Some(v)) => format!(
            "{p} {v}（来源：{}；{}）{}",
            source_label(yt.source.as_deref()),
            yt.install.as_deref().unwrap_or("?"),
            if yt.outdated {
                format!(" 版本已 {} 天未更新：运行 `{}`", yt.age_days.unwrap_or(0), ldm_core::ytdlp::update_command(Path::new(p)).join(" "))
            } else {
                String::new()
            }
        ),
        (Some(p), None) => format!("{p} 无法运行 --version"),
        _ => format!("未找到（可选）：YouTube 与回退下载需要 yt-dlp。{}", ldm_core::ytdlp::install_hint()),
    };
    check("yt-dlp", !cfg.ytdlp.enabled || (yt.usable && !yt.outdated), yt_detail);
    if cfg.ytdlp.enabled {
        let deno = match (&yt.deno_path, &yt.deno_source) {
            (Some(p), s) => format!("{p}（来源：{}）", source_label(s.as_deref())),
            _ => "未找到（可选）：YouTube 的 JS 验证需要 deno，`ldm tools install deno`".into(),
        };
        println!("[INFO] deno: {deno}");
        println!("[INFO] yt-dlp 自动安装: {}，自动更新: {}（`ldm tools status`）", cfg.ytdlp.auto_install, cfg.ytdlp.auto_update);
    }
    let sock = paths::socket_path();
    let live = Client::connect(&sock).is_ok();
    check("ldmd", live, format!("{} {}", sock.display(), if live { "运行中" } else { "未运行" }));
    let dl = cfg.paths.download_dir();
    match paths::sandbox_hidden(&dl) {
        Some(why) => check("下载目录", false, why),
        None => check("下载目录", std::fs::create_dir_all(&dl).is_ok(), dl.display().to_string()),
    }
    let home = paths::home();
    if ldm_core::ytdlp::in_flatpak() {
        // the browsers' manifests live on the host, outside the sandbox's view
        println!("[INFO] Native Messaging: 在 Flatpak 沙箱内无法检查；请在宿主机运行 packaging/flatpak/host-integration.sh install");
        println!("{}", if bad == 0 { "一切正常" } else { "存在警告（见上）" });
        return Ok(0);
    }
    for (b, d) in [
        ("Chrome", ".config/google-chrome/NativeMessagingHosts"),
        ("Chromium", ".config/chromium/NativeMessagingHosts"),
        ("Firefox", ".mozilla/native-messaging-hosts"),
    ] {
        let p = home.join(d).join(format!("{}.json", ldm_ipc::NATIVE_HOST_NAME));
        let ok = p.exists();
        let detail = if ok {
            let t = std::fs::read_to_string(&p).unwrap_or_default();
            let v: Value = serde_json::from_str(&t).unwrap_or_default();
            let path = v["path"].as_str().unwrap_or("");
            format!("{} -> {} {}", p.display(), path, if Path::new(path).exists() { "" } else { "（host 不存在！）" })
        } else {
            format!("{} 未安装", p.display())
        };
        check(&format!("Native Messaging ({b})"), ok, detail);
    }
    println!("{}", if bad == 0 { "一切正常" } else { "存在警告（见上）" });
    Ok(0)
}

fn source_label(s: Option<&str>) -> &'static str {
    match s {
        Some("user") => "用户指定",
        Some("managed") => "LDM 托管",
        Some("system") => "系统 PATH",
        _ => "?",
    }
}

// ---------------------------------------------------------------- ldm tools

fn tool_arg(t: &Option<String>) -> Result<Option<ldm_core::tools::Tool>> {
    match t.as_deref() {
        None | Some("") | Some("all") => Ok(None),
        Some(s) => ldm_core::tools::Tool::parse(s).map(Some).ok_or_else(|| anyhow!("未知工具 {s}（yt-dlp | deno）")),
    }
}

/// `ldm tools …`: through the daemon when it runs (it knows which binaries running jobs
/// use and re-queues jobs waiting for yt-dlp), otherwise directly.
fn tools_cmd(cmd: ToolsCmd, json_out: bool) -> Result<i32> {
    let (action, tool, version) = match cmd {
        ToolsCmd::Status => return tools_status(json_out),
        ToolsCmd::Install { tool, version } => ("install", tool, version),
        ToolsCmd::Update { tool } => ("update", tool, None),
        ToolsCmd::Check { tool } => ("check", tool, None),
        ToolsCmd::Rollback { tool } => ("rollback", tool, None),
        ToolsCmd::Remove { tool } => ("remove", tool, None),
    };
    let t = tool_arg(&tool)?;
    if version.is_some() && t.is_none() {
        bail!("--version 需要指定工具（yt-dlp 或 deno）");
    }
    let r = tools_run(action, t, version.as_deref())?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&r)?);
    } else {
        print_tools_result(action, &r);
    }
    let failed = r.as_object().map(|o| o.values().any(|v| v.get("error").is_some())).unwrap_or(false);
    Ok(if failed { 1 } else { 0 })
}

fn tools_run(action: &str, t: Option<ldm_core::tools::Tool>, version: Option<&str>) -> Result<Value> {
    if action != "check" && action != "status" {
        eprintln!(
            "{}…",
            match action {
                "install" => "正在从 GitHub 官方发布页下载并校验",
                "update" => "正在检查更新",
                "rollback" => "正在回滚",
                _ => "正在处理",
            }
        );
    }
    if let Ok(mut c) = connect() {
        let r =
            call(&mut c, types::TOOLS_ACTION, json!({"action": action, "tool": t.map(|t| t.name()), "version": version, "wait": true}))?;
        return Ok(r["result"].clone());
    }
    let cfg = Config::load().unwrap_or_default();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        use ldm_core::tools::{Installer, Layout, Tool};
        let http = ldm_core::http::HttpClient::new(&cfg.engine)?;
        let layout = Layout::from_config(&cfg.ytdlp);
        let list: Vec<Tool> = match t {
            Some(t) => vec![t],
            None if action == "install" => vec![Tool::YtDlp, Tool::Deno],
            None => [Tool::YtDlp, Tool::Deno].into_iter().filter(|t| layout.current_version(*t).is_some()).collect(),
        };
        let mut out = serde_json::Map::new();
        for tool in list {
            let mut ins = Installer::new(http.clone(), &cfg.ytdlp);
            let tty = std::io::IsTerminal::is_terminal(&std::io::stderr());
            let last = std::sync::Mutex::new(Instant::now());
            ins.progress = tty.then(|| -> ldm_core::tools::ProgressFn {
                std::sync::Arc::new(move |phase: &str, got: u64, total: Option<u64>| {
                    let mut l = last.lock().unwrap();
                    if phase == "download" && l.elapsed() < Duration::from_millis(300) && total != Some(got) {
                        return;
                    }
                    *l = Instant::now();
                    let pct = total.filter(|t| *t > 0).map(|t| format!(" {:5.1}%", got as f64 * 100.0 / t as f64)).unwrap_or_default();
                    eprint!("\r{tool} {phase}{pct} {:>10}   ", if got > 0 { human(got as f64) } else { String::new() });
                    let _ = std::io::stderr().flush();
                })
            });
            let r: ldm_core::Result<Value> = match action {
                "install" => ins.install(tool, version, &[]).await.map(|r| json!(r)),
                "update" => ins.update(tool, &[]).await.map(|r| json!(r)),
                "check" => ins.check(tool).await.map(|(c, l)| {
                    let newer = c.as_deref().is_none_or(|c| ldm_core::tools::version_newer(&l, c));
                    json!({"current": c, "latest": l, "updateAvailable": newer})
                }),
                "rollback" => ins.rollback(tool).await.map(|(from, to)| json!({"from": from, "to": to})),
                _ => ins.remove(tool, &[]).await.map(|v| json!({"removed": v})),
            };
            if tty {
                eprint!("\r{:60}\r", "");
            }
            match r {
                Ok(v) => out.insert(tool.name().into(), v),
                Err(e) => out.insert(tool.name().into(), json!({"error": e.to_string(), "code": e.code()})),
            };
        }
        Ok::<Value, anyhow::Error>(Value::Object(out))
    })
}

fn print_tools_result(action: &str, r: &Value) {
    let Some(o) = r.as_object() else { return };
    for (tool, v) in o {
        if tool == "requeued" {
            if v.as_u64().unwrap_or(0) > 0 {
                println!("已重新排队 {} 个等待 yt-dlp 的任务", v);
            }
            continue;
        }
        if let Some(e) = v["error"].as_str() {
            println!("{tool}: 失败：{e}");
            continue;
        }
        let install_line = |i: &Value| {
            format!(
                "{} {}{} → {}\n  来源 {}\n  校验 SHA-256（{}）{}；GPG：{}",
                tool,
                i["version"].as_str().unwrap_or("?"),
                if i["reused"].as_bool().unwrap_or(false) { "（已存在，直接切换）" } else { "" },
                i["path"].as_str().unwrap_or(""),
                i["url"].as_str().unwrap_or(""),
                i["checksum_source"].as_str().unwrap_or("?"),
                i["sha256"].as_str().map(|h| format!(" {}", &h[..h.len().min(16)])).unwrap_or_default(),
                i["gpg"].as_str().unwrap_or("-"),
            )
        };
        match action {
            "install" => println!("{}", install_line(v)),
            "update" => match (&v["install"], v["updated"].as_bool().unwrap_or(false)) {
                (i, true) if !i.is_null() => println!("已更新 {} → {}", v["from"].as_str().unwrap_or("（无）"), install_line(i)),
                _ => println!("{tool}: 已是最新（{}）", v["latest"].as_str().unwrap_or("?")),
            },
            "check" => println!(
                "{tool}: 当前 {}，最新 {}{}",
                v["current"].as_str().unwrap_or("（未安装托管版本）"),
                v["latest"].as_str().unwrap_or("?"),
                if v["updateAvailable"].as_bool().unwrap_or(false) { "（可更新：ldm tools update）" } else { "" }
            ),
            "rollback" => println!("{tool}: {} → {}", v["from"].as_str().unwrap_or("?"), v["to"].as_str().unwrap_or("?")),
            _ => println!(
                "{tool}: 已删除 {}",
                v["removed"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(", ")).unwrap_or_default()
            ),
        }
    }
}

fn tools_status(json_out: bool) -> Result<i32> {
    let st = match connect() {
        Ok(mut c) => call(&mut c, types::TOOLS_STATUS, json!({}))?,
        Err(_) => {
            let cfg = Config::load().unwrap_or_default();
            let layout = ldm_core::tools::Layout::from_config(&cfg.ytdlp);
            let rt = tokio::runtime::Runtime::new()?;
            let (yt, deno) = rt.block_on(async {
                (
                    ldm_core::tools::status(ldm_core::tools::Tool::YtDlp, &cfg.ytdlp.path, &layout).await,
                    ldm_core::tools::status(ldm_core::tools::Tool::Deno, &cfg.ytdlp.deno_path, &layout).await,
                )
            });
            json!({"yt-dlp": yt, "deno": deno, "enabled": cfg.ytdlp.enabled, "autoInstall": cfg.ytdlp.auto_install,
                   "autoUpdate": cfg.ytdlp.auto_update, "toolsDir": layout.root})
        }
    };
    if json_out {
        println!("{}", serde_json::to_string_pretty(&st)?);
        return Ok(0);
    }
    println!("托管目录: {}", st["toolsDir"].as_str().unwrap_or(""));
    println!("自动安装: {}  自动更新: {}", st["autoInstall"].as_str().unwrap_or(""), st["autoUpdate"].as_str().unwrap_or(""));
    for t in ["yt-dlp", "deno"] {
        let v = &st[t];
        let src = match v["source"].as_str() {
            Some("user") => "用户指定",
            Some("managed") => "LDM 托管",
            Some("system") => "系统",
            _ => "",
        };
        match v["path"].as_str() {
            Some(p) => println!("{t}: {} [{src}] {p}", v["version"].as_str().unwrap_or("?")),
            None => println!("{t}: 未找到（ldm tools install {t}）"),
        }
        let versions: Vec<&str> =
            v["managed_versions"].as_array().map(|a| a.iter().filter_map(|x| x.as_str()).collect()).unwrap_or_default();
        if !versions.is_empty() {
            println!(
                "  托管版本: {}（当前 {}，可回滚到 {}）",
                versions.join(", "),
                v["managed_current"].as_str().unwrap_or("-"),
                v["managed_previous"].as_str().unwrap_or("-")
            );
        }
        if let Some(m) = v["managed_meta"].as_object() {
            println!(
                "  校验: SHA-256（{}）；GPG：{}",
                m.get("checksum_source").and_then(|x| x.as_str()).unwrap_or("?"),
                m.get("gpg").and_then(|x| x.as_str()).unwrap_or("-")
            );
        }
        if let Some(l) = v["latest"].as_str() {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
            let when = match v["last_check_ms"].as_i64().filter(|t| *t > 0) {
                Some(t) if now - t < 3_600_000 => format!("{} 分钟前", (now - t) / 60_000),
                Some(t) if now - t < 172_800_000 => format!("{} 小时前", (now - t) / 3_600_000),
                Some(t) => format!("{} 天前", (now - t) / 86_400_000),
                None => "-".into(),
            };
            println!("  最新发布: {l}（检查于 {when}）{}", if v["update_available"].as_bool().unwrap_or(false) { " — 可更新" } else { "" });
        }
        if let Some(e) = v["last_error"].as_str() {
            println!("  上次检查失败: {e}");
        }
    }
    Ok(0)
}

/// Consent prompt (terminal only) when a yt-dlp job needs yt-dlp. `true` = installed.
fn ask_install_ytdlp(c: &mut Client) -> Result<bool> {
    use std::io::IsTerminal;
    let dir = ldm_core::tools::Layout::from_config(&Config::load().unwrap_or_default().ytdlp).root;
    if !std::io::stdin().is_terminal() {
        eprintln!("需要 yt-dlp：运行 `ldm tools install` 后任务会自动重新排队（或设置 ytdlp.auto_install = always）");
        return Ok(false);
    }
    eprint!("此任务需要 yt-dlp（LDM 不内置）。是否从 GitHub 官方发布页下载 yt-dlp 与 deno 到\n  {}\n并校验 SHA-256？[y/N] ", dir.display());
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes" | "是") {
        eprintln!("未安装。之后可运行 `ldm tools install`，任务会自动重新排队。");
        return Ok(false);
    }
    eprintln!("正在从 GitHub 官方发布页下载并校验…");
    let r = call(c, types::TOOLS_ACTION, json!({"action": "install", "wait": true}))?;
    print_tools_result("install", &r["result"]);
    Ok(true)
}
