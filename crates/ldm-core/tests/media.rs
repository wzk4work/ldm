//! HLS / DASH integration tests. Fixtures are generated with ffmpeg at test
//! time and served by the local test server.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use ldm_core::engine::{JobEnv, OutputSpec};
use ldm_core::media::{plan_media, run_media_job, AudioPreference, MediaJobSpec};
use ldm_core::Config;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ff(dir: &Path, args: &[&str]) {
    let o = Command::new("ffmpeg").current_dir(dir).args(["-y", "-hide_banner", "-loglevel", "error"]).args(args).output().unwrap();
    assert!(o.status.success(), "ffmpeg {:?}: {}", args, String::from_utf8_lossy(&o.stderr));
}

const SRC: &[&str] =
    &["-f", "lavfi", "-i", "testsrc=size=320x240:rate=25:duration=6", "-f", "lavfi", "-i", "sine=frequency=440:duration=6"];
const ENC: &[&str] =
    &["-c:v", "libx264", "-preset", "ultrafast", "-g", "25", "-keyint_min", "25", "-sc_threshold", "0", "-c:a", "aac", "-b:a", "64k"];

fn hls_args<'a>(extra: &[&'a str], out: &'a str) -> Vec<&'a str> {
    let mut v: Vec<&str> = SRC.to_vec();
    v.extend_from_slice(ENC);
    v.extend_from_slice(&["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod"]);
    v.extend_from_slice(extra);
    v.push(out);
    v
}

#[derive(Debug, Default)]
struct Probe {
    duration: f64,
    video: usize,
    audio: usize,
    height: u32,
    format: String,
}

fn probe(p: &Path) -> Probe {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration,format_name:stream=codec_type,height", "-of", "json"])
        .arg(p)
        .output()
        .unwrap();
    assert!(o.status.success(), "ffprobe failed on {}", p.display());
    let v: serde_json::Value = serde_json::from_slice(&o.stdout).unwrap();
    let mut r = Probe {
        duration: v["format"]["duration"].as_str().unwrap_or("0").parse().unwrap(),
        format: v["format"]["format_name"].as_str().unwrap_or("").into(),
        ..Default::default()
    };
    for s in v["streams"].as_array().unwrap() {
        match s["codec_type"].as_str() {
            Some("video") => {
                r.video += 1;
                r.height = s["height"].as_u64().unwrap_or(0) as u32;
            }
            Some("audio") => r.audio += 1,
            _ => {}
        }
    }
    r
}

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    out: PathBuf,
    srv: ldm_testsrv::Server,
}

async fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("www");
    let out = tmp.path().join("out");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    let srv = ldm_testsrv::start(root.clone(), "127.0.0.1:0").await.unwrap();
    Fx { _tmp: tmp, root, out, srv }
}

fn cfg() -> Config {
    let mut c = Config::default();
    c.engine.retry_base_ms = 20;
    c.engine.timeout_secs = 10;
    c
}

fn spec(f: &Fx, url: String, name: &str) -> MediaJobSpec {
    MediaJobSpec {
        url,
        work_dir: f.out.join(format!("work-{name}")),
        output: OutputSpec { exact_path: Some(f.out.join(name)), ..Default::default() },
        ..Default::default()
    }
}

fn near(a: f64, b: f64) -> bool {
    (a - b).abs() < 0.6
}

macro_rules! need_ffmpeg {
    () => {
        if !have_ffmpeg() {
            eprintln!("SKIP: ffmpeg not installed");
            return;
        }
    };
}

