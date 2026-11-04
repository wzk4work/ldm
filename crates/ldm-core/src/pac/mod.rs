//! Proxy auto-config (PAC): fetch / read a PAC script, evaluate `FindProxyForURL` in a small
//! sandboxed interpreter ([`js`]), and pick a proxy per request with fallback order (own code).
//!
//! * Source: `http(s)://` URL (fetched without any proxy), `file://` URL or absolute path.
//! * Refresh: on an interval (`engine.pac_refresh_secs`) and early after a failure (script error,
//!   unreachable proxy, failed fetch). A failed refresh keeps the last good script.
//! * Result `"PROXY a:1; SOCKS5 b:2; DIRECT"` → entries tried in order; a proxy whose TCP port does
//!   not answer is skipped (health cached briefly), so the next entry is used.
//! * No script yet / script error → DIRECT (what browsers do), logged.
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, ToSocketAddrs, UdpSocket};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{Datelike, NaiveDateTime, Timelike};
use tracing::{debug, info, warn};

use crate::error::{LdmError, Result};

pub mod js;
use js::{JsResult, Val};

const MAX_PAC_BYTES: usize = 1 << 20;
const RESULT_TTL: Duration = Duration::from_secs(60);
const RESULT_CAP: usize = 2048;
const HEALTH_OK_TTL: Duration = Duration::from_secs(30);
const HEALTH_BAD_TTL: Duration = Duration::from_secs(10);
const HEALTH_TIMEOUT: Duration = Duration::from_millis(1500);
const DNS_TTL: Duration = Duration::from_secs(300);
const DNS_NEG_TTL: Duration = Duration::from_secs(30);

/// One entry of a `FindProxyForURL` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entry {
    Direct,
    /// Proxy URL usable by reqwest (`http://`, `https://`, `socks4a://`, `socks5h://`).
    Proxy(url::Url),
}

/// Parse `"PROXY a:1; HTTPS b:2; SOCKS5 c:3; SOCKS d:4; DIRECT"`. Unknown entries are skipped.
pub fn parse_result(s: &str) -> Vec<Entry> {
    let mut out = vec![];
    for part in s.split(';') {
        let mut it = part.split_whitespace();
        let Some(kind) = it.next() else { continue };
        let kind = kind.to_ascii_uppercase();
        if kind == "DIRECT" {
            out.push(Entry::Direct);
            continue;
        }
        let Some(addr) = it.next() else { continue };
        let scheme = match kind.as_str() {
            "PROXY" | "HTTP" => "http",
            "HTTPS" => "https",
            // remote DNS through SOCKS, as browsers do for SOCKS5; SOCKS/SOCKS4 = v4(a)
            "SOCKS5" => "socks5h",
            "SOCKS" | "SOCKS4" => "socks4a",
            _ => continue,
        };
        if let Ok(u) = url::Url::parse(&format!("{scheme}://{addr}")) {
            if u.host_str().is_some() && matches!(u.path(), "" | "/") && u.query().is_none() && u.username().is_empty() {
                out.push(Entry::Proxy(u));
            }
        }
    }
    out
}

/// Where the script comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Url(url::Url),
    File(PathBuf),
}

impl Source {
    pub fn parse(s: &str) -> Result<Source> {
        let s = s.trim();
        let bad = || LdmError::Other(format!("engine.pac_url must be an http(s):// or file:// URL or an absolute path, got {s:?}"));
        if let Some(rest) = s.strip_prefix("~/") {
            let home = std::env::var_os("HOME").ok_or_else(bad)?;
            return Ok(Source::File(PathBuf::from(home).join(rest)));
        }
        if s.starts_with('/') {
            return Ok(Source::File(PathBuf::from(s)));
        }
        let u = url::Url::parse(s).map_err(|_| bad())?;
        match u.scheme() {
            "http" | "https" => Ok(Source::Url(u)),
            "file" => u.to_file_path().map(Source::File).map_err(|_| bad()),
            _ => Err(bad()),
        }
    }

    fn describe(&self) -> String {
        match self {
            Source::Url(u) => crate::util::security::redact_url(u.as_str()),
            Source::File(p) => p.display().to_string(),
        }
    }
}

// ---------------------------------------------------------------- helpers (the PAC "standard library")

/// DNS + clock access for the helpers; cached, and replaceable in tests.
pub struct PacHost {
    dns: Mutex<HashMap<String, (Vec<IpAddr>, Instant)>>,
    my_ip: Mutex<Option<(String, Instant)>>,
    /// Fixed clock for tests (local and GMT are the same then).
    pub fixed_now: Option<NaiveDateTime>,
    /// Fixed resolver answers for tests (`host → addresses`); unknown hosts don't resolve.
    pub fixed_dns: Option<HashMap<String, Vec<IpAddr>>>,
}

impl Default for PacHost {
    fn default() -> Self {
        Self { dns: Mutex::new(HashMap::new()), my_ip: Mutex::new(None), fixed_now: None, fixed_dns: None }
    }
}

