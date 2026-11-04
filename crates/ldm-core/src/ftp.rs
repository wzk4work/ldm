//! FTP / FTPS download: passive mode (EPSV, then PASV), binary type,
//! `SIZE` for the total and `REST` to resume a partial file. One data connection per job.
//!
//! FTPS (RFC 4217) with rustls:
//! - `ftps://` is implicit TLS (TLS from the first byte, default port 990);
//! - `ftpes://` is explicit TLS (`AUTH TLS` on the control connection, required);
//! - `ftp://` follows `[ftp] tls` ("off" / "try" / "require") or a per-site `ftp_explicit_tls`.
//!
//! With TLS the data channel is protected too (`PBSZ 0`, `PROT P`) and is opened with the control
//! connection's client config, so the server can resume the control session (servers such as
//! vsftpd `require_ssl_reuse` insist on that). Certificates are verified against the bundled
//! webpki roots; `[[sites.tls]] accept_invalid_cert` turns that off for one host.
//! Own implementation of RFC 959 / 2428 / 3659 / 4217 commands.

use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tracing::{info, warn};

use crate::config::Config;
use crate::engine::http_job::HttpOutcome;
use crate::engine::{resolve_output, verify_sha256, HttpJobSpec, JobEnv};
use crate::error::{LdmError, Result};
use crate::paths::ensure_private_dir;
use crate::util::filename;

/// URL schemes handled by this module.
pub fn is_ftp_url(u: &str) -> bool {
    ["ftp://", "ftps://", "ftpes://"].iter().any(|s| u.len() >= s.len() && u[..s.len()].eq_ignore_ascii_case(s))
}

/// How the control connection is protected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlsMode {
    Off,
    /// AUTH TLS if the server accepts it, plain otherwise.
    Try,
    /// AUTH TLS or fail.
    Explicit,
    /// TLS from the first byte (port 990).
    Implicit,
}

/// TLS mode and default port for `url` under `cfg`.
pub fn tls_mode(url: &url::Url, cfg: &Config) -> (TlsMode, u16) {
    match url.scheme() {
        "ftps" => (TlsMode::Implicit, 990),
        "ftpes" => (TlsMode::Explicit, 21),
        _ => {
            let site = url.host_str().and_then(|h| cfg.sites.tls_for(h)).map(|t| t.ftp_explicit_tls).unwrap_or(false);
            let m = match cfg.ftp.tls.as_str() {
                _ if site => TlsMode::Explicit,
                "require" => TlsMode::Explicit,
                "try" => TlsMode::Try,
                _ => TlsMode::Off,
            };
            (m, 21)
        }
    }
}

/// Accepts any certificate but still checks the handshake signatures (per-site opt-in only).
#[derive(Debug)]
struct AcceptAnyCert(Arc<CryptoProvider>);

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// Environment variable naming an extra PEM trust anchor. Honoured only in debug builds (tests);
/// release builds ignore it.
pub const TEST_CA_ENV: &str = "LDM_TEST_FTPS_CA";

/// Client config for one job (control and data connections share it, which shares the session
/// cache and lets the data channel resume the control session).
pub fn client_config(accept_invalid_cert: bool) -> Result<Arc<ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| LdmError::Other(format!("TLS setup: {e}")))?;
    let cfg = if accept_invalid_cert {
        builder.dangerous().with_custom_certificate_verifier(Arc::new(AcceptAnyCert(provider))).with_no_client_auth()
    } else {
        let mut roots = RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        if cfg!(debug_assertions) {
            if let Some(p) = std::env::var_os(TEST_CA_ENV).filter(|v| !v.is_empty()) {
                use rustls::pki_types::pem::PemObject;
                for c in CertificateDer::pem_file_iter(&p).map_err(|e| LdmError::Other(format!("{TEST_CA_ENV}: {e}")))? {
                    let c = c.map_err(|e| LdmError::Other(format!("{TEST_CA_ENV}: {e}")))?;
                    roots.add(c).map_err(|e| LdmError::Other(format!("{TEST_CA_ENV}: {e}")))?;
                }
            }
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(Arc::new(cfg))
}

#[derive(Clone)]
struct Tls {
    connector: TlsConnector,
    name: ServerName<'static>,
    timeout: Duration,
}

impl Tls {
    async fn wrap(&self, tcp: TcpStream, what: &str) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
        match tokio::time::timeout(self.timeout, self.connector.connect(self.name.clone(), tcp)).await {
            Err(_) => Err(LdmError::Timeout(format!("FTPS {what} handshake"))),
            Ok(Ok(s)) => Ok(s),
            Ok(Err(e)) => Err(tls_error(e, what)),
        }
    }
}

