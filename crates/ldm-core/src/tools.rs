//! LDM-managed helper tools: yt-dlp and deno (original work).
//!
//! LDM never ships yt-dlp or deno inside its packages. On first use (with the user's consent,
//! `ytdlp.auto_install`) it downloads the official release binaries into its own data
//! directory and keeps them current (`ytdlp.auto_update`). Layout below
//! `$XDG_DATA_HOME/ldm/tools` (Flatpak: `~/.var/app/io.github.wzk4work.LDM/data/ldm/tools`, which is
//! writable and executable inside the sandbox without extra permissions):
//!
//! ```text
//! .lock                         flock(2): one install / update / rollback / remove at a time
//! state.json                    last update check, ETag, latest seen version per tool
//! .tmp/                         downloads in progress (same filesystem → rename is atomic)
//! yt-dlp/<version>/yt-dlp       one directory per installed version, plus meta.json
//! yt-dlp/current  -> <version>  relative symlink, switched atomically (new link + rename)
//! yt-dlp/previous -> <version>  rollback target (kept on disk)
//! deno/<version>/deno           same layout for deno
//! ```
//!
//! Callers resolve `current` to the *versioned* path once per job, so a job keeps running
//! the file it started with while an update switches the pointer; older versions are pruned
//! only when no running job uses them.
//!
//! Verification:
//! * yt-dlp – SHA-256 against the release's `SHA2-256SUMS` (mandatory). When `gpg` is
//!   installed, `SHA2-256SUMS.sig` is also checked against yt-dlp's release signing key
//!   (embedded, fingerprint pinned); without `gpg` that step is reported as skipped.
//! * deno – SHA-256 against the release's `<asset>.sha256sum`; for releases that do not
//!   publish one, against the pinned list [`DENO_PINNED`]; with neither, LDM refuses.
use std::collections::BTreeMap;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{header, Method, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::{info, warn};

use crate::config::YtdlpConfig;
use crate::error::{LdmError, Result};
use crate::http::{HttpClient, RequestCtx};

/// yt-dlp's release signing key (public, from the yt-dlp repository's `public.key`).
pub const YTDLP_SIGNING_KEY: &str = include_str!("keys/yt-dlp-signing.asc");
/// Pinned primary-key fingerprint of [`YTDLP_SIGNING_KEY`].
pub const YTDLP_SIGNING_FPR: &str = "AC0CBBE6848D6A873464AF4E57CF65933B5A7581";

/// deno checksums for releases whose `.sha256sum` file is unavailable (version, asset, sha256).
pub const DENO_PINNED: &[(&str, &str, &str)] =
    &[("2.9.8", "deno-x86_64-unknown-linux-gnu.zip", "e890975bc2549807bbcc0b584ffafda72cc7c93349dd99c80483dd784f6c4609")];

/// Largest download accepted (yt-dlp_linux ≈ 40 MB, deno zip ≈ 45 MB).
const MAX_DOWNLOAD: u64 = 400 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Tool {
    #[serde(rename = "yt-dlp")]
    YtDlp,
    #[serde(rename = "deno")]
    Deno,
}

impl Tool {
    pub const ALL: [Tool; 2] = [Tool::YtDlp, Tool::Deno];

    pub fn name(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp",
            Tool::Deno => "deno",
        }
    }

    pub fn parse(s: &str) -> Option<Tool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "yt-dlp" | "ytdlp" | "yt_dlp" => Some(Tool::YtDlp),
            "deno" => Some(Tool::Deno),
            _ => None,
        }
    }

    pub fn bin_name(self) -> &'static str {
        self.name()
    }

    pub fn repo(self) -> &'static str {
        match self {
            Tool::YtDlp => "yt-dlp/yt-dlp",
            Tool::Deno => "denoland/deno",
        }
    }

    /// Release tag for a version (`2026.10.21` / `v2.9.8`).
    pub fn tag_for(self, version: &str) -> String {
        let v = normalize_version(version);
        match self {
            Tool::YtDlp => v,
            Tool::Deno => format!("v{v}"),
        }
    }
}

impl std::fmt::Display for Tool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------- versions

/// `v2.9.8` → `2.9.8`; surrounding whitespace removed.
pub fn normalize_version(v: &str) -> String {
    let v = v.trim();
    v.strip_prefix('v').or_else(|| v.strip_prefix('V')).unwrap_or(v).to_string()
}

/// Numeric components of a version (`2026.10.21`, `2026.10.21.1`, `v2.9.8`, `2.9.8-rc.1`
/// → leading digits of each dot-separated part before any `-`/`+` suffix).
pub fn parse_version(v: &str) -> Option<Vec<u64>> {
    let v = normalize_version(v);
    let core = v.split(['-', '+']).next()?;
    if core.is_empty() {
        return None;
    }
    core.split('.').map(|p| if p.is_empty() || !p.bytes().all(|b| b.is_ascii_digit()) { None } else { p.parse().ok() }).collect()
}

/// `a` is strictly newer than `b` (unparsable versions are never newer).
pub fn version_newer(a: &str, b: &str) -> bool {
    match (parse_version(a), parse_version(b)) {
        (Some(mut x), Some(mut y)) => {
            let n = x.len().max(y.len());
            x.resize(n, 0);
            y.resize(n, 0);
            x > y
        }
        _ => false,
    }
}

/// Version from `<bin> --version` output (`2026.10.21` / `deno 2.9.8 (stable, …)`).
pub fn parse_version_output(tool: Tool, out: &str) -> Option<String> {
    let line = out.lines().map(str::trim).find(|l| !l.is_empty())?;
    let v = match tool {
        Tool::YtDlp => line.split_whitespace().next()?,
        Tool::Deno => {
            let mut it = line.split_whitespace();
            match it.next()? {
                "deno" => it.next()?,
                x => x,
            }
        }
    };
    parse_version(v).map(|_| normalize_version(v))
}

/// Release asset for this machine. yt-dlp: the standalone binary, else the zipapp when
/// Python is available. deno: the gnu zip.
pub fn asset_for(tool: Tool, arch: &str, have_python: bool) -> Option<&'static str> {
    match (tool, arch) {
        (Tool::YtDlp, "x86_64") => Some("yt-dlp_linux"),
        (Tool::YtDlp, "aarch64") => Some("yt-dlp_linux_aarch64"),
        (Tool::YtDlp, _) if have_python => Some("yt-dlp"),
        (Tool::Deno, "x86_64") => Some("deno-x86_64-unknown-linux-gnu.zip"),
        (Tool::Deno, "aarch64") => Some("deno-aarch64-unknown-linux-gnu.zip"),
        _ => None,
    }
}

pub fn host_asset(tool: Tool) -> Result<&'static str> {
    let py = search_path("python3").is_some();
    asset_for(tool, std::env::consts::ARCH, py)
        .ok_or_else(|| LdmError::Other(format!("no official {tool} build for {} (yt-dlp's zipapp needs python3)", std::env::consts::ARCH)))
}

// ---------------------------------------------------------------- checksums

/// Look up `name` in a `sha256sum`-style list (`<hex>  <name>` or `<hex> *<name>`).
pub fn checksum_for(sums: &str, name: &str) -> Option<String> {
    sums.lines().find_map(|l| {
        let mut it = l.split_whitespace();
        let hex = it.next()?;
        let file = it.next()?.trim_start_matches('*');
        let file = file.rsplit('/').next().unwrap_or(file);
        (file == name && hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then(|| hex.to_ascii_lowercase())
    })
}

pub fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    std::io::copy(&mut f, &mut h)?;
    Ok(hex::encode(h.finalize()))
}

