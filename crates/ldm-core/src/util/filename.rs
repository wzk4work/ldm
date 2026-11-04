//! File-name derivation and sanitizing.
use std::path::{Path, PathBuf};

use percent_encoding::percent_decode_str;

const MAX_NAME_BYTES: usize = 200;

/// Make a safe single path component: strips separators and control
/// characters, replaces characters that are illegal on common filesystems,
/// refuses `.`/`..`, limits the length (keeping the extension).
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out: String = base
        .chars()
        .map(|c| match c {
            c if c.is_control() => '_',
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            c => c,
        })
        .collect();
    out = out.trim().trim_matches('.').trim().to_string();
    if out.is_empty() || out == ".." || out == "." {
        return format!("download_{}", chrono::Local::now().format("%Y%m%d_%H%M%S"));
    }
    if out.len() > MAX_NAME_BYTES {
        let (stem, ext) = split_ext(&out);
        let ext = if ext.len() < 16 { ext } else { "" };
        let mut stem = stem.to_string();
        while stem.len() + ext.len() + 1 > MAX_NAME_BYTES {
            stem.pop();
        }
        out = if ext.is_empty() { stem } else { format!("{stem}.{ext}") };
    }
    out
}

/// `("archive.tar", "gz")` style split on the last dot.
pub fn split_ext(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(i) if i > 0 && i + 1 < name.len() => (&name[..i], &name[i + 1..]),
        _ => (name, ""),
    }
}

pub fn extension_of(name: &str) -> String {
    split_ext(name).1.to_ascii_lowercase()
}

/// Parse `Content-Disposition` (RFC 6266: `filename*=` preferred over `filename=`).
pub fn from_content_disposition(v: &str) -> Option<String> {
    let mut plain = None;
    for part in v.split(';') {
        let part = part.trim();
        let lower = part.to_ascii_lowercase();
        if lower.starts_with("filename*=") {
            let val = &part[10..];
            // charset'lang'pct-encoded
            let enc = val.splitn(3, '\'').nth(2).unwrap_or(val);
            let dec = percent_decode_str(enc.trim_matches('"')).decode_utf8_lossy().to_string();
            if !dec.is_empty() {
                return Some(dec);
            }
        } else if lower.starts_with("filename=") {
            let val = part[9..].trim().trim_matches('"');
            if !val.is_empty() {
                plain = Some(percent_decode_str(val).decode_utf8_lossy().to_string());
            }
        }
    }
    plain
}

/// Last non-empty path segment of a URL, percent-decoded.
pub fn from_url(u: &str) -> Option<String> {
    let parsed = url::Url::parse(u).ok()?;
    let seg = parsed.path_segments()?.rfind(|s| !s.is_empty())?.to_string();
    let dec = percent_decode_str(&seg).decode_utf8_lossy().to_string();
    if dec.is_empty() {
        None
    } else {
        Some(dec)
    }
}

/// Choose a free path in `dir` according to the conflict policy.
/// `rename_numbered` (and `ask`, whose question is asked by the GUI before the job starts) → `name (1).ext`, …
pub fn unique_path(dir: &Path, name: &str, policy: &str) -> PathBuf {
    let first = dir.join(name);
    if policy == "overwrite" || !taken(&first) {
        return first;
    }
    let (stem, ext) = split_ext(name);
    for n in 1..10_000 {
        let cand = if ext.is_empty() { format!("{stem} ({n})") } else { format!("{stem} ({n}).{ext}") };
        let p = dir.join(cand);
        if !taken(&p) {
            return p;
        }
    }
    dir.join(format!("{}_{}", chrono::Local::now().timestamp_millis(), name))
}

fn taken(p: &Path) -> bool {
    p.exists() || merging_path(p).exists()
}

/// Temporary file name used while assembling (`final.file.ldm.merging`).
pub fn merging_path(dest: &Path) -> PathBuf {
    let mut s = dest.as_os_str().to_owned();
    s.push(".ldm.merging");
    PathBuf::from(s)
}

/// Replace the extension (`index.m3u8` → `index.mp4`).
pub fn with_extension(name: &str, ext: &str) -> String {
    let (stem, _) = split_ext(name);
    format!("{stem}.{ext}")
}

/// Separators between a video title and a site name ("Title - YouTube", "Title | Site", "标题_哔哩哔哩_bilibili").
const TITLE_SEPS: &[&str] = &[" - ", " | ", " – ", " — ", " · ", " • ", " :: ", " / ", "_"];

