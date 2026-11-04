//! Minimal HTTP/1.1 static file server for LDM tests.
//!
//! Behaviour is selected per request through query parameters:
//! `norange=1` ignore Range; `fakerange=1` honour only the `bytes=0-0` probe;
//! `rate=N` throttle each response to N bytes/s; `slowrest=N` throttle every
//! ranged response that does not start at 0; `fail=N` answer the first N
//! requests with 503; `drop=N&dropcount=M` cut the first M ranged responses
//! after N bytes; `etag=X`; `cd=NAME` Content-Disposition; `status=CODE`;
//! `nolength=1` omit Content-Length; `rewrite=FROM|TO` substitute text;
//! `needcookie=k=v`, `needref=SUBSTR`, `needua=SUBSTR`, `needauth=user:pass` (Basic) access checks.
//! `live=ID` on a VOD `.m3u8`: serve it as a growing live playlist (no ENDLIST) – the
//! first request for (path, ID) starts the clock, then `2 + elapsed / TARGETDURATION`
//! segments are published; `livewin=N` (default 3) is the sliding window size.
//! A request carrying `If-None-Match` equal to the file's ETag (default `"<len hex>"`, or the
//! content of a sibling `X.etag`) gets `304 Not Modified`; a
//! missing file `X` with a sibling `X.redirect` answers `302` to the URL stored in it.
//! `GET /__stats` returns JSON statistics, `GET /__reset` clears them.
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default, Debug)]
pub struct Stats {
    pub requests: Vec<serde_json::Value>,
    pub counters: HashMap<String, u64>,
    pub active_per_path: HashMap<String, i64>,
    pub max_active_per_path: HashMap<String, i64>,
    pub max_active: i64,
    /// `live=ID`: first request time per (path, ID).
    pub live_started: HashMap<String, Instant>,
}

/// Turn a VOD media playlist into the live window published `elapsed` after the start.
pub fn live_window(vod: &str, elapsed: Duration, win: usize) -> String {
    let mut head = vec![];
    let mut segs: Vec<Vec<&str>> = vec![];
    let mut cur: Vec<&str> = vec![];
    let mut target = 1.0f64;
    for l in vod.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(t) = l.strip_prefix("#EXT-X-TARGETDURATION:") {
            target = t.trim().parse().unwrap_or(1.0f64).max(0.1);
        }
        if l.starts_with("#EXT-X-ENDLIST") || l.starts_with("#EXT-X-MEDIA-SEQUENCE") || l.starts_with("#EXT-X-PLAYLIST-TYPE") {
            continue;
        }
        if l.starts_with("#EXTINF") || (!cur.is_empty() && l.starts_with('#')) {
            cur.push(l);
        } else if !l.starts_with('#') {
            cur.push(l);
            segs.push(std::mem::take(&mut cur));
        } else if segs.is_empty() {
            head.push(l);
        }
    }
    let n = (2 + (elapsed.as_secs_f64() / target) as usize).min(segs.len());
    let from = n.saturating_sub(win.max(1));
    let mut out = String::new();
    for h in head {
        out.push_str(h);
        out.push('\n');
    }
    out.push_str(&format!("#EXT-X-MEDIA-SEQUENCE:{from}\n"));
    for sg in &segs[from..n] {
        for l in sg {
            out.push_str(l);
            out.push('\n');
        }
    }
    out
}

pub struct Server {
    pub addr: SocketAddr,
    pub stats: Arc<Mutex<Stats>>,
    active: Arc<AtomicI64>,
}

impl Server {
    pub fn url(&self, path: &str) -> String {
        format!("http://{}{}", self.addr, path)
    }
    pub fn stats_json(&self) -> serde_json::Value {
        stats_to_json(&self.stats.lock().unwrap())
    }
    pub fn reset(&self) {
        *self.stats.lock().unwrap() = Stats::default();
    }
    /// Ranges requested for a path (`None` = no Range header).
    pub fn ranges_for(&self, path: &str) -> Vec<Option<String>> {
        self.stats
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|r| r["path"] == path && r["method"] == "GET")
            .map(|r| r["range"].as_str().map(String::from))
            .collect()
    }
    pub fn max_active_for(&self, path: &str) -> i64 {
        *self.stats.lock().unwrap().max_active_per_path.get(path).unwrap_or(&0)
    }
    pub fn active_now(&self) -> i64 {
        self.active.load(Ordering::SeqCst)
    }
}

