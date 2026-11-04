//! Daemon-side media classification.
use super::filename;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Hls,
    Dash,
    /// progressive audio/video file
    Progressive,
    /// a single segment of a stream (ts/m4s/…) – usually covered by a manifest
    Segment,
    /// any other file
    File,
}

impl MediaKind {
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Hls => "hls",
            MediaKind::Dash => "dash",
            MediaKind::Progressive => "progressive",
            MediaKind::Segment => "segment",
            MediaKind::File => "file",
        }
    }
}

const PROGRESSIVE_EXTS: &[&str] = &[
    "mp4", "m4v", "webm", "mkv", "mov", "avi", "flv", "mpg", "mpeg", "wmv", "3gp", "ogv", "mp3", "m4a", "aac", "ogg", "opus", "flac",
    "wav", "wma",
];
const SEGMENT_EXTS: &[&str] = &["ts", "m4s", "m4f", "cmfv", "cmfa", "aac"];

pub fn url_ext(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let last = path.rsplit('/').next().unwrap_or("");
    let ext = filename::extension_of(last);
    // Unified Streaming style ".../live.isml/.m3u8" / ".../.mpd": the whole last segment is the extension.
    match last.strip_prefix('.') {
        Some(e) if ext.is_empty() && !e.is_empty() && e.chars().all(|c| c.is_ascii_alphanumeric()) => e.to_ascii_lowercase(),
        _ => ext,
    }
}

/// Classify by URL extension and MIME type.
pub fn classify(url: &str, mime: Option<&str>) -> MediaKind {
    let ext = url_ext(url);
    let mime = mime.unwrap_or("").to_ascii_lowercase();
    if ext == "m3u8" || mime.contains("mpegurl") {
        return MediaKind::Hls;
    }
    if ext == "mpd" || mime.contains("dash+xml") {
        return MediaKind::Dash;
    }
    if SEGMENT_EXTS.contains(&ext.as_str()) && ext != "aac" {
        return MediaKind::Segment;
    }
    if mime == "video/mp2t" {
        return MediaKind::Segment;
    }
    if mime.starts_with("video/") || mime.starts_with("audio/") {
        return MediaKind::Progressive;
    }
    if PROGRESSIVE_EXTS.contains(&ext.as_str()) {
        return MediaKind::Progressive;
    }
    MediaKind::File
}

/// Sniff a manifest body.
pub fn sniff_manifest(body: &[u8]) -> Option<MediaKind> {
    let head = String::from_utf8_lossy(&body[..body.len().min(512)]);
    let t = head.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with("#EXTM3U") {
        Some(MediaKind::Hls)
    } else if t.starts_with("<?xml") && head.contains("<MPD") || t.starts_with("<MPD") {
        Some(MediaKind::Dash)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_only_manifest_segment() {
        assert_eq!(classify("https://h/k8s/live/stable/live.isml/.m3u8", None), MediaKind::Hls);
        assert_eq!(classify("https://h/x.isml/.mpd?t=1", None), MediaKind::Dash);
        assert_eq!(url_ext("https://h/.hidden-file"), "");
    }
    #[test]
    fn table() {
        assert_eq!(classify("http://h/a/index.m3u8?x", None), MediaKind::Hls);
        assert_eq!(classify("http://h/a/x", Some("application/vnd.apple.mpegurl")), MediaKind::Hls);
        assert_eq!(classify("http://h/a/m.mpd", None), MediaKind::Dash);
        assert_eq!(classify("http://h/a/x", Some("application/dash+xml")), MediaKind::Dash);
        assert_eq!(classify("http://h/seg0.ts", None), MediaKind::Segment);
        assert_eq!(classify("http://h/v.mp4", None), MediaKind::Progressive);
        assert_eq!(classify("http://h/v", Some("video/webm")), MediaKind::Progressive);
        assert_eq!(classify("http://h/a.zip", None), MediaKind::File);
        assert_eq!(sniff_manifest(b"#EXTM3U\n#EXT-X-VERSION:3"), Some(MediaKind::Hls));
        assert_eq!(sniff_manifest(b"<?xml version=\"1.0\"?>\n<MPD xmlns"), Some(MediaKind::Dash));
        assert_eq!(sniff_manifest(b"PK\x03\x04"), None);
    }
}