/// Compare a computed digest with the expected one.
pub fn verify_sha256(actual: &str, expected: Option<&str>) -> Result<()> {
    match expected {
        None => Err(LdmError::Other("no published checksum: refusing to install an unverified binary".into())),
        Some(e) if e.eq_ignore_ascii_case(actual) => Ok(()),
        Some(e) => Err(LdmError::Checksum { expected: e.to_ascii_lowercase(), actual: actual.to_string() }),
    }
}

// ---------------------------------------------------------------- gpg

/// Binary key material from an ASCII-armored OpenPGP block.
pub fn dearmor(armored: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let mut lines = armored.lines().map(str::trim);
    lines.find(|l| l.starts_with("-----BEGIN PGP"))?;
    // armor headers end at the first blank line
    for l in lines.by_ref() {
        if l.is_empty() {
            break;
        }
    }
    let body: String = lines.take_while(|l| !l.starts_with('=') && !l.starts_with("-----END")).collect();
    base64::engine::general_purpose::STANDARD.decode(body).ok().filter(|b| !b.is_empty())
}

/// Verify `sig` over `data` with `key` (armored), requiring a VALIDSIG for `fpr`. Runs gpg in
/// a throw-away home directory (never touches `~/.gnupg`, starts no agent).
pub fn gpg_verify(gpg: &Path, key: &str, fpr: &str, data: &[u8], sig: &[u8], scratch: &Path) -> Result<()> {
    let kb = dearmor(key).ok_or_else(|| LdmError::Other("signing key is not valid OpenPGP armor".into()))?;
    let home = scratch.join(format!("gnupg-{}", unique()));
    std::fs::create_dir_all(&home)?;
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
    let res = (|| {
        std::fs::write(home.join("key.gpg"), &kb)?;
        std::fs::write(home.join("data"), data)?;
        std::fs::write(home.join("data.sig"), sig)?;
        let out = std::process::Command::new(gpg)
            .arg("--batch")
            .arg("--no-tty")
            .arg("--homedir")
            .arg(&home)
            .args(["--no-autostart", "--no-default-keyring", "--keyring"])
            .arg(home.join("key.gpg"))
            .args(["--trust-model", "always", "--status-fd", "1", "--verify"])
            .arg(home.join("data.sig"))
            .arg(home.join("data"))
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| LdmError::Other(format!("cannot run gpg: {e}")))?;
        let status = String::from_utf8_lossy(&out.stdout);
        let ok = status.lines().any(|l| l.starts_with("[GNUPG:] VALIDSIG ") && l.split_whitespace().any(|t| t.eq_ignore_ascii_case(fpr)));
        if ok {
            Ok(())
        } else {
            let why =
                status.lines().find(|l| l.contains("BADSIG") || l.contains("ERRSIG") || l.contains("NO_PUBKEY")).unwrap_or("no VALIDSIG");
            Err(LdmError::Other(format!("GPG signature check failed ({why})")))
        }
    })();
    let _ = std::fs::remove_dir_all(&home);
    res
}

// ---------------------------------------------------------------- zip (deno)

fn le16(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 2).map(|x| u16::from_le_bytes([x[0], x[1]]) as u64)
}
fn le32(b: &[u8], at: usize) -> Option<u64> {
    b.get(at..at + 4).map(|x| u32::from_le_bytes([x[0], x[1], x[2], x[3]]) as u64)
}

/// Extract one entry (by base name) from a zip archive (stored or deflate; no zip64).
pub fn zip_extract(zip: &[u8], name: &str, limit: usize) -> Result<Vec<u8>> {
    let bad = |m: &str| LdmError::Format(format!("zip: {m}"));
    // end of central directory: last occurrence of PK\5\6 within the final 64 KiB + 22
    let from = zip.len().saturating_sub(65_557);
    let eocd = (from..zip.len().saturating_sub(21)).rev().find(|&i| zip[i..i + 4] == *b"PK\x05\x06").ok_or_else(|| bad("no end record"))?;
    let entries = le16(zip, eocd + 10).ok_or_else(|| bad("short"))?;
    let mut p = le32(zip, eocd + 16).ok_or_else(|| bad("short"))? as usize;
    for _ in 0..entries {
        if zip.get(p..p + 4) != Some(b"PK\x01\x02") {
            return Err(bad("broken central directory"));
        }
        let method = le16(zip, p + 10).ok_or_else(|| bad("short"))?;
        let csize = le32(zip, p + 20).ok_or_else(|| bad("short"))?;
        let usize_ = le32(zip, p + 24).ok_or_else(|| bad("short"))?;
        let nlen = le16(zip, p + 28).ok_or_else(|| bad("short"))? as usize;
        let xlen = le16(zip, p + 30).ok_or_else(|| bad("short"))? as usize;
        let clen = le16(zip, p + 32).ok_or_else(|| bad("short"))? as usize;
        let off = le32(zip, p + 42).ok_or_else(|| bad("short"))? as usize;
        let ename = zip.get(p + 46..p + 46 + nlen).ok_or_else(|| bad("short"))?;
        let ename = String::from_utf8_lossy(ename);
        p += 46 + nlen + xlen + clen;
        if ename.rsplit('/').next() != Some(name) {
            continue;
        }
        if csize == 0xFFFF_FFFF || usize_ == 0xFFFF_FFFF || off == 0xFFFF_FFFF {
            return Err(bad("zip64 entries are not supported"));
        }
        if usize_ as usize > limit {
            return Err(bad("entry too large"));
        }
        if zip.get(off..off + 4) != Some(b"PK\x03\x04") {
            return Err(bad("broken local header"));
        }
        let ln = le16(zip, off + 26).ok_or_else(|| bad("short"))? as usize;
        let lx = le16(zip, off + 28).ok_or_else(|| bad("short"))? as usize;
        let start = off + 30 + ln + lx;
        let data = zip.get(start..start + csize as usize).ok_or_else(|| bad("truncated entry"))?;
        let out = match method {
            0 => data.to_vec(),
            8 => miniz_oxide::inflate::decompress_to_vec_with_limit(data, limit).map_err(|e| bad(&format!("inflate: {e:?}")))?,
            m => return Err(bad(&format!("unsupported compression method {m}"))),
        };
        if out.len() as u64 != usize_ {
            return Err(bad("size mismatch after inflate"));
        }
        return Ok(out);
    }
    Err(bad(&format!("no entry named {name}")))
}

// ---------------------------------------------------------------- lookup

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// explicit `ytdlp.path` / `ytdlp.deno_path`
    User,
    /// LDM-managed copy in the tools directory
    Managed,
    /// found on `$PATH` (or `~/.local/bin`, `~/.deno/bin`)
    System,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::User => "user",
            Source::Managed => "managed",
            Source::System => "system",
        }
    }
    /// Short label for UIs.
    pub fn label(self) -> &'static str {
        match self {
            Source::User => "用户指定",
            Source::Managed => "LDM 管理",
            Source::System => "系统",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub path: PathBuf,
    pub source: Source,
}

/// First executable `name` on `$PATH`.
pub fn search_path(name: &str) -> Option<PathBuf> {
    search_path_in(name, std::env::var_os("PATH").as_deref())
}

pub fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0).unwrap_or(false)
}

