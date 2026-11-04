//! Envelope dispatch shared by the Unix socket and the WebSocket bridge.
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use ldm_core::http::RequestCtx;
use ldm_core::manager::Manager;
use ldm_core::store::QueueRow;
use ldm_core::LdmError;
use ldm_ipc::messages::{types, DownloadAdd, DownloadBatch, HelloAck, InterceptDecision, MediaCandidate};
use ldm_ipc::{Envelope, PROTOCOL_VERSION};
use ldm_ws_bridge::{Backend, ClientInfo};
use serde_json::{json, Value};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info};

pub struct Daemon {
    pub mgr: Manager,
    pub events: broadcast::Sender<Envelope>,
    pub ws_port: AtomicU16,
    pub shutdown: CancellationToken,
}

fn err(env: &Envelope, e: &LdmError) -> Envelope {
    Envelope::error(env.id, e.code(), e.to_string())
}

fn bad(env: &Envelope, msg: impl Into<String>) -> Envelope {
    Envelope::error(env.id, "E_FORMAT", msg)
}

fn id_of(p: &Value) -> Option<String> {
    p.get("id").or(p.get("jobId")).and_then(|v| v.as_str()).map(String::from)
}

impl Daemon {
    pub fn new(mgr: Manager, shutdown: CancellationToken) -> Arc<Daemon> {
        let (tx, _) = broadcast::channel(1024);
        let d = Arc::new(Daemon { mgr: mgr.clone(), events: tx, ws_port: AtomicU16::new(0), shutdown });
        // manager events -> envelopes (id 0 = unsolicited)
        let mut rx = mgr.subscribe();
        let tx = d.events.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        let _ = tx.send(Envelope::new(0, ev.kind, ev.payload));
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        });
        d
    }

    fn ruleset_env(&self, id: u64) -> Envelope {
        Envelope::new(id, types::RULESET_PUSH, serde_json::to_value(self.mgr.ruleset()).unwrap_or_default())
    }

    fn add_one(&self, env: &Envelope, mut add: DownloadAdd, client: &str) -> Envelope {
        if add.source.is_none() {
            add.source = Some(client.to_string());
        }
        let cfg = self.mgr.config();
        let req_id = add.request_id.clone();
        match self.mgr.add(add) {
            Ok(r) => {
                let d = InterceptDecision {
                    action: if r.ignored.is_some() { "pass".into() } else { "take".into() },
                    save_path: r.job.as_ref().and_then(|j| j.final_path.clone()),
                    job_id: r.job.as_ref().map(|j| j.id.clone()),
                    request_id: req_id,
                    reason: r.ignored.clone().or_else(|| r.duplicate.then(|| "duplicate".to_string())),
                };
                let mut v = serde_json::to_value(d).unwrap_or_default();
                v["duplicate"] = json!(r.duplicate);
                if let Some(j) = &r.job {
                    v["kind"] = json!(j.kind);
                    // yt-dlp job without a yt-dlp and `auto_install = ask`: the client may ask for consent
                    if j.kind == "ytdlp"
                        && cfg.ytdlp.enabled
                        && ldm_core::ytdlp::find(&cfg.ytdlp).is_none()
                        && cfg.ytdlp.path.trim().is_empty()
                        && ldm_core::tools::AutoInstall::parse(&cfg.ytdlp.auto_install) == ldm_core::tools::AutoInstall::Ask
                    {
                        v["toolsNeeded"] = json!("yt-dlp");
                    }
                }
                env.reply(types::INTERCEPT_DECISION, v)
            }
            Err(e) => err(env, &e),
        }
    }

    /// Message types a browser-side client (extension via native host, WS bridge) may send.
    /// Configuration, queue management, removal, import/export and shutdown stay local (GUI/CLI).
    pub fn browser_allowed(kind: &str) -> bool {
        matches!(
            kind,
            types::HELLO
                | types::PING
                | types::DOWNLOAD_ADD
                | types::DOWNLOAD_BATCH
                | types::MEDIA_CANDIDATE
                | types::MEDIA_GEOMETRY
                | types::MEDIA_PROBE
                | types::CANDIDATE_LIST
                | types::TAB_STATE
                | types::PROXY_INFO
                | types::COOKIES_QUERY_RESULT
                | types::SUBSCRIBE
                | types::STATUS
                | types::YTDLP_STATUS
                | types::JOB_LIST
                | types::JOB_GET
                | types::JOB_PAUSE
                | types::JOB_RESUME
        )
    }

    pub async fn dispatch(&self, env: Envelope, client: &str) -> Vec<Envelope> {
        debug!(kind = %env.kind, id = env.id, client, "request");
        if matches!(client, "extension" | "ws") && !Self::browser_allowed(&env.kind) {
            return vec![Envelope::error(env.id, "E_FORBIDDEN", format!("{} is not available to browser clients", env.kind))];
        }
        let p = &env.payload;
        let ack = |extra: Value| {
            let mut v = json!({"ok": true});
            if let (Value::Object(a), Value::Object(b)) = (&mut v, extra) {
                a.extend(b);
            }
            env.reply(types::ACK, v)
        };
        let r: Result<Vec<Envelope>, LdmError> = async {
            Ok(match env.kind.as_str() {
                types::HELLO => {
                    info!(client, browser = p.get("browser").and_then(|v| v.as_str()).unwrap_or("-"), "hello");
                    let port = self.ws_port.load(Ordering::Relaxed);
                    let ackp = HelloAck {
                        protocol_version: PROTOCOL_VERSION,
                        features: ["http", "hls", "dash", "queue", "siteRules"].iter().map(|s| s.to_string()).collect(),
                        rule_set_version: self.mgr.ruleset().version,
                        daemon_version: Some(env!("CARGO_PKG_VERSION").into()),
                        ws_port: (port != 0).then_some(port),
                    };
                    vec![env.reply(types::HELLO_ACK, serde_json::to_value(ackp).unwrap_or_default()), self.ruleset_env(env.id)]
                }
                types::PING => vec![env.reply(types::PONG, json!({}))],
                types::DOWNLOAD_ADD => {
                    let mut add: DownloadAdd = match env.payload_as() {
                        Ok(a) => a,
                        Err(e) => return Ok(vec![bad(&env, format!("DownloadAdd: {e}"))]),
                    };
                    let cfg = self.mgr.config();
                    let listy = add.url.contains("list=") || add.url.contains("/playlist");
                    let routed = add.kind.as_deref() == Some("ytdlp")
                        || ldm_core::ytdlp::site_rule(&cfg.ytdlp, &add.url) == ldm_core::ytdlp::SiteRule::Always;
                    if add.playlist || (cfg.ytdlp.playlist && listy && routed) {
                        if add.source.is_none() {
                            add.source = Some(client.to_string());
                        }
                        return Ok(vec![match self.mgr.add_playlist(add).await {
                            Ok(jobs) => env.reply(
                                types::INTERCEPT_DECISION,
                                json!({"action": "take", "jobId": jobs.first().map(|j| j.id.clone()), "jobIds": jobs.iter().map(|j| j.id.clone()).collect::<Vec<_>>(), "kind": "ytdlp", "duplicate": false}),
                            ),
                            Err(e) => err(&env, &e),
                        }]);
                    }
                    vec![self.add_one(&env, add, client)]
                }
                types::DOWNLOAD_BATCH => {
                    let b: DownloadBatch = match env.payload_as() {
                        Ok(b) => b,
                        Err(e) => return Ok(vec![bad(&env, format!("DownloadBatch: {e}"))]),
                    };
                    let mut ids = vec![];
                    let mut errors = vec![];
                    for a in b.items {
                        let r = self.add_one(&env, a, client);
                        match r.payload.get("jobId").and_then(|v| v.as_str()) {
                            Some(id) => ids.push(id.to_string()),
                            None => errors.push(r.payload.clone()),
                        }
                    }
                    vec![ack(json!({"jobIds": ids, "errors": errors}))]
                }
                types::MEDIA_CANDIDATE => {
                    let mc: MediaCandidate = match env.payload_as() {
                        Ok(m) => m,
                        Err(e) => return Ok(vec![bad(&env, format!("MediaCandidate: {e}"))]),
                    };
                    let shown = self.mgr.media_candidate(&mc)?.is_some();
                    vec![ack(json!({"shown": shown}))]
                }
                types::MEDIA_PROBE => {
                    let url = p.get("url").and_then(|v| v.as_str()).unwrap_or("");
                    let ctx = RequestCtx {
                        referrer: p.get("referrer").or(p.get("page_url")).and_then(|v| v.as_str()).map(String::from),
                        cookies: p.get("cookies").and_then(|v| v.as_str()).map(String::from),
                        user_agent: p.get("user_agent").and_then(|v| v.as_str()).map(String::from),
                        headers: vec![],
                    };
                    let plan = if p.get("kind").and_then(|v| v.as_str()) == Some("ytdlp") {
                        self.mgr.probe_ytdlp(url).await?
                    } else {
                        self.mgr.probe(url, &ctx).await?
                    };
                    vec![env.reply(types::MEDIA_VARIANTS, json!({"url": url, "source": plan.source, "qualities": plan.qualities, "duration": plan.duration, "live": plan.live, "tracks": plan.tracks.len()}))]
                }
                types::CANDIDATE_LIST => {
                    let tab = p.get("tab_id").or(p.get("tabId")).and_then(|v| v.as_i64());
                    vec![env.reply(types::CANDIDATE_LIST, json!({"candidates": self.mgr.candidates(tab)?}))]
                }
                types::COOKIES_QUERY_RESULT => {
                    let jid = p.get("jobId").or(p.get("job_id")).and_then(|v| v.as_str());
                    let retried = match jid {
                        Some(j) => self.mgr.cookies_refreshed(j, p.get("cookies").and_then(|v| v.as_str()))?,
                        None => false,
                    };
                    vec![ack(json!({"retried": retried}))]
                }
                types::PROXY_INFO => {
                    self.mgr.set_browser_proxy(p)?;
                    vec![ack(json!({}))]
                }
                types::MEDIA_GEOMETRY | types::TAB_STATE => vec![ack(json!({}))],
                types::SUBSCRIBE => vec![ack(json!({"subscribed": p.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true)}))],
                types::JOB_LIST => {
                    let st = p.get("state").and_then(|v| v.as_str());
                    vec![env.reply(types::JOB_LIST, json!({"jobs": self.mgr.list(st)?}))]
                }
                types::JOB_GET => {
                    let id = id_of(p).unwrap_or_default();
                    vec![env.reply(types::JOB_GET, json!({"job": self.mgr.get(&id)?}))]
                }
                types::JOB_PAUSE => {
                    if p.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
                        vec![ack(json!({"count": self.mgr.pause_all()?}))]
                    } else {
                        self.mgr.pause(&id_of(p).unwrap_or_default())?;
                        vec![ack(json!({}))]
                    }
                }
                types::JOB_RESUME => {
                    if p.get("all").and_then(|v| v.as_bool()).unwrap_or(false) {
                        vec![ack(json!({"count": self.mgr.resume_all()?}))]
                    } else {
                        self.mgr.resume(&id_of(p).unwrap_or_default())?;
                        vec![ack(json!({}))]
                    }
                }
                types::JOB_REMOVE => {
                    if p.get("completed").and_then(|v| v.as_bool()).unwrap_or(false) {
                        vec![ack(json!({"count": self.mgr.remove_completed()?}))]
                    } else {
                        let del = p.get("deleteFiles").or(p.get("delete_files")).and_then(|v| v.as_bool()).unwrap_or(false);
                        self.mgr.remove(&id_of(p).unwrap_or_default(), del)?;
                        vec![ack(json!({}))]
                    }
                }
                types::JOB_STOP_RECORDING => {
                    self.mgr.stop_recording(&id_of(p).unwrap_or_default())?;
                    vec![ack(json!({}))]
                }
                types::JOB_SET => {
                    let id = id_of(p).unwrap_or_default();
                    vec![env.reply(types::JOB_GET, json!({"job": self.mgr.set(&id, p)?}))]
                }
                types::CONFIG_GET => vec![env.reply(types::CONFIG_GET, json!({"config": *self.mgr.config(), "path": ldm_core::paths::config_file()}))],
                types::CONFIG_SET => {
                    let patch = p.get("patch").cloned().unwrap_or_else(|| p.clone());
                    let c = self.mgr.config_set(&patch)?;
                    vec![env.reply(types::CONFIG_GET, json!({"config": *c}))]
                }
                types::QUEUE_LIST => vec![env.reply(types::QUEUE_LIST, json!({"queues": self.mgr.queues()?}))],
                types::QUEUE_START => vec![ack(json!({"resumed": self.mgr.queue_start(&id_of(p).unwrap_or_default())?}))],
                types::QUEUE_STOP => {
                    self.mgr.queue_stop(&id_of(p).unwrap_or_default())?;
                    vec![ack(json!({}))]
                }
                types::QUEUE_DELETE => vec![ack(json!({"moved": self.mgr.queue_delete(&id_of(p).unwrap_or_default())?}))],
                types::QUEUE_SET => {
                    let q: QueueRow = serde_json::from_value(p.get("queue").cloned().unwrap_or_else(|| p.clone()))?;
                    self.mgr.set_queue(q)?;
                    vec![ack(json!({}))]
                }
                types::EXPORT => {
                    let fmt = p.get("format").and_then(|v| v.as_str()).unwrap_or("json");
                    vec![env.reply(types::EXPORT, json!({"format": fmt, "data": self.mgr.export(fmt)?}))]
                }
                types::IMPORT => {
                    let data = p.get("data").and_then(|v| v.as_str()).unwrap_or("");
                    let paused = p.get("paused").and_then(|v| v.as_bool()).unwrap_or(false);
                    vec![ack(json!({"added": self.mgr.import(data, paused)?}))]
                }
                types::TOOLS_STATUS => vec![env.reply(types::TOOLS_STATUS, self.mgr.tools_status().await)],
                types::TOOLS_ACTION => {
                    let action = p.get("action").and_then(|v| v.as_str()).unwrap_or("").to_string();
                    let tool = match p.get("tool").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
                        None => None,
                        Some(t) => match ldm_core::tools::Tool::parse(t) {
                            Some(t) => Some(t),
                            None => return Ok(vec![bad(&env, format!("unknown tool {t} (yt-dlp | deno)"))]),
                        },
                    };
                    let version = p.get("version").and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
                    if p.get("wait").and_then(|v| v.as_bool()).unwrap_or(false) {
                        let r = self.mgr.tools_action(&action, tool, version.as_deref()).await?;
                        vec![ack(json!({"result": r}))]
                    } else {
                        // GUI / DBus: run in the background, results arrive as ToolsProgress / ToolsChanged
                        let mgr = self.mgr.clone();
                        tokio::spawn(async move {
                            let _ = mgr.tools_action(&action, tool, version.as_deref()).await;
                        });
                        vec![ack(json!({"started": true}))]
                    }
                }
                types::YTDLP_STATUS => vec![env.reply(types::YTDLP_STATUS, serde_json::to_value(self.mgr.ytdlp_status().await)?)],
                types::STATUS => {
                    let mut s = self.mgr.status()?;
                    let port = self.ws_port.load(Ordering::Relaxed);
                    s["wsPort"] = if port == 0 { Value::Null } else { json!(port) };
                    vec![env.reply(types::STATUS, s)]
                }
                types::SHUTDOWN => {
                    let tok = self.shutdown.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        tok.cancel();
                    });
                    vec![ack(json!({"shuttingDown": true}))]
                }
                other => vec![Envelope::error(env.id, "E_UNKNOWN_TYPE", format!("unknown message type '{other}'"))],
            })
        }
        .await;
        match r {
            Ok(v) => v,
            Err(e) => vec![err(&env, &e)],
        }
    }
}

/// WebSocket bridge backend.
pub struct WsBackend(pub Arc<Daemon>);

#[async_trait]
impl Backend for WsBackend {
    async fn handle(&self, env: Envelope, _client: &ClientInfo) -> Vec<Envelope> {
        self.0.dispatch(env, "extension").await
    }
    fn subscribe(&self) -> broadcast::Receiver<Envelope> {
        self.0.events.subscribe()
    }
}