impl PacHost {
    fn resolve(&self, host: &str) -> Vec<IpAddr> {
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host.parse::<IpAddr>() {
            return vec![ip];
        }
        if let Some(m) = &self.fixed_dns {
            return m.get(host).cloned().unwrap_or_default();
        }
        if let Some((v, t)) = self.dns.lock().unwrap().get(host) {
            if t.elapsed() < if v.is_empty() { DNS_NEG_TTL } else { DNS_TTL } {
                return v.clone();
            }
        }
        let v: Vec<IpAddr> = (host, 0u16).to_socket_addrs().map(|it| it.map(|a| a.ip()).collect()).unwrap_or_default();
        let mut d = self.dns.lock().unwrap();
        if d.len() > 4096 {
            d.clear();
        }
        d.insert(host.to_string(), (v.clone(), Instant::now()));
        v
    }

    fn resolve4(&self, host: &str) -> Option<Ipv4Addr> {
        self.resolve(host).into_iter().find_map(|a| match a {
            IpAddr::V4(v) => Some(v),
            _ => None,
        })
    }

    /// Address of the interface used for the default route (no packet is sent).
    fn my_ip(&self, v6: bool) -> String {
        if !v6 {
            if let Some((ip, t)) = self.my_ip.lock().unwrap().clone() {
                if t.elapsed() < DNS_TTL {
                    return ip;
                }
            }
        }
        let probe = |bind: &str, target: &str| -> Option<IpAddr> {
            let s = UdpSocket::bind(bind).ok()?;
            s.connect(target).ok()?;
            Some(s.local_addr().ok()?.ip())
        };
        let ip = if v6 { probe("[::]:0", "[2001:db8::1]:53") } else { probe("0.0.0.0:0", "192.0.2.1:53") };
        let ip =
            ip.filter(|i| !i.is_unspecified()).map(|i| i.to_string()).unwrap_or_else(|| if v6 { "::1" } else { "127.0.0.1" }.to_string());
        if !v6 {
            *self.my_ip.lock().unwrap() = Some((ip.clone(), Instant::now()));
        }
        ip
    }

    fn now(&self, gmt: bool) -> NaiveDateTime {
        self.fixed_now.unwrap_or_else(|| if gmt { chrono::Utc::now().naive_utc() } else { chrono::Local::now().naive_local() })
    }
}

/// Glob with `*` and `?` (the PAC `shExpMatch`).
pub fn sh_exp_match(s: &str, pat: &str) -> bool {
    let s: Vec<char> = s.chars().collect();
    let p: Vec<char> = pat.chars().collect();
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while i < s.len() {
        if j < p.len() && (p[j] == '?' || p[j] == s[i]) {
            i += 1;
            j += 1;
        } else if j < p.len() && p[j] == '*' {
            star = Some((j, i));
            j += 1;
        } else if let Some((sj, si)) = star {
            j = sj + 1;
            i = si + 1;
            star = Some((sj, si + 1));
        } else {
            return false;
        }
    }
    while j < p.len() && p[j] == '*' {
        j += 1;
    }
    j == p.len()
}

const DAYS: [&str; 7] = ["SUN", "MON", "TUE", "WED", "THU", "FRI", "SAT"];
const MONTHS: [&str; 12] = ["JAN", "FEB", "MAR", "APR", "MAY", "JUN", "JUL", "AUG", "SEP", "OCT", "NOV", "DEC"];

/// Split a trailing `"GMT"` argument off.
fn gmt_args(a: &[Val]) -> (&[Val], bool) {
    match a.last() {
        Some(Val::Str(s)) if s.eq_ignore_ascii_case("GMT") => (&a[..a.len() - 1], true),
        _ => (a, false),
    }
}

/// `lo <= x <= hi`, wrapping around when `lo > hi` (e.g. FRI..MON, 22h..6h, NOV..FEB).
fn in_wrapped<T: PartialOrd>(x: T, lo: T, hi: T) -> bool {
    if lo <= hi {
        lo <= x && x <= hi
    } else {
        x >= lo || x <= hi
    }
}

fn weekday_range(h: &PacHost, a: &[Val]) -> JsResult<Val> {
    let (a, gmt) = gmt_args(a);
    let day = |v: &Val| DAYS.iter().position(|d| d.eq_ignore_ascii_case(&v.to_str()));
    let today = h.now(gmt).weekday().num_days_from_sunday() as usize;
    Ok(Val::Bool(match a {
        [d] => day(d) == Some(today),
        [d1, d2] => match (day(d1), day(d2)) {
            (Some(x), Some(y)) => in_wrapped(today, x, y),
            _ => false,
        },
        _ => false,
    }))
}