/// Well-known site / brand names that never belong to the video title.
const KNOWN_BRANDS: &[&str] = &[
    "youtube",
    "vimeo",
    "dailymotion",
    "bilibili",
    "哔哩哔哩",
    "twitter",
    "facebook",
    "instagram",
    "tiktok",
    "抖音",
    "twitch",
    "reddit",
    "pornhub",
    "pornhubcom",
    "xvideos",
    "xhamster",
    "youku",
    "优酷",
    "爱奇艺",
    "iqiyi",
    "腾讯视频",
    "西瓜视频",
    "niconico",
    "ニコニコ動画",
    "rumble",
    "odysee",
    "streamable",
    "ted",
    "archiveorg",
    "internetarchive",
];

/// Second-level public suffixes so `bbc.co.uk` → `bbc`.
const SLD_SUFFIXES: &[&str] = &["co", "com", "net", "org", "gov", "edu", "ac", "ne", "or"];

fn norm_brand(s: &str) -> String {
    s.chars().filter(|c| c.is_alphanumeric()).flat_map(|c| c.to_lowercase()).collect()
}

/// Registrable brand label of a host: `www.pornhub.com` → `pornhub`, `news.bbc.co.uk` → `bbc`.
pub fn host_brand(host: &str) -> Option<String> {
    let labels: Vec<&str> = host.trim_end_matches('.').split('.').filter(|l| !l.is_empty()).collect();
    if labels.len() < 2 {
        return labels.first().map(|l| l.to_ascii_lowercase());
    }
    let n = labels.len();
    let idx = if n >= 3 && labels[n - 1].len() == 2 && SLD_SUFFIXES.contains(&labels[n - 2]) { n - 3 } else { n - 2 };
    Some(labels[idx].to_ascii_lowercase())
}

fn is_brand_segment(seg: &str, brand: Option<&str>, host_norm: &str) -> bool {
    let n = norm_brand(seg);
    if n.is_empty() {
        return false;
    }
    // "video Dailymotion", "Watch on Vimeo": a generic word in front of the site name
    for head in BRAND_HEADS {
        if let Some(rest) = n.strip_prefix(head) {
            if !rest.is_empty() && is_brand_norm(rest, brand, host_norm) {
                return true;
            }
        }
    }
    is_brand_norm(&n, brand, host_norm)
}

/// Generic words that may precede a site name in a title suffix.
const BRAND_HEADS: &[&str] = &["video", "videos", "watchon", "on"];

fn is_brand_norm(n: &str, brand: Option<&str>, host_norm: &str) -> bool {
    let n = n.to_string();
    if let Some(b) = brand.filter(|b| b.len() >= 2) {
        // "Pornhub", "Pornhub.com", "www.example.com", "Example Video"? (only brand + optional tld/"tv"/"video")
        if n == b || n == host_norm || host_norm.ends_with(&n) && n.starts_with(b) {
            return true;
        }
        if let Some(rest) = n.strip_prefix(b) {
            if ["com", "net", "org", "tv", "io", "cn", "jp", "co", "couk", "video", "videos", "official"].contains(&rest) {
                return true;
            }
        }
    }
    let known: Vec<String> = KNOWN_BRANDS.iter().map(|k| norm_brand(k)).collect();
    known.iter().any(|k| n == *k || n == format!("{k}com") || n == format!("{k}tv"))
        // two known names glued together ("哔哩哔哩_bilibili" after a "_" split is handled
        // segment by segment; "哔哩哔哩bilibili" written without a separator)
        || known.iter().any(|a| n.starts_with(a.as_str()) && known.iter().any(|b| n.len() == a.len() + b.len() && n.ends_with(b.as_str())))
}