/// Lookup order: user setting → managed copy → system (`$PATH`, then the usual per-user dirs).
/// A user setting that does not resolve is *not* skipped silently: it yields `None` so the
/// misconfiguration is visible.
pub fn resolve(tool: Tool, user_path: &str, layout: &Layout) -> Option<Found> {
    resolve_with(tool, user_path, layout, std::env::var_os("PATH").as_deref())
}

/// [`resolve`] with an explicit `$PATH` value.
pub fn resolve_with(tool: Tool, user_path: &str, layout: &Layout, path_env: Option<&std::ffi::OsStr>) -> Option<Found> {
    let p = user_path.trim();
    if !p.is_empty() {
        let pb = PathBuf::from(crate::paths::expand(p));
        let hit = if pb.components().count() > 1 { is_executable(&pb).then_some(pb) } else { search_path_in(p, path_env) };
        return hit.map(|path| Found { path, source: Source::User });
    }
    if let Some(path) = layout.current_bin(tool) {
        return Some(Found { path, source: Source::Managed });
    }
    let extra = match tool {
        Tool::YtDlp => crate::paths::home().join(".local/bin/yt-dlp"),
        Tool::Deno => crate::paths::home().join(".deno/bin/deno"),
    };
    search_path_in(tool.bin_name(), path_env)
        .or_else(|| is_executable(&extra).then_some(extra))
        .map(|path| Found { path, source: Source::System })
}

/// First executable `name` in a `$PATH`-style list.
pub fn search_path_in(name: &str, path_env: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    path_env.and_then(|paths| std::env::split_paths(paths).map(|d| d.join(name)).find(|p| is_executable(p)))
}

// ---------------------------------------------------------------- layout / state

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct ToolState {
    pub last_check_ms: i64,
    pub etag: Option<String>,
    pub latest: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct State {
    pub tools: BTreeMap<String, ToolState>,
}

impl State {
    pub fn get(&self, t: Tool) -> ToolState {
        self.tools.get(t.name()).cloned().unwrap_or_default()
    }
    pub fn entry(&mut self, t: Tool) -> &mut ToolState {
        self.tools.entry(t.name().to_string()).or_default()
    }
}

/// Per-version metadata (`<tool>/<version>/meta.json`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Meta {
    pub tool: String,
    pub version: String,
    pub asset: String,
    pub url: String,
    pub sha256: String,
    /// where the expected checksum came from (`SHA2-256SUMS`, `<asset>.sha256sum`, `pinned`)
    pub checksum_source: String,
    /// `verified` / `gpg not available` / `no signature (test server)`
    pub gpg: String,
    pub installed_at_ms: i64,
}

fn unique() -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("{}-{n}", std::process::id())
}

/// Write `data` to `path` atomically (temp file in the same dir + rename).
fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(".{}.{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("f"), unique()));
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub root: PathBuf,
}

/// Held while an install / update / rollback / remove runs; released on drop.
pub struct ToolsLock {
    _f: std::fs::File,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Layout {
        Layout { root: root.into() }
    }

    pub fn default_root() -> PathBuf {
        crate::paths::data_dir().join("tools")
    }

    pub fn from_config(cfg: &YtdlpConfig) -> Layout {
        let d = cfg.tools_dir.trim();
        if d.is_empty() {
            Layout::new(Self::default_root())
        } else {
            Layout::new(crate::paths::expand(d))
        }
    }

    pub fn tool_dir(&self, t: Tool) -> PathBuf {
        self.root.join(t.name())
    }

    pub fn version_dir(&self, t: Tool, v: &str) -> PathBuf {
        self.tool_dir(t).join(normalize_version(v))
    }

    pub fn bin_in(&self, t: Tool, v: &str) -> PathBuf {
        self.version_dir(t, v).join(t.bin_name())
    }

    fn link_target(&self, t: Tool, name: &str) -> Option<String> {
        let target = std::fs::read_link(self.tool_dir(t).join(name)).ok()?;
        let v = target.file_name()?.to_string_lossy().into_owned();
        parse_version(&v).map(|_| v)
    }

    pub fn current_version(&self, t: Tool) -> Option<String> {
        self.link_target(t, "current")
    }

    pub fn previous_version(&self, t: Tool) -> Option<String> {
        self.link_target(t, "previous").filter(|v| is_executable(&self.bin_in(t, v)))
    }

    /// The versioned binary `current` points at (not the link itself: a running job keeps
    /// its file when the pointer moves).
    pub fn current_bin(&self, t: Tool) -> Option<PathBuf> {
        let v = self.current_version(t)?;
        let p = self.bin_in(t, &v);
        is_executable(&p).then_some(p)
    }

    pub fn meta(&self, t: Tool, v: &str) -> Option<Meta> {
        std::fs::read(self.version_dir(t, v).join("meta.json")).ok().and_then(|b| serde_json::from_slice(&b).ok())
    }

