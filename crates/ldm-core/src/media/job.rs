//! Segmented manifest job.
use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;
use tracing::{info, warn};

use super::{dash, hls, mux};
use crate::engine::{resolve_output, JobEnv, OutputSpec};
use crate::error::{LdmError, Result};
use crate::http::{HttpClient, RequestCtx};
use crate::paths::ensure_private_dir;
use crate::util::classify::{self, MediaKind};
use crate::util::filename;

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

const MANIFEST_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyRef {
    pub uri: String,
    pub iv: [u8; 16],
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaSeg {
    pub url: String,
    /// inclusive byte range
    pub range: Option<(u64, u64)>,
    pub key: Option<KeyRef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Container {
    Ts,
    Fmp4,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// `muxed` | `video` | `audio`
    pub kind: String,
    pub label: String,
    pub init: Option<MediaSeg>,
    pub segments: Vec<MediaSeg>,
    pub container: Container,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QualityRow {
    pub label: String,
    pub height: Option<u32>,
    pub bandwidth: u64,
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SubtitleTrack {
    pub lang: Option<String>,
    pub name: Option<String>,
    /// WebVTT segment (or whole-file) URLs in order.
    pub segments: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MediaPlan {
    /// `hls` | `dash`
    pub source: String,
    pub tracks: Vec<Track>,
    pub duration: Option<f64>,
    pub qualities: Vec<QualityRow>,
    pub live: bool,
    /// Subtitle tracks saved as `.vtt` sidecar files next to the video.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub subtitles: Vec<SubtitleTrack>,
    /// Live HLS: the media playlist that is polled while recording.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist_url: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct MediaJobSpec {
    pub url: String,
    pub ctx: RequestCtx,
    pub work_dir: PathBuf,
    pub output: OutputSpec,
    pub filename_hint: Option<String>,
    pub preferred_height: Option<u32>,
    /// Kind `av`: `url` is a whole video file and this the matching whole audio file.
    pub audio_url: Option<String>,
}

#[derive(Debug, Clone)]
pub struct MediaOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub segments: u64,
    pub source: String,
    pub remuxed: bool,
    pub extra_files: Vec<PathBuf>,
}

fn seg_from_hls(s: &hls::Segment) -> Result<MediaSeg> {
    let key = match &s.key {
        None => None,
        Some(k) => match &k.method {
            hls::KeyMethod::None => None,
            hls::KeyMethod::Aes128 => Some(KeyRef {
                uri: k.uri.clone().ok_or_else(|| LdmError::Key("AES-128 key without URI".into()))?,
                iv: k.iv.unwrap_or_else(|| hls::sequence_iv(s.seq)),
            }),
            hls::KeyMethod::Other(m) => return Err(LdmError::UnsupportedKey(format!("EXT-X-KEY METHOD={m}"))),
        },
    };
    Ok(MediaSeg { url: s.uri.clone(), range: s.byterange.map(|(o, l)| (o, o + l.max(1) - 1)), key })
}

fn track_from_media(pl: &hls::MediaPlaylist, kind: &str, label: String) -> Result<Track> {
    if !pl.end_list {
        return Err(LdmError::Format("live HLS playlist (no EXT-X-ENDLIST) is not supported".into()));
    }
    track_any(pl, kind, label)
}

/// Track from a playlist window, live or not.
fn track_any(pl: &hls::MediaPlaylist, kind: &str, label: String) -> Result<Track> {
    if pl.segments.is_empty() {
        return Err(LdmError::Format("media playlist has no segments".into()));
    }
    let init = pl.segments[0].map.as_ref().map(|m| MediaSeg {
        url: m.uri.clone(),
        range: m.byterange.map(|(o, l)| (o, o + l.max(1) - 1)),
        key: None,
    });
    let container = if init.is_some() { Container::Fmp4 } else { Container::Ts };
    let segments = pl.segments.iter().map(seg_from_hls).collect::<Result<Vec<_>>>()?;
    Ok(Track { kind: kind.into(), label, init, segments, container })
}

async fn fetch_text(client: &HttpClient, url: &str, ctx: &RequestCtx) -> Result<(String, String)> {
    let (b, final_url) = client.get_bytes(url, ctx, None, MANIFEST_LIMIT).await?;
    Ok((String::from_utf8_lossy(&b).into_owned(), final_url))
}

/// Build a download plan for an HLS or DASH URL.
pub async fn plan_media(
    client: &HttpClient,
    url: &str,
    ctx: &RequestCtx,
    max_height: u32,
    preferred: Option<u32>,
    audio: &crate::media::AudioPreference,
) -> Result<MediaPlan> {
    let (text, final_url) = fetch_text(client, url, ctx).await?;
    let kind = classify::sniff_manifest(text.as_bytes()).unwrap_or_else(|| classify::classify(url, None));
    match kind {
        MediaKind::Hls => plan_hls(client, &text, &final_url, ctx, max_height, preferred, audio).await,
        MediaKind::Dash => plan_dash(&text, &final_url, max_height, preferred, audio),
        _ => Err(LdmError::Format("URL is neither an HLS nor a DASH manifest".into())),
    }
}

async fn plan_hls(
    client: &HttpClient,
    text: &str,
    url: &str,
    ctx: &RequestCtx,
    max_height: u32,
    preferred: Option<u32>,
    audio: &crate::media::AudioPreference,
) -> Result<MediaPlan> {
    match hls::parse(text, url)? {
        hls::Playlist::Media(pl) => {
            if !pl.end_list {
                return Ok(MediaPlan { source: "hls".into(), live: true, playlist_url: Some(url.to_string()), ..Default::default() });
            }
            let duration = Some(pl.duration());
            let t = track_from_media(&pl, "muxed", "default".into())?;
            Ok(MediaPlan {
                source: "hls".into(),
                tracks: vec![t],
                duration,
                qualities: vec![],
                live: false,
                playlist_url: None,
                ..Default::default()
            })
        }
        hls::Playlist::Master(m) => {
            // one row per video height (no audio-track picker); keep preferred-audio label
            let mut qualities: Vec<QualityRow> = vec![];
            let mut seen_h = BTreeSet::new();
            let mut ranked: Vec<&hls::Variant> = m.variants.iter().filter(|v| !v.is_audio_only()).collect();
            ranked.sort_by_key(|v| std::cmp::Reverse((v.height(), v.bandwidth)));
            for v in &ranked {
                let h = v.height();
                if !seen_h.insert(h) {
                    continue;
                }
                // re-pick under this exact height with the audio preference so the label matches what we download
                let pick = hls::select_variant(&m.variants, &m.renditions, h, Some(h), audio).unwrap_or(v);
                qualities.push(QualityRow {
                    label: pick.label(),
                    height: pick.resolution.map(|r| r.1),
                    bandwidth: pick.bandwidth,
                    kind: "video".into(),
                });
            }
            for v in m.variants.iter().filter(|v| v.is_audio_only()) {
                qualities.push(QualityRow { label: v.label(), height: None, bandwidth: v.bandwidth, kind: "audio".into() });
            }
            // separate audio renditions can be fetched alone ("仅音频", preferred height 0)
            if let Some(r) = m.renditions.iter().find(|r| r.kind == "AUDIO" && r.uri.is_some()) {
                qualities.push(QualityRow {
                    label: r.name.clone().unwrap_or_else(|| "audio".into()),
                    height: None,
                    bandwidth: 0,
                    kind: "audio".into(),
                });
            }
            let v = hls::select_variant(&m.variants, &m.renditions, max_height, preferred, audio)
                .ok_or_else(|| LdmError::Format("master playlist has no variants".into()))?;
            info!(variant = %v.label(), uri = %v.uri, audio_group = ?v.audio_group, "selected HLS variant");
            let (vt, vurl) = fetch_text(client, &v.uri, ctx).await?;
            let hls::Playlist::Media(vpl) = hls::parse(&vt, &vurl)? else {
                return Err(LdmError::Format("variant URI points to another master playlist".into()));
            };
            if !vpl.end_list {
                // live variant: recorded by polling (separate audio renditions are not merged while live)
                return Ok(MediaPlan { source: "hls".into(), live: true, playlist_url: Some(vurl), qualities, ..Default::default() });
            }
            let duration = Some(vpl.duration());
            let mut tracks = vec![];
            // separate audio rendition for the chosen AUDIO group (DEFAULT=YES, language filter when set)
            let audio_url = v.audio_group.as_ref().and_then(|g| {
                let group: Vec<&hls::Rendition> = m.renditions.iter().filter(|r| &r.group_id == g && r.kind == "AUDIO").collect();
                hls::select_audio_rendition(&group, audio).and_then(|r| r.uri.clone())
            });
            match audio_url {
                Some(aurl) => {
                    tracks.push(track_from_media(&vpl, "video", v.label())?);
                    let (at, aurl2) = fetch_text(client, &aurl, ctx).await?;
                    let hls::Playlist::Media(apl) = hls::parse(&at, &aurl2)? else {
                        return Err(LdmError::Format("audio rendition is not a media playlist".into()));
                    };
                    tracks.push(track_from_media(&apl, "audio", "audio".into())?);
                }
                None => tracks.push(track_from_media(&vpl, "muxed", v.label())?),
            }
            let tracks = audio_only_filter(tracks, preferred)?;
            let mut subtitles = vec![];
            let mut langs = BTreeSet::new();
            for r in m.renditions.iter().filter(|r| r.kind == "SUBTITLES") {
                let Some(su) = &r.uri else { continue };
                if !langs.insert(r.language.clone().or(r.name.clone()).unwrap_or_default()) {
                    continue;
                }
                // a missing subtitle playlist never fails the video
                match fetch_text(client, su, ctx).await.and_then(|(t, u)| hls::parse(&t, &u)) {
                    Ok(hls::Playlist::Media(spl)) => subtitles.push(SubtitleTrack {
                        lang: r.language.clone(),
                        name: r.name.clone(),
                        segments: spl.segments.iter().map(|x| x.uri.clone()).collect(),
                    }),
                    Ok(_) => {}
                    Err(e) => warn!(err = %e, "subtitle playlist skipped"),
                }
            }
            Ok(MediaPlan { source: "hls".into(), tracks, duration, qualities, live: false, playlist_url: None, subtitles })
        }
    }
}

/// `preferred_height == Some(0)` means "audio only": keep just the separate audio track.
fn audio_only_filter(tracks: Vec<Track>, preferred: Option<u32>) -> Result<Vec<Track>> {
    if preferred != Some(0) {
        return Ok(tracks);
    }
    let audio: Vec<Track> = tracks.into_iter().filter(|t| t.kind == "audio").collect();
    if audio.is_empty() {
        return Err(LdmError::Format("this stream has no separate audio-only rendition".into()));
    }
    Ok(audio)
}

/// Tracks (video + audio) chosen from a DASH period.
fn dash_tracks(
    period: &dash::Period,
    max_height: u32,
    preferred: Option<u32>,
    audio: &crate::media::AudioPreference,
) -> Result<Vec<Track>> {
    let sel = dash::select(period, max_height, preferred, audio);
    if sel.is_empty() {
        return Err(LdmError::Format("no downloadable representation in MPD".into()));
    }
    let mut tracks = vec![];
    for (a, r) in sel {
        if a.protected {
            return Err(LdmError::UnsupportedKey("DRM ContentProtection present".into()));
        }
        let conv = |s: &dash::SegRef| MediaSeg { url: s.url.clone(), range: s.range, key: None };
        let kind = if a.content_type == "audio" { "audio" } else { "video" };
        tracks.push(Track {
            kind: kind.into(),
            label: r.label(),
            init: r.init.as_ref().map(conv),
            segments: r.segments.iter().map(conv).collect(),
            container: Container::Fmp4,
        });
    }
    Ok(tracks)
}

fn plan_dash(text: &str, url: &str, max_height: u32, preferred: Option<u32>, audio: &crate::media::AudioPreference) -> Result<MediaPlan> {
    let mpd = dash::parse(text, url)?;
    let period = mpd.periods.first().ok_or_else(|| LdmError::Format("MPD has no Period".into()))?;
    let qualities = period
        .adaptation_sets
        .iter()
        .flat_map(|a| {
            a.representations.iter().map(move |r| QualityRow {
                label: r.label(),
                height: r.height,
                bandwidth: r.bandwidth,
                kind: a.content_type.clone(),
            })
        })
        .collect();
    if mpd.dynamic {
        return Ok(MediaPlan { source: "dash".into(), qualities, live: true, playlist_url: Some(url.to_string()), ..Default::default() });
    }
    let mut tracks = audio_only_filter(dash_tracks(period, max_height, preferred, audio)?, preferred)?;
    if tracks.len() == 1 && tracks[0].kind != "audio" {
        tracks[0].kind = "muxed".into();
    }
    let mut langs = BTreeSet::new();
    let subtitles = period
        .adaptation_sets
        .iter()
        .filter(|a| a.content_type == "text")
        .filter_map(|a| {
            // WebVTT, TTML or SRT as plain files (TTML inside MP4 "stpp" segments is not extracted)
            let r = a.representations.iter().find(|r| {
                r.mime
                    .as_deref()
                    .map(|m| m.contains("vtt") || m.contains("ttml") || m.contains("subrip") || m == "text/plain")
                    .unwrap_or(true)
                    && r.init.is_none()
            })?;
            langs.insert(a.lang.clone().unwrap_or_default()).then(|| SubtitleTrack {
                lang: a.lang.clone(),
                name: None,
                segments: r.segments.iter().map(|x| x.url.clone()).collect(),
            })
        })
        .collect();
    Ok(MediaPlan {
        source: "dash".into(),
        tracks,
        duration: period.duration.or(mpd.duration),
        qualities,
        live: false,
        playlist_url: None,
        subtitles,
    })
}

/// Plan for a video file + a separate audio file (each one whole fMP4/MP4 file, as seen in
/// normal page traffic): two single-segment tracks that are merged like DASH tracks.
pub fn av_plan(video_url: &str, audio_url: &str) -> MediaPlan {
    let one = |kind: &str, url: &str| Track {
        kind: kind.into(),
        label: kind.into(),
        init: None,
        segments: vec![MediaSeg { url: url.to_string(), range: None, key: None }],
        container: Container::Fmp4,
    };
    MediaPlan { source: "av".into(), tracks: vec![one("video", video_url), one("audio", audio_url)], ..Default::default() }
}

/// Quality list for UI (`MediaProbe`), without downloading media.
pub async fn probe_media(
    client: &HttpClient,
    url: &str,
    ctx: &RequestCtx,
    max_height: u32,
    audio: &crate::media::AudioPreference,
) -> Result<MediaPlan> {
    match plan_media(client, url, ctx, max_height, None, audio).await {
        Ok(p) => Ok(p),
        Err(LdmError::Format(m)) if m.contains("live") => Ok(MediaPlan { source: "hls".into(), live: true, ..Default::default() }),
        Err(e) => Err(e),
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MediaState {
    url: String,
    signature: String,
    done: Vec<BTreeSet<usize>>,
}

fn signature(plan: &MediaPlan) -> String {
    let mut s = plan.source.clone();
    for t in &plan.tracks {
        s.push_str(&format!("|{}:{}:{}", t.kind, t.segments.len(), t.segments.first().map(|x| x.url.as_str()).unwrap_or("")));
    }
    s
}

fn seg_path(dir: &Path, track: usize, idx: usize) -> PathBuf {
    dir.join(format!("t{track}_{idx:05}.seg"))
}

fn init_path(dir: &Path, track: usize) -> PathBuf {
    dir.join(format!("t{track}_init.seg"))
}

fn decrypt(key: &[u8], iv: &[u8; 16], data: &[u8]) -> Result<Vec<u8>> {
    if key.len() != 16 {
        return Err(LdmError::Key(format!("AES-128 key must be 16 bytes, got {}", key.len())));
    }
    let dec = Aes128CbcDec::new_from_slices(key, iv).map_err(|e| LdmError::Key(e.to_string()))?;
    let mut buf = data.to_vec();
    let pt = dec.decrypt_padded_mut::<Pkcs7>(&mut buf).map_err(|e| LdmError::Key(format!("decrypt: {e}")))?;
    Ok(pt.to_vec())
}

struct KeyCache {
    keys: tokio::sync::Mutex<HashMap<String, Vec<u8>>>,
}

impl KeyCache {
    async fn get(&self, env: &JobEnv, ctx: &RequestCtx, uri: &str) -> Result<Vec<u8>> {
        let mut g = self.keys.lock().await;
        if let Some(k) = g.get(uri) {
            return Ok(k.clone());
        }
        let (k, _) = env.client.get_bytes(uri, ctx, None, 4096).await.map_err(|e| LdmError::Key(format!("fetch key {uri}: {e}")))?;
        g.insert(uri.to_string(), k.clone());
        Ok(k)
    }
}

/// Download one segment into `path` (streamed when unencrypted).
async fn fetch_segment(env: &JobEnv, ctx: &RequestCtx, seg: &MediaSeg, keys: &KeyCache, path: &Path) -> Result<u64> {
    let _permit = tokio::select! {
        p = env.conn_sem.clone().acquire_owned() => p.map_err(|_| LdmError::Canceled)?,
        _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
    };
    let resp = match seg.range {
        Some((a, b)) => env.client.get_range(&seg.url, ctx, a, Some(b)).await?,
        None => env.client.get_range(&seg.url, ctx, 0, None).await?,
    };
    if seg.range.map(|(a, _)| a > 0).unwrap_or(false) && resp.status() == reqwest::StatusCode::OK {
        return Err(LdmError::RangeIgnored);
    }
    let tmp = path.with_extension("part");
    let mut stream = resp.bytes_stream();
    let read_to = Duration::from_secs(env.cfg.engine.timeout_secs.max(1));
    let mut buf: Vec<u8> = Vec::new();
    let mut file = if seg.key.is_none() { Some(tokio::io::BufWriter::new(tokio::fs::File::create(&tmp).await?)) } else { None };
    let mut n = 0u64;
    loop {
        let next = tokio::select! {
            x = tokio::time::timeout(read_to, stream.next()) => x,
            _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
        };
        let chunk = match next {
            Err(_) => return Err(LdmError::Timeout("segment read timed out".into())),
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(e.into()),
            Ok(Some(Ok(c))) => c,
        };
        env.throttle(chunk.len()).await;
        n += chunk.len() as u64;
        env.progress.add_got(chunk.len() as u64);
        match &mut file {
            Some(f) => f.write_all(&chunk).await?,
            None => buf.extend_from_slice(&chunk),
        }
    }
    if let Some((a, b)) = seg.range {
        if n != b - a + 1 {
            return Err(LdmError::Network(format!("short segment: {n} of {} bytes", b - a + 1)));
        }
    }
    match (file, &seg.key) {
        (Some(mut f), _) => {
            f.flush().await?;
        }
        (None, Some(k)) => {
            let key = keys.get(env, ctx, &k.uri).await?;
            let plain = decrypt(&key, &k.iv, &buf)?;
            tokio::fs::write(&tmp, plain).await?;
        }
        (None, None) => unreachable!(),
    }
    tokio::fs::rename(&tmp, path).await?;
    Ok(n)
}

/// Run (or resume) a segmented media job.
pub async fn run_media_job(env: &JobEnv, spec: &MediaJobSpec) -> Result<MediaOutcome> {
    let cfg = env.cfg.clone();
    ensure_private_dir(&spec.work_dir)?;
    env.progress.set_phase("Probing");
    let audio = cfg.media.audio_pref();
    let plan = match &spec.audio_url {
        Some(a) => av_plan(&spec.url, a),
        None => tokio::select! {
            p = plan_media(&env.client, &spec.url, &spec.ctx, cfg.media.hls_max_height, spec.preferred_height, &audio) => p?,
            _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
        },
    };
    if plan.live {
        if let (true, Some(pl)) = (cfg.media.live_record, plan.playlist_url.clone()) {
            return if plan.source == "dash" { record_live_dash(env, spec, &pl).await } else { record_live_hls(env, spec, &pl).await };
        }
        return Err(LdmError::Format("live streams are not supported (no EXT-X-ENDLIST / dynamic MPD)".into()));
    }
    let audio_only = !plan.tracks.is_empty() && plan.tracks.iter().all(|t| t.kind == "audio");
    let container_ext = if cfg.media.output_container == "mkv" {
        "mkv"
    } else if audio_only {
        "m4a"
    } else {
        "mp4"
    };
    let base_name = output_name(spec, container_ext);
    env.progress.set_filename(&base_name);

    let sig = signature(&plan);
    let state_file = spec.work_dir.join("media_state.json");
    let mut st: MediaState = std::fs::read_to_string(&state_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if st.signature != sig || st.url != spec.url {
        st = MediaState { url: spec.url.clone(), signature: sig, done: vec![BTreeSet::new(); plan.tracks.len()] };
    }
    // trust the files on disk
    for (ti, t) in plan.tracks.iter().enumerate() {
        st.done[ti].retain(|i| seg_path(&spec.work_dir, ti, *i).exists());
        let _ = t;
    }
    let total_segs: usize = plan.tracks.iter().map(|t| t.segments.len() + t.init.is_some() as usize).sum();
    env.progress.segments_total.store(total_segs as u64, std::sync::atomic::Ordering::Relaxed);
    let mut already: u64 = 0;
    for (ti, t) in plan.tracks.iter().enumerate() {
        for i in 0..t.segments.len() {
            if st.done[ti].contains(&i) {
                already += std::fs::metadata(seg_path(&spec.work_dir, ti, i)).map(|m| m.len()).unwrap_or(0);
            }
        }
    }
    env.progress.set_got(already);
    env.progress.set_total(None);
    env.progress.set_phase("Running");

    let keys = Arc::new(KeyCache { keys: tokio::sync::Mutex::new(HashMap::new()) });
    let state = Arc::new(Mutex::new(st));
    let save = |st: &MediaState| {
        if let Ok(t) = serde_json::to_vec(st) {
            let tmp = state_file.with_extension("json.tmp");
            if std::fs::write(&tmp, t).is_ok() {
                let _ = std::fs::rename(&tmp, &state_file);
            }
        }
    };
    // jobs: (track, idx or usize::MAX for init)
    let mut todo: Vec<(usize, usize)> = Vec::new();
    for (ti, t) in plan.tracks.iter().enumerate() {
        if t.init.is_some() && !init_path(&spec.work_dir, ti).exists() {
            todo.push((ti, usize::MAX));
        }
        for i in 0..t.segments.len() {
            if !state.lock().unwrap().done[ti].contains(&i) {
                todo.push((ti, i));
            }
        }
    }
    let done_count = total_segs - todo.len();
    env.progress.segments_done.store(done_count as u64, std::sync::atomic::Ordering::Relaxed);
    let parallel = cfg.media.segment_parallel.clamp(1, 32) as usize;
    let plan = Arc::new(plan);
    let mut set: JoinSet<Result<()>> = JoinSet::new();
    let mut it = todo.into_iter();
    let mut last_save = Instant::now();
    let mut failure: Option<LdmError> = None;
    loop {
        while failure.is_none() && set.len() < parallel {
            let Some((ti, idx)) = it.next() else { break };
            let (env, ctx, keys, plan, dir, state) =
                (env.clone(), spec.ctx.clone(), keys.clone(), plan.clone(), spec.work_dir.clone(), state.clone());
            set.spawn(async move {
                let seg = if idx == usize::MAX { plan.tracks[ti].init.clone().unwrap() } else { plan.tracks[ti].segments[idx].clone() };
                let path = if idx == usize::MAX { init_path(&dir, ti) } else { seg_path(&dir, ti, idx) };
                let ecfg = &env.cfg.engine;
                let mut attempt = 0u32;
                loop {
                    match fetch_segment(&env, &ctx, &seg, &keys, &path).await {
                        Ok(_) => break,
                        Err(e) if e.retryable() && attempt < ecfg.max_retries => {
                            attempt += 1;
                            warn!(track = ti, seg = idx, attempt, err = %e, "segment retry");
                            tokio::select! {
                                _ = tokio::time::sleep(crate::engine::http_job::backoff(ecfg.retry_base_ms, attempt)) => {}
                                _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                if idx != usize::MAX {
                    state.lock().unwrap().done[ti].insert(idx);
                }
                env.progress.segments_done.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(())
            });
        }
        let Some(r) = set.join_next().await else { break };
        match r.map_err(|e| LdmError::Other(e.to_string()))? {
            Ok(()) => {}
            Err(e) => {
                if failure.is_none() {
                    failure = Some(e);
                    env.cancel.cancel();
                }
            }
        }
        if last_save.elapsed() > Duration::from_millis(500) {
            save(&state.lock().unwrap());
            last_save = Instant::now();
        }
    }
    save(&state.lock().unwrap());
    if let Some(e) = failure {
        return Err(e);
    }
    if env.cancel.is_cancelled() {
        return Err(LdmError::Canceled);
    }

    // assemble each track: init + segments in order
    env.progress.set_phase("Assembling");
    let mut track_files = Vec::new();
    for (ti, t) in plan.tracks.iter().enumerate() {
        let ext = match t.container {
            Container::Ts => "ts",
            Container::Fmp4 => {
                if t.kind == "audio" {
                    "m4a"
                } else {
                    "mp4"
                }
            }
        };
        let tf = spec.work_dir.join(format!("track{ti}.{ext}"));
        let dir = spec.work_dir.clone();
        let n = t.segments.len();
        let has_init = t.init.is_some();
        let tf2 = tf.clone();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tf2)?);
            if has_init {
                std::io::copy(&mut std::fs::File::open(init_path(&dir, ti))?, &mut out)?;
            }
            for i in 0..n {
                std::io::copy(&mut std::fs::File::open(seg_path(&dir, ti, i))?, &mut out)?;
            }
            use std::io::Write;
            out.flush()?;
            Ok(())
        })
        .await
        .map_err(|e| LdmError::Other(e.to_string()))??;
        track_files.push((t.kind.clone(), tf));
    }

    let use_ffmpeg = mux::ffmpeg_available(&cfg.media.ffmpeg_path).await;
    let mut extra = Vec::new();
    let (path, remuxed) = if use_ffmpeg {
        let dest = resolve_output(&cfg, &spec.output, &base_name);
        let inputs: Vec<PathBuf> = track_files.iter().map(|(_, p)| p.clone()).collect();
        let tmp_out = spec.work_dir.join(format!("muxed.{container_ext}"));
        mux::remux(&cfg.media.ffmpeg_path, &inputs, &tmp_out).await?;
        move_file(&tmp_out, &dest)?;
        (dest, true)
    } else {
        warn!("ffmpeg not available: writing the concatenated track(s) without remuxing");
        let mut first = None;
        for (kind, tf) in &track_files {
            let ext = tf.extension().and_then(|e| e.to_str()).unwrap_or("bin").to_string();
            let mut name = filename::with_extension(&base_name, &ext);
            if first.is_some() {
                name = filename::with_extension(&base_name, &format!("{kind}.{ext}"));
            }
            let dest = resolve_output(&cfg, &spec.output_for_ext(&ext), &name);
            move_file(tf, &dest)?;
            if first.is_none() {
                first = Some(dest);
            } else {
                extra.push(dest);
            }
        }
        (first.unwrap(), false)
    };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(&spec.work_dir);
    if cfg.media.subtitles {
        for st in &plan.subtitles {
            match save_subtitle(env, spec, st, &path).await {
                Ok(p) => extra.push(p),
                Err(e) => warn!(err = %e, lang = ?st.lang, "subtitle download failed"),
            }
        }
    }
    Ok(MediaOutcome { path, bytes, segments: total_segs as u64, source: plan.source.clone(), remuxed, extra_files: extra })
}

impl MediaJobSpec {
    fn output_for_ext(&self, ext: &str) -> OutputSpec {
        let mut o = self.output.clone();
        if let Some(p) = &o.exact_path {
            o.exact_path = Some(p.with_extension(ext));
        }
        o
    }
}

/// Join WebVTT segments into one file: keep the first header, drop repeated `WEBVTT` /
/// `X-TIMESTAMP-MAP` header blocks of later segments.
pub fn merge_vtt(parts: &[String]) -> String {
    let mut out = String::from("WEBVTT\n");
    for p in parts {
        let p = p.trim_start_matches('\u{feff}').replace("\r\n", "\n");
        let body = if p.starts_with("WEBVTT") {
            // header ends at the first blank line
            match p.find("\n\n") {
                Some(i) => p[i + 2..].to_string(),
                None => String::new(),
            }
        } else {
            p.to_string()
        };
        let body = body.trim_matches('\n');
        if !body.is_empty() {
            out.push('\n');
            out.push_str(body);
            out.push('\n');
        }
    }
    coalesce_vtt_cues(&out)
}

/// Segmented WebVTT repeats a cue that straddles a segment boundary in both segments
/// (either verbatim, or split at the boundary). Drop exact repeats and join a cue with
/// the following one when the text is identical and the times touch or overlap.
fn coalesce_vtt_cues(vtt: &str) -> String {
    fn ts(v: &str) -> Option<u64> {
        let v = v.trim();
        let (hms, ms) = v.split_once('.')?;
        let p: Vec<&str> = hms.split(':').collect();
        let (h, m, s) = match p.len() {
            3 => (p[0].parse::<u64>().ok()?, p[1].parse::<u64>().ok()?, p[2].parse::<u64>().ok()?),
            2 => (0, p[0].parse::<u64>().ok()?, p[1].parse::<u64>().ok()?),
            _ => return None,
        };
        Some(((h * 60 + m) * 60 + s) * 1000 + ms.get(..3)?.parse::<u64>().ok()?)
    }
    struct Cue {
        head: Vec<String>, // optional id line
        start: u64,
        end: u64,
        end_txt: String,
        start_txt: String,
        settings: String,
        text: String,
    }
    let mut header = String::new();
    let mut blocks: Vec<std::result::Result<Cue, String>> = vec![];
    for (i, b) in vtt.split("\n\n").enumerate() {
        let b = b.trim_matches('\n');
        if b.is_empty() {
            continue;
        }
        if i == 0 && b.starts_with("WEBVTT") {
            header = b.to_string();
            continue;
        }
        let lines: Vec<&str> = b.lines().collect();
        let Some(ti) = lines.iter().position(|l| l.contains("-->")) else {
            blocks.push(Err(b.to_string()));
            continue;
        };
        let (a, rest) = lines[ti].split_once("-->").unwrap();
        let rest = rest.trim();
        let (e, settings) = rest.split_once(' ').unwrap_or((rest, ""));
        match (ts(a), ts(e)) {
            (Some(s), Some(en)) => blocks.push(Ok(Cue {
                head: lines[..ti].iter().map(|s| s.to_string()).collect(),
                start: s,
                end: en,
                start_txt: a.trim().to_string(),
                end_txt: e.to_string(),
                settings: settings.trim().to_string(),
                text: lines[ti + 1..].join("\n"),
            })),
            _ => blocks.push(Err(b.to_string())),
        }
    }
    let mut out: Vec<std::result::Result<Cue, String>> = vec![];
    for b in blocks {
        if let (Ok(c), Some(Ok(prev))) = (&b, out.last_mut()) {
            if c.text == prev.text && c.settings == prev.settings && c.start <= prev.end && c.start >= prev.start {
                if c.end > prev.end {
                    prev.end = c.end;
                    prev.end_txt = c.end_txt.clone();
                }
                continue;
            }
        }
        out.push(b);
    }
    let mut s = if header.is_empty() { "WEBVTT".to_string() } else { header };
    s.push('\n');
    for b in out {
        s.push('\n');
        match b {
            Ok(c) => {
                for h in &c.head {
                    s.push_str(h);
                    s.push('\n');
                }
                s.push_str(&c.start_txt);
                s.push_str(" --> ");
                s.push_str(&c.end_txt);
                if !c.settings.is_empty() {
                    s.push(' ');
                    s.push_str(&c.settings);
                }
                s.push('\n');
                s.push_str(&c.text);
                s.push('\n');
            }
            Err(raw) => {
                s.push_str(&raw);
                s.push('\n');
            }
        }
    }
    s
}

/// Pick the output format from the content: WebVTT is merged, SRT kept as `.srt`,
/// TTML / DFXP converted to WebVTT.
pub fn subtitle_file(parts: &[String]) -> Result<(&'static str, String)> {
    let first = parts.first().map(|p| p.trim_start_matches('\u{feff}').trim_start()).unwrap_or("");
    if first.starts_with("<?xml") || first.starts_with("<tt") {
        let mut cues = vec![];
        for p in parts {
            cues.push(ttml_to_vtt_cues(p)?);
        }
        return Ok(("vtt", format!("WEBVTT\n\n{}", cues.into_iter().filter(|c| !c.is_empty()).collect::<Vec<_>>().join("\n"))));
    }
    if !first.starts_with("WEBVTT") && first.lines().take(3).any(|l| l.contains("-->") && l.contains(',')) {
        return Ok(("srt", parts.join("\n")));
    }
    Ok(("vtt", merge_vtt(parts)))
}

/// TTML clock value ("00:00:01.500", "1.5s", "45f" unsupported → None) → WebVTT timestamp.
fn ttml_time(v: &str) -> Option<String> {
    let secs = if let Some(x) = v.strip_suffix("ms") {
        x.parse::<f64>().ok()? / 1000.0
    } else if let Some(x) = v.strip_suffix('s') {
        x.parse::<f64>().ok()?
    } else {
        let p: Vec<&str> = v.split(':').collect();
        if p.len() != 3 {
            return None;
        }
        p[0].parse::<f64>().ok()? * 3600.0 + p[1].parse::<f64>().ok()? * 60.0 + p[2].replace(',', ".").parse::<f64>().ok()?
    };
    let ms = (secs * 1000.0).round() as u64;
    Some(format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000))
}

fn ttml_to_vtt_cues(xml: &str) -> Result<String> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| LdmError::Format(format!("TTML: {e}")))?;
    let mut out = String::new();
    for p in doc.descendants().filter(|n| n.tag_name().name() == "p") {
        let (Some(b), Some(e)) = (p.attribute("begin").and_then(ttml_time), p.attribute("end").and_then(ttml_time)) else { continue };
        let mut text = String::new();
        for n in p.descendants() {
            if n.is_text() {
                text.push_str(n.text().unwrap_or(""));
            } else if n.tag_name().name() == "br" {
                text.push('\n');
            }
        }
        let text: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
        if !text.is_empty() {
            out.push_str(&format!("{b} --> {e}\n{}\n\n", text.join("\n")));
        }
    }
    Ok(out.trim_end().to_string() + if out.is_empty() { "" } else { "\n" })
}

async fn save_subtitle(env: &JobEnv, spec: &MediaJobSpec, st: &SubtitleTrack, video: &Path) -> Result<PathBuf> {
    let mut parts = vec![];
    for u in &st.segments {
        parts.push(fetch_text(&env.client, u, &spec.ctx).await?.0);
    }
    let tag = filename::sanitize(st.lang.as_deref().or(st.name.as_deref()).unwrap_or("sub"));
    let stem = video.file_stem().and_then(|s| s.to_str()).unwrap_or("video");
    let (ext, body) = subtitle_file(&parts)?;
    let dest = video.with_file_name(format!("{stem}.{tag}.{ext}"));
    std::fs::write(&dest, body)?;
    Ok(dest)
}

#[cfg(test)]
mod subtitle_tests {
    use super::*;

    #[test]
    fn ttml_and_srt_and_vtt() {
        let ttml = r#"<?xml version="1.0"?><tt xmlns="http://www.w3.org/ns/ttml"><body><div>
            <p begin="00:00:01.000" end="00:00:02.500">Hello<br/>world</p><p begin="3s" end="4500ms">Bye</p></div></body></tt>"#;
        let (ext, v) = subtitle_file(&[ttml.to_string()]).unwrap();
        assert_eq!(ext, "vtt");
        assert_eq!(v, "WEBVTT\n\n00:00:01.000 --> 00:00:02.500\nHello\nworld\n\n00:00:03.000 --> 00:00:04.500\nBye\n");
        let (ext, _) = subtitle_file(&["1\n00:00:01,000 --> 00:00:02,000\nhi\n".to_string()]).unwrap();
        assert_eq!(ext, "srt");
        let (ext, v) =
            subtitle_file(&["WEBVTT\n\n00:01.000 --> 00:02.000\na\n".into(), "WEBVTT\n\n00:02.000 --> 00:03.000\nb\n".into()]).unwrap();
        assert_eq!((ext, v.matches("WEBVTT").count()), ("vtt", 1));
    }

    #[test]
    fn vtt_boundary_cues_coalesced() {
        // real-world shape (Unified Streaming): a cue straddling a 4 s boundary is split / repeated
        let a = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n1\n00:00:23.000 --> 00:00:24.000\nYou're a jerk, Thom.\n";
        let b = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n2\n00:00:24.000 --> 00:00:24.500\nYou're a jerk, Thom.\n\n00:00:25.000 --> 00:00:27.000\nNext\n";
        let c = "WEBVTT\n\n00:00:25.000 --> 00:00:27.000\nNext\n\n00:00:27.000 --> 00:00:28.000\nOther\n";
        let v = merge_vtt(&[a.into(), b.into(), c.into()]);
        assert_eq!(v.matches("You're a jerk").count(), 1, "{v}");
        assert!(v.contains("1\n00:00:23.000 --> 00:00:24.500\nYou're a jerk, Thom."), "{v}");
        assert_eq!(v.matches("Next").count(), 1, "{v}");
        assert!(v.contains("00:00:27.000 --> 00:00:28.000\nOther"));
        // different text at touching times stays separate
        let v = merge_vtt(&["WEBVTT\n\n00:01.000 --> 00:02.000\na\n".into(), "WEBVTT\n\n00:02.000 --> 00:03.000\nb\n".into()]);
        assert!(v.contains("\na\n") && v.contains("\nb\n"));
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LiveState {
    playlist: String,
    /// next file index (live_00000.seg …)
    next: usize,
    /// media sequence numbers already stored (bounded window)
    seen: VecDeque<u64>,
    has_init: bool,
    started_ms: i64,
    recorded_secs: f64,
}

fn live_seg(dir: &Path, i: usize) -> PathBuf {
    dir.join(format!("live_{i:05}.seg"))
}

/// How many leading segments of the first live playlist to skip so recording starts at the live
/// edge: keep the newest segments covering at least 3 target durations (and at least one segment).
pub fn live_edge_skip(durations: &[f64], target: f64) -> usize {
    let want = 3.0 * target.max(1.0);
    let mut acc = 0.0;
    for (i, d) in durations.iter().enumerate().rev() {
        acc += d;
        if acc >= want {
            return i;
        }
    }
    0
}

/// Record a live HLS stream: poll the media playlist, store every new
/// segment in order, stop at `EXT-X-ENDLIST`, the duration cap (`media.live_max_duration`) or a user
/// "停止录制" (`JobSet stop_recording`), then assemble like a VOD download. Pausing keeps the recorded part;
/// resuming continues the same recording.
async fn record_live_hls(env: &JobEnv, spec: &MediaJobSpec, playlist_url: &str) -> Result<MediaOutcome> {
    let cfg = env.cfg.clone();
    let dir = &spec.work_dir;
    let state_file = dir.join("live_state.json");
    let mut st: LiveState = std::fs::read_to_string(&state_file).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default();
    if st.playlist != playlist_url {
        st = LiveState { playlist: playlist_url.to_string(), started_ms: chrono::Utc::now().timestamp_millis(), ..Default::default() };
    }
    let save = |st: &LiveState| {
        if let Ok(t) = serde_json::to_vec(st) {
            let tmp = state_file.with_extension("json.tmp");
            if std::fs::write(&tmp, t).is_ok() {
                let _ = std::fs::rename(&tmp, &state_file);
            }
        }
    };
    let base_name = output_name(spec, if cfg.media.output_container == "mkv" { "mkv" } else { "mp4" });
    env.progress.set_filename(&base_name);
    env.progress.set_total(None);
    let mut got: u64 = (0..st.next).map(|i| std::fs::metadata(live_seg(dir, i)).map(|m| m.len()).unwrap_or(0)).sum();
    env.progress.set_got(got);
    env.progress.segments_done.store(st.next as u64, Ordering::Relaxed);
    env.progress.set_live(true);
    env.progress.set_recorded_secs(st.recorded_secs);
    env.progress.set_phase("Recording");
    let keys = KeyCache { keys: tokio::sync::Mutex::new(HashMap::new()) };
    let cap = cfg.media.live_max_duration as f64;
    let mut container = Container::Ts;
    let mut misses = 0u32;
    info!(playlist = %crate::util::security::redact_url(playlist_url), "recording live HLS");
    loop {
        let (text, final_url) = match fetch_text(&env.client, playlist_url, &spec.ctx).await {
            Ok(x) => x,
            Err(e) if e.retryable() && misses < cfg.engine.max_retries => {
                misses += 1;
                warn!(err = %e, "live playlist refresh failed, retrying");
                (String::new(), String::new())
            }
            Err(e) => return Err(e),
        };
        let mut ended = false;
        let mut wait = 2.0f64;
        if !text.is_empty() {
            misses = 0;
            let hls::Playlist::Media(pl) = hls::parse(&text, &final_url)? else {
                return Err(LdmError::Format("live playlist turned into a master playlist".into()));
            };
            ended = pl.end_list;
            wait = (pl.target_duration / 2.0).clamp(0.5, 10.0);
            if pl.segments.is_empty() && !ended {
                // nothing yet
            } else if !pl.segments.is_empty() {
                let t = track_any(&pl, "muxed", "live".into())?;
                container = t.container;
                if st.next == 0 && st.seen.is_empty() && !ended && !cfg.media.live_from_start {
                    let skip = live_edge_skip(&pl.segments.iter().map(|s| s.duration).collect::<Vec<_>>(), pl.target_duration);
                    if skip > 0 {
                        info!(skipped = skip, "live HLS: starting at the live edge");
                    }
                    st.seen.extend(pl.segments.iter().take(skip).map(|s| s.seq));
                }
                if let (Some(init), false) = (&t.init, st.has_init) {
                    fetch_segment(env, &spec.ctx, init, &keys, &init_path(dir, 0)).await?;
                    st.has_init = true;
                }
                for (i, seg) in pl.segments.iter().enumerate() {
                    if st.seen.contains(&seg.seq) {
                        continue;
                    }
                    if (cap > 0.0 && st.recorded_secs >= cap) || env.progress.stop_requested() {
                        break;
                    }
                    let n = fetch_segment(env, &spec.ctx, &t.segments[i], &keys, &live_seg(dir, st.next)).await?;
                    got += n;
                    env.progress.set_got(got);
                    st.next += 1;
                    st.recorded_secs += seg.duration;
                    env.progress.set_recorded_secs(st.recorded_secs);
                    st.seen.push_back(seg.seq);
                    while st.seen.len() > 4096 {
                        st.seen.pop_front();
                    }
                    env.progress.segments_done.store(st.next as u64, Ordering::Relaxed);
                    save(&st);
                }
            }
        }
        let capped = cap > 0.0 && st.recorded_secs >= cap;
        if ended || capped || env.progress.stop_requested() {
            info!(segments = st.next, secs = st.recorded_secs, ended, capped, "live recording finished");
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs_f64(wait)) => {}
            _ = env.progress.stop_requested_wait() => {}
            _ = env.cancel.cancelled() => {
                save(&st);
                return Err(LdmError::Canceled);
            }
        }
    }
    if st.next == 0 {
        return Err(LdmError::Format("live recording stopped before any segment was received".into()));
    }
    save(&st);
    // assemble: init + segments in order, then remux / move like a VOD track
    env.progress.set_phase("Assembling");
    let ext = if container == Container::Fmp4 { "mp4" } else { "ts" };
    let tf = dir.join(format!("live.{ext}"));
    let (d2, tf2, n, has_init) = (dir.clone(), tf.clone(), st.next, st.has_init);
    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut out = std::io::BufWriter::new(std::fs::File::create(&tf2)?);
        if has_init {
            std::io::copy(&mut std::fs::File::open(init_path(&d2, 0))?, &mut out)?;
        }
        for i in 0..n {
            std::io::copy(&mut std::fs::File::open(live_seg(&d2, i))?, &mut out)?;
        }
        use std::io::Write;
        out.flush()?;
        Ok(())
    })
    .await
    .map_err(|e| LdmError::Other(e.to_string()))??;
    let (path, remuxed) = if mux::ffmpeg_available(&cfg.media.ffmpeg_path).await {
        let dest = resolve_output(&cfg, &spec.output, &base_name);
        let tmp_out = dir.join(format!("muxed.{}", if cfg.media.output_container == "mkv" { "mkv" } else { "mp4" }));
        mux::remux(&cfg.media.ffmpeg_path, std::slice::from_ref(&tf), &tmp_out).await?;
        move_file(&tmp_out, &dest)?;
        (dest, true)
    } else {
        let name = filename::with_extension(&base_name, ext);
        let dest = resolve_output(&cfg, &spec.output_for_ext(ext), &name);
        move_file(&tf, &dest)?;
        (dest, false)
    };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(dir);
    Ok(MediaOutcome { path, bytes, segments: st.next as u64, source: "hls-live".into(), remuxed, extra_files: vec![] })
}

/// Record a dynamic MPD: re-read the manifest, start at the live edge
/// (last few segments), fetch new segments of the chosen video/audio representations, stop when
/// the MPD turns static (stream ended), at `media.live_max_duration` of wall time, or on "停止录制".
/// Segments that are listed but not yet published (404) are retried at the next refresh.
async fn record_live_dash(env: &JobEnv, spec: &MediaJobSpec, mpd_url: &str) -> Result<MediaOutcome> {
    const EDGE: usize = 3;
    let cfg = env.cfg.clone();
    let dir = &spec.work_dir;
    let base_name = output_name(spec, if cfg.media.output_container == "mkv" { "mkv" } else { "mp4" });
    env.progress.set_filename(&base_name);
    env.progress.set_total(None);
    env.progress.set_live(true);
    env.progress.set_phase("Recording");
    let keys = KeyCache { keys: tokio::sync::Mutex::new(HashMap::new()) };
    let started = Instant::now();
    let cap = cfg.media.live_max_duration;
    // per track: (kind, has_init, seen urls, files written)
    let mut state: Vec<(String, bool, std::collections::HashSet<String>, usize)> = vec![];
    let mut got = 0u64;
    let mut total_files = 0u64;
    let mut first = true;
    info!(mpd = %crate::util::security::redact_url(mpd_url), "recording dynamic MPD");
    loop {
        let (text, final_url) = fetch_text(&env.client, mpd_url, &spec.ctx).await?;
        let mpd = dash::parse(&text, &final_url)?;
        let ended = !mpd.dynamic;
        let period = mpd.periods.last().ok_or_else(|| LdmError::Format("MPD has no Period".into()))?;
        if period.adaptation_sets.iter().any(|a| a.protected) {
            return Err(LdmError::UnsupportedKey("DRM ContentProtection present".into()));
        }
        let tracks = dash_tracks(period, cfg.media.hls_max_height, spec.preferred_height, &cfg.media.audio_pref())?;
        if state.is_empty() {
            state = tracks.iter().map(|t| (t.kind.clone(), false, Default::default(), 0)).collect();
        }
        for (ti, t) in tracks.iter().enumerate().take(state.len()) {
            let st = &mut state[ti];
            if first {
                // join at the live edge, like a player would
                let skip = t.segments.len().saturating_sub(EDGE);
                st.2.extend(t.segments[..skip].iter().map(|s| s.url.clone()));
            }
            if let (Some(init), false) = (&t.init, st.1) {
                fetch_segment(env, &spec.ctx, init, &keys, &init_path(dir, ti)).await?;
                st.1 = true;
            }
            for seg in &t.segments {
                if st.2.contains(&seg.url) {
                    continue;
                }
                match fetch_segment(env, &spec.ctx, seg, &keys, &seg_path(dir, ti, st.3)).await {
                    Ok(n) => {
                        got += n;
                        env.progress.set_got(got);
                        st.2.insert(seg.url.clone());
                        st.3 += 1;
                        total_files += 1;
                        env.progress.segments_done.store(total_files, Ordering::Relaxed);
                        env.progress.set_recorded_secs(started.elapsed().as_secs_f64());
                    }
                    Err(LdmError::HttpStatus(404, _)) if !ended => break, // not published yet
                    Err(e) => return Err(e),
                }
            }
        }
        first = false;
        let capped = cap > 0 && started.elapsed().as_secs() >= cap;
        if ended || capped || env.progress.stop_requested() {
            info!(files = total_files, ended, capped, "dynamic MPD recording finished");
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
            _ = env.progress.stop_requested_wait() => {}
            _ = env.cancel.cancelled() => return Err(LdmError::Canceled),
        }
    }
    if state.iter().all(|s| s.3 == 0) {
        return Err(LdmError::Format("live recording stopped before any segment was received".into()));
    }
    env.progress.set_phase("Assembling");
    let mut inputs = vec![];
    for (ti, (kind, has_init, _, n)) in state.iter().enumerate() {
        if *n == 0 {
            continue;
        }
        let tf = dir.join(format!("live{ti}.{}", if kind == "audio" { "m4a" } else { "mp4" }));
        let (d2, tf2, n, has_init) = (dir.clone(), tf.clone(), *n, *has_init);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut out = std::io::BufWriter::new(std::fs::File::create(&tf2)?);
            if has_init {
                std::io::copy(&mut std::fs::File::open(init_path(&d2, ti))?, &mut out)?;
            }
            for i in 0..n {
                std::io::copy(&mut std::fs::File::open(seg_path(&d2, ti, i))?, &mut out)?;
            }
            use std::io::Write;
            out.flush()?;
            Ok(())
        })
        .await
        .map_err(|e| LdmError::Other(e.to_string()))??;
        inputs.push((kind.clone(), tf));
    }
    let mut extra = vec![];
    let (path, remuxed) = if mux::ffmpeg_available(&cfg.media.ffmpeg_path).await {
        let dest = resolve_output(&cfg, &spec.output, &base_name);
        let tmp_out = dir.join(format!("muxed.{}", if cfg.media.output_container == "mkv" { "mkv" } else { "mp4" }));
        mux::remux(&cfg.media.ffmpeg_path, &inputs.iter().map(|(_, p)| p.clone()).collect::<Vec<_>>(), &tmp_out).await?;
        move_file(&tmp_out, &dest)?;
        (dest, true)
    } else {
        let mut first_path = None;
        for (kind, tf) in &inputs {
            let ext = tf.extension().and_then(|e| e.to_str()).unwrap_or("mp4").to_string();
            let name = if first_path.is_none() {
                filename::with_extension(&base_name, &ext)
            } else {
                filename::with_extension(&base_name, &format!("{kind}.{ext}"))
            };
            let dest = resolve_output(&cfg, &spec.output_for_ext(&ext), &name);
            move_file(tf, &dest)?;
            if first_path.is_none() {
                first_path = Some(dest);
            } else {
                extra.push(dest);
            }
        }
        (first_path.unwrap(), false)
    };
    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let _ = std::fs::remove_dir_all(dir);
    Ok(MediaOutcome { path, bytes, segments: total_files, source: "dash-live".into(), remuxed, extra_files: extra })
}

fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d)?;
    }
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