/// Clean a page / tab title for use as a file name (media is named after the video title):
/// drops notification counters like `(3) `, and site names at the end or start
/// (`" - YouTube"`, `" | Example"`, `" – example.com"`, `"_哔哩哔哩_bilibili"`) when they match the
/// page's registrable domain or a known brand.
pub fn clean_title(title: &str, page_url: Option<&str>) -> String {
    let host = page_url.and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    let brand = host_brand(&host);
    let host_norm = norm_brand(&host);
    let mut t: String = title.split_whitespace().collect::<Vec<_>>().join(" ");
    // "(1) ", "(12+) ", "[3] " unread counters
    loop {
        let trimmed = t.trim_start();
        let close = match trimmed.chars().next() {
            Some('(') => ')',
            Some('[') => ']',
            _ => break,
        };
        let Some(end) = trimmed.find(close) else { break };
        let inner = &trimmed[1..end];
        if !inner.is_empty() && inner.trim_end_matches('+').chars().all(|c| c.is_ascii_digit()) {
            t = trimmed[end + 1..].trim_start().to_string();
        } else {
            break;
        }
    }
    // strip brand segments from the end (repeatedly: "标题_哔哩哔哩_bilibili"), then one from the start
    loop {
        let mut cut = None;
        for sep in TITLE_SEPS {
            if let Some(i) = t.rfind(sep) {
                let seg = &t[i + sep.len()..];
                if i > 0 && is_brand_segment(seg, brand.as_deref(), &host_norm) && cut.is_none_or(|c: usize| i > c) {
                    cut = Some(i);
                }
            }
        }
        match cut {
            Some(i) => t = t[..i].trim_end().to_string(),
            None => break,
        }
    }
    for sep in TITLE_SEPS {
        if let Some(i) = t.find(sep) {
            let seg = &t[..i];
            let rest = t[i + sep.len()..].trim();
            if !rest.is_empty() && is_brand_segment(seg, brand.as_deref(), &host_norm) {
                t = rest.to_string();
                break;
            }
        }
    }
    let mut t = t.trim().to_string();
    // "NASA on TikTok" (og:title style): drop a trailing " on <site>"
    if let Some(i) = t.to_lowercase().rfind(" on ") {
        if i > 0 && is_brand_norm(&norm_brand(&t[i + 4..]), brand.as_deref(), &host_norm) {
            t = t[..i].trim_end().to_string();
        }
    }
    // a title that was nothing but the site name, or a login / sign-up dialog's title, is
    // useless as a file name (the caller falls back to og:title / h1 / the URL)
    if is_brand_segment(&t, brand.as_deref(), &host_norm) || is_auth_title(&t) {
        return String::new();
    }
    t
}

/// Tab titles that sites set while a login / sign-up dialog is open.
fn is_auth_title(t: &str) -> bool {
    const AUTH: &[&str] = &[
        "sign up",
        "signup",
        "log in",
        "login",
        "sign in",
        "signin",
        "log in or sign up",
        "sign up or log in",
        "登录",
        "注册",
        "登录/注册",
        "登录或注册",
    ];
    let n = t.trim().to_lowercase();
    AUTH.contains(&n.as_str())
}

/// Linux-only file-name sanitising for names that come from titles or the user rather than
/// from a server: only `/`, `\\` and control characters are replaced (Linux file systems accept
/// `:` `?` `|` …, so a title like `Q&A: why?` stays readable), length capped at 200 bytes on a
/// UTF-8 boundary with the extension preserved.
pub fn sanitize_linux(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    out = out.trim().trim_matches('.').trim().to_string();
    if out.is_empty() || out == ".." || out == "." {
        return sanitize(name);
    }
    if out.len() > MAX_NAME_BYTES {
        let (stem, ext) = split_ext(&out);
        let ext = if ext.len() < 16 { ext } else { "" };
        let mut stem = stem.to_string();
        while stem.len() + ext.len() + 1 > MAX_NAME_BYTES {
            stem.pop();
        }
        out = if ext.is_empty() { stem } else { format!("{stem}.{ext}") };
    }
    out
}