fn time_range(h: &PacHost, a: &[Val]) -> JsResult<Val> {
    let (a, gmt) = gmt_args(a);
    let n: Vec<u32> = a.iter().map(|v| v.num().max(0.0) as u32).collect();
    let now = h.now(gmt);
    let (hh, mm, ss) = (now.hour(), now.minute(), now.second());
    let t = hh * 3600 + mm * 60 + ss;
    Ok(Val::Bool(match n.as_slice() {
        [x] => hh == *x,
        // hours: start inclusive, end exclusive (9, 17 = 09:00:00–16:59:59)
        [x, y] => {
            if x <= y {
                *x <= hh && hh < *y
            } else {
                hh >= *x || hh < *y
            }
        }
        [h1, m1, h2, m2] => in_wrapped(t, h1 * 3600 + m1 * 60, h2 * 3600 + m2 * 60 + 59),
        [h1, m1, s1, h2, m2, s2] => in_wrapped(t, h1 * 3600 + m1 * 60 + s1, h2 * 3600 + m2 * 60 + s2),
        _ => false,
    }))
}

#[derive(Clone, Copy, PartialEq)]
enum DatePart {
    Day(u32),
    Month(u32),
    Year(i32),
}

fn date_range(h: &PacHost, a: &[Val]) -> JsResult<Val> {
    let (a, gmt) = gmt_args(a);
    let part = |v: &Val| -> Option<DatePart> {
        match v {
            Val::Str(s) => MONTHS.iter().position(|m| m.eq_ignore_ascii_case(s)).map(|m| DatePart::Month(m as u32 + 1)),
            _ => {
                let n = v.num();
                if n.is_nan() || n < 1.0 {
                    None
                } else if n <= 31.0 {
                    Some(DatePart::Day(n as u32))
                } else {
                    Some(DatePart::Year(n as i32))
                }
            }
        }
    };
    let Some(parts) = a.iter().map(part).collect::<Option<Vec<_>>>() else { return Ok(Val::Bool(false)) };
    let now = h.now(gmt).date();
    // project "now" and both bounds onto the fields that are given, compare as (year, month, day)
    let key = |ps: &[DatePart], fill_from_now: bool| -> (i32, u32, u32) {
        let (mut y, mut m, mut d) = if fill_from_now { (now.year(), now.month(), now.day()) } else { (0, 0, 0) };
        for p in ps {
            match *p {
                DatePart::Year(v) => y = v,
                DatePart::Month(v) => m = v,
                DatePart::Day(v) => d = v,
            }
        }
        (y, m, d)
    };
    let kinds = |ps: &[DatePart]| ps.iter().map(std::mem::discriminant).collect::<Vec<_>>();
    let mask = |k: (i32, u32, u32), ps: &[DatePart]| -> (i32, u32, u32) {
        let has = |f: fn(&DatePart) -> bool| ps.iter().any(f);
        (
            if has(|p| matches!(p, DatePart::Year(_))) { k.0 } else { 0 },
            if has(|p| matches!(p, DatePart::Month(_))) { k.1 } else { 0 },
            if has(|p| matches!(p, DatePart::Day(_))) { k.2 } else { 0 },
        )
    };
    Ok(Val::Bool(match parts.len() {
        1 => mask(key(&[], true), &parts) == mask(key(&parts, false), &parts),
        2 | 4 | 6 => {
            let (lo, hi) = parts.split_at(parts.len() / 2);
            if kinds(lo) != kinds(hi) {
                return Ok(Val::Bool(false));
            }
            let cur = mask(key(&[], true), lo);
            let (l, u) = (mask(key(lo, false), lo), mask(key(hi, false), lo));
            if lo.iter().any(|p| matches!(p, DatePart::Year(_))) {
                l <= cur && cur <= u
            } else {
                in_wrapped(cur, l, u)
            }
        }
        _ => false,
    }))
}

fn ipv4_num(s: &str) -> Option<u32> {
    s.trim().parse::<Ipv4Addr>().ok().map(u32::from)
}

