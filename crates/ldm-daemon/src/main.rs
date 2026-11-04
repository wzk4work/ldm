//! ldmd – LDM user-session daemon.
mod handler;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::Parser;
use ldm_core::manager::Manager;
use ldm_core::store::Store;
use ldm_core::{paths, Config};
use ldm_ipc::messages::types;
use ldm_ipc::{Envelope, MAX_MESSAGE_BYTES};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use handler::{Daemon, WsBackend};

#[derive(Parser, Debug)]
#[command(name = "ldmd", version, about = "LDM 下载守护进程 (user session daemon)")]
struct Args {
    /// Unix socket path (default $XDG_RUNTIME_DIR/ldm.sock or $LDM_SOCKET)
    #[arg(long)]
    socket: Option<PathBuf>,
    /// Config file (default ~/.config/ldm/config.toml or $LDM_CONFIG)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Database file (default ~/.local/share/ldm/downloads.db or $LDM_DB)
    #[arg(long)]
    db: Option<PathBuf>,
    /// Enable the localhost WebSocket bridge regardless of config
    #[arg(long)]
    ws: bool,
    /// WebSocket bridge port (overrides config; 0 = any free port)
    #[arg(long)]
    ws_port: Option<u16>,
    /// Log filter (e.g. info, debug, ldm_core=debug)
    #[arg(long, default_value = "info")]
    log: String,
}

fn init_logging(filter: &str) {
    let f = std::env::var("LDM_LOG").unwrap_or_else(|_| filter.to_string());
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::new(f)).with_writer(std::io::stderr).with_target(false).init();
}

/// Refuse to start twice; remove a stale socket.
fn claim_socket(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            bail!("ldmd is already running (socket {} is live)", path.display());
        }
        warn!(path = %path.display(), "removing stale socket");
        std::fs::remove_file(path).ok();
    }
    if let Some(dir) = path.parent() {
        paths::ensure_private_dir(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let l = UnixListener::bind(path).with_context(|| format!("bind {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(l)
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    init_logging(&args.log);
    if let Err(e) = run(args).await {
        error!("{e:#}");
        eprintln!("ldmd: {e:#}");
        std::process::exit(1);
    }
}

async fn run(args: Args) -> Result<()> {
    if let Some(s) = &args.socket {
        std::env::set_var("LDM_SOCKET", s);
    }
    let cfg_path = args.config.clone().unwrap_or_else(paths::config_file);
    let mut cfg = Config::load_from(&cfg_path).with_context(|| format!("load {}", cfg_path.display()))?;
    if args.ws {
        cfg.browser.ws_bridge_enabled = true;
    }
    if let Some(p) = args.ws_port {
        cfg.browser.ws_bridge_port = p;
    }
    let sock_path = paths::socket_path();
    let listener = claim_socket(&sock_path)?;
    let db_path = args.db.clone().unwrap_or_else(paths::db_path);
    let store = Store::open(&db_path).with_context(|| format!("open {}", db_path.display()))?;
    let ws_cfg = cfg.browser.clone();
    let mgr = Manager::new(cfg, Some(cfg_path.clone()), store)?;
    let shutdown = CancellationToken::new();
    let daemon = Daemon::new(mgr.clone(), shutdown.clone());
    mgr.start();
    info!(socket = %sock_path.display(), db = %db_path.display(), config = %cfg_path.display(), version = env!("CARGO_PKG_VERSION"), "ldmd started");

    if ws_cfg.ws_bridge_enabled {
        let bcfg = ldm_ws_bridge::BridgeConfig {
            host: ws_cfg.ws_bridge_host.clone(),
            port: ws_cfg.ws_bridge_port,
            port_fallback: 10,
            token: (!ws_cfg.ws_bridge_token.is_empty()).then(|| ws_cfg.ws_bridge_token.clone()),
        };
        match ldm_ws_bridge::bind(&bcfg).await {
            Ok((l, port)) => {
                daemon.ws_port.store(port, Ordering::Relaxed);
                let _ = std::fs::write(paths::runtime_dir().join("ldm-ws.port"), port.to_string());
                info!(port, "WebSocket bridge listening on {}:{port}", bcfg.host);
                // machine-readable line for scripts/tests
                println!("WS_PORT {port}");
                tokio::spawn(ldm_ws_bridge::serve(l, Arc::new(bcfg), Arc::new(WsBackend(daemon.clone())), shutdown.clone()));
            }
            Err(e) => warn!(err = %e, "WebSocket bridge disabled: bind failed"),
        }
    }
    println!("READY {}", sock_path.display());

    let my_uid = paths::uid();
    let accept = {
        let daemon = daemon.clone();
        let shutdown = shutdown.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = tokio::select! {
                    r = listener.accept() => match r { Ok(v) => v, Err(e) => { warn!(err = %e, "accept"); continue; } },
                    _ = shutdown.cancelled() => break,
                };
                match stream.peer_cred() {
                    Ok(c) if c.uid() == my_uid => {}
                    Ok(c) => {
                        warn!(uid = c.uid(), "rejecting connection from another user");
                        continue;
                    }
                    Err(e) => {
                        warn!(err = %e, "peer credentials unavailable");
                        continue;
                    }
                }
                let (d, s) = (daemon.clone(), shutdown.clone());
                tokio::spawn(async move {
                    if let Err(e) = unix_client(stream, d, s).await {
                        tracing::debug!(err = %e, "client ended");
                    }
                });
            }
        })
    };

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => info!("SIGINT"),
        _ = sigterm.recv() => info!("SIGTERM"),
        _ = shutdown.cancelled() => info!("shutdown requested"),
    }
    shutdown.cancel();
    mgr.shutdown(Duration::from_secs(10)).await;
    accept.abort();
    let _ = std::fs::remove_file(&sock_path);
    let _ = std::fs::remove_file(paths::runtime_dir().join("ldm-ws.port"));
    info!("ldmd stopped");
    Ok(())
}

