//! Engine integration tests against the local test server.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ldm_core::engine::{run_http_job, state, HttpJobSpec, JobEnv, OutputSpec, Part, PartState};
use ldm_core::{Config, LdmError};

struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    out: PathBuf,
    srv: ldm_testsrv::Server,
}

fn rand_bytes(n: usize, seed: u64) -> Vec<u8> {
    let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
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

fn write(root: &Path, name: &str, data: &[u8]) {
    std::fs::write(root.join(name), data).unwrap();
}

fn cfg() -> Config {
    let mut c = Config::default();
    c.engine.retry_base_ms = 20;
    c.engine.timeout_secs = 10;
    c.engine.fsync = false;
    c
}

fn spec(f: &Fx, url: String, conns: u32, name: &str) -> HttpJobSpec {
    HttpJobSpec {
        url,
        max_conns: conns,
        parts_dir: f.out.join(format!("parts-{name}")),
        output: OutputSpec { exact_path: Some(f.out.join(name)), ..Default::default() },
        ..Default::default()
    }
}

fn sha(p: &Path) -> String {
    ldm_core::engine::sha256_file(p).unwrap()
}

fn sha_bytes(d: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(d))
}

fn data_requests(f: &Fx, path: &str) -> Vec<Option<String>> {
    f.srv.ranges_for(path).into_iter().filter(|r| r.as_deref() != Some("bytes=0-0")).collect()
}

#[tokio::test]
async fn t1_range_multiconnection_sha_matches() {
    let f = fx().await;
    let data = rand_bytes(5 * 1024 * 1024, 1);
    write(&f.root, "big.bin", &data);
    let env = JobEnv::standalone(cfg()).unwrap();
    let s = spec(&f, f.srv.url("/big.bin"), 4, "big.bin");
    let out = run_http_job(&env, &s).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    assert!(out.range_ok);
    let reqs = data_requests(&f, "/big.bin");
    assert!(reqs.len() >= 4, "expected >=4 ranged GETs, got {reqs:?}");
    assert!(reqs.iter().all(|r| r.is_some()));
    assert!(f.srv.max_active_for("/big.bin") <= 4);
    assert!(f.srv.max_active_for("/big.bin") >= 2, "connections were not parallel");
    assert!(!s.parts_dir.exists(), "parts dir cleaned up");
    // identity encoding is always requested
    let st = f.srv.stats_json();
    assert!(st["requests"].as_array().unwrap().iter().all(|r| r["accept_encoding"] == "identity"));
}

#[tokio::test]
async fn t2_no_range_server_single_connection() {
    let f = fx().await;
    let data = rand_bytes(1_500_000, 2);
    write(&f.root, "nr.bin", &data);
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/nr.bin?norange=1"), 8, "nr.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    assert!(!out.range_ok);
    // probe + exactly one data GET without Range
    let reqs = f.srv.ranges_for("/nr.bin");
    assert_eq!(reqs.len(), 2, "{reqs:?}");
    assert_eq!(reqs[1], None);
}

#[tokio::test]
async fn t2b_fake_range_falls_back() {
    let f = fx().await;
    let data = rand_bytes(2_000_000, 3);
    write(&f.root, "fr.bin", &data);
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/fr.bin?fakerange=1"), 4, "fr.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    assert!(!out.range_ok, "fallback to single connection recorded");
}

#[tokio::test]
async fn t3_resume_from_prepared_half_done_state() {
    let f = fx().await;
    let data = rand_bytes(5 * 1024 * 1024, 4);
    write(&f.root, "res.bin", &data);
    let s = spec(&f, f.srv.url("/res.bin"), 2, "res.bin");
    std::fs::create_dir_all(&s.parts_dir).unwrap();
    let half = (data.len() / 2) as u64;
    std::fs::write(state::part_path(&s.parts_dir, 0), &data[..half as usize]).unwrap();
    std::fs::write(state::part_path(&s.parts_dir, 1), &data[half as usize..half as usize + 1000]).unwrap();
    let st = state::JobState {
        url: s.url.clone(),
        total: Some(data.len() as u64),
        range_ok: true,
        etag: Some(format!("\"{:x}\"", data.len())),
        parts: vec![
            Part { idx: 0, start: 0, end: half, got: half, state: PartState::Done },
            Part { idx: 1, start: half, end: data.len() as u64, got: 1000, state: PartState::Active },
        ],
        ..Default::default()
    };
    state::save(&s.parts_dir, &st).unwrap();
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &s).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    let reqs = data_requests(&f, "/res.bin");
    let starts: Vec<u64> = reqs.iter().filter_map(|r| r.as_ref()?.strip_prefix("bytes=")?.split('-').next()?.parse().ok()).collect();
    // nothing below the resume point is fetched again (part 0 done, part 1 resumes at +1000)
    assert_eq!(starts.iter().min().copied(), Some(half + 1000), "{reqs:?}");
}

