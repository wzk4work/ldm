//! HLS playlist parser (RFC 8216 subset) – own implementation
//!.
use crate::error::{LdmError, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Variant {
    pub bandwidth: u64,
    pub average_bandwidth: Option<u64>,
    pub resolution: Option<(u32, u32)>,
    pub codecs: Option<String>,
    pub frame_rate: Option<f64>,
    pub audio_group: Option<String>,
    pub uri: String,
}

impl Variant {
    pub fn height(&self) -> u32 {
        self.resolution.map(|r| r.1).unwrap_or(0)
    }
    /// No RESOLUTION and only audio codecs listed (e.g. `CODECS="mp4a.40.2"`): an audio-only variant.
    pub fn is_audio_only(&self) -> bool {
        self.resolution.is_none()
            && self.codecs.as_deref().is_some_and(|c| {
                c.split(',')
                    .map(|x| x.trim().to_ascii_lowercase())
                    .all(|x| ["mp4a", "ac-3", "ec-3", "opus", "flac", "mp3", "alac"].iter().any(|a| x.starts_with(a)))
            })
    }
    /// Quality row for UI lists: `720p · 1400 kbps · avc1…`.
    pub fn label(&self) -> String {
        let mut s = match self.resolution {
            Some((_, h)) => format!("{h}p"),
            None if self.is_audio_only() => "audio".to_string(),
            None => "?p".to_string(),
        };
        s.push_str(&format!(" · {} kbps", self.bandwidth / 1000));
        if let Some(c) = &self.codecs {
            s.push_str(&format!(" · {c}"));
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Rendition {
    pub kind: String,
    pub group_id: String,
    pub name: Option<String>,
    pub language: Option<String>,
    pub default: bool,
    /// EXT-X-MEDIA CHANNELS (e.g. "2", "6"); None when the attribute is absent.
    pub channels: Option<u32>,
    pub uri: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MasterPlaylist {
    pub variants: Vec<Variant>,
    pub renditions: Vec<Rendition>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum KeyMethod {
    None,
    Aes128,
    Other(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Key {
    pub method: KeyMethod,
    pub uri: Option<String>,
    pub iv: Option<[u8; 16]>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InitSection {
    pub uri: String,
    /// (offset, length)
    pub byterange: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Segment {
    pub uri: String,
    pub duration: f64,
    /// (offset, length)
    pub byterange: Option<(u64, u64)>,
    pub key: Option<Key>,
    pub map: Option<InitSection>,
    pub seq: u64,
    pub discontinuity: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct MediaPlaylist {
    pub target_duration: f64,
    pub media_sequence: u64,
    pub segments: Vec<Segment>,
    pub end_list: bool,
    pub playlist_type: Option<String>,
}

impl MediaPlaylist {
    pub fn duration(&self) -> f64 {
        self.segments.iter().map(|s| s.duration).sum()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playlist {
    Master(MasterPlaylist),
    Media(MediaPlaylist),
}

/// Resolve a (possibly relative) URI against the playlist URL.
pub fn resolve(base: &str, rel: &str) -> String {
    match url::Url::parse(base).and_then(|b| b.join(rel)) {
        Ok(u) => u.to_string(),
        Err(_) => rel.to_string(),
    }
}

/// Parse an attribute list (`A=1,B="x,y",C=0x1F`), honouring quoted strings.
pub fn parse_attrs(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        while i < b.len() && (b[i] == b',' || b[i] == b' ') {
            i += 1;
        }
        let ks = i;
        while i < b.len() && b[i] != b'=' && b[i] != b',' {
            i += 1;
        }
        let key = s[ks..i].trim().to_string();
        if i >= b.len() || b[i] != b'=' {
            if !key.is_empty() {
                out.push((key, String::new()));
            }
            continue;
        }
        i += 1;
        let val = if i < b.len() && b[i] == b'"' {
            i += 1;
            let vs = i;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            let v = s[vs..i].to_string();
            i += 1;
            v
        } else {
            let vs = i;
            while i < b.len() && b[i] != b',' {
                i += 1;
            }
            s[vs..i].trim().to_string()
        };
        out.push((key, val));
    }
    out
}

fn attr<'a>(a: &'a [(String, String)], k: &str) -> Option<&'a str> {
    a.iter().find(|(x, _)| x == k).map(|(_, v)| v.as_str())
}

pub fn parse_iv(s: &str) -> Option<[u8; 16]> {
    let h = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    let bytes = hex::decode(format!("{:0>32}", h)).ok()?;
    bytes.try_into().ok()
}

/// IV derived from the media sequence number (RFC 8216 §5.2).
pub fn sequence_iv(seq: u64) -> [u8; 16] {
    let mut iv = [0u8; 16];
    iv[8..].copy_from_slice(&seq.to_be_bytes());
    iv
}

fn parse_byterange(v: &str) -> Option<(u64, Option<u64>)> {
    let (len, off) = match v.split_once('@') {
        Some((l, o)) => (l, Some(o.trim().parse().ok()?)),
        None => (v, None),
    };
    Some((len.trim().parse().ok()?, off))
}

pub fn parse(text: &str, base_url: &str) -> Result<Playlist> {
    let text = text.trim_start_matches('\u{feff}');
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(LdmError::Format("not an M3U8 playlist (missing #EXTM3U)".into()));
    }
    let lines: Vec<&str> = lines.collect();
    if lines.iter().any(|l| l.starts_with("#EXT-X-STREAM-INF")) {
        return Ok(Playlist::Master(parse_master(&lines, base_url)));
    }
    Ok(Playlist::Media(parse_media(&lines, base_url)?))
}

fn parse_master(lines: &[&str], base: &str) -> MasterPlaylist {
    let mut m = MasterPlaylist::default();
    let mut pending: Option<Vec<(String, String)>> = None;
    for l in lines {
        if let Some(a) = l.strip_prefix("#EXT-X-STREAM-INF:") {
            pending = Some(parse_attrs(a));
        } else if let Some(a) = l.strip_prefix("#EXT-X-MEDIA:") {
            let a = parse_attrs(a);
            m.renditions.push(Rendition {
                kind: attr(&a, "TYPE").unwrap_or("").to_string(),
                group_id: attr(&a, "GROUP-ID").unwrap_or("").to_string(),
                name: attr(&a, "NAME").map(String::from),
                language: attr(&a, "LANGUAGE").map(String::from),
                default: attr(&a, "DEFAULT") == Some("YES"),
                channels: attr(&a, "CHANNELS").and_then(|v| v.split('/').next()?.parse().ok()),
                uri: attr(&a, "URI").map(|u| resolve(base, u)),
            });
        } else if !l.starts_with('#') {
            if let Some(a) = pending.take() {
                m.variants.push(Variant {
                    bandwidth: attr(&a, "BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0),
                    average_bandwidth: attr(&a, "AVERAGE-BANDWIDTH").and_then(|v| v.parse().ok()),
                    resolution: attr(&a, "RESOLUTION").and_then(|r| {
                        let (w, h) = r.split_once(['x', 'X'])?;
                        Some((w.parse().ok()?, h.parse().ok()?))
                    }),
                    codecs: attr(&a, "CODECS").map(String::from),
                    frame_rate: attr(&a, "FRAME-RATE").and_then(|v| v.parse().ok()),
                    audio_group: attr(&a, "AUDIO").map(String::from),
                    uri: resolve(base, l),
                });
            }
        }
    }
    m
}

fn parse_media(lines: &[&str], base: &str) -> Result<MediaPlaylist> {
    let mut p = MediaPlaylist::default();
    let mut key: Option<Key> = None;
    let mut map: Option<InitSection> = None;
    let mut dur = 0.0;
    let mut br: Option<(u64, Option<u64>)> = None;
    let mut disc = false;
    let mut seq_started = false;
    let mut seq = 0u64;
    let mut last_end: std::collections::HashMap<String, u64> = Default::default();
    for l in lines {
        if let Some(v) = l.strip_prefix("#EXT-X-TARGETDURATION:") {
            p.target_duration = v.trim().parse().unwrap_or(0.0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            p.media_sequence = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-PLAYLIST-TYPE:") {
            p.playlist_type = Some(v.trim().to_string());
        } else if let Some(v) = l.strip_prefix("#EXTINF:") {
            dur = v.split(',').next().unwrap_or("0").trim().parse().unwrap_or(0.0);
        } else if let Some(v) = l.strip_prefix("#EXT-X-BYTERANGE:") {
            br = Some(parse_byterange(v).ok_or_else(|| LdmError::Format(format!("bad BYTERANGE {v}")))?);
        } else if l.starts_with("#EXT-X-DISCONTINUITY") && !l.starts_with("#EXT-X-DISCONTINUITY-SEQUENCE") {
            disc = true;
        } else if let Some(v) = l.strip_prefix("#EXT-X-KEY:") {
            let a = parse_attrs(v);
            let method = match attr(&a, "METHOD").unwrap_or("NONE") {
                "NONE" => KeyMethod::None,
                "AES-128" => KeyMethod::Aes128,
                o => KeyMethod::Other(o.to_string()),
            };
            key = if method == KeyMethod::None {
                None
            } else {
                Some(Key { method, uri: attr(&a, "URI").map(|u| resolve(base, u)), iv: attr(&a, "IV").and_then(parse_iv) })
            };
        } else if let Some(v) = l.strip_prefix("#EXT-X-MAP:") {
            let a = parse_attrs(v);
            let uri = attr(&a, "URI").ok_or_else(|| LdmError::Format("EXT-X-MAP without URI".into()))?;
            let byterange = attr(&a, "BYTERANGE").and_then(parse_byterange).map(|(len, off)| (off.unwrap_or(0), len));
            map = Some(InitSection { uri: resolve(base, uri), byterange });
        } else if *l == "#EXT-X-ENDLIST" {
            p.end_list = true;
        } else if !l.starts_with('#') {
            if !seq_started {
                seq = p.media_sequence;
                seq_started = true;
            }
            let uri = resolve(base, l);
            let byterange = br.take().map(|(len, off)| {
                let off = off.unwrap_or_else(|| *last_end.get(&uri).unwrap_or(&0));
                last_end.insert(uri.clone(), off + len);
                (off, len)
            });
            p.segments.push(Segment { uri, duration: dur, byterange, key: key.clone(), map: map.clone(), seq, discontinuity: disc });
            seq += 1;
            dur = 0.0;
            disc = false;
        }
    }
    if p.playlist_type.as_deref() == Some("VOD") {
        p.end_list = true;
    }
    Ok(p)
}

/// Rank a video variant under the audio preference (higher is better).
fn audio_rank(v: &Variant, renditions: &[Rendition], pref: &crate::media::AudioPreference) -> (u8, u32) {
    use crate::media::AudioPreference as P;
    let group: Vec<&Rendition> =
        v.audio_group.as_ref().map(|g| renditions.iter().filter(|r| r.kind == "AUDIO" && &r.group_id == g).collect()).unwrap_or_default();
    let channels = group.iter().filter_map(|r| r.channels).max().unwrap_or(0);
    let lang_hit = match pref {
        P::Language(code) => group.iter().any(|r| r.language.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(code))) as u8,
        _ => 0,
    };
    let tier = crate::media::AudioPreference::codec_tier(v.codecs.as_deref());
    match pref {
        P::Aac => (tier, channels),
        P::MostChannels => (channels.min(255) as u8, tier as u32),
        P::Language(_) => (lang_hit.saturating_mul(4).saturating_add(tier), channels),
    }
}

/// Within one AUDIO group: language filter (when set) → DEFAULT=YES → first.
pub fn select_audio_rendition<'a>(group: &[&'a Rendition], pref: &crate::media::AudioPreference) -> Option<&'a Rendition> {
    use crate::media::AudioPreference as P;
    let filtered: Vec<&Rendition> = match pref {
        P::Language(code) => {
            let m: Vec<_> = group.iter().copied().filter(|r| r.language.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(code))).collect();
            if m.is_empty() {
                group.to_vec()
            } else {
                m
            }
        }
        _ => group.to_vec(),
    };
    filtered.iter().copied().find(|r| r.default).or_else(|| filtered.first().copied())
}

/// Pick a video variant: height ≤ cap (or closest when the user asked for a specific height),
/// then apply `audio` preference (AAC by default), then bandwidth.
pub fn select_variant<'a>(
    variants: &'a [Variant],
    renditions: &[Rendition],
    max_height: u32,
    preferred: Option<u32>,
    audio: &crate::media::AudioPreference,
) -> Option<&'a Variant> {
    let cap = preferred.unwrap_or(max_height);
    let video: Vec<&Variant> = variants.iter().filter(|v| !v.is_audio_only()).collect();
    let pool: Vec<&Variant> = if video.is_empty() { variants.iter().collect() } else { video };
    let under: Vec<&Variant> = pool.iter().copied().filter(|v| cap == 0 || v.height() <= cap).collect();
    if under.is_empty() {
        return if preferred.is_some() {
            // explicit height below every rendition → smallest, then audio pref among ties
            let min_h = pool.iter().map(|v| v.height()).min()?;
            pool.iter().copied().filter(|v| v.height() == min_h).max_by_key(|v| {
                let (a, b) = audio_rank(v, renditions, audio);
                (a, b, v.bandwidth)
            })
        } else {
            // configured default cap with nothing under it → global highest bandwidth
            pool.into_iter().max_by_key(|v| v.bandwidth)
        };
    }
    // height first (pick the best video quality), then audio preference among that height
    under.into_iter().max_by_key(|v| {
        let (a, b) = audio_rank(v, renditions, audio);
        (v.height(), a, b, v.bandwidth)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MASTER: &str = "#EXTM3U\n#EXT-X-VERSION:4\n\
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"audio/en.m3u8\"\n\
#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=640x360,CODECS=\"avc1.4d401e,mp4a.40.2\",AUDIO=\"aud\"\nlow/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280x720,CODECS=\"avc1.4d401f,mp4a.40.2\",AUDIO=\"aud\"\nmid/index.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=9000000,RESOLUTION=3840x2160\nhttps://cdn.example/4k.m3u8\n";

    /// Apple bipbop–shaped master: same video URI × AAC / AC-3 / E-AC-3.
    /// Default preference must pick AAC.
    #[test]
    fn bipbop_style_prefers_aac_then_ec3_then_ac3() {
        let t = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud1",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="2",URI="a1/prog_index.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud2",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="6",URI="a2/prog_index.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud3",NAME="English",LANGUAGE="en",DEFAULT=YES,AUTOSELECT=YES,CHANNELS="6",URI="a3/prog_index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=541052,CODECS="avc1.640015,mp4a.40.2",RESOLUTION=480x270,AUDIO="aud1"
v2/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=902298,CODECS="avc1.64001e,mp4a.40.2",RESOLUTION=640x360,AUDIO="aud1"
v3/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=763555,CODECS="avc1.640015,ac-3",RESOLUTION=480x270,AUDIO="aud2"
v2/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=1124801,CODECS="avc1.64001e,ac-3",RESOLUTION=640x360,AUDIO="aud2"
v3/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=571555,CODECS="avc1.640015,ec-3",RESOLUTION=480x270,AUDIO="aud3"
v2/prog_index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=932298,CODECS="avc1.64001e,ec-3",RESOLUTION=640x360,AUDIO="aud3"
v3/prog_index.m3u8
"#;
        let Playlist::Master(m) = parse(t, "http://h/master.m3u8").unwrap() else { panic!() };
        assert_eq!(m.renditions.len(), 3);
        assert_eq!(m.renditions[0].channels, Some(2));
        assert_eq!(m.renditions[1].channels, Some(6));
        let aac = crate::media::AudioPreference::Aac;
        let v = select_variant(&m.variants, &m.renditions, 1080, Some(270), &aac).unwrap();
        assert_eq!(v.audio_group.as_deref(), Some("aud1"));
        assert_eq!(v.height(), 270);
        assert!(v.codecs.as_deref().unwrap().contains("mp4a"));
        // without an explicit height, still AAC at the highest video under the default cap
        let v = select_variant(&m.variants, &m.renditions, 1080, None, &aac).unwrap();
        assert_eq!(v.audio_group.as_deref(), Some("aud1"));
        assert_eq!(v.height(), 360);
        // most_channels → AC-3 or E-AC-3 (both 6 ch); AC-3 has higher bandwidth at 270p
        let mc = crate::media::AudioPreference::MostChannels;
        let v = select_variant(&m.variants, &m.renditions, 1080, Some(270), &mc).unwrap();
        assert!(matches!(v.audio_group.as_deref(), Some("aud2") | Some("aud3")), "{:?}", v.audio_group);
        // only AC-3 left
        let only = r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud2",NAME="English",LANGUAGE="en",DEFAULT=YES,CHANNELS="6",URI="a2/prog_index.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=763555,CODECS="avc1.640015,ac-3",RESOLUTION=480x270,AUDIO="aud2"
v2/prog_index.m3u8
"#;
        let Playlist::Master(m2) = parse(only, "http://h/m.m3u8").unwrap() else { panic!() };
        let v = select_variant(&m2.variants, &m2.renditions, 1080, Some(270), &aac).unwrap();
        assert_eq!(v.audio_group.as_deref(), Some("aud2"));
        let group: Vec<_> = m.renditions.iter().filter(|r| r.group_id == "aud1").collect();
        assert_eq!(select_audio_rendition(&group, &aac).unwrap().uri.as_deref(), Some("http://h/a1/prog_index.m3u8"));
    }

    #[test]
    fn selection_skips_audio_only_and_falls_back_low() {
        let t = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=494000,CODECS=\"mp4a.40.2,avc1.42C00D\",RESOLUTION=224x100\nv100.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2469000,CODECS=\"mp4a.40.2,avc1.640028\",RESOLUTION=1680x750\nv750.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=137000,CODECS=\"mp4a.40.2\"\na.m3u8\n";
        let Playlist::Master(m) = parse(t, "http://h/m.m3u8").unwrap() else { panic!() };
        assert!(m.variants[2].is_audio_only() && !m.variants[0].is_audio_only());
        assert!(m.variants[2].label().starts_with("audio"));
        let pref = crate::media::AudioPreference::Aac;
        assert_eq!(select_variant(&m.variants, &m.renditions, 0, Some(90), &pref).unwrap().uri, "http://h/v100.m3u8");
        assert_eq!(select_variant(&m.variants, &m.renditions, 0, Some(480), &pref).unwrap().uri, "http://h/v100.m3u8");
        assert_eq!(select_variant(&m.variants, &m.renditions, 0, None, &pref).unwrap().uri, "http://h/v750.m3u8");
    }

    #[test]
    fn master_parsing_and_selection() {
        let Playlist::Master(m) = parse(MASTER, "http://h/v/master.m3u8").unwrap() else { panic!() };
        assert_eq!(m.variants.len(), 3);
        assert_eq!(m.variants[0].uri, "http://h/v/low/index.m3u8");
        assert_eq!(m.variants[1].codecs.as_deref(), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(m.variants[2].uri, "https://cdn.example/4k.m3u8");
        assert_eq!(m.renditions[0].uri.as_deref(), Some("http://h/v/audio/en.m3u8"));
        assert!(m.renditions[0].default);
        let pref = crate::media::AudioPreference::Aac;
        assert_eq!(select_variant(&m.variants, &m.renditions, 1080, None, &pref).unwrap().height(), 720);
        assert_eq!(select_variant(&m.variants, &m.renditions, 1080, Some(360), &pref).unwrap().height(), 360);
        assert_eq!(select_variant(&m.variants, &m.renditions, 0, None, &pref).unwrap().height(), 2160);
        assert_eq!(select_variant(&m.variants, &m.renditions, 100, None, &pref).unwrap().bandwidth, 9000000);
        assert_eq!(m.variants[1].label(), "720p · 2800 kbps · avc1.4d401f,mp4a.40.2");
    }

    #[test]
    fn media_with_key_byterange_map() {
        let t = "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:7\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
#EXT-X-KEY:METHOD=AES-128,URI=\"k.key\",IV=0x000102030405060708090a0b0c0d0e0f\n#EXTINF:4.0,\n#EXT-X-BYTERANGE:1000@720\nall.mp4\n\
#EXTINF:4.0,\n#EXT-X-BYTERANGE:500\nall.mp4\n#EXT-X-KEY:METHOD=NONE\n#EXT-X-DISCONTINUITY\n#EXTINF:2.5,\nc.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(t, "http://h/p/x.m3u8").unwrap() else { panic!() };
        assert!(m.end_list);
        assert_eq!(m.segments.len(), 3);
        assert_eq!(m.segments[0].seq, 7);
        assert_eq!(m.segments[0].byterange, Some((720, 1000)));
        assert_eq!(m.segments[1].byterange, Some((1720, 500)));
        assert_eq!(m.segments[0].key.as_ref().unwrap().method, KeyMethod::Aes128);
        assert_eq!(m.segments[0].key.as_ref().unwrap().iv.unwrap()[15], 0x0f);
        assert_eq!(m.segments[0].key.as_ref().unwrap().uri.as_deref(), Some("http://h/p/k.key"));
        assert!(m.segments[2].key.is_none());
        assert!(m.segments[2].discontinuity);
        assert_eq!(m.segments[0].map.as_ref().unwrap().byterange, Some((0, 720)));
        assert!((m.duration() - 10.5).abs() < 1e-9);
    }

    #[test]
    fn live_and_errors() {
        let Playlist::Media(m) = parse("#EXTM3U\n#EXTINF:2,\na.ts\n", "http://h/").unwrap() else { panic!() };
        assert!(!m.end_list);
        assert!(parse("<html>", "http://h/").is_err());
        let Playlist::Media(m) =
            parse("#EXTM3U\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"x\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n", "http://h/").unwrap()
        else {
            panic!()
        };
        assert!(matches!(m.segments[0].key.as_ref().unwrap().method, KeyMethod::Other(_)));
        assert_eq!(sequence_iv(1)[15], 1);
        assert_eq!(parse_iv("0x1").unwrap()[15], 1);
    }

    #[test]
    fn captured_sample_playlist() {
        let t = "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:2.000000,\nhttp://origin.invalid:8765/share/sample/hls/seg0.ts\n#EXT-X-ENDLIST\n";
        let Playlist::Media(m) = parse(t, "http://origin.invalid:8765/share/sample/hls/index.m3u8").unwrap() else { panic!() };
        assert_eq!(m.segments.len(), 1);
        assert_eq!(m.segments[0].uri, "http://origin.invalid:8765/share/sample/hls/seg0.ts");
    }
}
