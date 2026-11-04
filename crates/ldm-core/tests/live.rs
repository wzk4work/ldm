//! Live recording through the Manager (native HLS): "Recording" state, elapsed time + bytes
//! progress, graceful "停止录制", `media.live_max_duration` auto-stop. The playlist is a local
//! fixture served by ldm-testsrv with `live=ID`, so it keeps growing like a real live stream.
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use ldm_core::manager::Manager;
use ldm_core::store::Store;
use ldm_core::Config;
use ldm_ipc::messages::DownloadAdd;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

struct Fx {
    tmp: tempfile::TempDir,
    srv: ldm_testsrv::Server,
}

/// 40 × 1 s MPEG-TS segments (VOD playlist; the server turns it into a live window).
async fn fx() -> Fx {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("www");
    std::fs::create_dir_all(&root).unwrap();
    let o = Command::new("ffmpeg")
        .current_dir(&root)
        .args(["-y", "-hide_banner", "-loglevel", "error"])
        .args(["-f", "lavfi", "-i", "testsrc=size=160x120:rate=25:duration=40", "-f", "lavfi", "-i", "sine=frequency=440:duration=40"])
        .args([
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            "25",
            "-keyint_min",
            "25",
            "-sc_threshold",
            "0",
            "-c:a",
            "aac",
            "-b:a",
            "48k",
        ])
        .args(["-f", "hls", "-hls_time", "1", "-hls_playlist_type", "vod", "-hls_segment_filename", "s%02d.ts", "ch.m3u8"])
        .output()
        .unwrap();
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    let srv = ldm_testsrv::start(root, "127.0.0.1:0").await.unwrap();
    Fx { tmp, srv }
}

