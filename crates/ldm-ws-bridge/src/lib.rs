//! Optional localhost WebSocket bridge.
//!
//! * `ldm.bridge.v1` – the LDM JSON envelope protocol, one envelope per text
//!   message (also used when the client offers no subprotocol).
//!
//! Security: bind to loopback only, refuse web-page origins
//! (`http(s)://…`), optional shared token, bounded message size.
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use ldm_ipc::messages::types;
use ldm_ipc::{Envelope, MAX_MESSAGE_BYTES, WS_SUBPROTOCOL};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::{HeaderValue, StatusCode};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Who is talking to the backend.
#[derive(Debug, Clone)]
pub struct ClientInfo {
    /// `ws-json`
    pub transport: &'static str,
    pub origin: Option<String>,
    pub peer: Option<SocketAddr>,
    pub conn_id: u64,
}

/// The daemon side of the bridge.
#[async_trait]
pub trait Backend: Send + Sync + 'static {
    /// Handle one request envelope and return the replies (in order).
    async fn handle(&self, env: Envelope, client: &ClientInfo) -> Vec<Envelope>;
    /// Unsolicited daemon events (progress, rule pushes, media buttons …).
    fn subscribe(&self) -> broadcast::Receiver<Envelope>;
}

#[derive(Debug, Clone)]
pub struct BridgeConfig {
    pub host: String,
    pub port: u16,
    /// How many successive ports to try if `port` is busy.
    pub port_fallback: u16,
    pub token: Option<String>,
}

impl Default for BridgeConfig {
    fn default() -> Self {
        Self { host: "127.0.0.1".into(), port: 17001, port_fallback: 10, token: None }
    }
}

/// Bind the listener (loopback only), trying fallback ports.
pub async fn bind(cfg: &BridgeConfig) -> std::io::Result<(TcpListener, u16)> {
    let ip: std::net::IpAddr = cfg.host.parse().map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "bad host"))?;
    if !ip.is_loopback() {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied, "the bridge only binds to loopback addresses"));
    }
    let mut last = None;
    for i in 0..=cfg.port_fallback {
        let port = if cfg.port == 0 { 0 } else { cfg.port.saturating_add(i) };
        match TcpListener::bind((ip, port)).await {
            Ok(l) => {
                let p = l.local_addr()?.port();
                return Ok((l, p));
            }
            Err(e) => last = Some(e),
        }
        if cfg.port == 0 {
            break;
        }
    }
    Err(last.unwrap_or_else(|| std::io::Error::other("bind failed")))
}

/// Accept loop.
pub async fn serve(listener: TcpListener, cfg: Arc<BridgeConfig>, backend: Arc<dyn Backend>, shutdown: CancellationToken) {
    let ids = Arc::new(AtomicU64::new(1));
    loop {
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r { Ok(v) => v, Err(e) => { warn!(err = %e, "accept failed"); continue; } },
            _ = shutdown.cancelled() => break,
        };
        let (cfg, backend, shutdown) = (cfg.clone(), backend.clone(), shutdown.clone());
        let conn_id = ids.fetch_add(1, Ordering::Relaxed);
        tokio::spawn(async move {
            if let Err(e) = connection(stream, peer, conn_id, cfg, backend, shutdown).await {
                debug!(conn = conn_id, err = %e, "ws connection ended");
            }
        });
    }
}

fn reject(code: StatusCode, why: &str) -> ErrorResponse {
    let mut r = ErrorResponse::new(Some(why.to_string()));
    *r.status_mut() = code;
    r
}

fn query_token(uri: &str) -> Option<String> {
    let q = uri.split_once('?')?.1;
    q.split('&').find_map(|kv| kv.strip_prefix("token=").map(String::from))
}

/// Origin policy: browser extensions or non-browser clients only.
pub fn origin_allowed(origin: Option<&str>) -> bool {
    match origin {
        None => true,
        Some(o) => {
            let o = o.trim().to_ascii_lowercase();
            o == "null" || o.starts_with("chrome-extension://") || o.starts_with("moz-extension://") || o.starts_with("extension://")
        }
    }
}

