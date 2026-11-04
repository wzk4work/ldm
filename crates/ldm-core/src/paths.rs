//! XDG directory resolution.
use std::path::{Path, PathBuf};

fn env_path(var: &str) -> Option<PathBuf> {
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

pub fn home() -> PathBuf {
    env_path("HOME").unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn config_dir() -> PathBuf {
    env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join("ldm")
}

pub fn data_dir() -> PathBuf {
    env_path("XDG_DATA_HOME").unwrap_or_else(|| home().join(".local/share")).join("ldm")
}

pub fn cache_dir() -> PathBuf {
    env_path("XDG_CACHE_HOME").unwrap_or_else(|| home().join(".cache")).join("ldm")
}

pub fn uid() -> u32 {
    // SAFETY: getuid has no preconditions.
    unsafe { libc::getuid() }
}

pub fn runtime_dir() -> PathBuf {
    env_path("XDG_RUNTIME_DIR").unwrap_or_else(|| PathBuf::from(format!("/tmp/ldm-{}", uid())))
}

/// `$LDM_SOCKET` or `$XDG_RUNTIME_DIR/ldm.sock`.
pub fn socket_path() -> PathBuf {
    env_path("LDM_SOCKET").unwrap_or_else(|| runtime_dir().join("ldm.sock"))
}

pub fn config_file() -> PathBuf {
    env_path("LDM_CONFIG").unwrap_or_else(|| config_dir().join("config.toml"))
}

/// `$LDM_DB` or `$XDG_DATA_HOME/ldm/downloads.db`.
pub fn db_path() -> PathBuf {
    env_path("LDM_DB").unwrap_or_else(|| data_dir().join("downloads.db"))
}

/// `XDG_DOWNLOAD_DIR` from `user-dirs.dirs`, else `~/Downloads`.
pub fn xdg_download_dir() -> PathBuf {
    if let Some(p) = env_path("XDG_DOWNLOAD_DIR") {
        return p;
    }
    let f = env_path("XDG_CONFIG_HOME").unwrap_or_else(|| home().join(".config")).join("user-dirs.dirs");
    if let Ok(text) = std::fs::read_to_string(f) {
        for line in text.lines() {
            if let Some(v) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") {
                return PathBuf::from(expand(v.trim_matches('"')));
            }
        }
    }
    home().join("Downloads")
}

/// Expand `~`, `$VAR` and `${VAR}` (XDG_* fall back to their spec defaults).
pub fn expand(s: &str) -> String {
    let mut out = String::new();
    let s = if let Some(rest) = s.strip_prefix("~/") {
        out.push_str(&home().to_string_lossy());
        out.push('/');
        rest
    } else if s == "~" {
        return home().to_string_lossy().into_owned();
    } else {
        s
    };
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let (name, next) = if b.get(i + 1) == Some(&b'{') {
                match s[i + 2..].find('}') {
                    Some(end) => (&s[i + 2..i + 2 + end], i + 3 + end),
                    None => {
                        out.push('$');
                        i += 1;
                        continue;
                    }
                }
            } else {
                let mut j = i + 1;
                while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                    j += 1;
                }
                (&s[i + 1..j], j)
            };
            if name.is_empty() {
                out.push('$');
                i += 1;
                continue;
            }
            out.push_str(&var_value(name));
            i = next;
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

fn var_value(name: &str) -> String {
    if let Ok(v) = std::env::var(name) {
        if !v.is_empty() {
            return v;
        }
    }
    let h = home();
    let d = match name {
        "XDG_CACHE_HOME" => h.join(".cache"),
        "XDG_CONFIG_HOME" => h.join(".config"),
        "XDG_DATA_HOME" => h.join(".local/share"),
        "XDG_DOWNLOAD_DIR" => xdg_download_dir(),
        "HOME" => h,
        _ => PathBuf::new(),
    };
    d.to_string_lossy().into_owned()
}

/// Create a directory (and parents) with mode 0700 for the leaf.
pub fn ensure_private_dir(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn expands_vars() {
        std::env::set_var("LDM_TEST_X", "abc");
        assert_eq!(expand("/a/${LDM_TEST_X}/$LDM_TEST_X/b"), "/a/abc/abc/b");
        assert!(expand("~/x").ends_with("/x"));
        assert!(!expand("${XDG_CACHE_HOME}/ldm").starts_with("/ldm"));
        assert_eq!(expand("cost$"), "cost$");
    }
}

/// Inside a Flatpak sandbox, a directory the app was not granted lies on the sandbox's own
/// root tmpfs: writes there succeed but are invisible to the host and vanish with the sandbox
/// (e.g. `~/Downloads` when the host has no `user-dirs.dirs`, or `~/Videos` without an
/// override). Returns a user-facing explanation in that case; `None` outside Flatpak.
pub fn sandbox_hidden(dir: &Path) -> Option<String> {
    if std::env::var_os("FLATPAK_ID").is_none_or(|v| v.is_empty()) {
        return None;
    }
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mount = covering_mount(&info, dir)?;
    (mount == Path::new("/")).then(|| {
        format!(
            "{} 不在 Flatpak 沙箱可写出的位置（文件会随沙箱消失）：请用 `flatpak override --user --filesystem={} io.github.wzk4work.LDM` 授权，\
             或改用已授权的下载目录（默认 xdg-download）",
            dir.display(),
            dir.display()
        )
    })
}

/// Mount point (from /proc/self/mountinfo text) that contains `dir`: the longest prefix.
fn covering_mount(mountinfo: &str, dir: &Path) -> Option<PathBuf> {
    mountinfo
        .lines()
        .filter_map(|l| l.split(' ').nth(4))
        .map(|m| PathBuf::from(unescape_mount(m)))
        .filter(|m| dir.starts_with(m))
        .max_by_key(|m| m.components().count())
}

fn unescape_mount(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push(u8::from_str_radix(&s[i + 1..i + 4], 8).unwrap_or(b'?'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod sandbox_tests {
    use super::*;

    const INFO: &str = "\
1 0 0:1 / / rw - tmpfs tmpfs rw
2 1 0:2 / /app ro - ext4 /dev/sda1 ro
3 1 0:3 /home/u/Downloads /home/u/Downloads rw - ext4 /dev/sda1 rw
4 1 0:3 /home/u/My\\040Files /home/u/My\\040Files rw - ext4 /dev/sda1 rw
5 1 0:3 /home/u/.var/app/x /home/u/.var/app/x rw - ext4 /dev/sda1 rw";

    #[test]
    fn longest_mount_prefix_decides() {
        assert_eq!(covering_mount(INFO, Path::new("/home/u/Downloads/LDM")).unwrap(), Path::new("/home/u/Downloads"));
        assert_eq!(covering_mount(INFO, Path::new("/home/u/Videos")).unwrap(), Path::new("/"));
        assert_eq!(covering_mount(INFO, Path::new("/home/u/DownloadsX")).unwrap(), Path::new("/"));
        assert_eq!(covering_mount(INFO, Path::new("/home/u/My Files/a")).unwrap(), Path::new("/home/u/My Files"));
    }
}