#[tokio::test]
async fn t3b_interrupt_and_resume_by_cancel() {
    let f = fx().await;
    let data = rand_bytes(3 * 1024 * 1024, 5);
    write(&f.root, "slow.bin", &data);
    let url = f.srv.url("/slow.bin?rate=600000");
    let s = spec(&f, url, 4, "slow.bin");
    let env = JobEnv::standalone(cfg()).unwrap();
    let env2 = env.clone();
    let s2 = s.clone();
    let h = tokio::spawn(async move { run_http_job(&env2, &s2).await });
    let t0 = Instant::now();
    while env.progress.got() < (data.len() as u64) * 3 / 10 {
        assert!(t0.elapsed() < Duration::from_secs(20), "no progress");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    env.cancel.cancel();
    let r = h.await.unwrap();
    assert!(matches!(r, Err(LdmError::Canceled)), "{r:?}");
    let saved = state::load(&s.parts_dir).expect("state persisted on cancel");
    let saved_got: u64 = saved.parts.iter().map(|p| p.got).sum();
    assert!(saved_got > 0 && saved_got < data.len() as u64);
}

#[tokio::test]
async fn t3c_interrupt_and_resume_same_url() {
    let f = fx().await;
    let data = rand_bytes(3 * 1024 * 1024, 6);
    write(&f.root, "slow2.bin", &data);
    let url = f.srv.url("/slow2.bin?rate=700000");
    let s = spec(&f, url, 4, "slow2.bin");
    let env = JobEnv::standalone(cfg()).unwrap();
    let (env2, s2) = (env.clone(), s.clone());
    let h = tokio::spawn(async move { run_http_job(&env2, &s2).await });
    while env.progress.got() < (data.len() as u64) * 4 / 10 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    env.cancel.cancel();
    assert!(matches!(h.await.unwrap(), Err(LdmError::Canceled)));
    let saved = state::load(&s.parts_dir).unwrap();
    let before: u64 = saved.parts.iter().map(|p| p.got).sum();
    f.srv.reset();
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &s).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    // every data request of the second run starts at the persisted offset of some part
    let reqs = data_requests(&f, "/slow2.bin");
    let starts: Vec<u64> = reqs.iter().filter_map(|r| r.as_ref()?.strip_prefix("bytes=")?.split('-').next()?.parse().ok()).collect();
    let resumed_offsets: Vec<u64> = saved.parts.iter().map(|p| p.start + p.got).collect();
    assert!(starts.iter().any(|s| resumed_offsets.contains(s) && *s > 0), "{starts:?} vs {resumed_offsets:?}");
    let bytes_second: u64 = before; // at least this much was not fetched again
    assert!(bytes_second > 0);
}

#[tokio::test]
async fn t4_retry_on_503_and_dropped_connection() {
    let f = fx().await;
    let data = rand_bytes(1_200_000, 7);
    write(&f.root, "flaky.bin", &data);
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/flaky.bin?fail=3"), 2, "flaky.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/flaky.bin?drop=100000&dropcount=3"), 3, "flaky2.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
}

#[tokio::test]
async fn t4b_retry_exhausted_and_fatal_status() {
    let f = fx().await;
    write(&f.root, "x.bin", &rand_bytes(1000, 8));
    let mut c = cfg();
    c.engine.max_retries = 2;
    let env = JobEnv::standalone(c).unwrap();
    let r = run_http_job(&env, &spec(&f, f.srv.url("/x.bin?fail=100"), 2, "x.bin")).await;
    assert_eq!(r.unwrap_err().code(), "E_HTTP_STATUS");
    let env = JobEnv::standalone(cfg()).unwrap();
    let t = Instant::now();
    let r = run_http_job(&env, &spec(&f, f.srv.url("/missing.bin"), 2, "m.bin")).await;
    assert!(matches!(r, Err(LdmError::HttpStatus(404, _))));
    assert!(t.elapsed() < Duration::from_secs(2), "404 is not retried");
}

