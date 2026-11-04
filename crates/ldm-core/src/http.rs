//! HTTP transport: request context, Range probe, ranged GET.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::{header, Client, Method, RequestBuilder, Response, StatusCode};

use crate::config::EngineConfig;
use crate::error::{LdmError, Result};
use crate::util::{filename, security};
use tracing::warn;

/// Browser session context forwarded with each request.
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RequestCtx {
    pub referrer: Option<String>,
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct Probe {
    pub status: u16,
    pub total: Option<u64>,
    pub range_ok: bool,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub final_url: String,
    pub filename: Option<String>,
    pub mime: Option<String>,
}

#[derive(Clone)]
pub struct HttpClient {
    client: Client,
    cfg: Arc<EngineConfig>,
    pac: Option<Arc<PacRouter>>,
}

fn hval(resp: &Response, name: header::HeaderName) -> Option<String> {
    resp.headers().get(name).and_then(|v| v.to_str().ok()).map(|s| s.to_string())
}

/// Parse `bytes a-b/total` → (a, b, total?)
pub fn parse_content_range(v: &str) -> Option<(u64, u64, Option<u64>)> {
    let v = v.trim();
    let rest = v.strip_prefix("bytes").unwrap_or(v).trim();
    let (range, total) = rest.split_once('/')?;
    let (a, b) = range.trim().split_once('-')?;
    let total = match total.trim() {
        "*" => None,
        t => Some(t.parse().ok()?),
    };
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, total))
}

/// PAC mode: one client per route ("DIRECT" or a proxy URL), chosen per request from the full URL.
struct PacRouter {
    pac: Arc<crate::pac::Pac>,
    cfg: EngineConfig,
    routes: Mutex<HashMap<String, Client>>,
    /// route key -> origins requests were routed through it for (redirect targets elsewhere are
    /// re-evaluated by the client's proxy closure at origin level, as reqwest only passes the
    /// connection target to it)
    origins: Arc<Mutex<HashMap<String, HashSet<String>>>>,
}

fn origin_of(u: &url::Url) -> String {
    format!("{}://{}:{}", u.scheme(), u.host_str().unwrap_or(""), u.port_or_known_default().unwrap_or(0))
}

impl PacRouter {
    fn client_for(&self, url: &str) -> Client {
        let Ok(u) = url::Url::parse(url) else { return self.route(None) };
        let choice = self.pac.select(&u);
        let key = choice.as_ref().map(|p| p.to_string()).unwrap_or_else(|| "DIRECT".into());
        {
            let mut o = self.origins.lock().unwrap();
            let set = o.entry(key).or_default();
            if set.len() > 4096 {
                set.clear();
            }
            set.insert(origin_of(&u));
        }
        self.route(choice)
    }

    fn route(&self, choice: Option<url::Url>) -> Client {
        let key = choice.as_ref().map(|p| p.to_string()).unwrap_or_else(|| "DIRECT".into());
        if let Some(c) = self.routes.lock().unwrap().get(&key) {
            return c.clone();
        }
        let (pac, origins, k) = (self.pac.clone(), self.origins.clone(), key.clone());
        let proxy = reqwest::Proxy::custom(move |dst| {
            let own = origins.lock().unwrap().get(&k).map(|s| s.contains(&origin_of(dst))).unwrap_or(false);
            if own {
                choice.clone()
            } else {
                pac.select(dst)
            }
        });
        let c = match base_builder(&self.cfg).proxy(proxy).build() {
            Ok(c) => c,
            Err(e) => {
                warn!(error = %e, "PAC route client failed; connecting DIRECT");
                base_builder(&self.cfg).no_proxy().build().unwrap_or_default()
            }
        };
        self.routes.lock().unwrap().insert(key, c.clone());
        c
    }
}