/// One Unix-socket client. Framing is detected from the first byte:
/// `{` → NDJSON, anything else → u32-LE length prefix (native-messaging style).
async fn unix_client(stream: UnixStream, daemon: Arc<Daemon>, shutdown: CancellationToken) -> Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut rd = BufReader::new(rd);
    let first = {
        let buf = rd.fill_buf().await?;
        if buf.is_empty() {
            return Ok(());
        }
        buf[0]
    };
    let ndjson = first == b'{' || first == b' ' || first == b'\n';
    let mut events: broadcast::Receiver<Envelope> = daemon.events.subscribe();
    let mut subscribed = false;
    let mut browser = false;
    // a subscribed GUI answers capture confirmations; dropped with the connection
    let mut _gui: Option<ldm_core::manager::GuiGuard> = None;
    let mut line = String::new();
    loop {
        let msg: Option<Vec<u8>> = tokio::select! {
            r = async {
                if ndjson {
                    line.clear();
                    let n = (&mut rd).take(MAX_MESSAGE_BYTES as u64 + 1).read_line(&mut line).await?;
                    if n == 0 { return Ok::<_, std::io::Error>(None); }
                    if n > MAX_MESSAGE_BYTES { return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too large")); }
                    Ok(Some(line.trim().as_bytes().to_vec()))
                } else {
                    ldm_ipc::framing::asyncio::read_native(&mut rd).await
                }
            } => r?,
            ev = events.recv() => {
                match ev {
                    Ok(ev) => {
                        let always = matches!(ev.kind.as_str(), types::RULESET_PUSH | types::SHOW_MEDIA_BUTTON | types::HIDE_MEDIA_BUTTON)
                            || (browser && matches!(ev.kind.as_str(), types::NOTIFY | types::GET_COOKIES));
                        if subscribed || always {
                            write_env(&mut wr, &ev, ndjson).await?;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                }
                continue;
            }
            _ = shutdown.cancelled() => break,
        };
        let Some(bytes) = msg else { break };
        if bytes.is_empty() {
            continue;
        }
        let replies = match Envelope::parse(&bytes) {
            Ok(env) => {
                if env.kind == types::HELLO {
                    browser = true;
                }
                if env.kind == types::SUBSCRIBE {
                    subscribed = env.payload.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
                    let gui = env.payload.get("role").and_then(|v| v.as_str()) == Some("gui");
                    _gui = if subscribed && gui { _gui.or_else(|| Some(daemon.mgr.gui_attach())) } else { None };
                }
                let client = env.payload.get("_client").and_then(|v| v.as_str()).unwrap_or("ipc").to_string();
                daemon.dispatch(env, &client).await
            }
            Err(e) => vec![Envelope::error(0, "E_FORMAT", e.to_string())],
        };
        for r in replies {
            write_env(&mut wr, &r, ndjson).await?;
        }
    }
    Ok(())
}

async fn write_env(wr: &mut tokio::net::unix::OwnedWriteHalf, env: &Envelope, ndjson: bool) -> std::io::Result<()> {
    let j = env.to_json();
    if ndjson {
        wr.write_all(j.as_bytes()).await?;
        wr.write_all(b"\n").await?;
        wr.flush().await
    } else {
        ldm_ipc::framing::asyncio::write_native(wr, j.as_bytes()).await
    }
}