fn tls_error(e: std::io::Error, what: &str) -> LdmError {
    match e.get_ref().and_then(|i| i.downcast_ref::<rustls::Error>()) {
        Some(rustls::Error::InvalidCertificate(c)) => LdmError::Security(format!(
            "FTPS {what}: certificate rejected ({c:?}); enable 'accept invalid certificate' for this site only if you trust it"
        )),
        Some(r) => LdmError::Network(format!("FTPS {what} handshake: {r}")),
        None => LdmError::Network(format!("FTPS {what} handshake: {e}")),
    }
}

/// Control or data stream, plain or TLS.
enum Io {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::client::TlsStream<TcpStream>>),
}

impl AsyncRead for Io {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_read(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Io {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_write(cx, buf),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_flush(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Io::Plain(s) => Pin::new(s).poll_shutdown(cx),
            Io::Tls(s) => Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

struct Control {
    io: BufReader<Io>,
    peer: SocketAddr,
    timeout: Duration,
}

impl Control {
    /// Read one (possibly multi-line) reply: `(code, text)`.
    async fn reply(&mut self) -> Result<(u16, String)> {
        let mut first = String::new();
        let n = tokio::time::timeout(self.timeout, self.io.read_line(&mut first))
            .await
            .map_err(|_| LdmError::Timeout("FTP reply".into()))??;
        if n == 0 {
            return Err(LdmError::Network("FTP control connection closed".into()));
        }
        let code: u16 =
            first.get(..3).and_then(|c| c.parse().ok()).ok_or_else(|| LdmError::Format(format!("bad FTP reply: {}", first.trim())))?;
        let mut text = first.trim_end().to_string();
        if first.as_bytes().get(3) == Some(&b'-') {
            let end = format!("{code} ");
            loop {
                let mut l = String::new();
                if self.io.read_line(&mut l).await? == 0 {
                    break;
                }
                text.push('\n');
                text.push_str(l.trim_end());
                if l.starts_with(&end) {
                    break;
                }
            }
        }
        Ok((code, text))
    }

    async fn cmd(&mut self, c: &str) -> Result<(u16, String)> {
        let w = self.io.get_mut();
        w.write_all(format!("{c}\r\n").as_bytes()).await?;
        w.flush().await?;
        self.reply().await
    }

    async fn expect(&mut self, c: &str, ok: &[u16]) -> Result<String> {
        let (code, text) = self.cmd(c).await?;
        if ok.contains(&code) {
            Ok(text)
        } else {
            let shown = if c.starts_with("PASS") { "PASS ***" } else { c };
            Err(ftp_error(code, &format!("{shown}: {text}")))
        }
    }

    /// Switch the control connection to TLS (after `AUTH TLS` was accepted).
    async fn start_tls(self, tls: &Tls) -> Result<Control> {
        if !self.io.buffer().is_empty() {
            return Err(LdmError::Format("FTPS: unexpected data before the TLS handshake".into()));
        }
        let Control { io, peer, timeout } = self;
        let tcp = match io.into_inner() {
            Io::Plain(t) => t,
            Io::Tls(_) => return Err(LdmError::Format("FTPS: control connection already uses TLS".into())),
        };
        let s = tls.wrap(tcp, "control").await?;
        Ok(Control { io: BufReader::new(Io::Tls(Box::new(s))), peer, timeout })
    }

    /// Open a passive data connection (TCP only; TLS is started after the transfer command).
    async fn passive(&mut self) -> Result<TcpStream> {
        let addr = match self.cmd("EPSV").await? {
            (229, t) => {
                // "229 Entering Extended Passive Mode (|||port|)"
                let port = t.split('|').filter(|s| !s.is_empty()).filter_map(|s| s.trim_end_matches(')').parse::<u16>().ok()).next_back();
                port.map(|p| SocketAddr::new(self.peer.ip(), p))
            }
            _ => None,
        };
        let addr = match addr {
            Some(a) => a,
            None => {
                let t = self.expect("PASV", &[227]).await?;
                let nums: Vec<u16> = t
                    .split(|c: char| !c.is_ascii_digit())
                    .filter(|s| !s.is_empty())
                    .skip(1) // the reply code
                    .filter_map(|s| s.parse().ok())
                    .collect();
                if nums.len() < 6 {
                    return Err(LdmError::Format(format!("bad PASV reply: {t}")));
                }
                let n = &nums[nums.len() - 6..];
                // use the control peer address (ignores private addresses some NATed servers report)
                SocketAddr::new(self.peer.ip(), n[4] * 256 + n[5])
            }
        };
        Ok(tokio::time::timeout(self.timeout, TcpStream::connect(addr))
            .await
            .map_err(|_| LdmError::Timeout("FTP data connect".into()))??)
    }
}

fn ftp_error(code: u16, text: &str) -> LdmError {
    match code {
        421 | 425 | 426 | 450 | 451 => LdmError::Network(format!("FTP {text}")),
        530 => LdmError::HttpStatus(401, format!("FTP login failed: {text}")),
        550 => LdmError::HttpStatus(404, format!("FTP {text}")),
        _ => LdmError::Other(format!("FTP {text}")),
    }
}

fn pct_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s).decode_utf8_lossy().into_owned()
}

/// Download `spec.url` (ftp|ftps|ftpes://[user[:pass]@]host[:port]/path).
pub async fn run_ftp_job(env: &JobEnv, spec: &HttpJobSpec) -> Result<HttpOutcome> {
    let url = url::Url::parse(&spec.url).map_err(|e| LdmError::Format(e.to_string()))?;
    let host = url.host_str().ok_or_else(|| LdmError::Format("FTP URL without host".into()))?.to_string();
    let (mode, default_port) = tls_mode(&url, &env.cfg);
    let port = url.port().unwrap_or(default_port);
    let user = if url.username().is_empty() { "anonymous".to_string() } else { pct_decode(url.username()) };
    let pass = url.password().map(pct_decode).unwrap_or_else(|| "ldm@".into());
    let path = pct_decode(url.path());
    if path.is_empty() || path.ends_with('/') {
        return Err(LdmError::Format("FTP URL points to a directory".into()));
    }
    ensure_private_dir(&spec.parts_dir)?;
    env.progress.set_phase("Probing");
    let timeout = Duration::from_secs(env.cfg.engine.connect_timeout_secs.max(5));
    let io_timeout = Duration::from_secs(env.cfg.engine.timeout_secs.max(5));
    let tcp = tokio::time::timeout(timeout, TcpStream::connect((host.as_str(), port)))
        .await
        .map_err(|_| LdmError::Timeout(format!("connect {host}:{port}")))??;
    let peer = tcp.peer_addr()?;
    let tls = if mode == TlsMode::Off {
        None
    } else {
        let insecure = env.cfg.sites.tls_for(&host).map(|t| t.accept_invalid_cert).unwrap_or(false);
        if insecure {
            warn!(host = %host, "FTPS: certificate verification is off for this site (accept_invalid_cert)");
        }
        let name = ServerName::try_from(host.clone()).map_err(|e| LdmError::Format(format!("FTPS host name: {e}")))?;
        Some(Tls { connector: TlsConnector::from(client_config(insecure)?), name, timeout: io_timeout })
    };
    let io = match (&tls, mode) {
        (Some(t), TlsMode::Implicit) => Io::Tls(Box::new(t.wrap(tcp, "control").await?)),
        _ => Io::Plain(tcp),
    };
    let mut c = Control { io: BufReader::new(io), peer, timeout: io_timeout };
    let (code, text) = c.reply().await?;
    if code != 220 {
        return Err(ftp_error(code, &text));
    }
    let mut secure = mode == TlsMode::Implicit;
    if let (Some(t), TlsMode::Explicit | TlsMode::Try) = (&tls, mode) {
        match c.cmd("AUTH TLS").await? {
            (234, _) | (334, _) => {
                c = c.start_tls(t).await?;
                secure = true;
            }
            (_, text) if mode == TlsMode::Explicit => {
                return Err(LdmError::Security(format!("FTPS required but the server refused AUTH TLS ({})", text.trim())));
            }
            (code, _) => info!(code, "FTP: server does not offer AUTH TLS; continuing without TLS"),
        }
    }
    match c.cmd(&format!("USER {user}")).await? {
        (230, _) => {}
        (331, _) | (332, _) => {
            c.expect(&format!("PASS {pass}"), &[230, 202]).await?;
        }
        (code, t) => return Err(ftp_error(code, &t)),
    }
    if secure {
        c.expect("PBSZ 0", &[200]).await?;
        c.expect("PROT P", &[200]).await?;
    }
    let data_tls = if secure { tls.clone() } else { None };
    c.expect("TYPE I", &[200]).await?;
    let total = match c.cmd(&format!("SIZE {path}")).await? {
        (213, t) => t.get(4..).and_then(|s| s.trim().parse::<u64>().ok()),
        _ => None,
    };
    let name = spec
        .filename_hint
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| filename::sanitize(&pct_decode(path.rsplit('/').next().unwrap_or("download"))));
    env.progress.set_filename(&name);
    env.progress.set_total(total);
    info!(host = %host, total = ?total, "FTP transfer");

    let part = spec.parts_dir.join("ftp.part");
    let mut have = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    if total.map(|t| have > t).unwrap_or(false) {
        have = 0;
    }
    let mut data = c.passive().await?;
    if have > 0 {
        match c.cmd(&format!("REST {have}")).await? {
            (350, _) => info!(offset = have, "FTP resume"),
            _ => have = 0,
        }
        if have == 0 {
            // REST refused: reopen data and start over
            drop(data);
            data = c.passive().await?;
        }
    }
    c.expect(&format!("RETR {path}"), &[125, 150]).await?;
    let mut data = match &data_tls {
        Some(t) => {
            let s = t.wrap(data, "data").await?;
            info!(resumed = ?s.get_ref().1.handshake_kind(), "FTPS data channel (PROT P)");
            Io::Tls(Box::new(s))
        }
        None => Io::Plain(data),
    };
    env.progress.set_phase("Running");
    env.progress.connections.store(1, std::sync::atomic::Ordering::Relaxed);
    let mut f = tokio::fs::OpenOptions::new().create(true).write(true).truncate(have == 0).open(&part).await?;
    if have > 0 {
        use tokio::io::AsyncSeekExt;
        f.seek(std::io::SeekFrom::Start(have)).await?;
    }
    env.progress.set_got(have);
    let mut got = have;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = tokio::select! {
            r = tokio::time::timeout(c.timeout, data.read(&mut buf)) => match r.map_err(|_| LdmError::Timeout("FTP data".into()))? {
                Ok(n) => n,
                // many FTPS servers close the data channel without close_notify; the 226 reply
                // and the SIZE check below decide whether the transfer is complete
                Err(e) if data_tls.is_some() && e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
                Err(e) => return Err(e.into()),
            },
            _ = env.cancel.cancelled() => {
                f.flush().await?;
                let _ = c.cmd("ABOR").await;
                return Err(LdmError::Canceled);
            }
        };
        if n == 0 {
            break;
        }
        env.throttle(n).await;
        f.write_all(&buf[..n]).await?;
        got += n as u64;
        env.progress.set_got(got);
    }
    f.flush().await?;
    if env.cfg.engine.fsync {
        f.sync_all().await?;
    }
    drop(f);
    if data_tls.is_some() {
        let _ = tokio::time::timeout(Duration::from_secs(2), data.shutdown()).await;
    }
    drop(data);
    env.progress.connections.store(0, std::sync::atomic::Ordering::Relaxed);
    let (code, text) = c.reply().await?;
    if code != 226 && code != 250 {
        return Err(ftp_error(code, &text));
    }
    let _ = c.cmd("QUIT").await;
    if let Some(t) = total {
        if got != t {
            return Err(LdmError::Network(format!("FTP transfer ended at {got} of {t} bytes")));
        }
    }
    env.progress.set_phase("Assembling");
    let dest = resolve_output(&env.cfg, &spec.output, &name);
    move_into(&part, &dest)?;
    verify_sha256(&dest, spec.sha256.as_deref())?;
    let _ = std::fs::remove_dir_all(&spec.parts_dir);
    Ok(HttpOutcome { path: dest, total: got, final_url: spec.url.clone(), mime: None, range_ok: true, splits: 0 })
}

