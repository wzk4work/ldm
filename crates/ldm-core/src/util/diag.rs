//! Diagnostics bundle: config, job summaries and the log tail,
//! with secrets removed so the file can be attached to a bug report by the user.

use serde_json::{json, Map, Value};

use super::security::redact_url;

const SECRET_KEYS: &[&str] = &[
    "password",
    "pass",
    "cookie",
    "cookies",
    "token",
    "secret",
    "authorization",
    "headers",
    "on_complete_command",
    "post_command",
    "command",
    "username",
    "user",
];

fn is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_KEYS.iter().any(|s| k == *s || k.ends_with(&format!("_{s}")))
}

/// Recursively blank secret keys and redact URL query strings in all strings.
pub fn redact(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                let blank =
                    is_secret(k) && !matches!(x, Value::Null | Value::Bool(_)) && x != &json!("") && x != &json!([]) && x != &json!({});
                out.insert(k.clone(), if blank { json!("<redacted>") } else { redact(x) });
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(redact).collect()),
        Value::String(s) => Value::String(redact_url(s)),
        o => o.clone(),
    }
}

/// Build the bundle. `jobs` is the JobList array; only non-identifying fields are kept.
pub fn bundle(config: &Value, jobs: &[Value], log_tail: &str, daemon_running: bool) -> Value {
    let jobs: Vec<Value> = jobs
        .iter()
        .map(|j| {
            let host = j["url"].as_str().and_then(|u| url::Url::parse(u).ok()).and_then(|u| u.host_str().map(str::to_string));
            json!({
                "id": j["id"], "kind": j["kind"], "state": j["state"], "host": host,
                "total": j["total"], "got": j["got"], "error_code": j["error_code"],
                "error": j["error"].as_str().map(redact_url), "source": j["source"],
            })
        })
        .collect();
    let log: Vec<String> = log_tail.lines().map(redact_url).collect();
    json!({
        "ldm_version": env!("CARGO_PKG_VERSION"),
        "created": chrono::Local::now().to_rfc3339(),
        "os": std::env::consts::OS, "arch": std::env::consts::ARCH,
        "daemon_running": daemon_running,
        "config": redact(config),
        "jobs": jobs,
        "log_tail": log,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_and_queries_are_removed() {
        let cfg = json!({
            "site_logins": [{"host": "h", "username": "bob", "password": "hunter2"}],
            "engine": {"on_complete_command": ["rm", "-rf"], "proxy": "http://u:p@proxy:8080/?k=v", "max_connections": 8},
            "browser": {"cookies": ""},
        });
        let jobs =
            vec![json!({"id": "j", "url": "https://cdn.example/a.mp4?sig=abc", "state": "Failed", "error": "403 at https://x/y?token=t"})];
        let b = bundle(&cfg, &jobs, "GET https://x/y?token=t\nok", true);
        let s = b.to_string();
        for leak in ["hunter2", "bob", "rm", "sig=abc", "token=t", "a.mp4"] {
            assert!(!s.contains(leak), "{leak} leaked: {s}");
        }
        assert_eq!(b["jobs"][0]["host"], "cdn.example");
        assert_eq!(b["config"]["engine"]["max_connections"], 8);
        assert_eq!(b["config"]["browser"]["cookies"], "", "empty values stay as they are");
    }
}