async fn connection(
    stream: TcpStream,
    peer: SocketAddr,
    conn_id: u64,
    cfg: Arc<BridgeConfig>,
    backend: Arc<dyn Backend>,
    shutdown: CancellationToken,
) -> anyhow::Result<()> {
    let decided: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let d2 = decided.clone();
    let cfg2 = cfg.clone();
    // the error type is fixed by tungstenite's handshake callback signature
    #[allow(clippy::result_large_err)]
    let callback = move |req: &Request, mut resp: Response| -> Result<Response, ErrorResponse> {
        let origin = req.headers().get("origin").and_then(|v| v.to_str().ok()).map(String::from);
        if !origin_allowed(origin.as_deref()) {
            return Err(reject(StatusCode::FORBIDDEN, "origin not allowed"));
        }
        if let Some(tok) = cfg2.token.as_deref().filter(|t| !t.is_empty()) {
            let given = req
                .headers()
                .get("x-ldm-token")
                .and_then(|v| v.to_str().ok())
                .map(String::from)
                .or_else(|| query_token(&req.uri().to_string()));
            if given.as_deref() != Some(tok) {
                return Err(reject(StatusCode::UNAUTHORIZED, "token required"));
            }
        }
        let offered: Vec<String> = req
            .headers()
            .get_all("sec-websocket-protocol")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(',').map(|s| s.trim().to_string()))
            .filter(|s| !s.is_empty())
            .collect();
        if !offered.is_empty() {
            if offered.iter().any(|p| p.as_str() == WS_SUBPROTOCOL) {
                resp.headers_mut().insert("sec-websocket-protocol", HeaderValue::from_static(WS_SUBPROTOCOL));
            } else {
                return Err(reject(StatusCode::BAD_REQUEST, "unsupported subprotocol"));
            }
        }
        *d2.lock().unwrap() = origin;
        Ok(resp)
    };
    let wsc = WebSocketConfig { max_message_size: Some(MAX_MESSAGE_BYTES), max_frame_size: Some(MAX_MESSAGE_BYTES), ..Default::default() };
    let ws = tokio_tungstenite::accept_hdr_async_with_config(stream, callback, Some(wsc)).await?;
    let origin = decided.lock().unwrap().clone();
    info!(conn = conn_id, origin = origin.as_deref().unwrap_or("-"), "ws client connected");
    let (mut tx, mut rx) = ws.split();
    let mut events = backend.subscribe();
    let mut subscribed = false;
    let info = ClientInfo { transport: "ws-json", origin, peer: Some(peer), conn_id };
    loop {
        tokio::select! {
            m = rx.next() => {
                let Some(m) = m else { break };
                let text = match m? {
                    Message::Text(t) => t,
                    Message::Binary(b) => String::from_utf8_lossy(&b).into_owned(),
                    Message::Close(_) => break,
                    _ => continue,
                };
                let mut out: Vec<Message> = Vec::new();
                    match Envelope::parse(text.as_bytes()) {
                        Ok(env) => {
                            if env.kind == types::SUBSCRIBE {
                                subscribed = env.payload.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                            }
                            for r in backend.handle(env, &info).await {
                                out.push(Message::Text(r.to_json()));
                            }
                        }
                        Err(e) => out.push(Message::Text(Envelope::error(0, "E_FORMAT", e.to_string()).to_json())),
                    }
                for m in out {
                    tx.send(m).await?;
                }
            }
            ev = events.recv() => {
                let ev = match ev {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                };
                let always = matches!(ev.kind.as_str(), types::RULESET_PUSH | types::SHOW_MEDIA_BUTTON | types::HIDE_MEDIA_BUTTON | types::NOTIFY | types::GET_COOKIES);
                if always || subscribed {
                    tx.send(Message::Text(ev.to_json())).await?;
                }
            }
            _ = shutdown.cancelled() => {
                let _ = tx.send(Message::Close(None)).await;
                break;
            }
        }
    }
    info!(conn = conn_id, "ws client disconnected");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        assert!(origin_allowed(None));
        assert!(origin_allowed(Some("chrome-extension://abcdef")));
        assert!(origin_allowed(Some("moz-extension://x")));
        assert!(!origin_allowed(Some("https://evil.example")));
        assert!(!origin_allowed(Some("http://localhost:8080")));
    }

    #[test]
    fn token_query() {
        assert_eq!(query_token("/?a=1&token=xyz"), Some("xyz".into()));
        assert_eq!(query_token("/"), None);
    }
}
