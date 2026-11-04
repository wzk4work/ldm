//! FTP download against a tiny in-test FTP server (passive mode, SIZE, REST).

use std::sync::{Arc, Mutex};

use ldm_core::config::Config;
use ldm_core::engine::{HttpJobSpec, JobEnv, OutputSpec};
use ldm_core::ftp::run_ftp_job;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

#[derive(Default)]
struct Log {
    cmds: Vec<String>,
}

/// Serves `data` as /pub/file.bin; `epsv` toggles EPSV support; `cut` closes the data
/// connection after that many bytes on the first RETR (to test resume).
async fn server(data: Arc<Vec<u8>>, epsv: bool, cut: Option<usize>, log: Arc<Mutex<Log>>) -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut retr_count = 0usize;
        loop {
            let (s, _) = l.accept().await.unwrap();
            let (r, mut w) = s.into_split();
            let mut r = BufReader::new(r);
            w.write_all(b"220-LDM test\r\n220 ready\r\n").await.unwrap();
            let mut rest = 0usize;
            let mut pasv: Option<TcpListener> = None;
            let mut line = String::new();
            loop {
                line.clear();
                if r.read_line(&mut line).await.unwrap_or(0) == 0 {
                    break;
                }
                let cmd = line.trim_end().to_string();
                log.lock().unwrap().cmds.push(cmd.clone());
                let (verb, arg) = cmd.split_once(' ').unwrap_or((cmd.as_str(), ""));
                let reply = match verb {
                    "USER" => "331 password please".to_string(),
                    "PASS" if arg == "pw" || arg == "ldm@" => "230 ok".into(),
                    "PASS" => "530 bad login".into(),
                    "TYPE" => "200 binary".into(),
                    "SIZE" if arg == "/pub/file.bin" => format!("213 {}", data.len()),
                    "SIZE" => "550 no such file".into(),
                    "EPSV" if epsv => {
                        let pl = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let p = pl.local_addr().unwrap().port();
                        pasv = Some(pl);
                        format!("229 Entering Extended Passive Mode (|||{p}|)")
                    }
                    "EPSV" => "500 not understood".into(),
                    "PASV" => {
                        let pl = TcpListener::bind("127.0.0.1:0").await.unwrap();
                        let p = pl.local_addr().unwrap().port();
                        pasv = Some(pl);
                        format!("227 Entering Passive Mode (10,0,0,1,{},{})", p / 256, p % 256)
                    }
                    "REST" => {
                        rest = arg.parse().unwrap();
                        format!("350 restarting at {rest}")
                    }
                    "RETR" if arg == "/pub/file.bin" => {
                        w.write_all(b"150 opening\r\n").await.unwrap();
                        let (mut d, _) = pasv.take().unwrap().accept().await.unwrap();
                        retr_count += 1;
                        let end = match cut {
                            Some(c) if retr_count == 1 => c,
                            _ => data.len(),
                        };
                        d.write_all(&data[rest..end]).await.unwrap();
                        drop(d);
                        rest = 0;
                        if end < data.len() {
                            "426 connection closed; transfer aborted".into()
                        } else {
                            "226 done".into()
                        }
                    }
                    "QUIT" => {
                        let _ = w.write_all(b"221 bye\r\n").await;
                        break;
                    }
                    _ => "502 not implemented".into(),
                };
                w.write_all(format!("{reply}\r\n").as_bytes()).await.unwrap();
            }
        }
    });
    port
}

fn rand(n: usize) -> Vec<u8> {
    let mut x = 0x1234_5678u64;
    (0..n)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

fn spec(url: String, dir: &std::path::Path) -> HttpJobSpec {
    HttpJobSpec {
        url,
        ctx: Default::default(),
        max_conns: 4,
        parts_dir: dir.join("work"),
        output: OutputSpec { dir: Some(dir.join("out")), ..Default::default() },
        filename_hint: None,
        sha256: None,
        title_stem: None,
    }
}

#[tokio::test]
async fn f1_ftp_epsv_with_login() {
    let data = Arc::new(rand(300_000));
    let log = Arc::new(Mutex::new(Log::default()));
    let port = server(data.clone(), true, None, log.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let env = JobEnv::standalone(Config::default()).unwrap();
    let out = run_ftp_job(&env, &spec(format!("ftp://bob:pw@127.0.0.1:{port}/pub/file.bin"), tmp.path())).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), *data);
    assert!(out.path.ends_with("out/file.bin"));
    let cmds = log.lock().unwrap().cmds.clone();
    assert!(cmds.contains(&"USER bob".to_string()) && cmds.contains(&"TYPE I".to_string()), "{cmds:?}");
    // wrong password → login error mapped to 401 (shows the site-login hint)
    let env = JobEnv::standalone(Config::default()).unwrap();
    let e = run_ftp_job(&env, &spec(format!("ftp://bob:nope@127.0.0.1:{port}/pub/file.bin"), tmp.path())).await.unwrap_err();
    assert!(e.to_string().contains("login"), "{e}");
}

#[tokio::test]
async fn f2_ftp_pasv_resume_after_cut() {
    let data = Arc::new(rand(200_000));
    let log = Arc::new(Mutex::new(Log::default()));
    let port = server(data.clone(), false, Some(70_000), log.clone()).await;
    let tmp = tempfile::tempdir().unwrap();
    let url = format!("ftp://127.0.0.1:{port}/pub/file.bin");
    let env = JobEnv::standalone(Config::default()).unwrap();
    let e = run_ftp_job(&env, &spec(url.clone(), tmp.path())).await.unwrap_err();
    assert!(e.retryable(), "a cut transfer is retryable: {e}");
    assert_eq!(std::fs::metadata(tmp.path().join("work/ftp.part")).unwrap().len(), 70_000);
    let env = JobEnv::standalone(Config::default()).unwrap();
    let out = run_ftp_job(&env, &spec(url, tmp.path())).await.unwrap();
    assert_eq!(std::fs::read(&out.path).unwrap(), *data);
    let cmds = log.lock().unwrap().cmds.clone();
    assert!(cmds.contains(&"REST 70000".to_string()), "{cmds:?}");
    assert!(cmds.contains(&"USER anonymous".to_string()));
    assert!(cmds.iter().filter(|c| *c == "PASV").count() >= 2, "PASV fallback used: {cmds:?}");
}
