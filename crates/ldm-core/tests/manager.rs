//! Manager (queue/scheduler/lifecycle) integration tests.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ldm_core::manager::Manager;
use ldm_core::store::{QueueRow, Store};
use ldm_core::Config;
use ldm_ipc::messages::{DownloadAdd, MediaCandidate};

fn rand_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(99);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn sha_bytes(d: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(d))
}

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
        let srv = ldm_testsrv::start(root.clone(), "127.0.0.1:0").await.unwrap();
        Fx { tmp, root, srv }
    }
    fn cfg(&self) -> Config {
        let mut c = Config::default();
        c.paths.download_dir = self.tmp.path().join("dl").to_string_lossy().into_owned();
        c.paths.part_dir = self.tmp.path().join("parts").to_string_lossy().into_owned();
        c.paths.by_category = false;
        c.engine.retry_base_ms = 20;
        c.engine.fsync = false;
        c.general.notify_on_complete = false;
        c
    }
    fn db(&self) -> PathBuf {
        self.tmp.path().join("db/downloads.db")
    }
    fn manager(&self, cfg: Config) -> Manager {
        Manager::new(cfg, None, Store::open(&self.db()).unwrap()).unwrap()
    }
}

fn add(url: String) -> DownloadAdd {
    DownloadAdd { url, ..Default::default() }
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
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn file_sha(p: &Path) -> String {
    sha_bytes(&std::fs::read(p).unwrap())
}

#[tokio::test]
async fn q1_global_concurrency_limit_is_respected() {
    let f = Fx::new().await;
    for i in 0..5 {
        std::fs::write(f.root.join(format!("f{i}.bin")), rand_bytes(300_000, i)).unwrap();
    }
    let mut c = f.cfg();
    c.queue.global_max_active = 2;
    let m = f.manager(c);
    m.start();
    let ids: Vec<String> = (0..5).map(|i| m.add(add(f.srv.url(&format!("/f{i}.bin?rate=600000")))).unwrap().job.unwrap().id).collect();
    let mut peak = 0;
    let t0 = Instant::now();
    loop {
        peak = peak.max(m.active_count());
        let done = ids.iter().filter(|id| m.get(id).unwrap()["state"] == "Completed").count();
        if done == 5 {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(30), "timeout");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(peak, 2, "exactly two jobs may run at once");
    for (i, id) in ids.iter().enumerate() {
        let j = m.get(id).unwrap();
        assert_eq!(file_sha(Path::new(j["final_path"].as_str().unwrap())), sha_bytes(&rand_bytes(300_000, i as u64)));
    }
}

#[tokio::test]
async fn q2_per_queue_limit_priority_and_disabled_queue() {
    let f = Fx::new().await;
    for i in 0..3 {
        std::fs::write(f.root.join(format!("p{i}.bin")), rand_bytes(200_000, 10 + i)).unwrap();
    }
    let mut c = f.cfg();
    c.queue.global_max_active = 5;
    let m = f.manager(c);
    m.set_queue(QueueRow {
        id: "solo".into(),
        name: "单任务".into(),
        concurrent: 1,
        schedule: None,
        enabled: true,
        extra: Default::default(),
    })
    .unwrap();
    m.set_queue(QueueRow {
        id: "off".into(),
        name: "停用".into(),
        concurrent: 3,
        schedule: None,
        enabled: false,
        extra: Default::default(),
    })
    .unwrap();
    let mk =
        |i: u32, q: &str| DownloadAdd { url: f.srv.url(&format!("/p{i}.bin?rate=400000")), queue: Some(q.into()), ..Default::default() };
    let a = m.add(mk(0, "solo")).unwrap().job.unwrap().id;
    let b = m.add(mk(1, "solo")).unwrap().job.unwrap().id;
    let off = m.add(mk(2, "off")).unwrap().job.unwrap().id;
    m.set(&b, &serde_json::json!({"priority": 10})).unwrap();
    m.start();
    let mut peak = 0;
    let t0 = Instant::now();
    let mut first_completed = None;
    loop {
        peak = peak.max(m.active_count());
        for id in [&a, &b] {
            if first_completed.is_none() && m.get(id).unwrap()["state"] == "Completed" {
                first_completed = Some(id.clone());
            }
        }
        if [&a, &b].iter().all(|id| m.get(id).unwrap()["state"] == "Completed") {
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(30));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(peak, 1);
    assert_eq!(first_completed.as_deref(), Some(b.as_str()), "higher priority runs first");
    assert_eq!(m.get(&off).unwrap()["state"], "Queued", "disabled queue must not start");
    assert!(m.add(DownloadAdd { url: f.srv.url("/p0.bin"), queue: Some("nope".into()), ..Default::default() }).is_err());
}

#[tokio::test]
async fn q3_pause_resume_and_restart_recovery() {
    let f = Fx::new().await;
    let data = rand_bytes(3_000_000, 7);
    std::fs::write(f.root.join("big.bin"), &data).unwrap();
    let url = f.srv.url("/big.bin?rate=150000");
    let id;
    {
        let m = f.manager(f.cfg());
        m.start();
        id = m.add(add(url.clone())).unwrap().job.unwrap().id;
        // wait until some bytes arrived, then pause
        let t0 = Instant::now();
        while m.get(&id).unwrap()["got"].as_u64().unwrap_or(0) < 300_000 {
            assert!(t0.elapsed() < Duration::from_secs(10));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        m.pause(&id).unwrap();
        let j = wait_state(&m, &id, "Paused", 10).await;
        let got_paused = j["got"].as_u64().unwrap();
        assert!(got_paused > 0 && got_paused < data.len() as u64, "{j}");
        // resume, then simulate a daemon stop mid-way (state returns to Queued)
        m.resume(&id).unwrap();
        wait_state(&m, &id, "Running", 10).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        m.shutdown(Duration::from_secs(5)).await;
        let j = m.get(&id).unwrap();
        assert_eq!(j["state"], "Queued", "{j}");
        assert!(!m.store().load_parts(&id).unwrap().is_empty(), "parts mirrored to SQLite");
    }
    f.srv.reset();
    // "restart": new manager over the same database
    let m = f.manager(f.cfg());
    m.start();
    let j = wait_state(&m, &id, "Completed", 30).await;
    assert_eq!(file_sha(Path::new(j["final_path"].as_str().unwrap())), sha_bytes(&data));
    // after restart only the missing bytes are requested
    let fetched: u64 = f
        .srv
        .ranges_for("/big.bin")
        .iter()
        .filter_map(|r| {
            r.as_deref().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split('-').next()).and_then(|s| s.parse::<u64>().ok())
        })
        .count() as u64;
    assert!(fetched >= 1);
    let first_start = f
        .srv
        .ranges_for("/big.bin")
        .iter()
        .filter_map(|r| {
            r.as_deref().and_then(|r| r.strip_prefix("bytes=")).and_then(|r| r.split('-').next()).and_then(|s| s.parse::<u64>().ok())
        })
        .filter(|s| *s > 0)
        .min();
    assert!(first_start.is_some(), "resumed ranges should not restart from zero");
}

#[tokio::test]
async fn q4_dedupe_segment_skip_remove_and_failures() {
    let f = Fx::new().await;
    std::fs::write(f.root.join("a.bin"), rand_bytes(50_000, 1)).unwrap();
    let mut c = f.cfg();
    c.queue.global_max_active = 1;
    let m = f.manager(c);
    // paused add → no download, dedupe returns the same job
    let r1 = m.add(DownloadAdd { url: f.srv.url("/a.bin"), paused: true, ..Default::default() }).unwrap();
    let r2 = m.add(DownloadAdd { url: f.srv.url("/a.bin"), ..Default::default() }).unwrap();
    assert!(r2.duplicate);
    assert_eq!(r1.job.as_ref().unwrap().id, r2.job.as_ref().unwrap().id);
    // a manifest from a page makes later .ts captures from the same page redundant
    let page = "http://example.test/watch".to_string();
    let h = m
        .add(DownloadAdd {
            url: f.srv.url("/v/index.m3u8"),
            page_url: Some(page.clone()),
            source: Some("extension".into()),
            paused: true,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(h.job.unwrap().kind, "hls");
    let seg = m
        .add(DownloadAdd { url: f.srv.url("/v/seg001.ts"), page_url: Some(page), source: Some("extension".into()), ..Default::default() })
        .unwrap();
    assert!(seg.ignored.is_some() && seg.job.is_none());
    // security: link-local / metadata and non-http schemes rejected
    assert_eq!(m.add(add("http://169.254.169.254/latest".into())).unwrap_err().code(), "E_SECURITY");
    assert_eq!(m.add(add("file:///etc/passwd".into())).unwrap_err().code(), "E_SECURITY");
    // remove
    let id = r1.job.unwrap().id;
    m.remove(&id[..6], false).unwrap();
    assert!(m.get(&id).is_err());
    // failure path → Failed with code; resume re-queues
    m.start();
    let bad = m.add(add(f.srv.url("/missing.bin"))).unwrap().job.unwrap().id;
    let j = wait_state(&m, &bad, "Failed", 10).await;
    assert_eq!(j["error_code"], "E_HTTP_STATUS");
    // export / import
    let csv = m.export("csv").unwrap();
    assert!(csv.lines().count() >= 2);
    let json = m.export("json").unwrap();
    assert!(!json.contains("\"cookies\""));
    let n = m.import(&format!("# list\n{}\n{}\n", f.srv.url("/x1.bin"), f.srv.url("/x2.bin")), true).unwrap();
    assert_eq!(n, 2);
}

#[tokio::test]
async fn q5_media_candidates_and_events() {
    let f = Fx::new().await;
    let m = f.manager(f.cfg());
    let mut rx = m.subscribe();
    let mc = MediaCandidate {
        tab_id: 7,
        url: "https://cdn.example/v/master.m3u8#x".into(),
        kind: String::new(),
        page_url: Some("https://example/watch".into()),
        ..Default::default()
    };
    let p = m.media_candidate(&mc).unwrap().unwrap();
    assert_eq!(p["items"][0]["kind"], "hls");
    let ev = rx.recv().await.unwrap();
    assert_eq!(ev.kind, "ShowMediaButton");
    let c = m.candidates(Some(7)).unwrap();
    assert_eq!(c.len(), 1);
    assert_eq!(c[0].url, "https://cdn.example/v/master.m3u8");
    let rs = m.ruleset();
    assert!(rs.capture_exts.contains(&"mp4".to_string()));
    let v1 = rs.version;
    m.config_set(&serde_json::json!({"capture": {"min_size": 1}})).unwrap();
    assert_eq!(m.ruleset().version, v1 + 1);
    assert_eq!(m.config().capture.min_size, 1);
}

/// Per-job speed limit (progress dialog "速度限制"), live change through JobSet.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q6_per_job_speed_limit_live() {
    let f = Fx::new().await;
    // 2 MiB: fast localhost finishes 600 KiB before the limit can be observed
    std::fs::write(f.root.join("lim.bin"), rand_bytes(2_000_000, 6)).unwrap();
    let m = f.manager(f.cfg());
    m.start();
    let mut r = add(f.srv.url("/lim.bin"));
    r.connections = Some(2);
    r.paused = true; // apply the limit before the job starts (avoids a localhost race)
    let id = m.add(r).unwrap().job.unwrap().id;
    m.set(&id, &serde_json::json!({"speed_limit_bps": 100_000})).unwrap();
    m.resume(&id).unwrap();
    let t0 = Instant::now();
    wait_state(&m, &id, "Running", 30).await;
    let t1 = Instant::now();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    // capped: well below 600 KB after ~1.5 s
    let got = m.get(&id).unwrap()["got"].as_u64().unwrap();
    let lim = 100_000.0 * (t1.elapsed().as_secs_f64() + 1.0) + 100_000.0;
    assert!((got as f64) < lim.min(1_500_000.0), "limit not applied: {got}");
    // lift the cap while running
    m.set(&id, &serde_json::json!({"speed_limit_bps": 0})).unwrap();
    let j = wait_state(&m, &id, "Completed", 30).await;
    assert!(t0.elapsed() < Duration::from_secs(30));
    assert!(j["last_try_at"].as_i64().unwrap() > 0, "last try recorded");
    assert!(j.get("speed_limit_bps").is_none(), "0 is not serialized");
}

/// Browser captures wait for the GUI dialog only while a GUI is attached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q7_capture_confirmation_with_gui() {
    let f = Fx::new().await;
    std::fs::write(f.root.join("c.bin"), rand_bytes(50_000, 7)).unwrap();
    std::fs::write(f.root.join("d.bin"), rand_bytes(50_000, 8)).unwrap();
    let m = f.manager(f.cfg());
    m.start();
    let mut ev = m.subscribe();
    // no GUI: starts immediately
    let r = DownloadAdd { url: f.srv.url("/c.bin"), source: Some("extension".into()), ..Default::default() };
    let a = m.add(r).unwrap().job.unwrap();
    assert!(!a.extra.awaiting_confirm);
    wait_state(&m, &a.id, "Completed", 10).await;
    // GUI attached: paused + CaptureConfirm event
    let guard = m.gui_attach();
    let r = DownloadAdd {
        url: f.srv.url("/d.bin"),
        source: Some("extension".into()),
        description: Some("from page".into()),
        ..Default::default()
    };
    let b = m.add(r).unwrap().job.unwrap();
    assert!(b.extra.awaiting_confirm);
    assert_eq!(b.state, "Paused");
    let t0 = Instant::now();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), ev.recv()).await.unwrap().unwrap();
        if e.kind == "CaptureConfirm" {
            assert_eq!(e.payload["id"], b.id.as_str());
            assert_eq!(e.payload["description"], "from page");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
    }
    // resume_all must not start unconfirmed captures
    m.resume_all().unwrap();
    assert_eq!(m.get(&b.id).unwrap()["state"], "Paused");
    // "开始下载" from the dialog: rename + resume
    m.set(&b.id, &serde_json::json!({"filename": "renamed.bin"})).unwrap();
    m.resume(&b.id).unwrap();
    let j = wait_state(&m, &b.id, "Completed", 10).await;
    assert!(j["final_path"].as_str().unwrap().ends_with("renamed.bin"));
    assert!(j.get("awaiting_confirm").is_none());
    drop(guard);
    assert_eq!(m.gui_count(), 0);
}

/// Queue start/stop/delete, "删除已完成", queue post action, mirrors, md5, site login, category dirs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q8_queue_ops_mirrors_checksums_logins() {
    let f = Fx::new().await;
    let data = rand_bytes(80_000, 9);
    std::fs::write(f.root.join("m.zip"), &data).unwrap();
    let mut cfg = f.cfg();
    cfg.sites.logins.push(ldm_core::config::SiteLogin { host: "127.0.0.*".into(), user: "u".into(), password: "p w".into() });
    let catdir = f.tmp.path().join("zips");
    cfg.paths
        .categories
        .insert("compressed".into(), ldm_core::config::CategoryOverride { dir: catdir.to_string_lossy().into(), exts: vec![] });
    cfg.paths.categories.insert("documents".into(), ldm_core::config::CategoryOverride { dir: String::new(), exts: vec!["log".into()] });
    let m = f.manager(cfg);
    m.start();
    let marker = f.tmp.path().join("queue-done");
    let night = QueueRow {
        id: "night".into(),
        name: "夜间".into(),
        concurrent: 1,
        schedule: None,
        enabled: false,
        extra: ldm_core::store::QueueExtra {
            on_done: "command".into(),
            on_done_command: vec!["touch".into(), marker.to_string_lossy().into()],
            manual: false,
        },
    };
    m.set_queue(night).unwrap();
    // primary URL fails (404), mirror needs the saved login; md5 checked
    use md5::{Digest, Md5};
    let md5 = hex::encode(Md5::digest(&data));
    let r = DownloadAdd {
        url: f.srv.url("/missing.zip"),
        mirrors: vec![f.srv.url("/m.zip?needauth=u:p%20w")],
        md5: Some(md5),
        queue: Some("night".into()),
        ..Default::default()
    };
    let id = m.add(r).unwrap().job.unwrap().id;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(m.get(&id).unwrap()["state"], "Queued", "disabled queue holds the job");
    m.queue_start("night").unwrap();
    let j = wait_state(&m, &id, "Completed", 15).await;
    let p = PathBuf::from(j["final_path"].as_str().unwrap());
    assert_eq!(p.parent().unwrap(), catdir, "category override dir");
    assert_eq!(std::fs::read(&p).unwrap(), data);
    let t0 = Instant::now();
    while !marker.exists() {
        assert!(t0.elapsed() < Duration::from_secs(5), "queue post-action did not run");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // bad md5 fails with E_CHECKSUM
    let r = DownloadAdd { url: f.srv.url("/m.zip?needauth=u:p%20w"), md5: Some("00".repeat(16)), ..Default::default() };
    let bad = m.add(r).unwrap().job.unwrap().id;
    let j = wait_state(&m, &bad, "Failed", 15).await;
    assert_eq!(j["error_code"], "E_CHECKSUM");
    // category ext override
    std::fs::write(f.root.join("x.log"), b"hello log").unwrap();
    let lg = m.add(add(f.srv.url("/x.log"))).unwrap().job.unwrap();
    assert_eq!(lg.category.as_deref(), Some("documents"));
    wait_state(&m, &lg.id, "Completed", 10).await;
    // queue stop/delete
    m.queue_stop("night").unwrap();
    assert!(!m.queues().unwrap().iter().find(|q| q.id == "night").unwrap().enabled);
    assert!(m.queue_delete("main").is_err());
    assert_eq!(m.queue_delete("night").unwrap(), 1);
    assert_eq!(m.get(&id).unwrap()["queue_id"], "main");
    assert_eq!(m.remove_completed().unwrap(), 2);
    assert!(m.get(&id).is_err());
}

/// 403 on a browser capture → GetCookies to the extension → CookiesQueryResult → retried once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q9_expired_cookies_are_refreshed_from_the_browser() {
    let f = Fx::new().await;
    std::fs::write(f.root.join("s.bin"), rand_bytes(40_000, 10)).unwrap();
    let m = f.manager(f.cfg());
    m.start();
    let mut ev = m.subscribe();
    let r = DownloadAdd {
        url: f.srv.url("/s.bin?needcookie=sid=new"),
        cookies: Some("sid=old".into()),
        source: Some("extension".into()),
        page_url: Some("http://page/".into()),
        ..Default::default()
    };
    let id = m.add(r).unwrap().job.unwrap().id;
    let j = wait_state(&m, &id, "Failed", 15).await;
    assert!(j["error"].as_str().unwrap().contains("Cookie"), "{j}");
    let t0 = Instant::now();
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), ev.recv()).await.unwrap().unwrap();
        if e.kind == "GetCookies" {
            assert_eq!(e.payload["jobId"], id.as_str());
            assert_eq!(e.payload["pageUrl"], "http://page/");
            break;
        }
        assert!(t0.elapsed() < Duration::from_secs(5));
    }
    assert!(!m.cookies_refreshed(&id, Some("sid=old")).unwrap(), "same cookies: no retry");
    assert!(m.cookies_refreshed(&id, Some("sid=new")).unwrap());
    wait_state(&m, &id, "Completed", 15).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q10_media_candidates_are_named_after_the_page_title() {
    let f = Fx::new().await;
    std::fs::create_dir_all(f.root.join("v")).unwrap();
    let data = rand_bytes(40_000, 7);
    std::fs::write(f.root.join("v/1080P_4000K_99.mp4"), &data).unwrap();
    let m = f.manager(f.cfg());
    m.start();
    let page = "https://www.example.com/watch?v=1".to_string();
    let r = m
        .add(DownloadAdd {
            url: f.srv.url("/v/1080P_4000K_99.mp4"),
            page_url: Some(page.clone()),
            title_hint: Some("(2) 周末 Vlog: day 1/2 - Example.com".into()),
            source: Some("extension".into()),
            ..Default::default()
        })
        .unwrap();
    let id = r.job.unwrap().id;
    let j = wait_state(&m, &id, "Completed", 10).await;
    let p = PathBuf::from(j["final_path"].as_str().unwrap());
    assert_eq!(p.file_name().unwrap().to_str().unwrap(), "周末 Vlog: day 1_2.mp4");
    assert_eq!(file_sha(&p), sha_bytes(&data));
    // without a title the URL name is kept
    let r = m.add(DownloadAdd { url: f.srv.url("/v/1080P_4000K_99.mp4?x=2"), ..Default::default() }).unwrap();
    let j = wait_state(&m, &r.job.unwrap().id, "Completed", 10).await;
    assert!(j["final_path"].as_str().unwrap().contains("1080P_4000K_99"), "{j}");
    // manifests: the cleaned title is the job's name (muxed later to .mp4)
    let h = m
        .add(DownloadAdd {
            url: f.srv.url("/hls/index.m3u8"),
            page_url: Some(page),
            title_hint: Some("Concert Live | Example".into()),
            paused: true,
            ..Default::default()
        })
        .unwrap()
        .job
        .unwrap();
    assert_eq!(h.kind, "hls");
    assert_eq!(h.filename.as_deref(), Some("Concert Live"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn q11_candidates_ranked_main_first_ads_last() {
    let f = Fx::new().await;
    let list = f.tmp.path().join("ads.txt");
    std::fs::write(&list, "# my list\n0.0.0.0 media.myads.example\n").unwrap();
    let mut c = f.cfg();
    c.media.ad_domains_file = list.to_string_lossy().into_owned();
    let m = f.manager(c);
    let page = Some("https://www.example.com/watch?v=1".to_string());
    let cand = |url: &str, kind: &str| MediaCandidate {
        tab_id: 5,
        url: url.into(),
        kind: kind.into(),
        page_url: page.clone(),
        ..Default::default()
    };
    // static fallback (no extension verdict): ad domain, user list, format
    m.media_candidate(&cand("https://s0.2mdn.net/v/x.mp4", "progressive")).unwrap();
    m.media_candidate(&cand("https://cdn.example.com/hls/master.m3u8", "hls")).unwrap();
    m.media_candidate(&cand("https://media.myads.example/p.mp4", "progressive")).unwrap();
    // the extension's verdict (all signals) is taken as is
    let mut pre = cand("https://adcdn.example.net/pre_videos/1/mp4_720.mp4", "progressive");
    pre.ad_score = Some(5.5);
    pre.ad_suspect = true;
    pre.ad_reasons = vec!["skip-control".into(), "short:20s".into()];
    let shown = m.media_candidate(&pre).unwrap().unwrap();
    // page-button items use the extension's flat shape: a boolean `ad` (a nested verdict object would be truthy)
    assert_eq!(shown["items"][0]["ad"], serde_json::json!(true));
    assert_eq!(shown["items"][0]["adScore"], serde_json::json!(5.5));
    assert_eq!(shown["items"][0]["adReasons"][0], "skip-control");
    // a plain page with one video
    let plain = m
        .media_candidate(&MediaCandidate {
            tab_id: 6,
            url: "https://www.example.org/media/download.mp4".into(),
            kind: "progressive".into(),
            page_url: Some("https://www.example.org/".into()),
            ..Default::default()
        })
        .unwrap()
        .unwrap();
    assert_eq!(plain["items"][0]["ad"], serde_json::json!(false), "{plain}");

    let rows = m.candidates(Some(5)).unwrap();
    let names: Vec<(&str, bool)> = rows.iter().map(|r| (r.url.rsplit('/').next().unwrap(), r.ad.as_ref().unwrap().ad)).collect();
    assert_eq!(names[0], ("master.m3u8", false), "{names:?}");
    assert!(names[1..].iter().all(|(_, ad)| *ad), "{names:?}");
    let by = |u: &str| rows.iter().find(|r| r.url.contains(u)).unwrap().ad.clone().unwrap();
    assert!(by("2mdn").reasons.contains(&"ad-domain:2mdn.net".to_string()));
    assert_eq!(by("myads").reasons[0], "user-ad-domain:media.myads.example");
    assert_eq!(by("pre_videos").reasons, ["skip-control", "short:20s"]);
    let plain = &m.candidates(Some(6)).unwrap()[0];
    assert!(!plain.ad.as_ref().unwrap().ad);
    // pushed to the extension with the rule set
    let rs = m.ruleset();
    assert!(rs.ad_ranking);
    assert_eq!(rs.ad_domains, ["media.myads.example"]);
    // downloading a suspected ad anyway: the reasons travel with the job ("下载文件信息")
    let j = m
        .add(DownloadAdd { url: f.srv.url("/x.mp4"), ad_reasons: Some(vec!["skip-control".into()]), paused: true, ..Default::default() })
        .unwrap()
        .job
        .unwrap();
    assert_eq!(m.get(&j.id).unwrap()["ad_reasons"], serde_json::json!(["skip-control"]));
    // switched off: no verdicts, original order
    m.config_set(&serde_json::json!({"media": {"ad_ranking": false}})).unwrap();
    assert!(!m.ruleset().ad_ranking);
    m.media_candidate(&cand("https://s0.2mdn.net/v/y.mp4", "progressive")).unwrap();
    let y = m.candidates(Some(5)).unwrap().into_iter().find(|r| r.url.ends_with("y.mp4")).unwrap();
    assert!(y.ad.is_none(), "no verdict while ranking is off");
}

/// A video file with a separate audio file (DownloadAdd.audio_url) becomes an `av` job.
#[tokio::test]
async fn q12_video_with_separate_audio_is_an_av_job() {
    let f = Fx::new().await;
    let m = f.manager(f.cfg());
    let r = m
        .add(DownloadAdd {
            url: f.srv.url("/v-30016.m4s"),
            audio_url: Some(f.srv.url("/v-30216.m4s")),
            page_url: Some("https://www.example.com/video/1".into()),
            source: Some("extension".into()),
            paused: true,
            ..Default::default()
        })
        .unwrap();
    let j = r.job.expect("job created, not ignored as a segment");
    assert_eq!(j.kind, "av");
    assert_eq!(j.extra.audio_url.as_deref(), Some(f.srv.url("/v-30216.m4s").as_str()));
    // an unsafe audio URL is refused
    assert!(m.add(DownloadAdd { url: f.srv.url("/v.m4s"), audio_url: Some("file:///etc/passwd".into()), ..Default::default() }).is_err());
}
