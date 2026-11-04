//! "Main video or ad?" scoring on the daemon side.
//!
//! The browser extension scores candidates with all signals (players, "Skip Ad" controls,
//! durations, VAST/IMA requests; see `extension/src/shared/adscore.ts`) and sends the verdict
//! with each `MediaCandidate`. This module is the static fallback for candidates that arrive
//! without one (CLI/API clients, older extensions): ad domains (built-in + the user's
//! `media.ad_domains_file`), ad-server subdomains, whole-word URL ad tokens, and "small
//! progressive file beside a manifest on another host". Same weights and threshold as the
//! extension. Nothing is ever dropped: a suspected ad is only ordered last and flagged.

use std::path::Path;

/// Weights (positive = ad-ish), mirrored from the extension.
pub mod weights {
    pub const AD_DOMAIN: f64 = 3.0;
    pub const USER_AD_DOMAIN: f64 = 3.0;
    pub const AD_SUBDOMAIN: f64 = 2.0;
    pub const AD_TOKEN: f64 = 2.0;
    pub const PROGRESSIVE_BESIDE_MANIFEST: f64 = 1.25;
    pub const OTHER_HOST: f64 = 0.75;
    pub const MANIFEST: f64 = -0.5;
}

/// Score at or above which a candidate counts as a suspected ad.
pub const AD_THRESHOLD: f64 = 2.5;
/// Progressive files above this size do not get the "beside a manifest" point.
pub const SMALL_PROGRESSIVE_BYTES: u64 = 30 * 1024 * 1024;

/// Our own short list of ad / ad-tech domains (subdomains match too); keep in sync with the extension.
pub const AD_DOMAINS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "googletagservices.com",
    "2mdn.net",
    "imasdk.googleapis.com",
    "adnxs.com",
    "adsrvr.org",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "criteo.com",
    "taboola.com",
    "outbrain.com",
    "amazon-adsystem.com",
    "moatads.com",
    "adform.net",
    "smartadserver.com",
    "spotxchange.com",
    "springserve.com",
    "teads.tv",
    "innovid.com",
    "serving-sys.com",
    "trafficjunky.net",
    "trafficjunky.com",
    "exoclick.com",
    "exosrv.com",
    "juicyads.com",
    "adtng.com",
    "trafficstars.com",
    "tsyndicate.com",
    "popads.net",
    "propellerads.com",
    "adsterra.com",
    "adspyglass.com",
    "smilewanted.com",
    "richaudience.com",
];

const AD_SUBDOMAIN_LABELS: &[&str] = &["ad", "ads", "adserver", "adservice", "adserving", "adimg", "adcdn", "banners", "vast"];

const AD_TOKENS: &[&str] = &[
    "ad",
    "ads",
    "adv",
    "advert",
    "adverts",
    "advertising",
    "vast",
    "vpaid",
    "vmap",
    "preroll",
    "prerolls",
    "midroll",
    "postroll",
    "creative",
    "creatives",
    "promo",
    "promos",
    "sponsor",
    "sponsored",
    "commercial",
    "commercials",
];
const AD_TOKEN_PAIRS: &[(&str, &str)] = &[("pre", "roll"), ("mid", "roll"), ("post", "roll"), ("ad", "break"), ("ad", "slot")];

#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AdVerdict {
    pub score: f64,
    pub ad: bool,
    /// `code` or `code:detail` (same codes as the extension)
    pub reasons: Vec<String>,
}

/// Lower-case word tokens; letter/digit runs are split (`ads123` → `ads`, `123`).
pub fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut last: Option<bool> = None; // Some(true) = digit
    for c in s.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            let d = c.is_ascii_digit();
            if last.is_some_and(|l| l != d) && !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            cur.push(c);
            last = Some(d);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            last = None;
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A path chunk that is an id, not words: hex with a digit (≥ 6 chars), or a long letter+digit mix.
pub fn is_opaque_id(c: &str) -> bool {
    let digit = c.chars().any(|x| x.is_ascii_digit());
    let letter = c.chars().any(|x| x.is_ascii_alphabetic());
    let mixed = digit && letter;
    (mixed && c.len() >= 6 && c.chars().all(|x| x.is_ascii_hexdigit())) || (mixed && c.len() >= 12)
}

