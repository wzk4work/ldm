//! yt-dlp integration tests against the fake yt-dlp in tests/fixtures (no network).
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ldm_core::manager::Manager;
use ldm_core::store::Store;
use ldm_core::Config;
use ldm_ipc::messages::DownloadAdd;

const YT: &str = "https://www.youtube.com/watch?v=aqz-KE-bpKQ";
const TITLE: &str = "Test clip: Big Buck Bunny (trailer) | 100% free";

struct Fx {
    tmp: tempfile::TempDir,
    root: PathBuf,
    srv: ldm_testsrv::Server,
}

impl Fx {
    async fn new() -> Fx {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("www");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("v.mp4"), vec![7u8; 5000]).unwrap();
        std::fs::write(root.join("page.m3u8"), "<!doctype html><html><body>Sign in</body></html>").unwrap();
        std::fs::write(
            root.join("drm.mpd"),
            r#"<?xml version="1.0"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S"><Period><AdaptationSet contentType="video" mimeType="video/mp4"><ContentProtection schemeIdUri="urn:uuid:edef8ba9-79d6-4ace-a3c8-27dcd51d21ed"/><Representation id="v" bandwidth="100000" width="320" height="180" codecs="avc1.4d401f"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#,
        )
        .unwrap();
        let srv = ldm_testsrv::start(root.clone(), "127.0.0.1:0").await.unwrap();
        Fx { tmp, root, srv }
    }
    /// A wrapper around the fake that sets its behaviour knobs (env is per process, tests run in parallel).
    fn fake(&self, env: &str) -> PathBuf {
        let fake = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-yt-dlp");
        let p = self.tmp.path().join("yt-dlp");
        let log = self.log_path();
        std::fs::write(
            &p,
            format!("#!/bin/sh\nexport FAKE_YTDLP_LOG='{}' {env}\nexec python3 '{}' \"$@\"\n", log.display(), fake.display()),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }
    fn log_path(&self) -> PathBuf {
        self.tmp.path().join("ytdlp-argv.jsonl")
    }
    /// argv of every yt-dlp invocation so far.
    /// yt-dlp invocations, without the `--version` probes LDM makes to label the binary.
    fn calls(&self) -> Vec<Vec<String>> {
        std::fs::read_to_string(self.log_path())
            .unwrap_or_default()
            .lines()
            .map(|l| serde_json::from_str::<Vec<String>>(l).unwrap())
            .filter(|a| a.as_slice() != ["--version"])
            .collect()
    }
    fn downloads(&self) -> Vec<Vec<String>> {
        self.calls().into_iter().filter(|a| !a.iter().any(|x| x == "-J" || x == "--version")).collect()
    }
    fn cfg(&self, env: &str) -> Config {
        let mut c = Config::default();
        c.paths.download_dir = self.tmp.path().join("dl").to_string_lossy().into_owned();
        c.paths.part_dir = self.tmp.path().join("parts").to_string_lossy().into_owned();
        c.paths.by_category = false;
        c.engine.retry_base_ms = 20;
        c.engine.max_retries = 1;
        c.engine.fsync = false;
        c.general.notify_on_complete = false;
        c.ytdlp.path = self.fake(env).to_string_lossy().into_owned();
        c
    }
    fn manager(&self, cfg: Config) -> Manager {
        let m = Manager::new(cfg, None, Store::open(&self.tmp.path().join("db/downloads.db")).unwrap()).unwrap();
        m.start();
        m
    }
}