fn stats_to_json(s: &Stats) -> serde_json::Value {
    serde_json::json!({
        "requests": s.requests,
        "max_active": s.max_active,
        "max_active_per_path": s.max_active_per_path,
        "counters": s.counters,
    })
}

pub async fn start(root: PathBuf, addr: &str) -> std::io::Result<Server> {
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let stats = Arc::new(Mutex::new(Stats::default()));
    let active = Arc::new(AtomicI64::new(0));
    let (s2, a2) = (stats.clone(), active.clone());
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { continue };
            let (root, stats, active) = (root.clone(), s2.clone(), a2.clone());
            tokio::spawn(async move {
                let _ = handle(sock, &root, &stats, &active).await;
            });
        }
    });
    Ok(Server { addr, stats, active })
}

fn qparams(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter(|s| !s.is_empty())
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, "1"));
            (k.to_string(), pct_decode(v))
        })
        .collect()
}

fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_range(h: &str, len: u64) -> Option<(u64, u64)> {
    let r = h.trim().strip_prefix("bytes=")?;
    let (a, b) = r.split_once('-')?;
    if a.is_empty() {
        let n: u64 = b.parse().ok()?;
        return Some((len.saturating_sub(n), len.saturating_sub(1)));
    }
    let a: u64 = a.parse().ok()?;
    let b: u64 = if b.is_empty() { len.saturating_sub(1) } else { b.parse::<u64>().ok()?.min(len.saturating_sub(1)) };
    if a > b || a >= len {
        return None;
    }
    Some((a, b))
}

struct ActiveGuard {
    active: Arc<AtomicI64>,
    stats: Arc<Mutex<Stats>>,
    path: String,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
        if let Some(v) = self.stats.lock().unwrap().active_per_path.get_mut(&self.path) {
            *v -= 1;
        }
    }
}