const GENERIC_STEMS: &[&str] = &["index", "master", "playlist", "manifest", "stream", "chunklist", "prog_index", "video", "main"];

fn output_name(spec: &MediaJobSpec, ext: &str) -> String {
    if let Some(h) = spec.output.filename.as_deref().or(spec.filename_hint.as_deref()).filter(|s| !s.trim().is_empty()) {
        // media hints are page titles or user input, not server names: Linux rules only
        let h = filename::sanitize_linux(h);
        let (stem, e) = filename::split_ext(&h);
        let known = ["m3u8", "mpd", "mp4", "mkv", "ts", "webm"];
        return if known.contains(&e.to_ascii_lowercase().as_str()) || e.is_empty() {
            format!("{stem}.{ext}")
        } else {
            format!("{h}.{ext}")
        };
    }
    let from_url = filename::from_url(&spec.url).unwrap_or_else(|| "video".into());
    let (stem, _) = filename::split_ext(&from_url);
    let mut stem = stem.trim_matches('.').to_string();
    let bare = stem.is_empty() || ["m3u8", "mpd", "ism", "isml"].contains(&stem.to_ascii_lowercase().as_str());
    if bare {
        // e.g. ".../tears-of-steel.ism/.m3u8": the real title is the parent segment
        if let Ok(u) = url::Url::parse(&spec.url) {
            let parent =
                u.path_segments().and_then(|s| s.filter(|p| !p.is_empty()).collect::<Vec<_>>().iter().rev().nth(1).map(|p| p.to_string()));
            if let Some(parent) = parent {
                let parent = percent_encoding::percent_decode_str(&parent).decode_utf8_lossy().to_string();
                let (ps, pe) = filename::split_ext(&parent);
                let ps = if ["ism", "isml", "smil"].contains(&pe.to_ascii_lowercase().as_str()) { ps.to_string() } else { parent.clone() };
                if !ps.trim_matches('.').is_empty() {
                    stem = ps;
                }
            }
        }
        if stem.is_empty() || stem.eq_ignore_ascii_case("m3u8") || stem.eq_ignore_ascii_case("mpd") {
            stem = "video".into();
        }
    } else if GENERIC_STEMS.contains(&stem.to_ascii_lowercase().as_str()) {
        // use the parent directory name when the file name is generic
        if let Ok(u) = url::Url::parse(&spec.url) {
            if let Some(parent) = u.path_segments().and_then(|s| s.collect::<Vec<_>>().iter().rev().nth(1).map(|p| p.to_string())) {
                if !parent.is_empty() {
                    stem = format!("{parent}_{stem}");
                }
            }
        }
    }
    filename::sanitize(&format!("{stem}.{ext}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aes_roundtrip() {
        use aes::cipher::BlockEncryptMut;
        type Enc = cbc::Encryptor<aes::Aes128>;
        let key = [7u8; 16];
        let iv = hls::sequence_iv(3);
        let pt = b"hello segment data that spans more than one block".to_vec();
        let mut buf = pt.clone();
        buf.resize(pt.len() + 16, 0);
        let ct = Enc::new_from_slices(&key, &iv).unwrap().encrypt_padded_mut::<Pkcs7>(&mut buf, pt.len()).unwrap().to_vec();
        assert_eq!(decrypt(&key, &iv, &ct).unwrap(), pt);
        assert!(decrypt(&key[..8], &iv, &ct).is_err());
        assert!(decrypt(&[9u8; 16], &iv, &ct).is_err());
    }

    #[test]
    fn names() {
        let s = MediaJobSpec { url: "http://h/videos/clip42/index.m3u8".into(), ..Default::default() };
        assert_eq!(output_name(&s, "mp4"), "clip42_index.mp4");
        let s = MediaJobSpec { url: "http://h/a/movie.m3u8".into(), ..Default::default() };
        assert_eq!(output_name(&s, "mp4"), "movie.mp4");
        let s = MediaJobSpec { url: "http://h/a/x.mpd".into(), filename_hint: Some("My Talk".into()), ..Default::default() };
        assert_eq!(output_name(&s, "mkv"), "My Talk.mkv");
        let s = MediaJobSpec { url: "https://h/k8s/tears-of-steel-aes.ism/.m3u8".into(), ..Default::default() };
        assert_eq!(output_name(&s, "mp4"), "tears-of-steel-aes.mp4");
        let s = MediaJobSpec { url: "https://h/live/channel1.isml/.mpd?x=1".into(), ..Default::default() };
        assert_eq!(output_name(&s, "mp4"), "channel1.mp4");
        let s = MediaJobSpec { url: "https://h/.m3u8".into(), ..Default::default() };
        assert_eq!(output_name(&s, "mp4"), "video.mp4");
    }

    #[test]
    fn unsupported_key_is_an_error() {
        let pl = hls::MediaPlaylist {
            end_list: true,
            segments: vec![hls::Segment {
                uri: "http://h/a.ts".into(),
                duration: 1.0,
                byterange: None,
                key: Some(hls::Key { method: hls::KeyMethod::Other("SAMPLE-AES".into()), uri: Some("k".into()), iv: None }),
                map: None,
                seq: 0,
                discontinuity: false,
            }],
            ..Default::default()
        };
        assert_eq!(track_from_media(&pl, "muxed", "x".into()).unwrap_err().code(), "E_UNSUPPORTED_KEY");
    }
}