#[tokio::test]
async fn m1_master_playlist_selects_best_variant_and_remuxes() {
    need_ffmpeg!();
    let f = fx().await;
    for (h, w) in [(240u32, 320u32), (360, 480)] {
        let mut a: Vec<String> = SRC.iter().map(|s| s.to_string()).collect();
        a.extend(ENC.iter().map(|s| s.to_string()));
        a.extend(["-vf".into(), format!("scale={w}:{h}")]);
        a.extend(["-f", "hls", "-hls_time", "2", "-hls_playlist_type", "vod"].iter().map(|s| s.to_string()));
        a.extend(["-hls_segment_filename".into(), format!("v{h}_%03d.ts"), format!("v{h}.m3u8")]);
        let refs: Vec<&str> = a.iter().map(|s| s.as_str()).collect();
        ff(&f.root, &refs);
    }
    std::fs::write(
        f.root.join("master.m3u8"),
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=400000,RESOLUTION=320x240\nv240.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=800000,RESOLUTION=480x360\nv360.m3u8\n",
    )
    .unwrap();
    let env = JobEnv::standalone(cfg()).unwrap();
    let plan = plan_media(&env.client, &f.srv.url("/master.m3u8"), &Default::default(), 1080, None, &AudioPreference::Aac).await.unwrap();
    assert_eq!(plan.qualities.len(), 2);
    assert_eq!(plan.tracks.len(), 1);
    assert!(plan.tracks[0].segments[0].url.contains("v360_"));
    // with a 240p cap the lower variant is used
    let p2 = plan_media(&env.client, &f.srv.url("/master.m3u8"), &Default::default(), 300, None, &AudioPreference::Aac).await.unwrap();
    assert!(p2.tracks[0].segments[0].url.contains("v240_"));

    let out = run_media_job(&env, &spec(&f, f.srv.url("/master.m3u8"), "m1.mp4")).await.unwrap();
    assert!(out.remuxed);
    let p = probe(&out.path);
    assert_eq!((p.video, p.audio, p.height), (1, 1, 360), "{p:?}");
    assert!(near(p.duration, 6.0), "{p:?}");
    assert!(p.format.contains("mp4"));
    // segments only from the chosen variant; work dir cleaned
    assert!(f.srv.ranges_for("/v240_000.ts").is_empty());
    assert!(!f.out.join("work-m1.mp4").exists());
}

#[tokio::test]
async fn m2_aes128_encrypted_hls() {
    need_ffmpeg!();
    let f = fx().await;
    let key: Vec<u8> = (0u8..16).map(|i| i.wrapping_mul(37).wrapping_add(5)).collect();
    std::fs::write(f.root.join("enc.key"), &key).unwrap();
    std::fs::write(f.root.join("keyinfo"), format!("enc.key\n{}\n", f.root.join("enc.key").display())).unwrap();
    ff(&f.root, &hls_args(&["-hls_key_info_file", "keyinfo", "-hls_segment_filename", "e_%03d.ts"], "enc.m3u8"));
    let pl = std::fs::read_to_string(f.root.join("enc.m3u8")).unwrap();
    assert!(pl.contains("METHOD=AES-128"));
    // segments really are encrypted (no TS sync byte pattern)
    let seg = std::fs::read(f.root.join("e_000.ts")).unwrap();
    assert!(!(seg[0] == 0x47 && seg[188] == 0x47));

    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_media_job(&env, &spec(&f, f.srv.url("/enc.m3u8"), "m2.mp4")).await.unwrap();
    let p = probe(&out.path);
    assert_eq!((p.video, p.audio), (1, 1));
    assert!(near(p.duration, 6.0), "{p:?}");
    // key fetched once (cache)
    assert_eq!(f.srv.ranges_for("/enc.key").len(), 1);
}

#[tokio::test]
async fn m3_fmp4_and_byterange_hls() {
    need_ffmpeg!();
    let f = fx().await;
    ff(
        &f.root,
        &hls_args(
            &["-hls_segment_type", "fmp4", "-hls_fmp4_init_filename", "init.mp4", "-hls_segment_filename", "f_%03d.m4s"],
            "fmp4.m3u8",
        ),
    );
    assert!(std::fs::read_to_string(f.root.join("fmp4.m3u8")).unwrap().contains("EXT-X-MAP"));
    ff(&f.root, &hls_args(&["-hls_flags", "single_file", "-hls_segment_filename", "single.ts"], "single.m3u8"));
    assert!(std::fs::read_to_string(f.root.join("single.m3u8")).unwrap().contains("EXT-X-BYTERANGE"));

    let env = JobEnv::standalone(cfg()).unwrap();
    let a = run_media_job(&env, &spec(&f, f.srv.url("/fmp4.m3u8"), "m3a.mp4")).await.unwrap();
    let p = probe(&a.path);
    assert_eq!((p.video, p.audio), (1, 1));
    assert!(near(p.duration, 6.0), "{p:?}");

    let env = JobEnv::standalone(cfg()).unwrap();
    let b = run_media_job(&env, &spec(&f, f.srv.url("/single.m3u8"), "m3b.mp4")).await.unwrap();
    let p = probe(&b.path);
    assert_eq!((p.video, p.audio), (1, 1));
    assert!(near(p.duration, 6.0), "{p:?}");
    let ranges = f.srv.ranges_for("/single.ts");
    assert!(ranges.len() >= 3 && ranges.iter().all(|r| r.as_deref().map(|s| s.starts_with("bytes=")).unwrap_or(false)), "{ranges:?}");
}