impl Fx {
    fn manager(&self, f: impl FnOnce(&mut Config)) -> Manager {
        let mut c = Config::default();
        c.paths.download_dir = self.tmp.path().join("dl").to_string_lossy().into_owned();
        c.paths.part_dir = self.tmp.path().join("parts").to_string_lossy().into_owned();
        c.paths.by_category = false;
        c.engine.retry_base_ms = 20;
        c.engine.fsync = false;
        c.general.notify_on_complete = false;
        f(&mut c);
        let m = Manager::new(c, None, Store::open(&self.tmp.path().join("db/downloads.db")).unwrap()).unwrap();
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
        assert!(t0.elapsed() < Duration::from_secs(secs), "job {id} did not reach {want}: {j}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn duration(p: &Path) -> f64 {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(p)
        .output()
        .unwrap();
    assert!(o.status.success(), "ffprobe failed on {}", p.display());
    String::from_utf8_lossy(&o.stdout).trim().parse().unwrap_or(0.0)
}

#[tokio::test]
async fn l1_native_hls_live_records_and_stops_gracefully() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg missing – skipped");
        return;
    }
    let f = fx().await;
    let m = f.manager(|_| {});
    let mut events = m.subscribe();
    let job = m.add(DownloadAdd { url: f.srv.url("/ch.m3u8?live=l1"), ..Default::default() }).unwrap().job.unwrap();
    let j = wait_state(&m, &job.id, "Recording", 20).await;
    assert_eq!(j["live"], true, "{j}");
    // progress grows: bytes and recorded media time, never a total
    let t0 = Instant::now();
    let mut first: Option<(u64, f64)> = None;
    loop {
        let row = m.list(None).unwrap().into_iter().find(|r| r["id"] == job.id.as_str()).unwrap();
        assert_eq!(row["state"], "Recording", "{row}");
        assert!(row["total"].is_null(), "{row}");
        let cur = (row["got"].as_u64().unwrap_or(0), row["recordedSecs"].as_f64().unwrap_or(0.0));
        match first {
            None if cur.0 > 0 => first = Some(cur),
            Some(a) if cur.0 > a.0 && cur.1 > a.1 && cur.1 >= 4.0 => break,
            _ => {}
        }
        assert!(t0.elapsed() < Duration::from_secs(20), "recording does not progress: {row}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    // the progress events carry the recorded time and phase
    let mut saw = false;
    while let Ok(e) = events.try_recv() {
        if e.kind == "JobProgress" && e.payload["jobId"] == job.id.as_str() && e.payload["recordedSecs"].as_f64().unwrap_or(0.0) > 0.0 {
            assert_eq!(e.payload["state"], "Recording", "{}", e.payload);
            saw = true;
        }
    }
    assert!(saw, "JobProgress with recordedSecs");
    let rec = m.get(&job.id).unwrap()["recordedSecs"].as_f64().unwrap();
    let t_stop = Instant::now();
    m.stop_recording(&job.id).unwrap();
    let j = wait_state(&m, &job.id, "Completed", 30).await;
    assert!(t_stop.elapsed() < Duration::from_secs(10), "stop is immediate, not after the next poll");
    let path = PathBuf::from(j["final_path"].as_str().unwrap());
    let d = duration(&path);
    assert!(d >= rec - 1.5 && d < 40.0, "file {d} s vs recorded {rec} s");
    assert!(m.stop_recording(&job.id).is_err(), "no longer recording");
    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn l2_native_hls_live_auto_stops_at_max_duration() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg missing – skipped");
        return;
    }
    let f = fx().await;
    let m = f.manager(|c| c.media.live_max_duration = 4);
    let job = m.add(DownloadAdd { url: f.srv.url("/ch.m3u8?live=l2"), ..Default::default() }).unwrap().job.unwrap();
    wait_state(&m, &job.id, "Recording", 20).await;
    let j = wait_state(&m, &job.id, "Completed", 30).await;
    let d = duration(Path::new(j["final_path"].as_str().unwrap()));
    assert!((3.5..8.0).contains(&d), "capped at ~4 s of media: {d}");
    let _ = std::fs::remove_file(j["final_path"].as_str().unwrap());
}

#[tokio::test]
async fn l3_live_window_is_a_growing_sliding_playlist() {
    let vod = "#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:1,\na.ts\n#EXTINF:1,\nb.ts\n#EXTINF:1,\nc.ts\n#EXTINF:1,\nd.ts\n#EXT-X-ENDLIST\n";
    let w0 = ldm_testsrv::live_window(vod, Duration::from_millis(100), 3);
    assert!(w0.contains("#EXT-X-MEDIA-SEQUENCE:0") && w0.contains("b.ts") && !w0.contains("c.ts") && !w0.contains("ENDLIST"), "{w0}");
    let w1 = ldm_testsrv::live_window(vod, Duration::from_millis(2100), 2);
    assert!(w1.contains("#EXT-X-MEDIA-SEQUENCE:2") && w1.contains("d.ts") && !w1.contains("b.ts") && !w1.contains("PLAYLIST-TYPE"), "{w1}");
}

#[tokio::test]
async fn l4_live_starts_at_the_edge_unless_live_from_start() {
    assert_eq!(ldm_core::media::job::live_edge_skip(&[1.0; 9], 1.0), 6);
    assert_eq!(ldm_core::media::job::live_edge_skip(&[2.0, 2.0], 2.0), 0);
    assert_eq!(ldm_core::media::job::live_edge_skip(&[], 4.0), 0);
    if !have_ffmpeg() {
        eprintln!("ffmpeg missing – skipped");
        return;
    }
    let f = fx().await;
    // the stream has been "on air" for ~8 s before we join; the playlist keeps a 30-segment DVR window
    let (edge, start) = (f.srv.url("/ch.m3u8?live=l4a&livewin=30"), f.srv.url("/ch.m3u8?live=l4b&livewin=30"));
    for u in [&edge, &start] {
        reqwest::get(u.as_str()).await.unwrap().text().await.unwrap();
    }
    tokio::time::sleep(Duration::from_secs(8)).await;
    let me = f.manager(|_| {});
    let a = me.add(DownloadAdd { url: edge, ..Default::default() }).unwrap().job.unwrap();
    let ms = Manager::new(
        {
            let mut c = Config::default();
            c.paths.download_dir = f.tmp.path().join("dl2").to_string_lossy().into_owned();
            c.paths.part_dir = f.tmp.path().join("parts2").to_string_lossy().into_owned();
            c.paths.by_category = false;
            c.engine.fsync = false;
            c.general.notify_on_complete = false;
            c.media.live_from_start = true;
            c
        },
        None,
        Store::open(&f.tmp.path().join("db2/downloads.db")).unwrap(),
    )
    .unwrap();
    ms.start();
    let b = ms.add(DownloadAdd { url: start, ..Default::default() }).unwrap().job.unwrap();
    for (m, id) in [(&me, &a.id), (&ms, &b.id)] {
        wait_state(m, id, "Recording", 20).await;
        let t0 = Instant::now();
        while m.get(id).unwrap()["recordedSecs"].as_f64().unwrap_or(0.0) < 3.0 {
            assert!(t0.elapsed() < Duration::from_secs(20));
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    me.stop_recording(&a.id).unwrap();
    ms.stop_recording(&b.id).unwrap();
    let ja = wait_state(&me, &a.id, "Completed", 30).await;
    let jb = wait_state(&ms, &b.id, "Completed", 30).await;
    let (da, db) = (duration(Path::new(ja["final_path"].as_str().unwrap())), duration(Path::new(jb["final_path"].as_str().unwrap())));
    assert!(da < 7.0, "edge start records only the newest ~3 s plus what arrived since: {da}");
    assert!(db >= 9.0, "live_from_start keeps the DVR window: {db}");
    let _ = std::fs::remove_file(ja["final_path"].as_str().unwrap());
    let _ = std::fs::remove_file(jb["final_path"].as_str().unwrap());
}