    /// Installed versions, newest first.
    pub fn installed(&self, t: Tool) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(self.tool_dir(t))
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.file_type().map(|ft| ft.is_dir()).unwrap_or(false))
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| parse_version(n).is_some() && is_executable(&self.bin_in(t, n)))
                    .collect()
            })
            .unwrap_or_default();
        v.sort_by(|a, b| {
            if version_newer(a, b) {
                std::cmp::Ordering::Less
            } else if version_newer(b, a) {
                std::cmp::Ordering::Greater
            } else {
                a.cmp(b)
            }
        });
        v
    }

    /// Point `<tool>/<name>` at `version` atomically (new symlink + rename over the old one).
    fn set_link(&self, t: Tool, name: &str, version: &str) -> Result<()> {
        let dir = self.tool_dir(t);
        let tmp = dir.join(format!(".{name}.{}", unique()));
        std::os::unix::fs::symlink(normalize_version(version), &tmp)?;
        std::fs::rename(&tmp, dir.join(name)).inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })?;
        Ok(())
    }

    /// Make `version` current; the old current becomes `previous`.
    pub fn switch(&self, t: Tool, version: &str) -> Result<()> {
        let v = normalize_version(version);
        if !is_executable(&self.bin_in(t, &v)) {
            return Err(LdmError::Other(format!("{t} {v} is not installed")));
        }
        let old = self.current_version(t);
        self.set_link(t, "current", &v)?;
        if let Some(o) = old.filter(|o| *o != v) {
            self.set_link(t, "previous", &o)?;
        }
        Ok(())
    }

    /// Swap `current` and `previous`; returns (from, to).
    pub fn rollback(&self, t: Tool) -> Result<(String, String)> {
        let prev = self.previous_version(t).ok_or_else(|| LdmError::Other(format!("no previous {t} version to roll back to")))?;
        let cur = self.current_version(t).unwrap_or_default();
        self.set_link(t, "current", &prev)?;
        if !cur.is_empty() {
            self.set_link(t, "previous", &cur)?;
        }
        Ok((cur, prev))
    }

    /// Delete versions other than current / previous / in use. Returns removed versions.
    pub fn prune(&self, t: Tool, in_use: &[PathBuf]) -> Vec<String> {
        let keep: Vec<String> = [self.current_version(t), self.link_target(t, "previous")].into_iter().flatten().collect();
        let mut removed = vec![];
        for v in self.installed(t) {
            let dir = self.version_dir(t, &v);
            if keep.contains(&v) || in_use.iter().any(|p| p.starts_with(&dir)) {
                continue;
            }
            if std::fs::remove_dir_all(&dir).is_ok() {
                removed.push(v);
            }
        }
        // leftovers of interrupted installs
        if let Ok(rd) = std::fs::read_dir(self.tool_dir(t)) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with(".stage-") {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        removed
    }

    /// Remove a tool entirely (refused while a running job uses it).
    pub fn remove_tool(&self, t: Tool, in_use: &[PathBuf]) -> Result<Vec<String>> {
        let dir = self.tool_dir(t);
        if in_use.iter().any(|p| p.starts_with(&dir)) {
            return Err(LdmError::Other(format!("{t} is used by a running download; stop it first")));
        }
        let v = self.installed(t);
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => Ok(v),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(v),
            Err(e) => Err(e.into()),
        }
    }

    pub fn tmp_dir(&self) -> PathBuf {
        self.root.join(".tmp")
    }

    fn lock_file(&self) -> Result<std::fs::File> {
        std::fs::create_dir_all(&self.root)?;
        Ok(std::fs::OpenOptions::new().create(true).truncate(false).read(true).write(true).open(self.root.join(".lock"))?)
    }

    /// Non-blocking: `Ok(None)` when another operation holds the lock.
    pub fn try_lock(&self) -> Result<Option<ToolsLock>> {
        use std::os::fd::AsRawFd;
        let f = self.lock_file()?;
        let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if r == 0 {
            return Ok(Some(ToolsLock { _f: f }));
        }
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(None)
        } else {
            Err(e.into())
        }
    }

    /// Wait up to `timeout` for the lock.
    pub async fn lock(&self, timeout: Duration) -> Result<ToolsLock> {
        let t0 = Instant::now();
        loop {
            if let Some(l) = self.try_lock()? {
                return Ok(l);
            }
            if t0.elapsed() >= timeout {
                return Err(LdmError::Other("another tools install/update is running (lock busy)".into()));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub fn load_state(&self) -> State {
        std::fs::read(self.root.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_state(&self, s: &State) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        write_atomic(&self.root.join("state.json"), &serde_json::to_vec_pretty(s)?)
    }
}

// ---------------------------------------------------------------- endpoints

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// release pages and downloads: `{web}/{repo}/releases/download/{tag}/{asset}`
    pub web: String,
    /// REST API: `{api}/repos/{repo}/releases/latest`
    pub api: String,
}

impl Endpoints {
    pub fn github() -> Endpoints {
        Endpoints { web: "https://github.com".into(), api: "https://api.github.com".into() }
    }

    pub fn is_github(&self) -> bool {
        *self == Self::github()
    }

    /// `ytdlp.release_base_url` (tests only): honoured for loopback `http://` URLs only.
    pub fn from_config(cfg: &YtdlpConfig) -> Endpoints {
        let b = cfg.release_base_url.trim().trim_end_matches('/');
        if b.is_empty() {
            return Self::github();
        }
        let ok = url::Url::parse(b)
            .ok()
            .filter(|u| u.scheme() == "http")
            .and_then(|u| {
                u.host_str().map(|h| h == "localhost" || h.parse::<std::net::IpAddr>().map(|ip| ip.is_loopback()).unwrap_or(false))
            })
            .unwrap_or(false);
        if !ok {
            warn!(url = b, "ytdlp.release_base_url is for tests and must be a loopback http:// URL; using GitHub");
            return Self::github();
        }
        Endpoints { web: b.to_string(), api: format!("{b}/api") }
    }

    pub fn latest_api(&self, t: Tool) -> String {
        format!("{}/repos/{}/releases/latest", self.api, t.repo())
    }
    pub fn latest_web(&self, t: Tool) -> String {
        format!("{}/{}/releases/latest", self.web, t.repo())
    }
    pub fn asset(&self, t: Tool, tag: &str, asset: &str) -> String {
        format!("{}/{}/releases/download/{tag}/{asset}", self.web, t.repo())
    }
}

// ---------------------------------------------------------------- installer

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct InstallReport {
    pub tool: String,
    pub version: String,
    pub previous: Option<String>,
    pub path: String,
    pub asset: String,
    pub url: String,
    pub sha256: String,
    pub checksum_source: String,
    pub gpg: String,
    /// the version was already on disk: only the pointer moved
    pub reused: bool,
    pub pruned: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct UpdateReport {
    pub tool: String,
    pub from: Option<String>,
    pub latest: Option<String>,
    pub updated: bool,
    pub install: Option<InstallReport>,
}

/// Progress callback: (phase, bytes so far, total).
pub type ProgressFn = Arc<dyn Fn(&str, u64, Option<u64>) + Send + Sync>;

#[derive(Clone)]
pub struct Installer {
    pub http: HttpClient,
    pub layout: Layout,
    pub endpoints: Endpoints,
    /// gpg binary (`None` = not installed → checksum-only)
    pub gpg: Option<PathBuf>,
    /// (armored key, pinned fingerprint) for yt-dlp's SHA2-256SUMS.sig
    pub signing_key: (String, String),
    pub lock_timeout: Duration,
    pub progress: Option<ProgressFn>,
}

impl Installer {
    pub fn new(http: HttpClient, cfg: &YtdlpConfig) -> Installer {
        Installer {
            http,
            layout: Layout::from_config(cfg),
            endpoints: Endpoints::from_config(cfg),
            gpg: search_path("gpg"),
            signing_key: (YTDLP_SIGNING_KEY.into(), YTDLP_SIGNING_FPR.into()),
            lock_timeout: Duration::from_secs(600),
            progress: None,
        }
    }

    fn report(&self, phase: &str, got: u64, total: Option<u64>) {
        if let Some(p) = &self.progress {
            p(phase, got, total);
        }
    }

    async fn get_small(&self, url: &str, limit: usize) -> Result<Option<Vec<u8>>> {
        let resp = self.http.request(Method::GET, url, &RequestCtx::default()).send().await?;
        match resp.status() {
            StatusCode::NOT_FOUND => Ok(None),
            s if s.is_success() => {
                let b = resp.bytes().await?;
                if b.len() > limit {
                    return Err(LdmError::Format(format!("{url}: unexpectedly large")));
                }
                Ok(Some(b.to_vec()))
            }
            s => Err(LdmError::HttpStatus(s.as_u16(), format!("{url}: {}", s.canonical_reason().unwrap_or("")))),
        }
    }

    /// Latest release version. Uses the cached ETag (a `304` costs no API quota) and falls
    /// back to the `releases/latest` web redirect when the API is unavailable / rate limited.
    pub async fn latest(&self, t: Tool, state: &mut State) -> Result<String> {
        let st = state.get(t);
        let mut rb = self
            .http
            .request(Method::GET, &self.endpoints.latest_api(t), &RequestCtx::default())
            .header(header::ACCEPT, "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28");
        if let (Some(et), Some(_)) = (st.etag.as_deref(), st.latest.as_deref()) {
            rb = rb.header(header::IF_NONE_MATCH, et);
        }
        let now = crate::store::now_ms();
        let api = async {
            let resp = rb.send().await?;
            let status = resp.status();
            if status == StatusCode::NOT_MODIFIED {
                return st.latest.clone().ok_or_else(|| LdmError::Other("304 without a cached version".into()));
            }
            if !status.is_success() {
                return Err(LdmError::HttpStatus(status.as_u16(), "GitHub API".into()));
            }
            let etag = resp.headers().get(header::ETAG).and_then(|v| v.to_str().ok()).map(str::to_string);
            let body = resp.bytes().await?;
            let v: serde_json::Value = serde_json::from_slice(&body).map_err(|e| LdmError::Format(format!("release JSON: {e}")))?;
            let tag = v.get("tag_name").and_then(|x| x.as_str()).ok_or_else(|| LdmError::Format("release JSON without tag_name".into()))?;
            let e = state.entry(t);
            e.etag = etag;
            Ok(normalize_version(tag))
        }
        .await;
        let res = match api {
            Ok(v) => Ok(v),
            Err(e) => {
                warn!(tool = %t, error = %e, "release API failed; trying the releases/latest redirect");
                self.latest_via_redirect(t).await.map_err(|e2| LdmError::Other(format!("cannot determine the latest {t}: {e}; {e2}")))
            }
        };
        let e = state.entry(t);
        e.last_check_ms = now;
        match &res {
            Ok(v) => {
                e.latest = Some(v.clone());
                e.last_error = None;
            }
            Err(err) => e.last_error = Some(err.to_string()),
        }
        res
    }

    async fn latest_via_redirect(&self, t: Tool) -> Result<String> {
        let resp = self.http.request(Method::HEAD, &self.endpoints.latest_web(t), &RequestCtx::default()).send().await?;
        let u = resp.url().path().to_string();
        let tag = u.rsplit_once("/releases/tag/").map(|(_, t)| t.trim_matches('/').to_string()).filter(|t| parse_version(t).is_some());
        tag.map(|t| normalize_version(&t)).ok_or_else(|| LdmError::Format(format!("no release tag in redirect target {u}")))
    }

    /// Download `url` into `dest`, hashing on the fly.
    async fn download(&self, url: &str, dest: &Path) -> Result<String> {
        use futures_util::StreamExt;
        let resp = self.http.request(Method::GET, url, &RequestCtx::default()).send().await?;
        let st = resp.status();
        if !st.is_success() {
            return Err(LdmError::HttpStatus(st.as_u16(), format!("{url}: {}", st.canonical_reason().unwrap_or(""))));
        }
        let total = resp.content_length();
        if total.is_some_and(|n| n > MAX_DOWNLOAD) {
            return Err(LdmError::Format(format!("{url}: download too large")));
        }
        let mut f = std::fs::File::create(dest)?;
        let mut h = Sha256::new();
        let mut got = 0u64;
        let mut last = Instant::now();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            got += chunk.len() as u64;
            if got > MAX_DOWNLOAD {
                return Err(LdmError::Format(format!("{url}: download too large")));
            }
            h.update(&chunk);
            f.write_all(&chunk)?;
            if last.elapsed() > Duration::from_millis(250) {
                self.report("download", got, total);
                last = Instant::now();
            }
        }
        f.sync_all()?;
        self.report("download", got, total);
        Ok(hex::encode(h.finalize()))
    }

    /// Install `version` (default: latest) and make it current. Takes the tools lock.
    pub async fn install(&self, t: Tool, version: Option<&str>, in_use: &[PathBuf]) -> Result<InstallReport> {
        let _lock = self.layout.lock(self.lock_timeout).await?;
        let mut state = self.layout.load_state();
        let v = match version {
            Some(v) => normalize_version(v),
            None => self.latest(t, &mut state).await?,
        };
        let r = self.install_locked(t, &v, in_use).await;
        let _ = self.layout.save_state(&state);
        r
    }

    /// Check for a newer release and install it. `Ok(updated=false)` when current.
    pub async fn update(&self, t: Tool, in_use: &[PathBuf]) -> Result<UpdateReport> {
        let _lock = self.layout.lock(self.lock_timeout).await?;
        let mut state = self.layout.load_state();
        let cur = self.layout.current_version(t);
        let latest = self.latest(t, &mut state).await;
        let _ = self.layout.save_state(&state);
        let latest = latest?;
        let mut rep = UpdateReport { tool: t.name().into(), from: cur.clone(), latest: Some(latest.clone()), ..Default::default() };
        let newer = match &cur {
            Some(c) => version_newer(&latest, c),
            None => true,
        };
        if newer {
            rep.install = Some(self.install_locked(t, &latest, in_use).await?);
            rep.updated = true;
        }
        Ok(rep)
    }

    /// Lightweight "is there something newer?" (records the check in state.json).
    pub async fn check(&self, t: Tool) -> Result<(Option<String>, String)> {
        let _lock = self.layout.lock(self.lock_timeout).await?;
        let mut state = self.layout.load_state();
        let latest = self.latest(t, &mut state).await;
        let _ = self.layout.save_state(&state);
        Ok((self.layout.current_version(t), latest?))
    }

    pub async fn rollback(&self, t: Tool) -> Result<(String, String)> {
        let _lock = self.layout.lock(self.lock_timeout).await?;
        self.layout.rollback(t)
    }

    pub async fn remove(&self, t: Tool, in_use: &[PathBuf]) -> Result<Vec<String>> {
        let _lock = self.layout.lock(self.lock_timeout).await?;
        self.layout.remove_tool(t, in_use)
    }

    async fn install_locked(&self, t: Tool, v: &str, in_use: &[PathBuf]) -> Result<InstallReport> {
        let asset = host_asset(t)?;
        let tag = t.tag_for(v);
        let url = self.endpoints.asset(t, &tag, asset);
        let previous = self.layout.current_version(t);
        let vdir = self.layout.version_dir(t, v);
        if is_executable(&self.layout.bin_in(t, v)) {
            if let Some(m) = self.layout.meta(t, v).filter(|m| !m.sha256.is_empty()) {
                // already on disk (e.g. rolling forward again): only the pointer moves
                self.layout.switch(t, v)?;
                let pruned = self.layout.prune(t, in_use);
                info!(tool = %t, version = v, "already installed; switched");
                return Ok(InstallReport {
                    tool: t.name().into(),
                    version: v.into(),
                    previous: previous.filter(|p| p != v),
                    path: self.layout.bin_in(t, v).to_string_lossy().into_owned(),
                    asset: m.asset,
                    url: m.url,
                    sha256: m.sha256,
                    checksum_source: m.checksum_source,
                    gpg: m.gpg,
                    reused: true,
                    pruned,
                });
            }
        }
        let tmp = self.layout.tmp_dir().join(format!("{}-{}", t.name(), unique()));
        std::fs::create_dir_all(&tmp)?;
        let r = self.fetch_verify_stage(t, v, &tag, asset, &url, &tmp).await;
        let _ = std::fs::remove_dir_all(&tmp);
        let (stage, meta) = r?;
        // move the staged version dir into place
        if vdir.exists() {
            let old = self.layout.tool_dir(t).join(format!(".stage-old-{}", unique()));
            std::fs::rename(&vdir, &old)?;
            let _ = std::fs::remove_dir_all(&old);
        }
        std::fs::rename(&stage, &vdir).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&stage);
        })?;
        self.layout.switch(t, v)?;
        let pruned = self.layout.prune(t, in_use);
        info!(tool = %t, version = v, checksum = %meta.checksum_source, gpg = %meta.gpg, "installed");
        Ok(InstallReport {
            tool: t.name().into(),
            version: v.into(),
            previous: previous.filter(|p| p != v),
            path: self.layout.bin_in(t, v).to_string_lossy().into_owned(),
            asset: meta.asset,
            url: meta.url,
            sha256: meta.sha256,
            checksum_source: meta.checksum_source,
            gpg: meta.gpg,
            reused: false,
            pruned,
        })
    }

    async fn fetch_verify_stage(&self, t: Tool, v: &str, tag: &str, asset: &str, url: &str, tmp: &Path) -> Result<(PathBuf, Meta)> {
        let file = tmp.join(asset);
        self.report("download", 0, None);
        let actual = self.download(url, &file).await?;
        self.report("verify", 0, None);
        let (checksum_source, gpg) = match t {
            Tool::YtDlp => {
                let sums_url = self.endpoints.asset(t, tag, "SHA2-256SUMS");
                let sums = self
                    .get_small(&sums_url, 1 << 20)
                    .await?
                    .ok_or_else(|| LdmError::Other(format!("{sums_url} is missing: refusing to install an unverified yt-dlp")))?;
                let sums_text = String::from_utf8_lossy(&sums).into_owned();
                verify_sha256(&actual, checksum_for(&sums_text, asset).as_deref())?;
                let gpg = match &self.gpg {
                    None => "gpg not available (checksum only)".to_string(),
                    Some(g) => {
                        let sig_url = self.endpoints.asset(t, tag, "SHA2-256SUMS.sig");
                        match self.get_small(&sig_url, 1 << 16).await? {
                            Some(sig) => {
                                let (k, fpr) = &self.signing_key;
                                let (g, k, fpr, scratch) = (g.clone(), k.clone(), fpr.clone(), tmp.to_path_buf());
                                tokio::task::spawn_blocking(move || gpg_verify(&g, &k, &fpr, &sums, &sig, &scratch))
                                    .await
                                    .map_err(|e| LdmError::Other(e.to_string()))??;
                                format!("verified ({})", self.signing_key.1)
                            }
                            None if self.endpoints.is_github() => {
                                return Err(LdmError::Other(format!("{sig_url} is missing: signature cannot be checked")));
                            }
                            None => "no signature published (test server)".to_string(),
                        }
                    }
                };
                ("SHA2-256SUMS".to_string(), gpg)
            }
            Tool::Deno => {
                let sum_url = self.endpoints.asset(t, tag, &format!("{asset}.sha256sum"));
                let published = self.get_small(&sum_url, 1 << 16).await?.and_then(|b| {
                    let s = String::from_utf8_lossy(&b).into_owned();
                    // the file holds `<hex>  <asset>` (older releases: the bare hash)
                    checksum_for(&s, asset).or_else(|| {
                        let h = s.split_whitespace().next()?.to_ascii_lowercase();
                        (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
                    })
                });
                let (expected, src) = match published {
                    Some(h) => (Some(h), format!("{asset}.sha256sum")),
                    None => (
                        DENO_PINNED.iter().find(|(pv, pa, _)| *pv == v && *pa == asset).map(|(_, _, h)| h.to_string()),
                        "pinned list".to_string(),
                    ),
                };
                verify_sha256(&actual, expected.as_deref())?;
                (src, "n/a (deno publishes no signature)".to_string())
            }
        };
        // stage the version dir next to its final place (same filesystem)
        let tdir = self.layout.tool_dir(t);
        std::fs::create_dir_all(&tdir)?;
        let stage = tdir.join(format!(".stage-{}", unique()));
        std::fs::create_dir_all(&stage)?;
        let bin = stage.join(t.bin_name());
        let staged = (|| -> Result<()> {
            match t {
                Tool::YtDlp => {
                    std::fs::rename(&file, &bin).or_else(|_| std::fs::copy(&file, &bin).map(|_| ()))?;
                }
                Tool::Deno => {
                    let zip = std::fs::read(&file)?;
                    let data = zip_extract(&zip, "deno", MAX_DOWNLOAD as usize * 3)?;
                    std::fs::write(&bin, data)?;
                }
            }
            std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))?;
            Ok(())
        })();
        if let Err(e) = staged {
            let _ = std::fs::remove_dir_all(&stage);
            return Err(e);
        }
        // the binary must actually run here (catches noexec mounts, wrong arch, missing python)
        self.report("check", 0, None);
        match run_version(t, &bin).await {
            Ok(got) => {
                if normalize_version(&got) != normalize_version(v) {
                    warn!(tool = %t, expected = v, got = %got, "release reports a different version");
                }
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&stage);
                return Err(e);
            }
        }
        let meta = Meta {
            tool: t.name().into(),
            version: v.into(),
            asset: asset.into(),
            url: url.into(),
            sha256: actual,
            checksum_source,
            gpg,
            installed_at_ms: crate::store::now_ms(),
        };
        write_atomic(&stage.join("meta.json"), &serde_json::to_vec_pretty(&meta)?)?;
        Ok((stage, meta))
    }
}