#[tokio::test]
async fn m4_dash_separate_audio_video_muxed() {
    need_ffmpeg!();
    let f = fx().await;
    for (name, tl) in [("tl.mpd", "1"), ("num.mpd", "0")] {
        let sub = f.root.join(name.trim_end_matches(".mpd"));
        std::fs::create_dir_all(&sub).unwrap();
        let mut a: Vec<&str> = SRC.to_vec();
        a.extend_from_slice(ENC);
        a.extend_from_slice(&[
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-f",
            "dash",
            "-seg_duration",
            "2",
            "-use_template",
            "1",
            "-use_timeline",
            tl,
            "manifest.mpd",
        ]);
        ff(&sub, &a);
    }
    for (path, name) in [("/tl/manifest.mpd", "m4a.mp4"), ("/num/manifest.mpd", "m4b.mp4")] {
        let env = JobEnv::standalone(cfg()).unwrap();
        let plan = plan_media(&env.client, &f.srv.url(path), &Default::default(), 1080, None, &AudioPreference::Aac).await.unwrap();
        assert_eq!(plan.source, "dash");
        assert_eq!(plan.tracks.len(), 2, "{path}");
        let out = run_media_job(&env, &spec(&f, f.srv.url(path), name)).await.unwrap();
        let p = probe(&out.path);
        assert_eq!((p.video, p.audio, p.height), (1, 1, 240), "{path} {p:?}");
        assert!(near(p.duration, 6.0), "{path} {p:?}");
    }
    // "仅音频": preferred height 0 keeps only the audio adaptation set
    let env = JobEnv::standalone(cfg()).unwrap();
    let url = f.srv.url("/tl/manifest.mpd");
    let plan = plan_media(&env.client, &url, &Default::default(), 1080, Some(0), &AudioPreference::Aac).await.unwrap();
    assert_eq!(plan.tracks.len(), 1);
    assert_eq!(plan.tracks[0].kind, "audio");
    assert!(plan.qualities.iter().any(|q| q.kind == "audio"));
    let mut s = spec(&f, url, "audio.m4a");
    s.preferred_height = Some(0);
    let out = run_media_job(&env, &s).await.unwrap();
    let p = probe(&out.path);
    assert_eq!((p.video, p.audio), (0, 1), "{p:?}");
}

#[tokio::test]
async fn m5_captured_sample_hls_via_rewrite() {
    let sample = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/fixtures/sample/hls"));
    if !sample.join("index.m3u8").exists() {
        eprintln!("SKIP: sample not present");
        return;
    }
    let f = fx().await;
    let dst = f.root.join("share/sample/hls");
    std::fs::create_dir_all(&dst).unwrap();
    std::fs::copy(sample.join("seg0.ts"), dst.join("seg0.ts")).unwrap();
    // a captured playlist with absolute segment URLs on another origin, rewritten by the test server
    let pl = std::fs::read_to_string(sample.join("index.m3u8")).unwrap();
    std::fs::write(dst.join("index.m3u8"), pl.replace("seg0.ts", "http://origin.invalid:8765/share/sample/hls/seg0.ts")).unwrap();
    let base = f.srv.url("");
    let rw = format!("http://origin.invalid:8765|{}", base.trim_end_matches('/'));
    let url = format!("{}?rewrite={}", f.srv.url("/share/sample/hls/index.m3u8"), urlencode(&rw));
    let mut c = cfg();
    c.media.ffmpeg_path = if have_ffmpeg() { "ffmpeg".into() } else { "/nonexistent/ffmpeg".into() };
    let env = JobEnv::standalone(c).unwrap();
    let mut s = spec(&f, url, "sample.mp4");
    s.output.exact_path = Some(f.out.join("sample.mp4"));
    let out = run_media_job(&env, &s).await.unwrap();
    assert_eq!(out.segments, 1);
    if have_ffmpeg() {
        let p = probe(&out.path);
        assert!(p.video >= 1 && near(p.duration, 2.0), "{p:?}");
    }
}

