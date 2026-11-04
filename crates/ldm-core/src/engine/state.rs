//! Resume state `job_state.json`.
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::parts::{Part, PartState};
use crate::error::Result;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct JobState {
    pub url: String,
    #[serde(default)]
    pub final_url: Option<String>,
    pub total: Option<u64>,
    pub range_ok: bool,
    #[serde(default)]
    pub etag: Option<String>,
    #[serde(default)]
    pub last_modified: Option<String>,
    #[serde(default)]
    pub filename: Option<String>,
    #[serde(default)]
    pub mime: Option<String>,
    pub parts: Vec<Part>,
}

pub fn state_path(parts_dir: &Path) -> PathBuf {
    parts_dir.join("job_state.json")
}

pub fn part_path(parts_dir: &Path, idx: u32) -> PathBuf {
    parts_dir.join(format!("part_{idx:04}.bin"))
}

pub fn load(parts_dir: &Path) -> Option<JobState> {
    let t = std::fs::read_to_string(state_path(parts_dir)).ok()?;
    serde_json::from_str(&t).ok()
}

/// Atomic save (write temp + rename).
pub fn save(parts_dir: &Path, st: &JobState) -> Result<()> {
    let p = state_path(parts_dir);
    let tmp = parts_dir.join("job_state.json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(st)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

/// After a crash the persisted `got` may disagree with the files on disk:
/// trust the on-disk length.
pub fn reconcile_with_disk(parts_dir: &Path, st: &mut JobState) {
    for p in st.parts.iter_mut() {
        let on_disk = std::fs::metadata(part_path(parts_dir, p.idx)).map(|m| m.len()).unwrap_or(0);
        let len = p.end - p.start;
        p.got = on_disk.min(len);
        p.state = if p.got == len { PartState::Done } else { PartState::Pending };
    }
}
