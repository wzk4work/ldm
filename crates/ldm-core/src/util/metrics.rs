//! Prometheus text exposition of the daemon status.
//! Printed by `ldm metrics`; point node_exporter's textfile collector at the output to scrape it.
//! LDM itself opens no metrics port.

use serde_json::Value;

pub fn prometheus(status: &Value) -> String {
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, v: f64, labels: &str| {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name}{labels} {v}\n"));
    };
    let n = |k: &str| status.get(k).and_then(|v| v.as_f64()).unwrap_or(0.0);
    gauge("ldm_up", "1 when the daemon answered", 1.0, &format!("{{version=\"{}\"}}", status["version"].as_str().unwrap_or("")));
    gauge("ldm_uptime_seconds", "Daemon uptime", n("uptimeSecs"), "");
    gauge("ldm_download_speed_bytes", "Current total download speed (bytes/s)", n("speedBps"), "");
    for (k, st) in [("active", "running"), ("queued", "queued"), ("paused", "paused"), ("completed", "completed"), ("failed", "failed")] {
        out.push_str(&format!("ldm_jobs{{state=\"{st}\"}} {}\n", n(k)));
    }
    out.insert_str(out.find("ldm_jobs{").unwrap_or(out.len()), "# HELP ldm_jobs Jobs by state\n# TYPE ldm_jobs gauge\n");
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn exposition_format() {
        let s = super::prometheus(&serde_json::json!({"version":"0.9.0","uptimeSecs":5,"speedBps":1024.5,"active":2,"failed":1}));
        assert!(s.contains("ldm_up{version=\"0.9.0\"} 1\n"));
        assert!(s.contains("ldm_download_speed_bytes 1024.5\n"));
        assert!(s.contains("# TYPE ldm_jobs gauge\nldm_jobs{state=\"running\"} 2\n"));
        assert!(s.contains("ldm_jobs{state=\"failed\"} 1\n") && s.contains("ldm_jobs{state=\"queued\"} 0\n"));
    }
}
