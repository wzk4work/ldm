//! SQLite persistence. The `downloads` table holds one row per job
//! (including priority, final_url, last_modified …). `parts` mirrors `job_state.json`
//! for the UI; the JSON file stays the source of truth for resume.
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// Job states plus `Queued` (scheduler waiting)
/// – `Pending` in the diagram.
pub mod state {
    pub const QUEUED: &str = "Queued";
    pub const PROBING: &str = "Probing";
    pub const RUNNING: &str = "Running";
    pub const ASSEMBLING: &str = "Assembling";
    /// Live stream being recorded (progress = recorded time + bytes; no percent / ETA).
    pub const RECORDING: &str = "Recording";
    pub const PAUSED: &str = "Paused";
    pub const COMPLETED: &str = "Completed";
    pub const FAILED: &str = "Failed";
    pub const CANCELED: &str = "Canceled";

    pub fn is_active(s: &str) -> bool {
        matches!(s, PROBING | RUNNING | RECORDING | ASSEMBLING)
    }
    pub fn is_terminal(s: &str) -> bool {
        matches!(s, COMPLETED | FAILED | CANCELED)
    }
}

const MIGRATIONS: &[&str] = &[
    // 1: initial schema
    r#"
CREATE TABLE IF NOT EXISTS settings (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS queues (
  id TEXT PRIMARY KEY,
  name TEXT NOT NULL,
  concurrent INTEGER NOT NULL DEFAULT 3,
  schedule_cron TEXT,
  schedule_json TEXT,
  enabled INTEGER NOT NULL DEFAULT 1
);
CREATE TABLE IF NOT EXISTS downloads (
  id TEXT PRIMARY KEY,
  url TEXT NOT NULL,
  final_url TEXT,
  kind TEXT NOT NULL DEFAULT 'http',
  final_path TEXT,
  filename TEXT,
  save_dir TEXT,
  state TEXT NOT NULL,
  total INTEGER,
  got INTEGER NOT NULL DEFAULT 0,
  etag TEXT,
  last_modified TEXT,
  referrer TEXT,
  page_url TEXT,
  cookies TEXT,
  user_agent TEXT,
  headers_json TEXT,
  mime TEXT,
  category TEXT,
  error_code TEXT,
  error TEXT,
  max_connections INTEGER NOT NULL DEFAULT 8,
  priority INTEGER NOT NULL DEFAULT 0,
  sha256 TEXT,
  preferred_height INTEGER,
  source TEXT,
  job_state_path TEXT,
  queue_id TEXT REFERENCES queues(id),
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS downloads_state ON downloads(state);
CREATE TABLE IF NOT EXISTS parts (
  job_id TEXT NOT NULL,
  idx INTEGER NOT NULL,
  start_off INTEGER NOT NULL,
  end_off INTEGER NOT NULL,
  got INTEGER NOT NULL DEFAULT 0,
  path TEXT NOT NULL,
  state TEXT NOT NULL,
  PRIMARY KEY (job_id, idx)
);
CREATE TABLE IF NOT EXISTS media_candidates (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  tab_id INTEGER NOT NULL,
  url TEXT NOT NULL,
  kind TEXT NOT NULL,
  mime TEXT,
  page_url TEXT,
  last_seen INTEGER NOT NULL,
  UNIQUE(tab_id, url)
);
INSERT OR IGNORE INTO queues(id, name, concurrent, enabled) VALUES ('main', '主队列', 3, 1);
"#,
    // 2: per-job / per-queue extras (description, speed limit, confirm flag,
    // mirrors, post actions …) kept as JSON so the UI can grow without churn
    r#"
ALTER TABLE downloads ADD COLUMN extra_json TEXT;
ALTER TABLE queues ADD COLUMN extra_json TEXT;
"#,
    // 3: ad ranking verdict per media candidate (score / flag / reasons as JSON)
    r#"
ALTER TABLE media_candidates ADD COLUMN ad_json TEXT;
"#,
];

/// Per-job options that the GUI edits (progress dialog, properties, capture
/// confirmation). Stored in `downloads.extra_json`, flattened into the job JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct JobExtra {
    /// Free text shown in the "描述" column.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Per-job speed cap in bytes/s (0 = none).
    #[serde(skip_serializing_if = "is_zero")]
    pub speed_limit_bps: u64,
    /// Last time a transfer was started (ms since epoch) – "最后尝试".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_try_at: Option<i64>,
    /// Captured from the browser and waiting for the user's choice in the
    /// "下载文件信息" dialog.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub awaiting_confirm: bool,
    /// Alternative URLs tried in order when the primary fails.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub mirrors: Vec<String>,
    /// Expected Content-MD5 (base64) or user-supplied md5 hex.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub md5: Option<String>,
    /// Post-completion action for this job: "" | "open" | "folder" | "shutdown" | "command".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub after: String,
    /// Fresh browser cookies were already requested once for the current failure (GetCookies).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cookie_refresh: bool,
    /// File stem taken from the page's video title (media candidates); wins over
    /// Content-Disposition, which then only contributes the extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title_name: Option<String>,
    /// The browser ranked this media as a suspected ad (reasons) – shown in "下载文件信息".
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ad_reasons: Vec<String>,
    /// Kind `av`: the separate audio file merged with the video file at `url`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_url: Option<String>,
    /// Kind `ytdlp`: what yt-dlp is given and, for a fallback, why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ytdlp: Option<YtdlpJob>,
    /// An eligible failure waits for the browser-cookie refresh before falling back.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_pending: Option<String>,
    /// Job log lines (`<ms> <text>`), e.g. the yt-dlp fallback decision.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct YtdlpJob {
    /// Page / video URL handed to yt-dlp.
    pub url: String,
    /// Fallback: the LDM kind that failed (`hls`, `http`, …); `None` = routed directly.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at: Option<i64>,
    /// Where the yt-dlp of the last run came from (`user` / `managed` / `system`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin_path: Option<String>,
    /// The managed yt-dlp was updated once because of an outdated-extractor error.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub update_retried: bool,
}

fn is_zero(v: &u64) -> bool {
    *v == 0
}

/// Queue options beyond the basic columns.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct QueueExtra {
    /// Action when the queue drains: "" | "shutdown" | "command".
    #[serde(skip_serializing_if = "String::is_empty")]
    pub on_done: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub on_done_command: Vec<String>,
    /// Started/stopped by the user ("开始队列"/"停止队列").
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub manual: bool,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobRow {
    pub id: String,
    pub url: String,
    pub final_url: Option<String>,
    pub kind: String,
    pub final_path: Option<String>,
    pub filename: Option<String>,
    pub save_dir: Option<String>,
    pub state: String,
    pub total: Option<u64>,
    pub got: u64,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub referrer: Option<String>,
    pub page_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cookies: Option<String>,
    pub user_agent: Option<String>,
    pub headers: Vec<(String, String)>,
    pub mime: Option<String>,
    pub category: Option<String>,
    pub error_code: Option<String>,
    pub error: Option<String>,
    pub max_connections: u32,
    pub priority: i64,
    pub sha256: Option<String>,
    pub preferred_height: Option<u32>,
    pub source: Option<String>,
    pub job_state_path: Option<String>,
    pub queue_id: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
    #[serde(flatten)]
    pub extra: JobExtra,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct QueueRow {
    pub id: String,
    pub name: String,
    pub concurrent: u32,
    /// Optional time window, e.g. `{"start":"01:00","end":"07:00","days":[1,2,3,4,5]}`.
    pub schedule: Option<serde_json::Value>,
    pub enabled: bool,
    #[serde(flatten, default)]
    pub extra: QueueExtra,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct PartRow {
    pub idx: u32,
    pub start: u64,
    pub end: i64,
    pub got: u64,
    pub path: String,
    pub state: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CandidateRow {
    pub tab_id: i64,
    pub url: String,
    pub kind: String,
    pub mime: Option<String>,
    pub page_url: Option<String>,
    pub last_seen: i64,
    /// "main video vs ad" verdict (see media::adscore); None = not scored
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ad: Option<crate::media::adscore::AdVerdict>,
}

pub struct Store {
    conn: Mutex<Connection>,
}

const JOB_COLS: &str = "id,url,final_url,kind,final_path,filename,save_dir,state,total,got,etag,last_modified,referrer,page_url,cookies,user_agent,headers_json,mime,category,error_code,error,max_connections,priority,sha256,preferred_height,source,job_state_path,queue_id,created_at,updated_at,finished_at,extra_json";

fn job_from_row(r: &Row) -> rusqlite::Result<JobRow> {
    let headers: Option<String> = r.get(16)?;
    Ok(JobRow {
        id: r.get(0)?,
        url: r.get(1)?,
        final_url: r.get(2)?,
        kind: r.get(3)?,
        final_path: r.get(4)?,
        filename: r.get(5)?,
        save_dir: r.get(6)?,
        state: r.get(7)?,
        total: r.get::<_, Option<i64>>(8)?.map(|v| v as u64),
        got: r.get::<_, i64>(9)? as u64,
        etag: r.get(10)?,
        last_modified: r.get(11)?,
        referrer: r.get(12)?,
        page_url: r.get(13)?,
        cookies: r.get(14)?,
        user_agent: r.get(15)?,
        headers: headers.and_then(|h| serde_json::from_str(&h).ok()).unwrap_or_default(),
        mime: r.get(17)?,
        category: r.get(18)?,
        error_code: r.get(19)?,
        error: r.get(20)?,
        max_connections: r.get::<_, i64>(21)? as u32,
        priority: r.get(22)?,
        sha256: r.get(23)?,
        preferred_height: r.get::<_, Option<i64>>(24)?.map(|v| v as u32),
        source: r.get(25)?,
        job_state_path: r.get(26)?,
        queue_id: r.get::<_, Option<String>>(27)?.unwrap_or_else(|| "main".into()),
        created_at: r.get(28)?,
        updated_at: r.get(29)?,
        finished_at: r.get(30)?,
        extra: r.get::<_, Option<String>>(31)?.and_then(|e| serde_json::from_str(&e).ok()).unwrap_or_default(),
    })
}

impl Store {
    pub fn open(path: &Path) -> Result<Store> {
        if let Some(d) = path.parent() {
            crate::paths::ensure_private_dir(d)?;
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_memory() -> Result<Store> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch("PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;")?;
        let ver: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        for (i, m) in MIGRATIONS.iter().enumerate().skip(ver as usize) {
            conn.execute_batch(&format!("BEGIN; {m}; PRAGMA user_version={}; COMMIT;", i + 1))?;
        }
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn schema_version(&self) -> Result<i64> {
        Ok(self.conn.lock().unwrap().query_row("PRAGMA user_version", [], |r| r.get(0))?)
    }

    pub fn upsert_job(&self, j: &JobRow) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let headers = serde_json::to_string(&j.headers).unwrap_or_else(|_| "[]".into());
        c.execute(
            &format!("INSERT OR REPLACE INTO downloads({JOB_COLS}) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29,?30,?31,?32)"),
            params![
                j.id, j.url, j.final_url, j.kind, j.final_path, j.filename, j.save_dir, j.state,
                j.total.map(|v| v as i64), j.got as i64, j.etag, j.last_modified, j.referrer, j.page_url,
                j.cookies, j.user_agent, headers, j.mime, j.category, j.error_code, j.error,
                j.max_connections as i64, j.priority, j.sha256, j.preferred_height.map(|v| v as i64),
                j.source, j.job_state_path, j.queue_id, j.created_at, j.updated_at, j.finished_at,
                serde_json::to_string(&j.extra).ok()
            ],
        )?;
        Ok(())
    }

    pub fn get_job(&self, id: &str) -> Result<Option<JobRow>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(&format!("SELECT {JOB_COLS} FROM downloads WHERE id=?1"), [id], job_from_row).optional()?)
    }

    /// Jobs ordered for display (newest first) or, with `scheduling`, by
    /// priority then age.
    pub fn list_jobs(&self, state: Option<&str>) -> Result<Vec<JobRow>> {
        let c = self.conn.lock().unwrap();
        let mut out = vec![];
        match state {
            Some(s) => {
                let mut st = c.prepare(&format!(
                    "SELECT {JOB_COLS} FROM downloads WHERE state=?1 ORDER BY priority DESC, created_at ASC, rowid ASC"
                ))?;
                for r in st.query_map([s], job_from_row)? {
                    out.push(r?);
                }
            }
            None => {
                let mut st = c.prepare(&format!("SELECT {JOB_COLS} FROM downloads ORDER BY created_at ASC, rowid ASC"))?;
                for r in st.query_map([], job_from_row)? {
                    out.push(r?);
                }
            }
        }
        Ok(out)
    }

    pub fn find_by_url(&self, url: &str) -> Result<Vec<JobRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare(&format!("SELECT {JOB_COLS} FROM downloads WHERE url=?1 ORDER BY created_at DESC"))?;
        let rows = st.query_map([url], job_from_row)?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn update_progress(&self, id: &str, got: u64, total: Option<u64>) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE downloads SET got=?2, total=COALESCE(?3,total), updated_at=?4 WHERE id=?1",
            params![id, got as i64, total.map(|v| v as i64), now_ms()],
        )?;
        Ok(())
    }

    pub fn set_extra(&self, id: &str, extra: &JobExtra) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("UPDATE downloads SET extra_json=?2 WHERE id=?1", params![id, serde_json::to_string(extra).ok()])?;
        Ok(())
    }

    pub fn set_state(&self, id: &str, state: &str, err: Option<(&str, &str)>) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let fin = if state::is_terminal(state) { Some(now_ms()) } else { None };
        c.execute(
            "UPDATE downloads SET state=?2, error_code=?3, error=?4, updated_at=?5, finished_at=COALESCE(?6, finished_at) WHERE id=?1",
            params![id, state, err.map(|e| e.0), err.map(|e| e.1), now_ms(), fin],
        )?;
        Ok(())
    }

    pub fn delete_job(&self, id: &str) -> Result<bool> {
        let c = self.conn.lock().unwrap();
        c.execute("DELETE FROM parts WHERE job_id=?1", [id])?;
        Ok(c.execute("DELETE FROM downloads WHERE id=?1", [id])? > 0)
    }

    /// Crash recovery: anything that was active becomes Queued.
    pub fn recover_interrupted(&self) -> Result<usize> {
        let c = self.conn.lock().unwrap();
        Ok(c.execute("UPDATE downloads SET state='Queued' WHERE state IN ('Probing','Running','Recording','Assembling')", [])?)
    }

    pub fn save_parts(&self, job_id: &str, parts: &[PartRow]) -> Result<()> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute("DELETE FROM parts WHERE job_id=?1", [job_id])?;
        for p in parts {
            tx.execute(
                "INSERT INTO parts(job_id,idx,start_off,end_off,got,path,state) VALUES (?1,?2,?3,?4,?5,?6,?7)",
                params![job_id, p.idx, p.start as i64, p.end, p.got as i64, p.path, p.state],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn load_parts(&self, job_id: &str) -> Result<Vec<PartRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT idx,start_off,end_off,got,path,state FROM parts WHERE job_id=?1 ORDER BY idx")?;
        let rows = st
            .query_map([job_id], |r| {
                Ok(PartRow {
                    idx: r.get::<_, i64>(0)? as u32,
                    start: r.get::<_, i64>(1)? as u64,
                    end: r.get(2)?,
                    got: r.get::<_, i64>(3)? as u64,
                    path: r.get(4)?,
                    state: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn list_queues(&self) -> Result<Vec<QueueRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT id,name,concurrent,schedule_json,enabled,extra_json FROM queues ORDER BY id")?;
        let rows = st
            .query_map([], |r| {
                let sched: Option<String> = r.get(3)?;
                Ok(QueueRow {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    concurrent: r.get::<_, i64>(2)? as u32,
                    schedule: sched.and_then(|s| serde_json::from_str(&s).ok()),
                    enabled: r.get::<_, i64>(4)? != 0,
                    extra: r.get::<_, Option<String>>(5)?.and_then(|e| serde_json::from_str(&e).ok()).unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn upsert_queue(&self, q: &QueueRow) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT OR REPLACE INTO queues(id,name,concurrent,schedule_json,enabled,extra_json) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                q.id,
                q.name,
                q.concurrent.max(1) as i64,
                q.schedule.as_ref().map(|s| s.to_string()),
                q.enabled as i64,
                serde_json::to_string(&q.extra).ok()
            ],
        )?;
        Ok(())
    }

    /// Delete a queue; its jobs move to `main`. The main queue cannot be deleted.
    pub fn delete_queue(&self, id: &str) -> Result<usize> {
        let c = self.conn.lock().unwrap();
        let moved = c.execute("UPDATE downloads SET queue_id='main' WHERE queue_id=?1", [id])?;
        c.execute("DELETE FROM queues WHERE id=?1 AND id<>'main'", [id])?;
        Ok(moved)
    }

    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row("SELECT value FROM settings WHERE key=?1", [key], |r| r.get(0)).optional()?)
    }

    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute("INSERT OR REPLACE INTO settings(key,value) VALUES (?1,?2)", params![key, value])?;
        Ok(())
    }

    pub fn upsert_candidate(&self, cand: &CandidateRow) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO media_candidates(tab_id,url,kind,mime,page_url,last_seen,ad_json) VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(tab_id,url) DO UPDATE SET kind=excluded.kind, mime=COALESCE(excluded.mime, mime), page_url=COALESCE(excluded.page_url, page_url), last_seen=excluded.last_seen, ad_json=COALESCE(excluded.ad_json, ad_json)",
            params![cand.tab_id, cand.url, cand.kind, cand.mime, cand.page_url, cand.last_seen, cand.ad.as_ref().and_then(|a| serde_json::to_string(a).ok())],
        )?;
        Ok(())
    }

    pub fn candidates(&self, tab_id: Option<i64>, newer_than: i64) -> Result<Vec<CandidateRow>> {
        let c = self.conn.lock().unwrap();
        let mut st = c.prepare("SELECT tab_id,url,kind,mime,page_url,last_seen,ad_json FROM media_candidates WHERE (?1 IS NULL OR tab_id=?1) AND last_seen>=?2 ORDER BY last_seen DESC")?;
        let rows = st
            .query_map(params![tab_id, newer_than], |r| {
                Ok(CandidateRow {
                    tab_id: r.get(0)?,
                    url: r.get(1)?,
                    kind: r.get(2)?,
                    mime: r.get(3)?,
                    page_url: r.get(4)?,
                    last_seen: r.get(5)?,
                    ad: r.get::<_, Option<String>>(6)?.and_then(|s| serde_json::from_str(&s).ok()),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn prune_candidates(&self, older_than: i64) -> Result<usize> {
        let c = self.conn.lock().unwrap();
        Ok(c.execute("DELETE FROM media_candidates WHERE last_seen<?1", [older_than])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, prio: i64, created: i64) -> JobRow {
        JobRow {
            id: id.into(),
            url: format!("http://h/{id}"),
            kind: "http".into(),
            state: state::QUEUED.into(),
            max_connections: 8,
            priority: prio,
            queue_id: "main".into(),
            created_at: created,
            updated_at: created,
            headers: vec![("X-A".into(), "1".into())],
            ..Default::default()
        }
    }

    #[test]
    fn roundtrip_and_ordering() {
        let s = Store::open_memory().unwrap();
        assert_eq!(s.schema_version().unwrap(), 3, "migrations 2 (extra_json) and 3 (ad_json) applied");
        s.upsert_job(&job("a", 0, 1)).unwrap();
        s.upsert_job(&job("b", 5, 2)).unwrap();
        s.upsert_job(&job("c", 0, 0)).unwrap();
        let q: Vec<String> = s.list_jobs(Some(state::QUEUED)).unwrap().into_iter().map(|j| j.id).collect();
        assert_eq!(q, ["b", "c", "a"]);
        let a = s.get_job("a").unwrap().unwrap();
        assert_eq!(a.headers, vec![("X-A".to_string(), "1".to_string())]);
        s.update_progress("a", 10, Some(100)).unwrap();
        s.set_state("a", state::RUNNING, None).unwrap();
        assert_eq!(s.recover_interrupted().unwrap(), 1);
        let a = s.get_job("a").unwrap().unwrap();
        assert_eq!((a.state.as_str(), a.got, a.total), ("Queued", 10, Some(100)));
        s.set_state("a", state::FAILED, Some(("E_HTTP_STATUS", "404"))).unwrap();
        let a = s.get_job("a").unwrap().unwrap();
        assert_eq!(a.error_code.as_deref(), Some("E_HTTP_STATUS"));
        assert!(a.finished_at.is_some());
        assert!(s.delete_job("a").unwrap());
        assert!(s.get_job("a").unwrap().is_none());
    }

    #[test]
    fn queues_parts_settings_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("db/downloads.db");
        {
            let s = Store::open(&p).unwrap();
            assert_eq!(s.list_queues().unwrap()[0].id, "main");
            s.upsert_queue(&QueueRow {
                id: "night".into(),
                name: "夜间".into(),
                concurrent: 1,
                schedule: Some(serde_json::json!({"start":"01:00","end":"07:00"})),
                enabled: true,
                extra: QueueExtra { on_done: "shutdown".into(), ..Default::default() },
            })
            .unwrap();
            let mut jx = JobExtra { description: Some("说明".into()), mirrors: vec!["http://m/".into()], ..Default::default() };
            s.upsert_job(&job("x", 0, 0)).unwrap();
            s.set_extra("x", &jx).unwrap();
            jx.after = "open".into();
            s.set_extra("x", &jx).unwrap();
            s.upsert_queue(&QueueRow {
                id: "tmp".into(),
                name: "t".into(),
                concurrent: 1,
                schedule: None,
                enabled: true,
                extra: Default::default(),
            })
            .unwrap();
            let mut moved = job("y", 0, 0);
            moved.queue_id = "tmp".into();
            s.upsert_job(&moved).unwrap();
            assert_eq!(s.delete_queue("tmp").unwrap(), 1);
            assert_eq!(s.get_job("y").unwrap().unwrap().queue_id, "main");
            s.save_parts("j", &[PartRow { idx: 0, start: 0, end: 10, got: 5, path: "p0".into(), state: "Active".into() }]).unwrap();
            s.set_setting("k", "v").unwrap();
            s.upsert_candidate(&CandidateRow {
                tab_id: 1,
                url: "http://h/a.m3u8".into(),
                kind: "hls".into(),
                mime: None,
                page_url: Some("http://h/".into()),
                last_seen: 10,
                ad: None,
            })
            .unwrap();
            s.upsert_candidate(&CandidateRow {
                tab_id: 1,
                url: "http://h/a.m3u8".into(),
                kind: "hls".into(),
                mime: Some("application/x-mpegurl".into()),
                page_url: None,
                last_seen: 20,
                ad: None,
            })
            .unwrap();
        }
        // reopen: migrations are idempotent and data persists
        let s = Store::open(&p).unwrap();
        assert_eq!(s.list_queues().unwrap().len(), 2);
        assert_eq!(s.list_queues().unwrap()[1].extra.on_done, "shutdown");
        let x = s.get_job("x").unwrap().unwrap();
        assert_eq!((x.extra.description.as_deref(), x.extra.after.as_str()), (Some("说明"), "open"));
        assert_eq!(x.extra.mirrors, ["http://m/"]);
        assert_eq!(s.load_parts("j").unwrap()[0].got, 5);
        assert_eq!(s.get_setting("k").unwrap().as_deref(), Some("v"));
        let c = s.candidates(Some(1), 0).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(
            (c[0].last_seen, c[0].mime.as_deref(), c[0].page_url.as_deref()),
            (20, Some("application/x-mpegurl"), Some("http://h/"))
        );
        assert_eq!(s.prune_candidates(30).unwrap(), 1);
    }
}