async fn wait_state(m: &Manager, id: &str, want: &str, secs: u64) -> serde_json::Value {
    let t0 = Instant::now();
    loop {
        let j = m.get(id).unwrap();
        if j["state"] == want {
            return j;
        }
        if t0.elapsed() > Duration::from_secs(secs) {
            panic!("job {id} did not reach {want}: {j}");
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
}

fn add(url: &str) -> DownloadAdd {
    DownloadAdd { url: url.into(), ..Default::default() }
}

fn stream(n: usize) -> Vec<u8> {
    (0..n).map(|k| (k % 251) as u8).collect()
}

#[tokio::test]
async fn y1_youtube_is_routed_to_ytdlp_named_after_title() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg(""));
    let r = m.add(add(YT)).unwrap();
    let job = r.job.unwrap();
    assert_eq!(job.kind, "ytdlp");
    let j = wait_state(&m, &job.id, "Completed", 20).await;
    let path = PathBuf::from(j["final_path"].as_str().unwrap());
    assert_eq!(path.file_name().unwrap().to_string_lossy(), format!("{TITLE}.mp4"));
    assert_eq!(path.parent().unwrap(), fx.tmp.path().join("dl"));
    let mut want = stream(200000);
    want.extend(stream(200000));
    assert_eq!(std::fs::read(&path).unwrap(), want, "video+audio merged");
    assert_eq!(j["via"], "yt-dlp");
    assert_eq!(j["category"], "video");
    assert_eq!(j["total"], 400000);
    let d = fx.downloads();
    assert_eq!(d.len(), 1);
    let a = d[0].join(" ");
    assert!(a.contains("--no-playlist") && a.contains("--continue") && a.contains("--merge-output-format mp4"), "{a}");
    assert!(a.contains("-f bv*[height<=1080][dynamic_range=SDR]+ba[acodec^=mp4a]/"), "default ≤1080 SDR + AAC: {a}");
    assert!(!a.contains("cookies"), "cookies only when enabled: {a}");
    // parts went to the job's work dir and were cleaned up
    assert!(!fx.tmp.path().join("parts").join(&job.id).join("ytdlp").exists());
}

#[tokio::test]
async fn y2_quality_choice_and_audio_only() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg(""));
    let a = m.add(DownloadAdd { preferred_height: Some(360), ..add("https://youtu.be/aqz-KE-bpKQ") }).unwrap().job.unwrap();
    wait_state(&m, &a.id, "Completed", 20).await;
    let b = m.add(DownloadAdd { preferred_height: Some(0), ..add("https://music.youtube.com/watch?v=aqz-KE-bpKQ") }).unwrap().job.unwrap();
    let j = wait_state(&m, &b.id, "Completed", 20).await;
    assert!(j["final_path"].as_str().unwrap().ends_with(".m4a"), "{j}");
    let d = fx.downloads();
    assert!(d[0].join(" ").contains("-f bv*[height<=360]"), "{:?}", d[0]);
    assert!(d[1].join(" ").contains("-f ba[acodec^=mp4a]/ba[ext=m4a]/ba "), "{:?}", d[1]);
}

#[tokio::test]
async fn y3_pause_terminates_and_resume_continues_part_files() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg("FAKE_YTDLP_SIZE=2000000 FAKE_YTDLP_DELAY=0.08"));
    let job = m.add(add(YT)).unwrap().job.unwrap();
    let t0 = Instant::now();
    loop {
        let j = m.get(&job.id).unwrap();
        if j["state"] == "Running" && j["got"].as_u64().unwrap_or(0) > 0 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "{j}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    m.pause(&job.id).unwrap();
    let j = wait_state(&m, &job.id, "Paused", 10).await;
    assert!(j["filename"].as_str().unwrap().starts_with("Test clip"), "{j}");
    let part_dir = fx.tmp.path().join("parts").join(&job.id).join("ytdlp");
    let parts: Vec<_> = std::fs::read_dir(&part_dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert!(parts.iter().any(|p| p.ends_with(".part")), "{parts:?}");
    m.resume(&job.id).unwrap();
    let j = wait_state(&m, &job.id, "Completed", 30).await;
    let mut want = stream(2000000);
    want.extend(stream(2000000));
    assert_eq!(std::fs::read(j["final_path"].as_str().unwrap()).unwrap(), want, "resumed bytes intact");
    let d = fx.downloads();
    assert_eq!(d.len(), 2, "one run before the pause, one after");
    assert_eq!(d[0], d[1], "same output template so yt-dlp finds its .part file");
}

#[tokio::test]
async fn y4_fallback_once_for_403_media_and_records_reason() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg(""));
    let url = fx.srv.url("/v.mp4?status=403");
    let req =
        DownloadAdd { title_hint: Some("Page video".into()), page_url: Some("https://video.example.com/watch/1".into()), ..add(&url) };
    let job = m.add(req).unwrap().job.unwrap();
    assert_eq!(job.kind, "http");
    let j = wait_state(&m, &job.id, "Completed", 20).await;
    assert_eq!(j["kind"], "ytdlp");
    assert_eq!(j["via"], "yt-dlp");
    assert_eq!(j["fallbackReason"], "HTTP 403");
    assert_eq!(j["ytdlp"]["fallback_from"], "http");
    assert_eq!(j["ytdlp"]["url"], "https://video.example.com/watch/1", "page URL handed to yt-dlp");
    let h = j["history"].as_array().unwrap();
    assert_eq!(h.len(), 2, "{h:?}");
    assert!(h[0].as_str().unwrap().contains("retrying once via yt-dlp (HTTP 403)"), "{h:?}");
    assert!(h[1].as_str().unwrap().contains(" yt-dlp 2026.10.10 (用户指定)"), "{h:?}");
    assert!(j["final_path"].as_str().unwrap().ends_with("/Page video.mp4"), "{j}");
    assert_eq!(fx.downloads().len(), 1);
}