/// Whole-word ad token in the URL path ("download", "header", "upload" never match).
pub fn ad_token_in(url: &str) -> Option<String> {
    let path = url::Url::parse(url)
        .map(|u| percent_encoding::percent_decode_str(u.path()).decode_utf8_lossy().into_owned())
        .unwrap_or_else(|_| url.to_string());
    // opaque ids ("ccfaa6de", "badb6ad1", long base64-ish runs) are not words: "6ad1" is no "ad"
    let t: Vec<String> = path
        .to_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|c| !c.is_empty() && !is_opaque_id(c))
        .flat_map(tokens)
        .collect();
    for i in 0..t.len() {
        if AD_TOKENS.contains(&t[i].as_str()) {
            return Some(t[i].clone());
        }
        if i + 1 < t.len() && AD_TOKEN_PAIRS.iter().any(|(a, b)| *a == t[i] && *b == t[i + 1]) {
            return Some(format!("{}-{}", t[i], t[i + 1]));
        }
    }
    None
}

pub fn host_of(url: &str) -> String {
    url::Url::parse(url).ok().and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase())).unwrap_or_default()
}

/// The list entry `host` falls under (exact or subdomain).
pub fn domain_listed<'a, S: AsRef<str>>(host: &str, list: &'a [S]) -> Option<&'a str> {
    list.iter().map(|d| d.as_ref()).find(|d| {
        let d = d.trim_start_matches("*.").trim_start_matches('.');
        !d.is_empty() && (host == d || host.ends_with(&format!(".{d}")))
    })
}

/// Parse a user ad-domain file: one domain per line, `#` comments, hosts-file lines (`0.0.0.0 ads.example`).
pub fn parse_domain_list(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let parts: Vec<&str> = line.split_whitespace().collect();
        let pick =
            if parts.len() > 1 && parts[0].chars().all(|c| c.is_ascii_digit() || c == '.' || c == ':') { parts[1] } else { parts[0] };
        let d = pick.to_ascii_lowercase();
        let d = d.trim_start_matches("*.").trim_start_matches('.');
        let valid = d.contains('.')
            && d.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
            && !d.starts_with('.')
            && !d.ends_with('.')
            && d.rsplit('.').next().is_some_and(|tld| tld.chars().any(|c| c.is_ascii_alphabetic()));
        if valid && d != "localhost" {
            out.push(d.to_string());
        }
    }
    out
}

/// Load `media.ad_domains_file` (missing / empty path → none; capped at 50 000 entries).
pub fn load_domain_file(path: &str) -> Vec<String> {
    if path.trim().is_empty() {
        return vec![];
    }
    let p = crate::paths::expand(path);
    match std::fs::read_to_string(Path::new(&p)) {
        Ok(t) => {
            let mut v = parse_domain_list(&t);
            v.truncate(50_000);
            v
        }
        Err(e) => {
            tracing::warn!(path = %p, err = %e, "cannot read media.ad_domains_file");
            vec![]
        }
    }
}

/// Input for the static scorer.
#[derive(Debug, Clone, Default)]
pub struct StaticCandidate<'a> {
    pub url: &'a str,
    /// `progressive` | `hls` | `dash` (or `media`/`manifest` from the classifier)
    pub kind: &'a str,
    pub size: Option<u64>,
}

fn is_manifest(kind: &str) -> bool {
    matches!(kind, "hls" | "dash" | "manifest")
}