/// `<bin> --version` → parsed version (60 s budget: the onefile yt-dlp unpacks itself first).
pub async fn run_version(t: Tool, bin: &Path) -> Result<String> {
    let fut = tokio::process::Command::new(bin)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .output();
    let out = match tokio::time::timeout(Duration::from_secs(60), fut).await {
        Err(_) => return Err(LdmError::Other(format!("{} --version timed out", bin.display()))),
        Ok(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(LdmError::Other(format!(
                "cannot execute {} ({e}); is {} mounted noexec?",
                bin.display(),
                bin.parent().map(|p| p.display().to_string()).unwrap_or_default()
            )))
        }
        Ok(Err(e)) => return Err(LdmError::Other(format!("cannot run {}: {e}", bin.display()))),
        Ok(Ok(o)) => o,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(LdmError::Other(format!("{} --version failed: {}", bin.display(), err.lines().last().unwrap_or("").trim())));
    }
    parse_version_output(t, &String::from_utf8_lossy(&out.stdout))
        .ok_or_else(|| LdmError::Other(format!("{} --version printed no version", bin.display())))
}

// ---------------------------------------------------------------- policy

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoInstall {
    Ask,
    Always,
    Never,
}

impl AutoInstall {
    pub fn parse(s: &str) -> AutoInstall {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "yes" | "auto" => AutoInstall::Always,
            "never" | "no" | "off" => AutoInstall::Never,
            _ => AutoInstall::Ask,
        }
    }
}