#[tokio::test]
async fn y5_fallback_is_one_shot() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg("FAKE_YTDLP_MODE=fail"));
    let job = m.add(DownloadAdd { title_hint: Some("Page video".into()), ..add(&fx.srv.url("/v.mp4?status=410")) }).unwrap().job.unwrap();
    let j = wait_state(&m, &job.id, "Failed", 20).await;
    assert_eq!(j["kind"], "ytdlp");
    assert_eq!(j["error_code"], "E_YTDLP");
    let e = j["error"].as_str().unwrap();
    assert!(e.contains("via yt-dlp") && e.contains("HTTP 410"), "{e}");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(m.get(&job.id).unwrap()["state"], "Failed");
    assert_eq!(fx.downloads().len(), 1, "exactly one yt-dlp attempt");
    // a manual retry re-runs yt-dlp (still no loop back to LDM)
    m.resume(&job.id).unwrap();
    wait_state(&m, &job.id, "Failed", 20).await;
    assert_eq!(fx.downloads().len(), 2);
}

#[tokio::test]
async fn y6_eligible_manifest_failure_falls_back_but_drm_network_404_do_not() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg(""));
    // HLS URL whose body is a web page: manifest parse failure → eligible
    let hls = m.add(add(&fx.srv.url("/page.m3u8"))).unwrap().job.unwrap();
    assert_eq!(hls.kind, "hls");
    let j = wait_state(&m, &hls.id, "Completed", 20).await;
    assert!(j["fallbackReason"].as_str().unwrap().starts_with("manifest:"), "{j}");
    let n = fx.downloads().len();
    // not eligible
    let drm = m.add(add(&fx.srv.url("/drm.mpd"))).unwrap().job.unwrap();
    let j = wait_state(&m, &drm.id, "Failed", 20).await;
    assert_eq!(j["error_code"], "E_UNSUPPORTED_KEY");
    let nf = m.add(DownloadAdd { title_hint: Some("x".into()), ..add(&fx.srv.url("/missing.mp4")) }).unwrap().job.unwrap();
    wait_state(&m, &nf.id, "Failed", 20).await;
    let net = m.add(DownloadAdd { title_hint: Some("x".into()), ..add("http://127.0.0.1:9/v.mp4") }).unwrap().job.unwrap();
    let j = wait_state(&m, &net.id, "Failed", 30).await;
    assert_eq!(j["kind"], "http");
    // a plain (non-media) file download with 403 never goes to yt-dlp
    let pz = m.add(add(&fx.srv.url("/a.zip?status=403"))).unwrap().job.unwrap();
    let j = wait_state(&m, &pz.id, "Failed", 20).await;
    assert_eq!(j["kind"], "http");
    for id in [&drm.id, &nf.id, &net.id] {
        assert_ne!(m.get(id).unwrap()["kind"], "ytdlp");
    }
    assert_eq!(fx.downloads().len(), n, "none of the ineligible failures ran yt-dlp");
}