fn move_into(from: &Path, to: &Path) -> Result<()> {
    if let Some(d) = to.parent() {
        std::fs::create_dir_all(d)?;
    }
    if std::fs::rename(from, to).is_err() {
        std::fs::copy(from, to)?;
        std::fs::remove_file(from)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::SiteTls;

    fn mode(u: &str, cfg: &Config) -> (TlsMode, u16) {
        tls_mode(&url::Url::parse(u).unwrap(), cfg)
    }

    #[test]
    fn schemes_and_modes() {
        assert!(is_ftp_url("FTP://h/x") && is_ftp_url("ftps://h/x") && is_ftp_url("ftpes://h/x"));
        assert!(!is_ftp_url("http://h/x") && !is_ftp_url("ftp:") && !is_ftp_url("sftp://h/x"));
        let mut cfg = Config::default();
        assert_eq!(cfg.ftp.tls, "off");
        assert_eq!(mode("ftps://h/x", &cfg), (TlsMode::Implicit, 990));
        assert_eq!(mode("ftpes://h/x", &cfg), (TlsMode::Explicit, 21));
        assert_eq!(mode("ftp://h/x", &cfg), (TlsMode::Off, 21));
        cfg.ftp.tls = "try".into();
        assert_eq!(mode("ftp://h/x", &cfg).0, TlsMode::Try);
        cfg.ftp.tls = "require".into();
        assert_eq!(mode("ftp://h/x", &cfg).0, TlsMode::Explicit);
        cfg.ftp.tls = "off".into();
        cfg.sites.tls.push(SiteTls { host: "secure.example".into(), ftp_explicit_tls: true, accept_invalid_cert: false });
        assert_eq!(mode("ftp://secure.example/x", &cfg).0, TlsMode::Explicit);
        assert_eq!(mode("ftp://other.example/x", &cfg).0, TlsMode::Off);
    }

    #[test]
    fn verification_is_on_by_default() {
        // a default config never disables verification for any host
        let cfg = Config::default();
        assert!(cfg.sites.tls_for("127.0.0.1").is_none());
        assert!(client_config(false).is_ok() && client_config(true).is_ok());
    }
}