#[tokio::test]
async fn m6_resume_after_failure_skips_done_segments() {
    need_ffmpeg!();
    let f = fx().await;
    ff(&f.root, &hls_args(&["-hls_segment_filename", "r_%03d.ts"], "res.m3u8"));
    // break segment 2 by hiding it
    let s2 = f.root.join("r_002.ts");
    let hidden = f.root.join("r_002.hidden");
    std::fs::rename(&s2, &hidden).unwrap();
    let mut c = cfg();
    c.engine.max_retries = 1;
    c.media.segment_parallel = 1;
    let env = JobEnv::standalone(c.clone()).unwrap();
    let sp = spec(&f, f.srv.url("/res.m3u8"), "m6.mp4");
    let err = run_media_job(&env, &sp).await.unwrap_err();
    assert_eq!(err.code(), "E_HTTP_STATUS", "{err}");
    assert!(sp.work_dir.join("media_state.json").exists(), "segments kept for resume");
    let first0 = f.srv.ranges_for("/r_000.ts").len();
    assert_eq!(first0, 1);

    std::fs::rename(&hidden, &s2).unwrap();
    let env = JobEnv::standalone(c).unwrap();
    let out = run_media_job(&env, &sp).await.unwrap();
    assert_eq!(f.srv.ranges_for("/r_000.ts").len(), 1, "segment 0 must not be re-downloaded");
    assert!(near(probe(&out.path).duration, 6.0));
}

#[tokio::test]
async fn m7_without_ffmpeg_writes_concatenated_ts() {
    need_ffmpeg!();
    let f = fx().await;
    ff(&f.root, &hls_args(&["-hls_segment_filename", "n_%03d.ts"], "noff.m3u8"));
    let mut c = cfg();
    c.media.ffmpeg_path = "/nonexistent/ffmpeg".into();
    let env = JobEnv::standalone(c).unwrap();
    let out = run_media_job(&env, &spec(&f, f.srv.url("/noff.m3u8"), "m7.mp4")).await.unwrap();
    assert!(!out.remuxed);
    assert_eq!(out.path.extension().unwrap(), "ts");
    let mut expect = Vec::new();
    for i in 0..3 {
        expect.extend(std::fs::read(f.root.join(format!("n_{i:03}.ts"))).unwrap());
    }
    assert_eq!(std::fs::read(&out.path).unwrap(), expect);
}