#[tokio::test]
async fn y7_settings_toggles_and_site_rules() {
    let fx = Fx::new().await;
    // auto_fallback off
    let mut c = fx.cfg("");
    c.ytdlp.auto_fallback = false;
    let m = fx.manager(c);
    let j = m.add(DownloadAdd { title_hint: Some("v".into()), ..add(&fx.srv.url("/v.mp4?status=403")) }).unwrap().job.unwrap();
    assert_eq!(wait_state(&m, &j.id, "Failed", 20).await["kind"], "http");
    // YouTube still routed (routing is not fallback)
    assert_eq!(m.add(add(YT)).unwrap().job.unwrap().kind, "ytdlp");
    drop(m);

    let fx = Fx::new().await;
    let mut c = fx.cfg("");
    c.ytdlp.enabled = false;
    let m = fx.manager(c);
    assert_ne!(m.add(DownloadAdd { paused: true, ..add(YT) }).unwrap().job.unwrap().kind, "ytdlp", "disabled: no routing");
    let j = m.add(DownloadAdd { title_hint: Some("v".into()), ..add(&fx.srv.url("/v.mp4?status=403")) }).unwrap().job.unwrap();
    assert_eq!(wait_state(&m, &j.id, "Failed", 20).await["kind"], "http");
    // explicit kind with yt-dlp disabled: clear error
    let j = m.add(DownloadAdd { kind: Some("ytdlp".into()), ..add("https://vimeo.com/1") }).unwrap().job.unwrap();
    let j = wait_state(&m, &j.id, "Failed", 20).await;
    assert!(j["error"].as_str().unwrap().contains("ytdlp.enabled"), "{j}");
    drop(m);

    let fx = Fx::new().await;
    let mut c = fx.cfg("");
    c.ytdlp.path = "/nonexistent/yt-dlp".into();
    let m = fx.manager(c);
    assert_ne!(m.add(DownloadAdd { paused: true, ..add(YT) }).unwrap().job.unwrap().kind, "ytdlp", "missing binary: no routing");
    let j = m.add(DownloadAdd { kind: Some("ytdlp".into()), ..add("https://vimeo.com/2") }).unwrap().job.unwrap();
    let e = wait_state(&m, &j.id, "Failed", 20).await;
    assert!(e["error"].as_str().unwrap().contains("ytdlp.path 指向的 yt-dlp 不存在"), "{e}");
    drop(m);

    // per-site rules: off blocks fallback (by page or URL host), always routes
    let fx = Fx::new().await;
    let mut c = fx.cfg("");
    c.ytdlp.site_rules.insert("blocked.example".into(), "off".into());
    c.ytdlp.site_rules.insert("always.example".into(), "always".into());
    c.ytdlp.site_rules.insert("youtube.com".into(), "off".into());
    let m = fx.manager(c);
    let j = m
        .add(DownloadAdd {
            title_hint: Some("v".into()),
            page_url: Some("https://www.blocked.example/v".into()),
            ..add(&fx.srv.url("/v.mp4?status=403"))
        })
        .unwrap()
        .job
        .unwrap();
    assert_eq!(wait_state(&m, &j.id, "Failed", 20).await["kind"], "http");
    assert_eq!(m.add(DownloadAdd { paused: true, ..add("https://cdn.always.example/watch/1") }).unwrap().job.unwrap().kind, "ytdlp");
    assert_ne!(m.add(DownloadAdd { paused: true, ..add(YT) }).unwrap().job.unwrap().kind, "ytdlp", "youtube = off");
    assert!(fx.downloads().is_empty());
    let rs = m.ruleset();
    assert!(rs.ytdlp && rs.ytdlp_sites.contains(&"always.example".into()) && !rs.ytdlp_sites.contains(&"youtube.com".into()));
    assert!(rs.ytdlp_off.contains(&"blocked.example".into()));
}

#[tokio::test]
async fn y8_probe_lists_qualities_and_caches() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg(""));
    let p = m.probe(YT, &Default::default()).await.unwrap();
    assert_eq!(p.source, "ytdlp");
    assert_eq!(p.duration, Some(33.0));
    let labels: Vec<_> = p.qualities.iter().map(|q| q.label.clone()).collect();
    assert_eq!(labels, ["2160p60 · vp09 · HDR", "1080p60 · avc1", "720p · avc1", "360p · avc1", "mp4a 129k"]);
    m.probe(YT, &Default::default()).await.unwrap();
    // the job reuses the cached -J for its title
    let j = m.add(add(YT)).unwrap().job.unwrap();
    wait_state(&m, &j.id, "Completed", 20).await;
    let probes = fx.calls().iter().filter(|a| a.iter().any(|x| x == "-J")).count();
    assert_eq!(probes, 1, "cached");
    // forced yt-dlp probe on another site
    let p = m.probe_ytdlp("https://vimeo.com/123").await.unwrap();
    assert_eq!(p.source, "ytdlp");
}