/// Score one page's candidates with the static signals.
pub fn score_static(items: &[StaticCandidate], user_domains: &[String]) -> Vec<AdVerdict> {
    use weights::*;
    let manifest_hosts: Vec<String> = items.iter().filter(|c| is_manifest(c.kind)).map(|c| host_of(c.url)).collect();
    let has_manifest = !manifest_hosts.is_empty();
    items
        .iter()
        .map(|c| {
            let mut v = AdVerdict::default();
            let mut add = |w: f64, r: String| {
                v.score += w;
                v.reasons.push(r);
            };
            let host = host_of(c.url);
            if let Some(d) = domain_listed(&host, user_domains) {
                add(USER_AD_DOMAIN, format!("user-ad-domain:{d}"));
            } else if let Some(d) = domain_listed(&host, AD_DOMAINS) {
                add(AD_DOMAIN, format!("ad-domain:{d}"));
            } else if host.split('.').count() > 2 && AD_SUBDOMAIN_LABELS.contains(&host.split('.').next().unwrap_or("")) {
                add(AD_SUBDOMAIN, format!("ad-subdomain:{host}"));
            }
            if let Some(t) = ad_token_in(c.url) {
                add(AD_TOKEN, format!("ad-token:{t}"));
            }
            if is_manifest(c.kind) {
                add(MANIFEST, "manifest".into());
            } else if has_manifest {
                if c.size.is_none_or(|s| s < SMALL_PROGRESSIVE_BYTES) {
                    add(PROGRESSIVE_BESIDE_MANIFEST, "progressive-beside-manifest".into());
                }
                if !host.is_empty() && !manifest_hosts.contains(&host) {
                    add(OTHER_HOST, format!("other-host:{host}"));
                }
            }
            v.score = (v.score * 100.0).round() / 100.0;
            v.ad = v.score >= AD_THRESHOLD;
            v
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(url: &str, kind: &str) -> AdVerdict {
        score_static(&[StaticCandidate { url, kind, size: None }], &[]).remove(0)
    }

    #[test]
    fn opaque_ids_are_not_words() {
        // real-world: Vimeo's CDN path ids ("…-a61a-8ecebadb6ad1/…") contain "6ad1"
        let u = "https://skyfire.vimeocdn.com/1792307690-0x5d6f/ccfaa6de-0af0-44cb-a61a-8ecebadb6ad1/v2/playlist/av/primary/playlist.mpd";
        assert_eq!(ad_token_in(u), None);
        assert!(is_opaque_id("8ecebadb6ad1") && is_opaque_id("ccfaa6de") && !is_opaque_id("ads123") && !is_opaque_id("preroll2"));
        assert_eq!(ad_token_in("https://e.com/ads123/clip.mp4").as_deref(), Some("ads"));
        assert_eq!(ad_token_in("https://e.com/v/preroll2.mp4").as_deref(), Some("preroll"));
    }

    #[test]
    fn tokens_and_word_boundaries() {
        assert_eq!(tokens("/x/ads123/Pre-Roll_v2.mp4"), ["x", "ads", "123", "pre", "roll", "v", "2", "mp", "4"]);
        assert_eq!(ad_token_in("https://cdn.example.com/ads/clip.mp4").as_deref(), Some("ads"));
        assert_eq!(ad_token_in("https://cdn.example.com/v/pre-roll.mp4").as_deref(), Some("pre-roll"));
        assert_eq!(ad_token_in("https://cdn.example.com/vast/creative/1.mp4").as_deref(), Some("vast"));
        for ok in [
            "https://cdn.example.com/download.mp4",
            "https://e.com/header/video.mp4",
            "https://e.com/upload/head.mp4",
            "https://e.com/adobe/padded.mp4",
            "https://e.com/v.mp4?ad=1",
        ] {
            assert_eq!(ad_token_in(ok), None, "{ok}");
        }
    }

    #[test]
    fn domains_builtin_user_and_subdomain() {
        assert!(one("https://pubads.g.doubleclick.net/x.mp4", "progressive").reasons.contains(&"ad-domain:doubleclick.net".to_string()));
        assert!(!one("https://notdoubleclick.net/x.mp4", "progressive").ad);
        assert!(one("https://ads.example.com/v.mp4", "progressive").reasons[0].starts_with("ad-subdomain"));
        let user = parse_domain_list("# mine\nads.example.org\n0.0.0.0 track.example.net  # hosts\n*.promo.example.com\nlocalhost\n127.0.0.1 localhost\nnot a domain\n1.2.3.4\n");
        assert_eq!(user, ["ads.example.org", "track.example.net", "promo.example.com"]);
        let v = score_static(&[StaticCandidate { url: "https://cdn.promo.example.com/v.mp4", kind: "progressive", size: None }], &user);
        assert_eq!(v[0].reasons, ["user-ad-domain:promo.example.com"]);
        assert!(v[0].ad);
    }

    #[test]
    fn format_signal_and_ranking_guards() {
        let items = [
            StaticCandidate { url: "https://media2.example.net/pre_videos/1/mp4_720.mp4", kind: "progressive", size: Some(3_000_000) },
            StaticCandidate { url: "https://cdn.example.com/hls/master.m3u8", kind: "hls", size: None },
            StaticCandidate { url: "https://cdn.example.com/full.mp4", kind: "progressive", size: Some(900_000_000) },
        ];
        let v = score_static(&items, &[]);
        assert_eq!(v[0].reasons, ["progressive-beside-manifest", "other-host:media2.example.net"]);
        assert!(!v[0].ad, "format alone is a hint, not a verdict");
        assert_eq!(v[1].reasons, ["manifest"]);
        assert!(v[2].reasons.is_empty());
        // a plain page with one video is never flagged
        let plain = one("https://www.example.com/media/download.mp4", "progressive");
        assert!(plain.reasons.is_empty() && !plain.ad);
    }

    #[test]
    fn builtin_list_matches_the_extension() {
        let ts = include_str!("../../../../extension/src/shared/adscore.ts");
        for d in AD_DOMAINS {
            assert!(ts.contains(&format!("\"{d}\"")), "{d} missing in extension list");
        }
        let n = ts.split("AD_DOMAINS: readonly string[] = [").nth(1).unwrap().split("];").next().unwrap().matches('"').count() / 2;
        assert_eq!(n, AD_DOMAINS.len());
    }
}