#[tokio::test]
async fn m8_live_and_drm_are_rejected() {
    let f = fx().await;
    std::fs::write(f.root.join("live.m3u8"), "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\na.ts\n").unwrap();
    std::fs::write(
        f.root.join("drm.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=SAMPLE-AES,URI=\"skd://x\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(
        f.root.join("drm.mpd"),
        r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT4S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/><Representation id="v" bandwidth="1000" height="240"><SegmentTemplate media="s$Number$.m4s" initialization="i.mp4" duration="2" timescale="1"/></Representation></AdaptationSet></Period></MPD>"#,
    )
    .unwrap();
    let mut no_rec = cfg();
    no_rec.media.live_record = false;
    let env = JobEnv::standalone(no_rec).unwrap();
    let e = run_media_job(&env, &spec(&f, f.srv.url("/live.m3u8"), "a.mp4")).await.unwrap_err();
    assert_eq!(e.code(), "E_FORMAT");
    let e = run_media_job(&env, &spec(&f, f.srv.url("/drm.m3u8"), "b.mp4")).await.unwrap_err();
    assert_eq!(e.code(), "E_UNSUPPORTED_KEY");
    let e = run_media_job(&env, &spec(&f, f.srv.url("/drm.mpd"), "c.mp4")).await.unwrap_err();
    assert_eq!(e.code(), "E_UNSUPPORTED_KEY");
}

/// Live HLS: the playlist grows while we poll, then gets EXT-X-ENDLIST; a second stream stops at the cap.
#[tokio::test]
async fn m9_live_hls_recording() {
    need_ffmpeg!();
    let f = fx().await;
    let src = f.root.join("src");
    std::fs::create_dir_all(&src).unwrap();
    let mut a: Vec<&str> = SRC.to_vec();
    a.extend_from_slice(ENC);
    a.extend_from_slice(&["-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod", "-hls_segment_filename", "s%d.ts", "full.m3u8"]);
    ff(&src, &a);
    let full = std::fs::read_to_string(src.join("full.m3u8")).unwrap();
    let segs: Vec<(String, String)> = {
        let lines: Vec<&str> = full.lines().collect();
        lines.windows(2).filter(|w| w[0].starts_with("#EXTINF")).map(|w| (w[0].to_string(), w[1].to_string())).collect()
    };
    assert!(segs.len() >= 5, "{full}");
    let window = |from: usize, to: usize, end: bool| {
        let mut s = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{from}\n");
        for (inf, uri) in &segs[from..to] {
            s.push_str(&format!("{inf}\n{uri}\n"));
        }
        if end {
            s.push_str("#EXT-X-ENDLIST\n");
        }
        s
    };
    std::fs::write(src.join("live.m3u8"), window(0, 2, false)).unwrap();
    let p = src.join("live.m3u8");
    let (w1, w2) = (window(1, 4, false), window(2, segs.len(), true));
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(1200)).await;
        std::fs::write(&p, w1).unwrap();
        tokio::time::sleep(Duration::from_millis(1200)).await;
        std::fs::write(&p, w2).unwrap();
    });
    let env = JobEnv::standalone(cfg()).unwrap();
    let plan = plan_media(&env.client, &f.srv.url("/src/live.m3u8"), &Default::default(), 1080, None, &AudioPreference::Aac).await.unwrap();
    assert!(plan.live && plan.playlist_url.is_some());
    let out = run_media_job(&env, &spec(&f, f.srv.url("/src/live.m3u8"), "live.mp4")).await.unwrap();
    writer.await.unwrap();
    assert_eq!(out.segments as usize, segs.len(), "every segment recorded exactly once");
    assert_eq!(out.source, "hls-live");
    let pr = probe(&out.path);
    assert_eq!((pr.video, pr.audio), (1, 1));
    assert!(near(pr.duration, 6.0), "{pr:?}");
    // never-ending stream: stops at the duration cap
    std::fs::write(src.join("forever.m3u8"), window(0, segs.len(), false)).unwrap();
    let mut c = cfg();
    c.media.live_max_duration = 2;
    let env = JobEnv::standalone(c).unwrap();
    let out = run_media_job(&env, &spec(&f, f.srv.url("/src/forever.m3u8"), "cap.mp4")).await.unwrap();
    assert!(out.segments >= 2 && (out.segments as usize) < segs.len(), "{}", out.segments);
}