#[tokio::test]
async fn y9_playlist_items_and_bot_check() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg(""));
    let jobs = m.add_playlist(DownloadAdd { paused: true, ..add("https://www.youtube.com/playlist?list=PL123") }).await.unwrap();
    assert_eq!(jobs.len(), 3);
    assert!(jobs.iter().all(|j| j.kind == "ytdlp" && j.url.contains("v=item")));
    let pl = fx.calls();
    assert!(pl[0].contains(&"--flat-playlist".to_string()));
    // a single video URL given as "playlist" is just that video
    let one = m.add_playlist(DownloadAdd { paused: true, ..add("https://youtu.be/solo") }).await.unwrap();
    assert_eq!(one.len(), 1);

    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg("FAKE_YTDLP_MODE=bot"));
    let j = m.add(add(YT)).unwrap().job.unwrap();
    let j = wait_state(&m, &j.id, "Failed", 20).await;
    let e = j["error"].as_str().unwrap();
    assert!(e.contains("not a bot") && e.contains("不会绕过"), "{e}");
}

#[tokio::test]
async fn y10_cookies_and_extra_args_only_when_configured() {
    let fx = Fx::new().await;
    let mut c = fx.cfg("");
    c.ytdlp.cookies_from_browser = "chromium".into();
    c.ytdlp.extra_args = vec!["--embed-metadata".into()];
    c.media.output_container = "mkv".into();
    let m = fx.manager(c);
    let j = m.add(add(YT)).unwrap().job.unwrap();
    let j = wait_state(&m, &j.id, "Completed", 20).await;
    assert!(j["final_path"].as_str().unwrap().ends_with(".mkv"));
    for a in fx.calls() {
        let s = a.join(" ");
        assert!(s.contains("--cookies-from-browser chromium") && s.contains("--embed-metadata"), "{s}");
    }
    let _ = &fx.root;
}

fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(t);
    v.extend_from_slice(body);
    v
}

#[tokio::test]
async fn y11_mp4_fragment_capture_is_not_media_and_falls_back_complete_file_does_not() {
    let fx = Fx::new().await;
    std::fs::write(fx.root.join("frag.mp4"), [bx(b"moof", &[0; 64]), bx(b"mdat", &[1; 4000])].concat()).unwrap();
    std::fs::write(fx.root.join("full.mp4"), [bx(b"ftyp", b"isom0000"), bx(b"moov", &[0; 64]), bx(b"mdat", &[1; 4000])].concat()).unwrap();
    let m = fx.manager(fx.cfg(""));
    let page = Some("https://www.social.example/videos/1".to_string());
    let a = m
        .add(DownloadAdd {
            title_hint: Some("Clip".into()),
            page_url: page.clone(),
            ..add(&fx.srv.url("/frag.mp4?bytestart=0&byteend=4100"))
        })
        .unwrap()
        .job
        .unwrap();
    let j = wait_state(&m, &a.id, "Completed", 20).await;
    assert_eq!(j["via"], "yt-dlp");
    assert!(j["fallbackReason"].as_str().unwrap().starts_with("not media: MP4 fragment"), "{j}");
    let b = m.add(DownloadAdd { title_hint: Some("Clip 2".into()), page_url: page, ..add(&fx.srv.url("/full.mp4")) }).unwrap().job.unwrap();
    let j = wait_state(&m, &b.id, "Completed", 20).await;
    assert_eq!(j["kind"], "http", "a complete MP4 stays an LDM download");
    assert!(j.get("via").is_none());
    assert_eq!(fx.downloads().len(), 1);
}

// ---------------------------------------------------------------- live recording (yt-dlp)

fn ffprobe_duration(p: &Path) -> f64 {
    let o = std::process::Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(p)
        .output()
        .unwrap();
    assert!(o.status.success(), "ffprobe failed on {}", p.display());
    String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0.0)
}

fn pid_alive(pid: i32) -> bool {
    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(s) => s.rsplit(')').next().and_then(|r| r.split_whitespace().next()) != Some("Z"),
        Err(_) => false,
    }
}