fn base_builder(cfg: &EngineConfig) -> reqwest::ClientBuilder {
    let block = cfg.block_link_local;
    let max_redirects = cfg.max_redirects;
    let policy = reqwest::redirect::Policy::custom(move |attempt| {
        if attempt.previous().len() >= max_redirects {
            attempt.error("too many redirects")
        } else if !matches!(attempt.url().scheme(), "http" | "https") {
            attempt.error("redirect to non-http scheme")
        } else if block && attempt.url().host().map(|h| security::host_blocked(&h)).unwrap_or(false) {
            attempt.error("redirect to blocked address")
        } else {
            attempt.follow()
        }
    });
    Client::builder()
        .connect_timeout(Duration::from_secs(cfg.connect_timeout_secs))
        .read_timeout(Duration::from_secs(cfg.timeout_secs))
        .redirect(policy)
        .user_agent(cfg.user_agent.clone())
        .pool_max_idle_per_host(32)
}

impl HttpClient {
    pub fn new(cfg: &EngineConfig) -> Result<Self> {
        let mut b = base_builder(cfg);
        let mut pac = None;
        match cfg.proxy.as_str() {
            "" | "system" => {}
            "none" => b = b.no_proxy(),
            "pac" => {
                if cfg.pac_url.trim().is_empty() {
                    return Err(LdmError::Other("proxy = \"pac\" needs engine.pac_url".into()));
                }
                let wait = Duration::from_secs(cfg.connect_timeout_secs.clamp(1, 15));
                let p = crate::pac::Pac::start(&cfg.pac_url, Duration::from_secs(cfg.pac_refresh_secs), wait)?;
                let sel = p.clone();
                // fallback client (origin-level decision); request() routes by the full URL
                b = b.proxy(reqwest::Proxy::custom(move |u| sel.select(u)));
                pac = Some(Arc::new(PacRouter {
                    pac: p,
                    cfg: cfg.clone(),
                    routes: Mutex::new(HashMap::new()),
                    origins: Arc::new(Mutex::new(HashMap::new())),
                }));
            }
            p => b = b.proxy(reqwest::Proxy::all(p).map_err(|e| LdmError::Other(format!("proxy: {e}")))?),
        }
        let client = b.build().map_err(|e| LdmError::Other(e.to_string()))?;
        Ok(Self { client, cfg: Arc::new(cfg.clone()), pac })
    }

    /// The PAC runtime when `proxy = "pac"`.
    pub fn pac(&self) -> Option<&Arc<crate::pac::Pac>> {
        self.pac.as_ref().map(|r| &r.pac)
    }

    pub fn config(&self) -> &EngineConfig {
        &self.cfg
    }

    pub fn request(&self, method: Method, url: &str, ctx: &RequestCtx) -> RequestBuilder {
        let mut rb = match &self.pac {
            Some(r) => r.client_for(url).request(method, url),
            None => self.client.request(method, url),
        };
        if let Some(ua) = ctx.user_agent.as_deref().filter(|s| !s.is_empty()) {
            rb = rb.header(header::USER_AGENT, ua);
        }
        if let Some(r) = ctx.referrer.as_deref().filter(|s| !s.is_empty()) {
            rb = rb.header(header::REFERER, r);
        }
        if let Some(c) = ctx.cookies.as_deref().filter(|s| !s.is_empty()) {
            rb = rb.header(header::COOKIE, c);
        }
        for (k, v) in &ctx.headers {
            let lk = k.to_ascii_lowercase();
            // never let a forwarded header break the ranged transfer
            if matches!(lk.as_str(), "range" | "accept-encoding" | "host" | "content-length" | "connection") {
                continue;
            }
            rb = rb.header(k.as_str(), v.as_str());
        }
        // identity encoding keeps byte offsets meaningful
        rb.header(header::ACCEPT_ENCODING, "identity")
    }

