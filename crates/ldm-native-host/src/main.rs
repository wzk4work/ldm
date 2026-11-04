//! ldm-native-host – Chrome/Firefox Native Messaging host.
//!
//! stdin/stdout carry u32-LE-length-prefixed JSON (the browser side); the
//! daemon side is the NDJSON Unix socket of `ldmd`. Envelopes are relayed
//! unchanged (browser-originated ones are tagged `_client = "extension"`).
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ldm_ipc::framing::{read_native, write_native};
use ldm_ipc::Envelope;
use serde_json::Value;

fn socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os("LDM_SOCKET").filter(|v| !v.is_empty()) {
        return PathBuf::from(p);
    }
    if let Some(r) = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(r).join("ldm.sock");
    }
    let uid = std::fs::metadata("/proc/self").map(|m| m.uid()).unwrap_or(0);
    PathBuf::from(format!("/tmp/ldm-{uid}/ldm.sock"))
}

fn log(msg: &str) {
    // stderr ends up in the browser's log; never write to stdout here.
    if std::env::var_os("LDM_HOST_QUIET").is_none() {
        eprintln!("ldm-native-host: {msg}");
    }
}

/// Origin policy: Chrome passes `chrome-extension://<id>/`, Firefox passes the
/// manifest path and the add-on id. Web origins are never accepted.
fn check_origin(args: &[String]) -> Result<()> {
    let origin = args.iter().skip(1).find(|a| a.contains("://")).cloned();
    if let Some(o) = &origin {
        if o.starts_with("http://") || o.starts_with("https://") {
            bail!("refusing web origin {o}");
        }
        if let Ok(allow) = std::env::var("LDM_ALLOWED_ORIGINS") {
            if !allow.split(',').any(|a| a.trim() == o.trim_end_matches('/') || a.trim() == o) {
                bail!("origin {o} not in LDM_ALLOWED_ORIGINS");
            }
        }
    }
    Ok(())
}

fn find_ldmd() -> PathBuf {
    if let Ok(p) = std::env::var("LDM_LDMD") {
        return PathBuf::from(p);
    }
    if let Ok(me) = std::env::current_exe() {
        let sib = me.with_file_name("ldmd");
        if sib.exists() {
            return sib;
        }
    }
    PathBuf::from("ldmd")
}

/// Connect to ldmd, starting it if needed.
fn ensure_daemon() -> Result<UnixStream> {
    let path = socket_path();
    if let Ok(s) = UnixStream::connect(&path) {
        return Ok(s);
    }
    if std::env::var_os("LDM_NO_AUTOSTART").is_none() {
        let ldmd = find_ldmd();
        log(&format!("starting {}", ldmd.display()));
        let mut cmd = Command::new(&ldmd);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        use std::os::unix::process::CommandExt;
        // detach into its own session so the browser closing the port does not kill it
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
        cmd.spawn().with_context(|| format!("spawn {}", ldmd.display()))?;
    }
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(8) {
        if let Ok(s) = UnixStream::connect(&path) {
            return Ok(s);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("cannot connect to ldmd at {}", path.display())
}

extern "C" {
    fn setsid() -> i32;
}
fn libc_setsid() {
    // SAFETY: setsid has no memory-safety preconditions.
    unsafe {
        setsid();
    }
}

type Out = Arc<Mutex<io::Stdout>>;

fn send_browser(out: &Out, v: &Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(v)?;
    let mut o = out.lock().unwrap();
    write_native(&mut *o, &bytes)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version") {
        println!("ldm-native-host {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if let Err(e) = run(&args) {
        log(&format!("{e:#}"));
        std::process::exit(1);
    }
}

fn run(args: &[String]) -> Result<()> {
    check_origin(args)?;
    let sock = ensure_daemon()?;
    let mut sock_w = sock.try_clone()?;
    let sock_r = BufReader::new(sock.try_clone()?);
    let out: Out = Arc::new(Mutex::new(io::stdout()));

    // daemon -> browser
    let reader = {
        let out = out.clone();
        std::thread::spawn(move || -> Result<()> {
            for line in sock_r.lines() {
                let line = line?;
                if line.trim().is_empty() {
                    continue;
                }
                let env = match Envelope::parse(line.as_bytes()) {
                    Ok(e) => e,
                    Err(e) => {
                        log(&format!("bad envelope from daemon: {e}"));
                        continue;
                    }
                };
                send_browser(&out, &serde_json::to_value(&env)?)?;
            }
            Ok(())
        })
    };

    // browser -> daemon
    let mut stdin = io::stdin().lock();
    while let Some(bytes) = read_native(&mut stdin)? {
        let v: Value = match serde_json::from_slice(&bytes) {
            Ok(v) => v,
            Err(e) => {
                send_browser(&out, &serde_json::to_value(Envelope::error(0, "E_FORMAT", e.to_string()))?)?;
                continue;
            }
        };
        match v {
            Value::Object(mut m) => {
                // the browser side is always tagged "extension" so ldmd applies its browser allowlist
                let p = m.entry("payload").or_insert_with(|| Value::Object(Default::default()));
                if let Value::Object(p) = p {
                    p.insert("_client".into(), Value::String("extension".into()));
                }
                let v = Value::Object(m);
                let mut line = serde_json::to_string(&v)?;
                line.push('\n');
                sock_w.write_all(line.as_bytes())?;
            }
            _ => send_browser(&out, &serde_json::to_value(Envelope::error(0, "E_FORMAT", "expected an envelope object"))?)?,
        }
    }
    // browser closed the port
    let _ = sock.shutdown(std::net::Shutdown::Both);
    let _ = reader.join();
    Ok(())
}