/// Starts a yt-dlp live job (fake `liveff`: a real ffmpeg writes an endless test stream) and
/// waits until it is recording with growing bytes and elapsed time.
async fn start_live(fx: &Fx, m: &Manager) -> (String, i32) {
    let job = m.add(add("https://www.youtube.com/watch?v=live2")).unwrap().job.unwrap();
    let j = wait_state(m, &job.id, "Recording", 20).await;
    assert_eq!(j["live"], true, "{j}");
    let pidfile = fx.tmp.path().join("ff.pid");
    let t0 = Instant::now();
    let (mut got0, mut rec0) = (0u64, 0f64);
    loop {
        let j = m.get(&job.id).unwrap();
        assert_eq!(j["state"], "Recording", "{j}");
        let (got, rec) = (j["got"].as_u64().unwrap_or(0), j["recordedSecs"].as_f64().unwrap_or(0.0));
        if got0 == 0 && got > 0 {
            (got0, rec0) = (got, rec);
        } else if got0 > 0 && got > got0 && rec > rec0 && rec >= 1.5 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "recording did not progress: {j}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    let pid: i32 = std::fs::read_to_string(&pidfile).unwrap().trim().parse().unwrap();
    assert!(pid_alive(pid));
    (job.id, pid)
}

fn live_cfg(fx: &Fx, extra: &str) -> Config {
    let pid = fx.tmp.path().join("ff.pid");
    fx.cfg(&format!("FAKE_YTDLP_MODE=liveff FAKE_YTDLP_CHILD_PID='{}' {extra}", pid.display()))
}

#[tokio::test]
async fn y20_live_stop_recording_is_graceful_and_playable() {
    let fx = Fx::new().await;
    let m = fx.manager(live_cfg(&fx, ""));
    let (id, ff) = start_live(&fx, &m).await;
    // list() shows the recording without a total
    let l = m.list(None).unwrap();
    let row = l.iter().find(|j| j["id"] == id.as_str()).unwrap();
    assert!(row["total"].is_null() && row["recordedSecs"].as_f64().unwrap() > 0.0, "{row}");
    m.stop_recording(&id).unwrap();
    let j = wait_state(&m, &id, "Completed", 40).await;
    let path = PathBuf::from(j["final_path"].as_str().unwrap());
    assert_eq!(path.file_name().unwrap().to_string_lossy(), "Fake live.mp4");
    let d = ffprobe_duration(&path);
    assert!(d > 1.0, "recorded duration {d}");
    assert!(!pid_alive(ff), "ffmpeg child must have exited");
    assert!(!fx.tmp.path().join("parts").join(&id).join("ytdlp").exists());
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn y21_live_auto_stop_at_max_duration() {
    let fx = Fx::new().await;
    let mut c = live_cfg(&fx, "");
    c.media.live_max_duration = 3;
    let m = fx.manager(c);
    let job = m.add(add("https://www.youtube.com/watch?v=live3")).unwrap().job.unwrap();
    wait_state(&m, &job.id, "Recording", 20).await;
    let t0 = Instant::now();
    let j = wait_state(&m, &job.id, "Completed", 40).await;
    assert!(t0.elapsed() < Duration::from_secs(30));
    let path = PathBuf::from(j["final_path"].as_str().unwrap());
    let d = ffprobe_duration(&path);
    assert!(d > 1.5 && d < 10.0, "auto-stopped recording lasts ~3 s: {d}");
    let pid: i32 = std::fs::read_to_string(fx.tmp.path().join("ff.pid")).unwrap().trim().parse().unwrap();
    assert!(!pid_alive(pid));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn y22_live_stop_finalises_partial_file_when_ytdlp_gives_none() {
    let fx = Fx::new().await;
    let m = fx.manager(live_cfg(&fx, "FAKE_YTDLP_STOP=crash"));
    let (id, ff) = start_live(&fx, &m).await;
    m.stop_recording(&id).unwrap();
    let j = wait_state(&m, &id, "Completed", 40).await;
    let path = PathBuf::from(j["final_path"].as_str().unwrap());
    assert!(path.extension().unwrap() == "mp4", "{}", path.display());
    assert!(ffprobe_duration(&path) > 1.0);
    assert!(!pid_alive(ff));
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn y23_stop_recording_rejects_non_live_and_live_record_off() {
    let fx = Fx::new().await;
    let m = fx.manager(fx.cfg("FAKE_YTDLP_DELAY=0.2"));
    let job = m.add(add(YT)).unwrap().job.unwrap();
    wait_state(&m, &job.id, "Running", 20).await;
    assert!(m.stop_recording(&job.id).is_err(), "a VOD download is not a recording");
    m.remove(&job.id, true).ok();
    let mut c = live_cfg(&fx, "");
    c.media.live_record = false;
    let m2 = Manager::new(c, None, Store::open(&fx.tmp.path().join("db2/downloads.db")).unwrap()).unwrap();
    m2.start();
    let job = m2.add(add("https://www.youtube.com/watch?v=live4")).unwrap().job.unwrap();
    let j = wait_state(&m2, &job.id, "Failed", 20).await;
    assert!(j["error"].to_string().contains("live_record"), "{j}");
}