impl js::Natives for PacHost {
    fn names(&self) -> &'static [&'static str] {
        &[
            "isPlainHostName",
            "dnsDomainIs",
            "localHostOrDomainIs",
            "isResolvable",
            "isInNet",
            "dnsResolve",
            "myIpAddress",
            "dnsDomainLevels",
            "shExpMatch",
            "weekdayRange",
            "dateRange",
            "timeRange",
            "convert_addr",
            "alert",
            "isResolvableEx",
            "dnsResolveEx",
            "myIpAddressEx",
            "isInNetEx",
        ]
    }

    fn call(&self, name: &str, a: &[Val]) -> JsResult<Val> {
        let s = |i: usize| a.get(i).map(|v| v.to_str()).unwrap_or_default();
        Ok(match name {
            "isPlainHostName" => Val::Bool(!s(0).contains('.')),
            "dnsDomainIs" => {
                let (h, d) = (s(0), s(1));
                Val::Bool(h.len() >= d.len() && h.ends_with(&d))
            }
            "localHostOrDomainIs" => {
                let (h, hd) = (s(0), s(1));
                Val::Bool(h == hd || (!h.contains('.') && hd.starts_with(&format!("{h}."))))
            }
            "isResolvable" => Val::Bool(self.resolve4(&s(0)).is_some()),
            "isResolvableEx" => Val::Bool(!self.resolve(&s(0)).is_empty()),
            "dnsResolve" => self.resolve4(&s(0)).map(|ip| Val::str(&ip.to_string())).unwrap_or(Val::Null),
            "dnsResolveEx" => {
                let v = self.resolve(&s(0));
                Val::str(&v.iter().map(|i| i.to_string()).collect::<Vec<_>>().join(";"))
            }
            "myIpAddress" => Val::str(&self.my_ip(false)),
            "myIpAddressEx" => Val::str(&[self.my_ip(false), self.my_ip(true)].join(";")),
            "isInNet" => {
                let ip = self.resolve4(&s(0)).map(u32::from);
                match (ip, ipv4_num(&s(1)), ipv4_num(&s(2))) {
                    (Some(ip), Some(p), Some(m)) => Val::Bool(ip & m == p & m),
                    _ => Val::Bool(false),
                }
            }
            "isInNetEx" => {
                let (ip, prefix) = (s(0), s(1));
                let Some((net, bits)) = prefix.split_once('/') else { return Ok(Val::Bool(false)) };
                let bits: u32 = bits.parse().unwrap_or(999);
                Val::Bool(match (ip.parse::<IpAddr>(), net.parse::<IpAddr>()) {
                    (Ok(IpAddr::V4(a)), Ok(IpAddr::V4(b))) if bits <= 32 => {
                        let m = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
                        u32::from(a) & m == u32::from(b) & m
                    }
                    (Ok(IpAddr::V6(a)), Ok(IpAddr::V6(b))) if bits <= 128 => {
                        let m = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
                        u128::from(a) & m == u128::from(b) & m
                    }
                    _ => false,
                })
            }
            "dnsDomainLevels" => Val::Num(s(0).matches('.').count() as f64),
            "shExpMatch" => Val::Bool(sh_exp_match(&s(0), &s(1))),
            "weekdayRange" => weekday_range(self, a)?,
            "dateRange" => date_range(self, a)?,
            "timeRange" => time_range(self, a)?,
            "convert_addr" => Val::Num(ipv4_num(&s(0)).map(|v| v as f64).unwrap_or(0.0)),
            "alert" => {
                debug!(msg = %s(0), "PAC alert()");
                Val::Undef
            }
            _ => return Err(format!("{name} is not defined")),
        })
    }
}

/// Evaluate `FindProxyForURL(url, host)` of `prog`.
pub fn evaluate(prog: &js::Program, host_env: &PacHost, url: &str, host: &str) -> JsResult<String> {
    let mut it = js::Interp::new(prog, host_env)?;
    let fname = if prog.has_function("FindProxyForURLEx") { "FindProxyForURLEx" } else { "FindProxyForURL" };
    let v = it.call_global(fname, vec![Val::str(url), Val::str(host)])?;
    match v {
        Val::Str(s) => Ok(s.to_string()),
        Val::Undef | Val::Null => Ok(String::new()),
        other => Ok(other.to_str()),
    }
}

/// Compile a script, checking that it defines `FindProxyForURL`.
pub fn compile(src: &str) -> Result<js::Program> {
    let p = js::parse(src).map_err(|e| LdmError::Format(format!("PAC script: {e}")))?;
    if !p.has_function("FindProxyForURL") && !p.has_function("FindProxyForURLEx") {
        return Err(LdmError::Format("PAC script: no FindProxyForURL function".into()));
    }
    Ok(p)
}

// ---------------------------------------------------------------- runtime

#[derive(Default)]
enum LoadState {
    #[default]
    Pending,
    Ready,
    Failed,
}

struct Shared {
    source: Source,
    script: RwLock<Option<Arc<js::Program>>>,
    state: Mutex<LoadState>,
    ready: Condvar,
    host: PacHost,
    results: Mutex<HashMap<String, (Vec<Entry>, Instant)>>,
    health: Mutex<HashMap<String, (bool, Instant)>>,
    last_error: Mutex<Option<String>>,
    loads: AtomicU64,
    warned_direct: AtomicBool,
}

/// Status for diagnostics.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PacStatus {
    pub source: String,
    pub loaded: bool,
    pub loads: u64,
    pub last_error: Option<String>,
}

pub struct Pac {
    shared: Arc<Shared>,
    refresh_tx: Mutex<mpsc::Sender<()>>,
    first_wait: Duration,
}