/// `ytdlp.auto_update` → check interval (`None` = off).
pub fn update_interval(s: &str) -> Option<Duration> {
    match s.trim().to_ascii_lowercase().as_str() {
        "off" | "never" | "no" => None,
        "weekly" => Some(Duration::from_secs(7 * 86400)),
        _ => Some(Duration::from_secs(86400)),
    }
}

/// An automatic check is due (`last_check_ms` from state.json).
pub fn update_due(auto_update: &str, last_check_ms: i64, now_ms: i64) -> bool {
    match update_interval(auto_update) {
        None => false,
        Some(iv) => now_ms - last_check_ms >= iv.as_millis() as i64,
    }
}

/// Status of one tool for doctor / GUI / `ldm tools status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct ToolStatus {
    pub tool: String,
    /// what jobs will run: path + source
    pub path: Option<String>,
    pub source: Option<Source>,
    pub version: Option<String>,
    pub managed_current: Option<String>,
    pub managed_previous: Option<String>,
    pub managed_versions: Vec<String>,
    pub managed_meta: Option<Meta>,
    pub latest: Option<String>,
    pub last_check_ms: i64,
    pub last_error: Option<String>,
    pub update_available: bool,
    pub tools_dir: String,
}

/// Collect the status (runs `--version` only for non-managed binaries).
pub async fn status(t: Tool, user_path: &str, layout: &Layout) -> ToolStatus {
    let found = resolve(t, user_path, layout);
    let st = layout.load_state().get(t);
    let cur = layout.current_version(t);
    let version = match &found {
        Some(f) if f.source == Source::Managed => cur.clone(),
        Some(f) => run_version(t, &f.path).await.ok(),
        None => None,
    };
    let update_available = match (&st.latest, &cur) {
        (Some(l), Some(c)) => version_newer(l, c),
        _ => false,
    };
    ToolStatus {
        tool: t.name().into(),
        path: found.as_ref().map(|f| f.path.to_string_lossy().into_owned()),
        source: found.as_ref().map(|f| f.source),
        version,
        managed_meta: cur.as_deref().and_then(|c| layout.meta(t, c)),
        managed_current: cur,
        managed_previous: layout.previous_version(t),
        managed_versions: layout.installed(t),
        latest: st.latest,
        last_check_ms: st.last_check_ms,
        last_error: st.last_error,
        update_available,
        tools_dir: layout.root.to_string_lossy().into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_bin(layout: &Layout, t: Tool, v: &str) -> PathBuf {
        let d = layout.version_dir(t, v);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(t.bin_name());
        std::fs::write(&p, format!("#!/bin/sh\necho {v}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn version_parsing_and_order() {
        assert_eq!(parse_version("2026.10.21"), Some(vec![2026, 10, 21]));
        assert_eq!(parse_version("v2.9.8"), Some(vec![2, 9, 8]));
        assert_eq!(parse_version("2.10.0-rc.1"), Some(vec![2, 10, 0]));
        assert_eq!(parse_version("nightly"), None);
        assert_eq!(parse_version(""), None);
        assert_eq!(parse_version("1..2"), None);
        assert!(version_newer("2026.10.21", "2026.9.30"));
        assert!(version_newer("2026.10.21.1", "2026.10.21"));
        assert!(!version_newer("2026.10.21", "2026.10.21.0"));
        assert!(version_newer("v2.10.0", "2.9.8"));
        assert!(!version_newer("garbage", "1.0"));
        assert_eq!(parse_version_output(Tool::YtDlp, "2026.10.21\n"), Some("2026.10.21".into()));
        assert_eq!(
            parse_version_output(Tool::Deno, "deno 2.9.8 (stable, release, x86_64-unknown-linux-gnu)\nv8 14.0\n"),
            Some("2.9.8".into())
        );
        assert_eq!(parse_version_output(Tool::YtDlp, "\n"), None);
        assert_eq!(Tool::Deno.tag_for("2.9.8"), "v2.9.8");
        assert_eq!(Tool::Deno.tag_for("v2.9.8"), "v2.9.8");
        assert_eq!(Tool::YtDlp.tag_for("2026.10.21"), "2026.10.21");
        assert_eq!(asset_for(Tool::YtDlp, "x86_64", false), Some("yt-dlp_linux"));
        assert_eq!(asset_for(Tool::YtDlp, "aarch64", false), Some("yt-dlp_linux_aarch64"));
        assert_eq!(asset_for(Tool::YtDlp, "riscv64", true), Some("yt-dlp"));
        assert_eq!(asset_for(Tool::YtDlp, "riscv64", false), None);
        assert_eq!(asset_for(Tool::Deno, "x86_64", false), Some("deno-x86_64-unknown-linux-gnu.zip"));
    }

    #[test]
    fn checksum_lists() {
        let sums = "d8bd33fc00000000000000000000000000000000000000000000000000000001  yt-dlp_linux\n\
                    aa00000000000000000000000000000000000000000000000000000000000002 *yt-dlp\n\
                    nothex  yt-dlp_macos\n";
        assert_eq!(checksum_for(sums, "yt-dlp_linux").unwrap(), "d8bd33fc00000000000000000000000000000000000000000000000000000001");
        assert!(checksum_for(sums, "yt-dlp").unwrap().starts_with("aa"));
        assert_eq!(checksum_for(sums, "yt-dlp_macos"), None);
        assert_eq!(checksum_for(sums, "yt-dlp_linux_aarch64"), None);
        // good / bad / missing
        let good = "d8bd33fc00000000000000000000000000000000000000000000000000000001";
        assert!(verify_sha256(good, Some(&good.to_ascii_uppercase())).is_ok());
        assert!(matches!(verify_sha256(good, Some(&"0".repeat(64))), Err(LdmError::Checksum { .. })));
        assert!(verify_sha256(good, None).is_err());
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("x"), b"abc").unwrap();
        assert_eq!(sha256_file(&d.path().join("x")).unwrap(), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
    }

    #[test]
    fn embedded_key_dearmors() {
        let k = dearmor(YTDLP_SIGNING_KEY).unwrap();
        // public-key packet (old- or new-format tag 6)
        assert!(k[0] == 0x99 || k[0] == 0xC6, "{:x}", k[0]);
        assert!(dearmor("garbage").is_none());
    }

    /// Build a zip (stored "a.txt", deflated "dir/deno") by hand.
    fn make_zip(deno_plain: &[u8]) -> Vec<u8> {
        let mut z = vec![];
        let mut central = vec![];
        let deflated = miniz_oxide::deflate::compress_to_vec(deno_plain, 6);
        let entries: [(&str, Vec<u8>, usize, u16); 2] = [("a.txt", b"hello".to_vec(), 5, 0), ("dir/deno", deflated, deno_plain.len(), 8)];
        for (name, body, plain_len, method) in &entries {
            let off = z.len() as u32;
            z.extend_from_slice(b"PK\x03\x04");
            z.extend_from_slice(&[20, 0, 0, 0]);
            z.extend_from_slice(&method.to_le_bytes());
            z.extend_from_slice(&[0; 8]);
            z.extend_from_slice(&(body.len() as u32).to_le_bytes());
            z.extend_from_slice(&(*plain_len as u32).to_le_bytes());
            z.extend_from_slice(&(name.len() as u16).to_le_bytes());
            z.extend_from_slice(&0u16.to_le_bytes());
            z.extend_from_slice(name.as_bytes());
            z.extend_from_slice(body);
            central.extend_from_slice(b"PK\x01\x02");
            central.extend_from_slice(&[20, 0, 20, 0, 0, 0]);
            central.extend_from_slice(&method.to_le_bytes());
            central.extend_from_slice(&[0; 8]);
            central.extend_from_slice(&(body.len() as u32).to_le_bytes());
            central.extend_from_slice(&(*plain_len as u32).to_le_bytes());
            central.extend_from_slice(&(name.len() as u16).to_le_bytes());
            central.extend_from_slice(&[0; 12]);
            central.extend_from_slice(&off.to_le_bytes());
            central.extend_from_slice(name.as_bytes());
        }
        let cd_off = z.len() as u32;
        z.extend_from_slice(&central);
        z.extend_from_slice(b"PK\x05\x06");
        z.extend_from_slice(&[0; 4]);
        z.extend_from_slice(&2u16.to_le_bytes());
        z.extend_from_slice(&2u16.to_le_bytes());
        z.extend_from_slice(&(central.len() as u32).to_le_bytes());
        z.extend_from_slice(&cd_off.to_le_bytes());
        z.extend_from_slice(&0u16.to_le_bytes());
        z
    }

    #[test]
    fn zip_entries() {
        let plain = b"#!/bin/sh\necho deno 2.9.8\n".repeat(20);
        let z = make_zip(&plain);
        assert_eq!(zip_extract(&z, "a.txt", 1 << 20).unwrap(), b"hello");
        assert_eq!(zip_extract(&z, "deno", 1 << 20).unwrap(), plain);
        assert!(zip_extract(&z, "deno", 10).is_err(), "size limit");
        assert!(zip_extract(&z, "missing", 1 << 20).is_err());
        assert!(zip_extract(b"not a zip at all, definitely not", "deno", 1 << 20).is_err());
    }

    #[test]
    fn lookup_order_user_managed_system() {
        let d = tempfile::tempdir().unwrap();
        let layout = Layout::new(d.path().join("tools"));
        let sys = d.path().join("sysbin");
        std::fs::create_dir_all(&sys).unwrap();
        let sysbin = sys.join("yt-dlp");
        std::fs::write(&sysbin, "#!/bin/sh\necho 2020.01.01\n").unwrap();
        std::fs::set_permissions(&sysbin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path_env = std::ffi::OsString::from(&sys);
        let f = resolve_with(Tool::YtDlp, "", &layout, Some(&path_env)).unwrap();
        assert_eq!((f.source, f.path.clone()), (Source::System, sysbin.clone()));
        // managed beats system
        let m = fake_bin(&layout, Tool::YtDlp, "2026.10.21");
        layout.switch(Tool::YtDlp, "2026.10.21").unwrap();
        let f = resolve_with(Tool::YtDlp, "", &layout, Some(&path_env)).unwrap();
        assert_eq!((f.source, f.path), (Source::Managed, m));
        // user beats managed; a broken user path is reported (None), not skipped
        let f = resolve_with(Tool::YtDlp, &sysbin.to_string_lossy(), &layout, Some(&path_env)).unwrap();
        assert_eq!(f.source, Source::User);
        assert!(resolve_with(Tool::YtDlp, "/nonexistent/yt-dlp", &layout, Some(&path_env)).is_none());
        // a bare name is looked up on PATH but counts as the user's choice
        assert_eq!(resolve_with(Tool::YtDlp, "yt-dlp", &layout, Some(&path_env)).unwrap().source, Source::User);
        // a managed binary that lost its exec bit is skipped
        std::fs::set_permissions(layout.bin_in(Tool::YtDlp, "2026.10.21"), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(resolve_with(Tool::YtDlp, "", &layout, Some(&path_env)).unwrap().source, Source::System);
    }

    #[test]
    fn atomic_switch_rollback_prune_remove() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path());
        let t = Tool::YtDlp;
        assert!(l.switch(t, "2026.1.1").is_err(), "not installed");
        let a = fake_bin(&l, t, "2026.1.1");
        let b = fake_bin(&l, t, "2026.2.1");
        let c = fake_bin(&l, t, "2026.3.1");
        l.switch(t, "2026.1.1").unwrap();
        assert_eq!(l.current_version(t).as_deref(), Some("2026.1.1"));
        assert_eq!(l.previous_version(t), None);
        l.switch(t, "2026.2.1").unwrap();
        // the resolved path is the versioned file, so a job started on it keeps it
        let running = l.current_bin(t).unwrap();
        assert_eq!(running, b);
        l.switch(t, "2026.3.1").unwrap();
        assert_eq!(l.current_bin(t).unwrap(), c);
        assert_eq!(l.previous_version(t).as_deref(), Some("2026.2.1"));
        assert!(running.exists());
        // relative symlink (survives moving the data dir)
        assert_eq!(std::fs::read_link(l.tool_dir(t).join("current")).unwrap(), PathBuf::from("2026.3.1"));
        assert_eq!(l.installed(t), vec!["2026.3.1", "2026.2.1", "2026.1.1"]);
        // prune keeps current + previous + in-use
        assert!(l.prune(t, std::slice::from_ref(&a)).is_empty() && a.exists());
        assert_eq!(l.prune(t, &[]), vec!["2026.1.1"]);
        assert!(!a.exists() && b.exists() && c.exists());
        // rollback swaps current/previous
        assert_eq!(l.rollback(t).unwrap(), ("2026.3.1".into(), "2026.2.1".into()));
        assert_eq!(l.current_bin(t).unwrap(), b);
        assert_eq!(l.previous_version(t).as_deref(), Some("2026.3.1"));
        l.rollback(t).unwrap();
        assert_eq!(l.current_version(t).as_deref(), Some("2026.3.1"));
        // no stray temp links
        let names: Vec<String> =
            std::fs::read_dir(l.tool_dir(t)).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.starts_with('.')), "{names:?}");
        // remove refuses while in use
        assert!(l.remove_tool(t, std::slice::from_ref(&c)).is_err());
        assert_eq!(l.remove_tool(t, &[]).unwrap().len(), 2);
        assert!(l.current_bin(t).is_none());
        assert!(l.rollback(t).is_err());
    }

    #[test]
    fn lock_contention() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path());
        let g = l.try_lock().unwrap().expect("first lock");
        assert!(l.try_lock().unwrap().is_none(), "second holder must wait");
        let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
        assert!(rt.block_on(l.lock(Duration::from_millis(300))).is_err());
        drop(g);
        // a waiter gets the lock once the holder releases it
        let g = l.try_lock().unwrap().unwrap();
        let l2 = l.clone();
        let h = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread().enable_time().build().unwrap();
            rt.block_on(l2.lock(Duration::from_secs(5))).is_ok()
        });
        std::thread::sleep(Duration::from_millis(300));
        drop(g);
        assert!(h.join().unwrap());
    }

    #[test]
    fn state_roundtrip_and_policy() {
        let d = tempfile::tempdir().unwrap();
        let l = Layout::new(d.path().join("t"));
        let mut s = l.load_state();
        s.entry(Tool::YtDlp).etag = Some("W/\"x\"".into());
        s.entry(Tool::YtDlp).latest = Some("2026.10.21".into());
        l.save_state(&s).unwrap();
        assert_eq!(l.load_state(), s);
        assert_eq!(AutoInstall::parse("ask"), AutoInstall::Ask);
        assert_eq!(AutoInstall::parse(""), AutoInstall::Ask);
        assert_eq!(AutoInstall::parse("always"), AutoInstall::Always);
        assert_eq!(AutoInstall::parse("never"), AutoInstall::Never);
        assert!(update_due("daily", 0, 86_400_000));
        assert!(!update_due("daily", 1, 86_400_000));
        assert!(!update_due("weekly", 0, 86_400_000));
        assert!(update_due("weekly", 0, 7 * 86_400_000));
        assert!(!update_due("off", 0, i64::MAX / 2));
    }

    #[test]
    fn endpoints_only_loopback_override() {
        let mut c = YtdlpConfig::default();
        assert!(Endpoints::from_config(&c).is_github());
        c.release_base_url = "http://127.0.0.1:9/".into();
        let e = Endpoints::from_config(&c);
        assert_eq!(e.api, "http://127.0.0.1:9/api");
        assert_eq!(e.latest_api(Tool::Deno), "http://127.0.0.1:9/api/repos/denoland/deno/releases/latest");
        assert_eq!(
            e.asset(Tool::YtDlp, "2026.10.21", "yt-dlp_linux"),
            "http://127.0.0.1:9/yt-dlp/yt-dlp/releases/download/2026.10.21/yt-dlp_linux"
        );
        for bad in ["https://example.com", "http://example.com", "file:///tmp"] {
            c.release_base_url = bad.into();
            assert!(Endpoints::from_config(&c).is_github(), "{bad}");
        }
    }
}