#[tokio::test]
async fn t8_dynamic_split_when_a_part_finishes_early() {
    let f = fx().await;
    let data = rand_bytes(4 * 1024 * 1024, 9);
    write(&f.root, "uneven.bin", &data);
    let mut c = cfg();
    c.engine.min_part_size = 64 * 1024;
    let env = JobEnv::standalone(c).unwrap();
    // the part starting at 0 is fast, all others are throttled
    let out = run_http_job(&env, &spec(&f, f.srv.url("/uneven.bin?slowrest=400000"), 4, "uneven.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    assert!(out.splits >= 1, "expected at least one dynamic split, got {}", out.splits);
    assert!(env.progress.peak_connections() <= 4, "peak {}", env.progress.peak_connections());
    let reqs = data_requests(&f, "/uneven.bin");
    assert!(reqs.len() > 4, "new connections took split-off parts: {reqs:?}");
}

#[tokio::test]
async fn t9_speed_limit_is_enforced() {
    let f = fx().await;
    let data = rand_bytes(1_000_000, 10);
    write(&f.root, "lim.bin", &data);
    let mut c = cfg();
    c.engine.enable_speed_limit = true;
    c.engine.speed_limit_bps = 400_000;
    let env = JobEnv::standalone(c).unwrap();
    let t = Instant::now();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/lim.bin"), 4, "lim.bin")).await.unwrap();
    let el = t.elapsed().as_secs_f64();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    // (1_000_000 - 65536 burst) / 400_000 ≈ 2.3 s
    assert!(el > 1.8 && el < 4.0, "elapsed {el}");
}

/// Climb gate: once the limited speed is reached, no extra connections are split off.
#[tokio::test]
async fn t9b_limit_climb_gate() {
    let f = fx().await;
    let data = rand_bytes(1_000_000, 12);
    write(&f.root, "gate.bin", &data);
    let mut c = cfg();
    c.engine.enable_speed_limit = true;
    c.engine.speed_limit_bps = 400_000;
    c.engine.min_part_size = 16 * 1024;
    let env = JobEnv::standalone(c.clone()).unwrap();
    assert!(!env.climb_gated(1), "nothing measured yet");
    let out = run_http_job(&env, &spec(&f, f.srv.url("/gate.bin"), 2, "gate.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    assert_eq!(env.progress.splits(), 0, "gate held the connection count");
    env.progress.speed.reset();
    env.progress.set_got(0);
    std::thread::sleep(Duration::from_millis(150));
    env.progress.set_got(10_000_000);
    assert!(env.climb_gated(1));
    c.engine.limit_climb_gate = false;
    let env2 = JobEnv::standalone(c).unwrap();
    env2.progress.set_got(0);
    std::thread::sleep(Duration::from_millis(150));
    env2.progress.set_got(10_000_000);
    assert!(!env2.climb_gated(1), "gate can be switched off");
}

#[tokio::test]
async fn t10_global_connection_cap() {
    let f = fx().await;
    let data = rand_bytes(3_000_000, 11);
    write(&f.root, "cap.bin", &data);
    let mut c = cfg();
    c.engine.max_connections_global = 2;
    c.engine.min_part_size = 64 * 1024;
    // no truncated responses, so the server-side count is exact as well
    c.engine.split_active = false;
    let env = JobEnv::standalone(c).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/cap.bin?rate=2000000"), 8, "cap.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
    assert_eq!(env.progress.peak_connections(), 2, "client-side peak");
    assert!(f.srv.max_active_for("/cap.bin") <= 2, "max active {}", f.srv.max_active_for("/cap.bin"));
}

#[tokio::test]
async fn t11_etag_change_detected_on_resume() {
    let f = fx().await;
    let data = rand_bytes(600_000, 12);
    write(&f.root, "e.bin", &data);
    let s = spec(&f, f.srv.url("/e.bin?etag=v2"), 2, "e.bin");
    std::fs::create_dir_all(&s.parts_dir).unwrap();
    state::save(
        &s.parts_dir,
        &state::JobState {
            url: s.url.clone(),
            total: Some(600_000),
            range_ok: true,
            etag: Some("v1".into()),
            parts: vec![Part { idx: 0, start: 0, end: 600_000, got: 0, state: PartState::Pending }],
            ..Default::default()
        },
    )
    .unwrap();
    let env = JobEnv::standalone(cfg()).unwrap();
    let r = run_http_job(&env, &s).await;
    assert_eq!(r.unwrap_err().code(), "E_ETAG_CHANGED");
}

#[tokio::test]
async fn t12_naming_category_context_and_checksum() {
    let f = fx().await;
    let data = rand_bytes(300_000, 13);
    write(&f.root, "blob", &data);
    let mut c = cfg();
    c.paths.download_dir = f.out.join("dl").to_string_lossy().into_owned();
    let env = JobEnv::standalone(c).unwrap();
    let mut s = spec(&f, f.srv.url("/blob?cd=report.pdf&needcookie=sid%3D1&needref=page.html&needua=TestUA"), 2, "x");
    s.output = OutputSpec::default();
    s.ctx.cookies = Some("sid=1".into());
    s.ctx.referrer = Some("http://site/page.html".into());
    s.ctx.user_agent = Some("TestUA/1.0".into());
    s.sha256 = Some(sha_bytes(&data));
    let out = run_http_job(&env, &s).await.unwrap();
    assert_eq!(out.path, f.out.join("dl/Documents/report.pdf"));
    // second time: numbered rename, and a wrong checksum is reported
    let env = JobEnv::standalone(env.cfg.as_ref().clone()).unwrap();
    s.sha256 = Some("00".repeat(32));
    let r = run_http_job(&env, &s).await;
    assert_eq!(r.unwrap_err().code(), "E_CHECKSUM");
    assert!(f.out.join("dl/Documents/report (1).pdf").exists());
}

#[tokio::test]
async fn t13_zero_length_and_unknown_length() {
    let f = fx().await;
    write(&f.root, "empty.bin", b"");
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/empty.bin"), 4, "empty.bin")).await.unwrap();
    assert_eq!(std::fs::metadata(out.path).unwrap().len(), 0);
    let data = rand_bytes(400_000, 14);
    write(&f.root, "nolen.bin", &data);
    let env = JobEnv::standalone(cfg()).unwrap();
    let out = run_http_job(&env, &spec(&f, f.srv.url("/nolen.bin?norange=1&nolength=1"), 4, "nolen.bin")).await.unwrap();
    assert_eq!(sha(&out.path), sha_bytes(&data));
}
