//! ffmpeg integration. Arguments are passed as an
//! array – never through a shell.
use std::path::{Path, PathBuf};

use tokio::process::Command;

use crate::error::{LdmError, Result};

/// `LDM_FAKE_FFMPEG=1` disables ffmpeg (tests / minimal systems).
pub fn ffmpeg_disabled() -> bool {
    std::env::var("LDM_FAKE_FFMPEG").map(|v| v == "1").unwrap_or(false)
}

pub async fn ffmpeg_available(bin: &str) -> bool {
    if ffmpeg_disabled() {
        return false;
    }
    Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn ffmpeg_available_blocking(bin: &str) -> bool {
    if ffmpeg_disabled() {
        return false;
    }
    std::process::Command::new(bin)
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn tail(s: &str, lines: usize) -> String {
    let v: Vec<&str> = s.lines().collect();
    v[v.len().saturating_sub(lines)..].join("\n")
}

/// Path argument for ffmpeg: a relative name such as `Q&A: part 1.mp4` (titles may contain
/// `:`) would be read as a protocol URL, so relative paths get an explicit `file:` prefix.
pub fn ffmpeg_path(p: &Path) -> String {
    let s = p.to_string_lossy().into_owned();
    if p.is_absolute() {
        s
    } else {
        format!("file:{s}")
    }
}

/// Remux one or two inputs into `out` with `-c copy`. With two inputs the
/// first provides video and the second audio.
pub async fn remux(bin: &str, inputs: &[PathBuf], out: &Path) -> Result<()> {
    let mut args: Vec<String> = vec!["-y".into(), "-hide_banner".into(), "-loglevel".into(), "error".into()];
    for i in inputs {
        args.push("-i".into());
        args.push(ffmpeg_path(i));
    }
    if inputs.len() > 1 {
        args.extend(["-map", "0:v:0?", "-map", "1:a:0?"].iter().map(|s| s.to_string()));
    } else {
        args.extend(["-map", "0"].iter().map(|s| s.to_string()));
    }
    args.extend(["-c", "copy"].iter().map(|s| s.to_string()));
    if out.extension().map(|e| e == "mp4" || e == "m4a").unwrap_or(false) {
        args.extend(["-movflags", "+faststart"].iter().map(|s| s.to_string()));
    }
    args.push(ffmpeg_path(out));
    let o = Command::new(bin).args(&args).output().await.map_err(|e| LdmError::Ffmpeg(format!("spawn {bin}: {e}")))?;
    if !o.status.success() {
        let _ = std::fs::remove_file(out);
        return Err(LdmError::Ffmpeg(format!("exit {:?}: {}", o.status.code(), tail(&String::from_utf8_lossy(&o.stderr), 8))));
    }
    Ok(())
}

/// Media duration via ffprobe (used by tests and `ldm doctor`).
pub async fn probe_duration(ffprobe: &str, f: &Path) -> Option<f64> {
    let o = Command::new(ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=nw=1:nk=1"])
        .arg(f)
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&o.stdout).trim().parse().ok()
}

#[cfg(test)]
mod path_tests {
    use super::*;

    #[test]
    fn colon_titles_are_not_ffmpeg_protocols() {
        assert_eq!(ffmpeg_path(Path::new("/d/Q&A: x.mp4")), "/d/Q&A: x.mp4");
        assert_eq!(ffmpeg_path(Path::new("Q&A: x.mp4")), "file:Q&A: x.mp4");
    }
}