    /// Range probe: `GET Range: bytes=0-0`. A 200 answer means Range is
    /// ignored; the body is *not* read (prototype lesson).
    pub async fn probe(&self, url: &str, ctx: &RequestCtx) -> Result<Probe> {
        let resp = self.request(Method::GET, url, ctx).header(header::RANGE, "bytes=0-0").send().await?;
        let status = resp.status();
        let mut p = Probe {
            status: status.as_u16(),
            etag: hval(&resp, header::ETAG),
            last_modified: hval(&resp, header::LAST_MODIFIED),
            final_url: resp.url().to_string(),
            filename: hval(&resp, header::CONTENT_DISPOSITION).and_then(|v| filename::from_content_disposition(&v)),
            mime: hval(&resp, header::CONTENT_TYPE).map(|m| m.split(';').next().unwrap_or("").trim().to_string()),
            ..Default::default()
        };
        match status {
            StatusCode::PARTIAL_CONTENT => {
                let cr = hval(&resp, header::CONTENT_RANGE).and_then(|v| parse_content_range(&v));
                match cr {
                    Some((0, _, total)) => {
                        p.total = total;
                        p.range_ok = true;
                    }
                    _ => {
                        p.range_ok = false;
                    }
                }
                drop(resp);
            }
            StatusCode::OK => {
                p.range_ok = false;
                p.total = resp.content_length();
                drop(resp); // do not download the whole body as a "probe"
                if p.total.is_none() {
                    if let Ok(h) = self.request(Method::HEAD, url, ctx).send().await {
                        if h.status().is_success() {
                            p.total = h.content_length().filter(|n| *n > 0);
                        }
                    }
                }
            }
            StatusCode::RANGE_NOT_SATISFIABLE => {
                // typically an empty resource
                p.total =
                    hval(&resp, header::CONTENT_RANGE).and_then(|v| v.rsplit('/').next().and_then(|t| t.trim().parse().ok())).or(Some(0));
                p.range_ok = false;
            }
            s => {
                return Err(LdmError::HttpStatus(s.as_u16(), s.canonical_reason().unwrap_or("").to_string()));
            }
        }
        Ok(p)
    }

    /// GET with `Range: bytes=from-to` (inclusive `to`), or open-ended.
    pub async fn get_range(&self, url: &str, ctx: &RequestCtx, from: u64, to: Option<u64>) -> Result<Response> {
        let mut rb = self.request(Method::GET, url, ctx);
        if from > 0 || to.is_some() {
            let r = match to {
                Some(t) => format!("bytes={from}-{t}"),
                None => format!("bytes={from}-"),
            };
            rb = rb.header(header::RANGE, r);
        }
        let resp = rb.send().await?;
        let st = resp.status();
        if st.is_success() {
            Ok(resp)
        } else {
            Err(LdmError::HttpStatus(st.as_u16(), st.canonical_reason().unwrap_or("").to_string()))
        }
    }

    /// Fetch a small resource fully (manifests, keys, media segments).
    pub async fn get_bytes(&self, url: &str, ctx: &RequestCtx, range: Option<(u64, u64)>, limit: usize) -> Result<(Vec<u8>, String)> {
        let resp = match range {
            Some((a, b)) => self.get_range(url, ctx, a, Some(b)).await?,
            None => self.get_range(url, ctx, 0, None).await?,
        };
        if range.is_some() && resp.status() == StatusCode::OK {
            return Err(LdmError::RangeIgnored);
        }
        let final_url = resp.url().to_string();
        if let Some(n) = resp.content_length() {
            if n as usize > limit {
                return Err(LdmError::Format(format!("resource too large ({n} bytes)")));
            }
        }
        let body = resp.bytes().await?;
        if body.len() > limit {
            return Err(LdmError::Format("resource too large".into()));
        }
        Ok((body.to_vec(), final_url))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn content_range() {
        assert_eq!(parse_content_range("bytes 0-0/22297"), Some((0, 0, Some(22297))));
        assert_eq!(parse_content_range("bytes 10-19/*"), Some((10, 19, None)));
        assert_eq!(parse_content_range("garbage"), None);
    }
}