/// HLS SUBTITLES rendition → `<name>.<lang>.vtt` next to the video; a broken subtitle playlist is skipped.
#[tokio::test]
async fn m10_hls_subtitles_saved_as_vtt() {
    need_ffmpeg!();
    let f = fx().await;
    ff(&f.root, &hls_args(&["-hls_segment_filename", "sv_%03d.ts"], "sv.m3u8"));
    std::fs::write(f.root.join("zh_0.vtt"), "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n00:00.000 --> 00:02.000\n你好\n")
        .unwrap();
    std::fs::write(f.root.join("zh_1.vtt"), "WEBVTT\n\n00:02.000 --> 00:04.000\n世界\n").unwrap();
    std::fs::write(
        f.root.join("zh.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nzh_0.vtt\n#EXTINF:2,\nzh_1.vtt\n#EXT-X-ENDLIST\n",
    )
    .unwrap();
    std::fs::write(
        f.root.join("subs.m3u8"),
        "#EXTM3U\n#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"s\",NAME=\"中文\",LANGUAGE=\"zh\",URI=\"zh.m3u8\"\n\
         #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"s\",NAME=\"English\",LANGUAGE=\"en\",URI=\"missing.m3u8\"\n\
         #EXT-X-STREAM-INF:BANDWIDTH=500000,RESOLUTION=320x240,SUBTITLES=\"s\"\nsv.m3u8\n",
    )
    .unwrap();
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_media_job(&env, &spec(&f, f.srv.url("/subs.m3u8"), "subbed.mp4")).await.unwrap();
    assert_eq!(out.extra_files.len(), 1, "{:?}", out.extra_files);
    let vtt_path = &out.extra_files[0];
    assert!(vtt_path.to_string_lossy().ends_with("subbed.zh.vtt"), "{vtt_path:?}");
    let vtt = std::fs::read_to_string(vtt_path).unwrap();
    assert!(vtt.starts_with("WEBVTT\n") && vtt.contains("你好") && vtt.contains("世界"), "{vtt}");
    assert_eq!(vtt.matches("WEBVTT").count(), 1, "{vtt}");
    assert!(!vtt.contains("X-TIMESTAMP-MAP"));
}

/// Dynamic MPD: joins at the live edge, follows new segments, finishes when the MPD turns static.
#[tokio::test]
async fn m11_dynamic_mpd_recording() {
    need_ffmpeg!();
    let f = fx().await;
    let sub = f.root.join("ld");
    std::fs::create_dir_all(&sub).unwrap();
    let mut a: Vec<&str> = SRC.to_vec();
    a.extend_from_slice(ENC);
    a.extend_from_slice(&[
        "-map",
        "0:v",
        "-map",
        "1:a",
        "-f",
        "dash",
        "-seg_duration",
        "1",
        "-use_template",
        "1",
        "-use_timeline",
        "0",
        "manifest.mpd",
    ]);
    ff(&sub, &a);
    let stat = std::fs::read_to_string(sub.join("manifest.mpd")).unwrap();
    let dynamic = |ago_ms: i64| {
        let ast = (chrono::Utc::now() - chrono::Duration::milliseconds(ago_ms)).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        let mut m = stat.replace(r#"type="static""#, &format!(r#"type="dynamic" availabilityStartTime="{ast}""#));
        if let Some(i) = m.find("mediaPresentationDuration=\"") {
            let j = m[i + 27..].find('"').unwrap() + i + 28;
            m.replace_range(i..j, "");
        }
        m
    };
    std::fs::write(sub.join("live.mpd"), dynamic(2500)).unwrap();
    let p = sub.join("live.mpd");
    let st2 = stat.clone();
    let writer = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(3000)).await;
        std::fs::write(&p, st2).unwrap(); // the broadcast ended
    });
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_media_job(&env, &spec(&f, f.srv.url("/ld/live.mpd"), "dl.mp4")).await.unwrap();
    writer.await.unwrap();
    assert_eq!(out.source, "dash-live");
    let pr = probe(&out.path);
    assert_eq!((pr.video, pr.audio), (1, 1), "{pr:?}");
    assert!(pr.duration > 4.5, "recorded to the end: {pr:?}");
    // a stream that never ends stops at the cap, starting from the live edge (not from segment 1)
    std::fs::write(sub.join("forever.mpd"), dynamic(5500)).unwrap();
    let mut c = cfg();
    c.media.live_max_duration = 1;
    let env = JobEnv::standalone(c).unwrap();
    let out = run_media_job(&env, &spec(&f, f.srv.url("/ld/forever.mpd"), "cap2.mp4")).await.unwrap();
    assert!(out.segments >= 2 && out.segments < 12, "{}", out.segments);
    assert_eq!(f.srv.ranges_for("/ld/chunk-stream0-00001.m4s").len(), 1, "second recording started at the live edge");
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