/// File-name stem from a (cleaned) title: path separators and control characters → `_`,
/// unicode (CJK …) kept, at most ~190 bytes cut on a char boundary so `.mp4` etc. still fit.
pub fn title_stem(title: &str) -> String {
    let mut s: String = title
        .chars()
        .map(|c| match c {
            '/' | '\\' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    s = s.trim().trim_matches('.').trim().to_string();
    const MAX: usize = 190;
    if s.len() > MAX {
        let mut end = MAX;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s = s.trim_end().to_string();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("a\\b\\c.txt"), "c.txt");
        assert_eq!(sanitize("we<i>rd:na|me?.zip"), "we_i_rd_na_me_.zip");
        assert!(sanitize("..").starts_with("download_"));
        assert!(sanitize("").starts_with("download_"));
        assert_eq!(sanitize("tab\tname.mp4"), "tab_name.mp4");
        let long = format!("{}.mp4", "x".repeat(400));
        let s = sanitize(&long);
        assert!(s.len() <= 200 && s.ends_with(".mp4"));
    }

    #[test]
    fn disposition() {
        assert_eq!(from_content_disposition("attachment; filename=\"a b.zip\"").unwrap(), "a b.zip");
        assert_eq!(from_content_disposition("attachment; filename=x.bin; filename*=UTF-8''%E4%B8%AD%E6%96%87.bin").unwrap(), "中文.bin");
        assert!(from_content_disposition("inline").is_none());
    }

    #[test]
    fn url_names() {
        assert_eq!(from_url("http://h/a/b%20c.mp4?x=1").unwrap(), "b c.mp4");
        assert!(from_url("http://h/").is_none());
    }

    #[test]
    fn unique() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("f.txt"), b"").unwrap();
        std::fs::write(d.path().join("f (1).txt"), b"").unwrap();
        assert_eq!(unique_path(d.path(), "f.txt", "rename_numbered"), d.path().join("f (2).txt"));
        assert_eq!(unique_path(d.path(), "f.txt", "overwrite"), d.path().join("f.txt"));
        assert_eq!(with_extension("index.m3u8", "mp4"), "index.mp4");
    }

    #[test]
    fn title_cleaning_strips_site_names() {
        let c = |t: &str, u: &str| clean_title(t, Some(u));
        assert_eq!(c("Some Clip Name - Pornhub.com", "https://www.pornhub.com/view_video.php?viewkey=x"), "Some Clip Name");
        assert_eq!(c("(3) Never Gonna Fix It - YouTube", "https://www.youtube.com/watch?v=x"), "Never Gonna Fix It");
        assert_eq!(c("A talk | Vimeo", "https://vimeo.com/1"), "A talk");
        // real-world: Dailymotion's tab title suffix
        assert_eq!(
            c("Passing By | movie | 2026 | Official Trailer - video Dailymotion", "https://www.dailymotion.com/video/x"),
            "Passing By | movie | 2026 | Official Trailer"
        );
        assert_eq!(c("Video games - a history", "https://example.com/v"), "Video games - a history");
        // real-world: TikTok's tab title while its sign-up dialog is open, and its og:title
        assert_eq!(c("Sign up | TikTok", "https://www.tiktok.com/@nasa/video/1"), "");
        assert_eq!(c("NASA on TikTok", "https://www.tiktok.com/@nasa/video/1"), "NASA");
        assert_eq!(c("Rock on Broadway", "https://www.tiktok.com/@x/video/1"), "Rock on Broadway");
        assert_eq!(
            c("News report – BBC News", "https://www.bbc.co.uk/news/x"),
            "News report – BBC News",
            "brand + extra word is not stripped"
        );
        assert_eq!(c("Report – BBC", "https://www.bbc.co.uk/news/x"), "Report");
        assert_eq!(c("我的视频标题_哔哩哔哩_bilibili", "https://www.bilibili.com/video/BV1"), "我的视频标题");
        assert_eq!(c("Example | My Movie", "https://example.org/v/1"), "My Movie", "site name as prefix");
        assert_eq!(c("Cats - Dogs - example.com", "https://www.example.com/a"), "Cats - Dogs", "inner separators kept");
        assert_eq!(c("Rock - Live 1999", "https://music.example.net/"), "Rock - Live 1999", "non-brand suffix kept");
        assert_eq!(c("YouTube", "https://www.youtube.com/"), "", "title that is only the site name");
        assert_eq!(clean_title("  Plain   title ", None), "Plain title");
        assert_eq!(c("[12] Inbox clip - Twitch", "https://www.twitch.tv/v"), "Inbox clip");
        assert_eq!(host_brand("news.bbc.co.uk").as_deref(), Some("bbc"));
        assert_eq!(host_brand("www.pornhub.com").as_deref(), Some("pornhub"));
    }

    #[test]
    fn title_stem_is_linux_safe_and_bounded() {
        assert_eq!(title_stem("AC/DC live\\set\u{7}"), "AC_DC live_set_");
        assert_eq!(title_stem("中文 标题：第1集"), "中文 标题：第1集");
        let long = "長".repeat(200); // 600 bytes
        let s = title_stem(&long);
        assert!(s.len() <= 190 && s.chars().all(|c| c == '長'), "{}", s.len());
        assert_eq!(title_stem(" ..hidden. "), "hidden");
        assert_eq!(sanitize_linux("Q&A: why? <live> | part 1/2.mp4"), "Q&A: why? <live> | part 1_2.mp4");
        let n = sanitize_linux(&format!("{}.mp4", "é".repeat(150)));
        assert!(n.len() <= 200 && n.ends_with(".mp4"));
    }
}