async fn handle(mut sock: TcpStream, root: &Path, stats: &Arc<Mutex<Stats>>, active: &Arc<AtomicI64>) -> std::io::Result<()> {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.windows(4).any(|w| w == b"\r\n\r\n") || buf.len() > 65536 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buf).into_owned();
    let mut lines = text.split("\r\n");
    let reqline = lines.next().unwrap_or("");
    let mut parts = reqline.split_whitespace();
    let method = parts.next().unwrap_or("GET").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let mut headers: HashMap<String, String> = HashMap::new();
    for l in lines {
        if l.is_empty() {
            break;
        }
        if let Some((k, v)) = l.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let path = pct_decode(path);
    let q = qparams(query);
    let range_h = headers.get("range").cloned();
    let key = format!("{path}?{query}");

    if path == "/__stats" {
        let body = stats_to_json(&stats.lock().unwrap()).to_string();
        return respond(&mut sock, 200, &[("Content-Type", "application/json".into())], body.as_bytes()).await;
    }
    if path == "/__reset" {
        *stats.lock().unwrap() = Stats::default();
        return respond(&mut sock, 200, &[], b"ok").await;
    }

    let nth = {
        let mut s = stats.lock().unwrap();
        s.requests.push(serde_json::json!({
            "method": method, "path": path, "query": query, "range": range_h,
            "ua": headers.get("user-agent"), "referer": headers.get("referer"),
            "cookie": headers.get("cookie"), "accept_encoding": headers.get("accept-encoding"),
            "if_none_match": headers.get("if-none-match"),
        }));
        let c = s.counters.entry(key.clone()).or_insert(0);
        *c += 1;
        *c
    };
    let now_active = active.fetch_add(1, Ordering::SeqCst) + 1;
    {
        let mut s = stats.lock().unwrap();
        s.max_active = s.max_active.max(now_active);
        let pa = {
            let e = s.active_per_path.entry(path.clone()).or_insert(0);
            *e += 1;
            *e
        };
        let m = s.max_active_per_path.entry(path.clone()).or_insert(0);
        *m = (*m).max(pa);
    }
    let _guard = ActiveGuard { active: active.clone(), stats: stats.clone(), path: path.clone() };

    if let Some(code) = q.get("status").and_then(|s| s.parse::<u16>().ok()) {
        return respond(&mut sock, code, &[], b"forced status").await;
    }
    if let Some(n) = q.get("fail").and_then(|s| s.parse::<u64>().ok()) {
        if nth <= n {
            return respond(&mut sock, 503, &[], b"try later").await;
        }
    }
    if let Some(c) = q.get("needcookie") {
        if !headers.get("cookie").map(|v| v.contains(c.as_str())).unwrap_or(false) {
            return respond(&mut sock, 403, &[], b"cookie required").await;
        }
    }
    if let Some(r) = q.get("needref") {
        if !headers.get("referer").map(|v| v.contains(r.as_str())).unwrap_or(false) {
            return respond(&mut sock, 403, &[], b"referer required").await;
        }
    }
    if let Some(cred) = q.get("needauth") {
        use base64::Engine as _;
        let want = format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(cred));
        if headers.get("authorization") != Some(&want) {
            return respond(&mut sock, 401, &[("WWW-Authenticate", "Basic realm=\"ldm-test\"".into())], b"login required").await;
        }
    }
    if let Some(u) = q.get("needua") {
        if !headers.get("user-agent").map(|v| v.contains(u.as_str())).unwrap_or(false) {
            return respond(&mut sock, 403, &[], b"user-agent required").await;
        }
    }

    let rel = path.trim_start_matches('/');
    if rel.split('/').any(|c| c == "..") {
        return respond(&mut sock, 400, &[], b"bad path").await;
    }
    let file = root.join(rel);
    let mut data = match tokio::fs::read(&file).await {
        Ok(d) => d,
        Err(_) => {
            let mut redir = file.clone().into_os_string();
            redir.push(".redirect");
            if let Ok(to) = tokio::fs::read_to_string(&redir).await {
                return respond(&mut sock, 302, &[("Location", to.trim().to_string())], b"").await;
            }
            return respond(&mut sock, 404, &[], b"not found").await;
        }
    };
    if let Some(rw) = q.get("rewrite") {
        if let Some((a, b)) = rw.split_once('|') {
            data = String::from_utf8_lossy(&data).replace(a, b).into_bytes();
        }
    }
    if let (Some(id), true) = (q.get("live"), rel.ends_with(".m3u8")) {
        let t0 = *stats.lock().unwrap().live_started.entry(format!("{path}#{id}")).or_insert_with(Instant::now);
        let win = q.get("livewin").and_then(|w| w.parse().ok()).unwrap_or(3);
        data = live_window(&String::from_utf8_lossy(&data), t0.elapsed(), win).into_bytes();
    }
    let len = data.len() as u64;
    let ctype = match file.extension().and_then(|e| e.to_str()).unwrap_or("") {
        "m3u8" => "application/vnd.apple.mpegurl",
        "mpd" => "application/dash+xml",
        "mp4" => "video/mp4",
        "ts" => "video/mp2t",
        "m4s" => "video/iso.segment",
        "html" => "text/html",
        "key" => "application/octet-stream",
        _ => "application/octet-stream",
    };
    // `X.etag` sidecar: the file's ETag (like GitHub's API, which changes it with the content)
    let mut side = file.clone().into_os_string();
    side.push(".etag");
    let side_etag = std::fs::read_to_string(&side).ok().map(|t| t.trim().to_string());
    let etag = q.get("etag").cloned().or(side_etag).unwrap_or_else(|| format!("\"{len:x}\""));
    if range_h.is_none() && headers.get("if-none-match").is_some_and(|v| v.trim() == etag) {
        return respond(&mut sock, 304, &[("ETag", etag)], b"").await;
    }
    let mut hdrs: Vec<(&str, String)> = vec![("Content-Type", ctype.into()), ("ETag", etag)];
    if let Some(cd) = q.get("cd") {
        hdrs.push(("Content-Disposition", format!("attachment; filename=\"{cd}\"")));
    }
    let norange = q.contains_key("norange");
    let fakerange = q.contains_key("fakerange");
    let mut status = 200;
    let (mut a, mut b) = (0u64, len.saturating_sub(1));
    if let Some(rh) = &range_h {
        let honour = !norange && (!fakerange || rh.trim() == "bytes=0-0");
        if honour {
            match parse_range(rh, len) {
                Some((x, y)) => {
                    status = 206;
                    a = x;
                    b = y;
                    hdrs.push(("Content-Range", format!("bytes {a}-{b}/{len}")));
                }
                None => {
                    hdrs.push(("Content-Range", format!("bytes */{len}")));
                    return respond(&mut sock, 416, &hdrs, b"").await;
                }
            }
        }
    }
    if !norange {
        hdrs.push(("Accept-Ranges", "bytes".into()));
    }
    let body: &[u8] = if len == 0 { &[] } else { &data[a as usize..=b as usize] };
    let nolength = q.contains_key("nolength") && status == 200;
    if !nolength {
        hdrs.push(("Content-Length", body.len().to_string()));
    }
    write_head(&mut sock, status, &hdrs).await?;
    if method == "HEAD" {
        return Ok(());
    }
    let mut rate: Option<u64> = q.get("rate").and_then(|s| s.parse().ok());
    if a > 0 {
        if let Some(r) = q.get("slowrest").and_then(|s| s.parse().ok()) {
            rate = Some(r);
        }
    }
    let drop_after: Option<usize> =
        match (q.get("drop").and_then(|s| s.parse().ok()), q.get("dropcount").and_then(|s| s.parse::<u64>().ok())) {
            (Some(n), Some(m)) if range_h.is_some() && rangecount(stats, &key) <= m => Some(n),
            (Some(n), None) if range_h.is_some() && rangecount(stats, &key) <= 1 => Some(n),
            _ => None,
        };
    let started = Instant::now();
    let mut sent = 0usize;
    let chunk = 16 * 1024;
    while sent < body.len() {
        let end = (sent + chunk).min(body.len());
        let end = match drop_after {
            Some(d) if end > d => d,
            _ => end,
        };
        if end <= sent {
            break;
        }
        sock.write_all(&body[sent..end]).await?;
        *stats.lock().unwrap().counters.entry(format!("bytes:{path}")).or_insert(0) += (end - sent) as u64;
        sent = end;
        if drop_after.map(|d| sent >= d).unwrap_or(false) {
            return Ok(()); // abrupt close
        }
        if let (Some(r), true) = (rate, sent < body.len()) {
            let should = Duration::from_secs_f64(sent as f64 / r as f64);
            let el = started.elapsed();
            if should > el {
                tokio::time::sleep(should - el).await;
            }
        }
    }
    sock.flush().await?;
    Ok(())
}

fn rangecount(stats: &Arc<Mutex<Stats>>, key: &str) -> u64 {
    let s = stats.lock().unwrap();
    s.requests
        .iter()
        .filter(|r| {
            format!("{}?{}", r["path"].as_str().unwrap_or(""), r["query"].as_str().unwrap_or("")) == key
                && r["range"].is_string()
                && r["range"] != "bytes=0-0"
        })
        .count() as u64
}

async fn write_head(sock: &mut TcpStream, status: u16, hdrs: &[(&str, String)]) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        206 => "Partial Content",
        403 => "Forbidden",
        404 => "Not Found",
        416 => "Range Not Satisfiable",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let mut h = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\n");
    for (k, v) in hdrs {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    sock.write_all(h.as_bytes()).await
}

async fn respond(sock: &mut TcpStream, status: u16, hdrs: &[(&str, String)], body: &[u8]) -> std::io::Result<()> {
    let mut h: Vec<(&str, String)> = hdrs.to_vec();
    h.push(("Content-Length", body.len().to_string()));
    write_head(sock, status, &h).await?;
    sock.write_all(body).await?;
    sock.flush().await
}