/// Demuxed HLS master offering AAC + AC-3 + E-AC-3 for the same 240p video.
/// Default `audio_preference=aac` must pick AAC and still mux video+audio into one file.
#[tokio::test]
async fn m12_audio_preference_aac_over_ac3_and_muxes() {
    if !have_ffmpeg() {
        return;
    }
    let f = fx().await;
    // shared video (no audio)
    ff(
        &f.root,
        &[
            "-f",
            "lavfi",
            "-i",
            "testsrc=size=320x240:rate=25:duration=4",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-an",
            "-f",
            "hls",
            "-hls_time",
            "2",
            "-hls_playlist_type",
            "vod",
            "-hls_segment_filename",
            "v_%03d.ts",
            "v.m3u8",
        ],
    );
    for (name, codec) in [("a_aac", "aac"), ("a_ac3", "ac3"), ("a_eac3", "eac3")] {
        ff(
            &f.root,
            &[
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=4",
                "-c:a",
                codec,
                "-b:a",
                "96k",
                "-ac",
                if codec == "aac" { "2" } else { "6" },
                "-f",
                "hls",
                "-hls_time",
                "2",
                "-hls_playlist_type",
                "vod",
                "-hls_segment_filename",
                &format!("{name}_%03d.ts"),
                &format!("{name}.m3u8"),
            ],
        );
    }
    std::fs::write(
        f.root.join("master.m3u8"),
        r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud1",NAME="English",LANGUAGE="en",DEFAULT=YES,CHANNELS="2",URI="a_aac.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud2",NAME="English",LANGUAGE="en",DEFAULT=YES,CHANNELS="6",URI="a_ac3.m3u8"
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud3",NAME="English",LANGUAGE="en",DEFAULT=YES,CHANNELS="6",URI="a_eac3.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=400000,CODECS="avc1.42c01e,mp4a.40.2",RESOLUTION=320x240,AUDIO="aud1"
v.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=600000,CODECS="avc1.42c01e,ac-3",RESOLUTION=320x240,AUDIO="aud2"
v.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=500000,CODECS="avc1.42c01e,ec-3",RESOLUTION=320x240,AUDIO="aud3"
v.m3u8
"#,
    )
    .unwrap();
    let env = JobEnv::standalone(cfg()).unwrap();
    let plan =
        plan_media(&env.client, &f.srv.url("/master.m3u8"), &Default::default(), 1080, Some(240), &AudioPreference::Aac).await.unwrap();
    assert_eq!(plan.tracks.len(), 2, "video + separate AAC audio: {:?}", plan.tracks.iter().map(|t| &t.label).collect::<Vec<_>>());
    assert!(plan.tracks.iter().any(|t| t.kind == "video"));
    assert!(plan.tracks.iter().any(|t| t.kind == "audio"));
    // the audio track must come from the AAC playlist
    let a = plan.tracks.iter().find(|t| t.kind == "audio").unwrap();
    assert!(a.segments[0].url.contains("a_aac"), "{}", a.segments[0].url);

    let out = run_media_job(&env, &spec(&f, f.srv.url("/master.m3u8"), "aac-mux.mp4")).await.unwrap();
    let pr = probe(&out.path);
    assert_eq!((pr.video, pr.audio), (1, 1), "auto-merged into one file: {pr:?}");
    assert!(pr.duration > 3.5, "{pr:?}");
    // codec name via ffprobe
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=codec_name", "-of", "csv=p=0"])
        .arg(&out.path)
        .output()
        .unwrap();
    let codec = String::from_utf8_lossy(&o.stdout).trim().to_string();
    assert_eq!(codec, "aac", "default preference must pick AAC, got {codec}");

    // only-AC-3 master still downloads (fallback)
    std::fs::write(
        f.root.join("ac3-only.m3u8"),
        r#"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID="aud2",NAME="English",LANGUAGE="en",DEFAULT=YES,CHANNELS="6",URI="a_ac3.m3u8"
#EXT-X-STREAM-INF:BANDWIDTH=600000,CODECS="avc1.42c01e,ac-3",RESOLUTION=320x240,AUDIO="aud2"
v.m3u8
"#,
    )
    .unwrap();
    let plan = plan_media(&env.client, &f.srv.url("/ac3-only.m3u8"), &Default::default(), 1080, None, &AudioPreference::Aac).await.unwrap();
    assert!(plan.tracks.iter().find(|t| t.kind == "audio").unwrap().segments[0].url.contains("a_ac3"));
}

/// Sites that stream one whole fMP4 file per track (video and audio separately, fetched with
/// Range requests; real-world: Bilibili's `…-30016.m4s` / `…-30216.m4s`): kind `av` downloads
/// both files and merges them into one MP4.
#[tokio::test]
async fn m13_separate_video_and_audio_files_are_merged() {
    need_ffmpeg!();
    let f = fx().await;
    let frag = ["-movflags", "frag_keyframe+empty_moov+default_base_moof", "-f", "mp4"];
    let mut v: Vec<&str> = SRC.to_vec();
    v.extend_from_slice(ENC);
    v.extend_from_slice(&["-map", "0:v"]);
    v.extend_from_slice(&frag);
    v.push("clip-1-30016.m4s");
    ff(&f.root, &v);
    let mut a: Vec<&str> = SRC.to_vec();
    a.extend_from_slice(ENC);
    a.extend_from_slice(&["-map", "1:a"]);
    a.extend_from_slice(&frag);
    a.push("clip-1-30216.m4s");
    ff(&f.root, &a);
    let env = JobEnv::standalone(cfg()).unwrap();
    let mut s = spec(&f, f.srv.url("/clip-1-30016.m4s?e=1"), "av.mp4");
    s.audio_url = Some(f.srv.url("/clip-1-30216.m4s?e=1"));
    let out = run_media_job(&env, &s).await.unwrap();
    assert_eq!(out.source, "av");
    let p = probe(&out.path);
    assert_eq!((p.video, p.audio, p.height), (1, 1, 240), "{p:?}");
    assert!(near(p.duration, 6.0), "{p:?}");
}