impl Pac {
    /// Start loading `source` in the background; refresh every `refresh`.
    pub fn start(source: &str, refresh: Duration, first_wait: Duration) -> Result<Arc<Pac>> {
        let src = Source::parse(source)?;
        let shared = Arc::new(Shared {
            source: src,
            script: RwLock::new(None),
            state: Mutex::new(LoadState::Pending),
            ready: Condvar::new(),
            host: PacHost::default(),
            results: Mutex::new(HashMap::new()),
            health: Mutex::new(HashMap::new()),
            last_error: Mutex::new(None),
            loads: AtomicU64::new(0),
            warned_direct: AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel();
        let sh = shared.clone();
        let refresh = refresh.max(Duration::from_secs(1));
        std::thread::Builder::new()
            .name("ldm-pac".into())
            .spawn(move || loader(sh, refresh, rx))
            .map_err(|e| LdmError::Other(format!("PAC loader thread: {e}")))?;
        Ok(Arc::new(Pac { shared, refresh_tx: Mutex::new(tx), first_wait }))
    }

    pub fn status(&self) -> PacStatus {
        PacStatus {
            source: self.shared.source.describe(),
            loaded: self.shared.script.read().unwrap().is_some(),
            loads: self.shared.loads.load(Ordering::Relaxed),
            last_error: self.shared.last_error.lock().unwrap().clone(),
        }
    }

    /// Ask the loader for an early refresh (rate-limited there).
    pub fn request_refresh(&self) {
        let _ = self.refresh_tx.lock().unwrap().send(());
    }

    fn script(&self) -> Option<Arc<js::Program>> {
        if let Some(p) = self.shared.script.read().unwrap().clone() {
            return Some(p);
        }
        // first use: wait (bounded) for the initial load instead of silently going direct
        let st = self.shared.state.lock().unwrap();
        let _ = self.shared.ready.wait_timeout_while(st, self.first_wait, |s| matches!(s, LoadState::Pending)).unwrap();
        self.shared.script.read().unwrap().clone()
    }

    /// Entries for `url` (cached for a minute; cache cleared on every script reload).
    pub fn entries(&self, url: &url::Url) -> Vec<Entry> {
        let host = url.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']').to_string();
        // like browsers, https URLs are passed without path/query
        let key = if url.scheme() == "https" {
            format!("https://{}{}/", url.host_str().unwrap_or(""), url.port().map(|p| format!(":{p}")).unwrap_or_default())
        } else {
            let mut u = url.clone();
            u.set_fragment(None);
            u.to_string()
        };
        if let Some((v, t)) = self.shared.results.lock().unwrap().get(&key) {
            if t.elapsed() < RESULT_TTL {
                return v.clone();
            }
        }
        let Some(prog) = self.script() else {
            if !self.shared.warned_direct.swap(true, Ordering::Relaxed) {
                warn!(source = %self.shared.source.describe(), "PAC script not available yet; connecting DIRECT");
            }
            return vec![Entry::Direct];
        };
        let v = match evaluate(&prog, &self.shared.host, &key, &host) {
            Ok(s) => {
                let v = parse_result(&s);
                debug!(host = %host, result = %s, "PAC");
                if v.is_empty() {
                    if !s.trim().is_empty() {
                        warn!(result = %s, "PAC returned nothing usable; connecting DIRECT");
                    }
                    vec![Entry::Direct]
                } else {
                    v
                }
            }
            Err(e) => {
                warn!(error = %e, "PAC FindProxyForURL failed; connecting DIRECT");
                *self.shared.last_error.lock().unwrap() = Some(format!("FindProxyForURL: {e}"));
                self.request_refresh();
                vec![Entry::Direct]
            }
        };
        let mut r = self.shared.results.lock().unwrap();
        if r.len() >= RESULT_CAP {
            r.clear();
        }
        r.insert(key, (v.clone(), Instant::now()));
        v
    }

    fn healthy(&self, u: &url::Url) -> bool {
        let key = format!("{}:{}", u.host_str().unwrap_or(""), u.port_or_known_default().unwrap_or(0));
        if let Some((ok, t)) = self.shared.health.lock().unwrap().get(&key) {
            if t.elapsed() < if *ok { HEALTH_OK_TTL } else { HEALTH_BAD_TTL } {
                return *ok;
            }
        }
        let host = u.host_str().unwrap_or("").trim_start_matches('[').trim_end_matches(']');
        let port = u.port_or_known_default().unwrap_or(if u.scheme().starts_with("socks") { 1080 } else { 80 });
        let addrs: Vec<SocketAddr> = (host, port).to_socket_addrs().map(|a| a.collect()).unwrap_or_default();
        let ok = addrs.iter().any(|a| TcpStream::connect_timeout(a, HEALTH_TIMEOUT).is_ok());
        self.shared.health.lock().unwrap().insert(key, (ok, Instant::now()));
        ok
    }

    /// Proxy for `url`, or `None` for DIRECT. Unreachable proxies are skipped in order; if none
    /// answers and there is no DIRECT entry, the first proxy is used anyway (the transfer then
    /// fails and is retried) and an early PAC refresh is requested.
    pub fn select(&self, url: &url::Url) -> Option<url::Url> {
        let entries = self.entries(url);
        let mut first_proxy = None;
        for (i, e) in entries.iter().enumerate() {
            match e {
                Entry::Direct => {
                    if i > 0 {
                        info!(host = ?url.host_str(), "PAC: earlier proxies unreachable, falling back to DIRECT");
                    }
                    return None;
                }
                Entry::Proxy(p) => {
                    if self.healthy(p) {
                        if i > 0 {
                            info!(host = ?url.host_str(), proxy = %p, "PAC: falling back to a later proxy");
                        }
                        return Some(p.clone());
                    }
                    if first_proxy.is_none() {
                        first_proxy = Some(p.clone());
                        self.request_refresh();
                    }
                }
            }
        }
        warn!(host = ?url.host_str(), "PAC: no proxy in the list is reachable");
        first_proxy
    }

    /// Proxy setting string for tools that take a single proxy (yt-dlp): URL or `none`.
    pub fn proxy_string_for(&self, url: &str) -> String {
        match url::Url::parse(url).ok().and_then(|u| self.select(&u)) {
            Some(p) => p.as_str().trim_end_matches('/').to_string(),
            None => "none".into(),
        }
    }
}

fn fetch(src: &Source, rt: &Option<tokio::runtime::Runtime>) -> Result<String> {
    match src {
        Source::File(p) => {
            let meta = std::fs::metadata(p)?;
            if meta.len() as usize > MAX_PAC_BYTES {
                return Err(LdmError::Format("PAC file too large".into()));
            }
            Ok(std::fs::read_to_string(p)?)
        }
        Source::Url(u) => {
            let rt = rt.as_ref().ok_or_else(|| LdmError::Other("no runtime".into()))?;
            rt.block_on(async {
                let c = reqwest::Client::builder()
                    .no_proxy()
                    .connect_timeout(Duration::from_secs(10))
                    .timeout(Duration::from_secs(30))
                    .build()
                    .map_err(|e| LdmError::Other(e.to_string()))?;
                let mut r = c.get(u.as_str()).send().await?;
                if !r.status().is_success() {
                    return Err(LdmError::HttpStatus(r.status().as_u16(), "PAC fetch".into()));
                }
                let mut body = Vec::new();
                while let Some(chunk) = r.chunk().await? {
                    body.extend_from_slice(&chunk);
                    if body.len() > MAX_PAC_BYTES {
                        return Err(LdmError::Format("PAC file too large".into()));
                    }
                }
                Ok(String::from_utf8_lossy(&body).into_owned())
            })
        }
    }
}

fn loader(sh: Arc<Shared>, refresh: Duration, rx: mpsc::Receiver<()>) {
    let rt = match sh.source {
        Source::Url(_) => tokio::runtime::Builder::new_current_thread().enable_all().build().ok(),
        Source::File(_) => None,
    };
    let min_gap = refresh.min(Duration::from_secs(5));
    let mut last: Option<Instant> = None;
    let mut last_text: Option<String> = None;
    loop {
        if let Some(t) = last {
            if let Some(rest) = min_gap.checked_sub(t.elapsed()) {
                std::thread::sleep(rest);
            }
        }
        last = Some(Instant::now());
        let res = fetch(&sh.source, &rt).and_then(|text| {
            if last_text.as_deref() == Some(text.as_str()) && sh.script.read().unwrap().is_some() {
                return Ok(None);
            }
            let p = compile(&text)?;
            Ok(Some((p, text)))
        });
        let ok = match res {
            Ok(Some((p, text))) => {
                *sh.script.write().unwrap() = Some(Arc::new(p));
                sh.results.lock().unwrap().clear();
                sh.health.lock().unwrap().clear();
                *sh.last_error.lock().unwrap() = None;
                let n = sh.loads.fetch_add(1, Ordering::Relaxed) + 1;
                info!(source = %sh.source.describe(), loads = n, bytes = text.len(), "PAC script loaded");
                last_text = Some(text);
                true
            }
            Ok(None) => {
                // unchanged; just forget cached proxy health so a recovered proxy is tried again
                sh.health.lock().unwrap().clear();
                *sh.last_error.lock().unwrap() = None;
                true
            }
            Err(e) => {
                let kept = sh.script.read().unwrap().is_some();
                warn!(source = %sh.source.describe(), error = %e, keeping_previous = kept, "PAC load failed");
                *sh.last_error.lock().unwrap() = Some(e.to_string());
                false
            }
        };
        {
            let mut st = sh.state.lock().unwrap();
            *st = if sh.script.read().unwrap().is_some() { LoadState::Ready } else { LoadState::Failed };
            sh.ready.notify_all();
        }
        let wait = if ok { refresh } else { refresh.min(Duration::from_secs(30)) };
        match rx.recv_timeout(wait) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Timeout) => while rx.try_recv().is_ok() {},
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    debug!("PAC loader stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn host_at(y: i32, m: u32, d: u32, h: u32, mi: u32) -> PacHost {
        let mut dns = HashMap::new();
        dns.insert("intranet.corp.example".to_string(), vec!["10.1.2.3".parse().unwrap()]);
        dns.insert("www.example.com".to_string(), vec!["93.184.216.34".parse().unwrap()]);
        PacHost {
            fixed_now: Some(NaiveDate::from_ymd_opt(y, m, d).unwrap().and_hms_opt(h, mi, 0).unwrap()),
            fixed_dns: Some(dns),
            ..Default::default()
        }
    }

    fn call(h: &PacHost, f: &str, args: &[Val]) -> bool {
        js::Natives::call(h, f, args).unwrap().truthy()
    }

    fn v(s: &str) -> Val {
        Val::str(s)
    }

    #[test]
    fn parse_results_in_order() {
        let e = parse_result("PROXY a.example:8080; SOCKS5 127.0.0.1:1080 ;HTTPS s.example:443;SOCKS s4:9; DIRECT; BOGUS x:1; PROXY");
        let s: Vec<String> = e
            .iter()
            .map(|x| match x {
                Entry::Direct => "DIRECT".into(),
                Entry::Proxy(u) => u.to_string(),
            })
            .collect();
        assert_eq!(s, ["http://a.example:8080/", "socks5h://127.0.0.1:1080", "https://s.example/", "socks4a://s4:9", "DIRECT"]);
        assert!(parse_result("").is_empty());
        assert!(parse_result("PROXY user:pw@evil:1").is_empty(), "credentials in a PAC result are ignored");
    }

    #[test]
    fn sources() {
        assert!(matches!(Source::parse("http://wpad/wpad.dat").unwrap(), Source::Url(_)));
        assert_eq!(Source::parse("/etc/proxy.pac").unwrap(), Source::File("/etc/proxy.pac".into()));
        assert_eq!(Source::parse("file:///etc/proxy.pac").unwrap(), Source::File("/etc/proxy.pac".into()));
        assert!(Source::parse("ftp://x/y.pac").is_err() && Source::parse("proxy.pac").is_err());
    }

    #[test]
    fn standard_helpers() {
        let h = host_at(2026, 10, 25, 14, 30); // a Sunday
        assert!(call(&h, "isPlainHostName", &[v("intranet")]) && !call(&h, "isPlainHostName", &[v("a.b")]));
        assert!(call(&h, "dnsDomainIs", &[v("www.corp.example"), v(".corp.example")]));
        assert!(!call(&h, "dnsDomainIs", &[v("corp.example"), v(".corp.example")]));
        assert!(call(&h, "localHostOrDomainIs", &[v("www"), v("www.corp.example")]));
        assert!(call(&h, "localHostOrDomainIs", &[v("www.corp.example"), v("www.corp.example")]));
        assert!(!call(&h, "localHostOrDomainIs", &[v("www.other.example"), v("www.corp.example")]));
        assert!(call(&h, "isResolvable", &[v("www.example.com")]) && !call(&h, "isResolvable", &[v("nope.invalid")]));
        assert!(call(&h, "isInNet", &[v("intranet.corp.example"), v("10.0.0.0"), v("255.0.0.0")]));
        assert!(!call(&h, "isInNet", &[v("www.example.com"), v("10.0.0.0"), v("255.0.0.0")]));
        assert!(call(&h, "isInNet", &[v("192.168.1.7"), v("192.168.0.0"), v("255.255.0.0")]));
        assert_eq!(js::Natives::call(&h, "dnsResolve", &[v("intranet.corp.example")]).unwrap().to_str(), "10.1.2.3");
        assert!(matches!(js::Natives::call(&h, "dnsResolve", &[v("nope.invalid")]).unwrap(), Val::Null));
        assert_eq!(js::Natives::call(&h, "dnsDomainLevels", &[v("a.b.c")]).unwrap().to_str(), "2");
        assert_eq!(js::Natives::call(&h, "convert_addr", &[v("1.2.3.4")]).unwrap().to_str(), "16909060");
        assert!(call(&h, "isInNetEx", &[v("10.9.8.7"), v("10.0.0.0/8")]) && !call(&h, "isInNetEx", &[v("11.0.0.1"), v("10.0.0.0/8")]));
        let ip = js::Natives::call(&h, "myIpAddress", &[]).unwrap().to_str();
        assert!(ip.parse::<Ipv4Addr>().is_ok(), "{ip}");
    }

    #[test]
    fn shell_glob() {
        assert!(sh_exp_match("http://home.netscape.com/people/ari/index.html", "*/ari/*"));
        assert!(!sh_exp_match("http://home.netscape.com/people/montulli/index.html", "*/ari/*"));
        assert!(sh_exp_match("a.example.com", "*.example.com") && !sh_exp_match("example.com", "*.example.com"));
        assert!(sh_exp_match("abc", "a?c") && sh_exp_match("", "*") && !sh_exp_match("abcd", "a?c"));
        assert!(sh_exp_match("x.y", "x.y") && !sh_exp_match("xay", "x.y"), "'.' is literal");
    }

    #[test]
    fn time_helpers() {
        let h = host_at(2026, 10, 25, 14, 30); // Sunday 14:30
        assert!(call(&h, "weekdayRange", &[v("SUN")]) && !call(&h, "weekdayRange", &[v("MON"), v("FRI")]));
        assert!(call(&h, "weekdayRange", &[v("SAT"), v("MON")]), "wraps around the week");
        assert!(call(&h, "weekdayRange", &[v("FRI"), v("SUN"), v("GMT")]));
        assert!(call(&h, "timeRange", &[Val::Num(14.0)]) && call(&h, "timeRange", &[Val::Num(9.0), Val::Num(17.0)]));
        assert!(!call(&h, "timeRange", &[Val::Num(9.0), Val::Num(14.0)]), "end hour is exclusive");
        assert!(call(&h, "timeRange", &[Val::Num(22.0), Val::Num(15.0)]), "wraps around midnight");
        assert!(call(&h, "timeRange", &[Val::Num(14.0), Val::Num(0.0), Val::Num(14.0), Val::Num(30.0)]));
        assert!(!call(&h, "timeRange", &[Val::Num(14.0), Val::Num(31.0), Val::Num(15.0), Val::Num(0.0)]));
        assert!(
            call(&h, "dateRange", &[Val::Num(25.0)]) && call(&h, "dateRange", &[v("OCT")]) && call(&h, "dateRange", &[Val::Num(2026.0)])
        );
        assert!(call(&h, "dateRange", &[v("SEP"), v("NOV")]) && !call(&h, "dateRange", &[v("JAN"), v("MAR")]));
        assert!(call(&h, "dateRange", &[v("NOV"), v("OCT")]), "month range wraps around the year");
        assert!(call(&h, "dateRange", &[Val::Num(1.0), v("OCT"), Val::Num(31.0), v("OCT")]));
        assert!(call(&h, "dateRange", &[v("OCT"), Val::Num(2025.0), v("JAN"), Val::Num(2027.0)]));
        assert!(!call(&h, "dateRange", &[Val::Num(1.0), v("JAN"), Val::Num(2027.0), Val::Num(31.0), v("DEC"), Val::Num(2027.0)]));
        assert!(call(&h, "dateRange", &[Val::Num(2025.0), Val::Num(2026.0), v("GMT")]));
    }

    #[test]
    fn sample_pac_files() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/pac");
        let h = host_at(2026, 10, 25, 14, 30);
        let corp = compile(&std::fs::read_to_string(dir.join("corporate.pac")).unwrap()).unwrap();
        let ev = |p: &js::Program, u: &str, host: &str| evaluate(p, &h, u, host).unwrap();
        assert_eq!(ev(&corp, "http://intranet/", "intranet"), "DIRECT");
        assert_eq!(ev(&corp, "http://intranet.corp.example/x", "intranet.corp.example"), "DIRECT");
        assert_eq!(ev(&corp, "http://192.168.1.10/", "192.168.1.10"), "DIRECT");
        assert_eq!(
            ev(&corp, "http://www.example.com/a.zip", "www.example.com"),
            "PROXY proxy1.corp.example:3128; PROXY proxy2.corp.example:3128; DIRECT"
        );
        assert_eq!(ev(&corp, "https://cdn.media.example/", "cdn.media.example"), "SOCKS5 socks.corp.example:1080; DIRECT");
        assert_eq!(ev(&corp, "http://nope.invalid/", "nope.invalid"), "DIRECT");
        let lb = compile(&std::fs::read_to_string(dir.join("load-balance.pac")).unwrap()).unwrap();
        let a = ev(&lb, "http://a.example/", "a.example");
        assert!(a.starts_with("PROXY p") && a.ends_with("; DIRECT"), "{a}");
        assert_eq!(a, ev(&lb, "http://a.example/", "a.example"), "deterministic");
        assert!(compile("function nope() {}").is_err());
        assert!(compile("function FindProxyForURL(u, h) { return /x/.test(u); }").is_err());
    }

    #[test]
    fn runtime_from_file_with_fallback_and_yt_dlp_string() {
        let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        let dead_port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dir = std::env::temp_dir().join(format!("ldm-pac-rt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.pac");
        std::fs::write(
            &f,
            format!(
                "function FindProxyForURL(url, host) {{\n  if (host == 'direct.example') return 'DIRECT';\n  return 'PROXY 127.0.0.1:{dead_port}; SOCKS5 127.0.0.1:{live_port}; DIRECT';\n}}"
            ),
        )
        .unwrap();
        let pac = Pac::start(f.to_str().unwrap(), Duration::from_secs(3600), Duration::from_secs(5)).unwrap();
        assert_eq!(pac.proxy_string_for("https://a.example/v.mp4"), format!("socks5h://127.0.0.1:{live_port}"));
        assert_eq!(pac.proxy_string_for("http://direct.example/x"), "none");
        let st = pac.status();
        assert!(st.loaded && st.loads == 1 && st.last_error.is_none(), "{st:?}");
        drop(live);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
