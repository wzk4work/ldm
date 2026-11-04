//! Assembling parts into the final file.
use std::io::Write;
use std::path::Path;

use super::parts::{verify_coverage, Part};
use super::state::part_path;
use crate::error::{LdmError, Result};
use crate::util::filename::merging_path;

/// Concatenate parts in `start` order into `dest` via `dest.ldm.merging`,
/// verifying that the parts cover `[0,total)` without gaps.
pub fn merge_parts(parts: &[Part], parts_dir: &Path, dest: &Path, total: u64, fsync: bool) -> Result<()> {
    verify_coverage(parts, total).map_err(LdmError::Gap)?;
    let mut ordered: Vec<&Part> = parts.iter().filter(|p| !p.is_empty()).collect();
    ordered.sort_by_key(|p| p.start);
    for p in &ordered {
        let len = std::fs::metadata(part_path(parts_dir, p.idx)).map(|m| m.len()).unwrap_or(0);
        if len != p.len() {
            return Err(LdmError::Gap(format!("part {} has {} bytes, expected {}", p.idx, len, p.len())));
        }
    }
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = merging_path(dest);
    // fast path: a single part on the same filesystem is simply renamed
    if ordered.len() == 1 && std::fs::rename(part_path(parts_dir, ordered[0].idx), &tmp).is_ok() {
        if fsync {
            std::fs::File::open(&tmp)?.sync_all()?;
        }
    } else {
        let res = (|| -> Result<()> {
            let mut out = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(&tmp)?);
            for p in &ordered {
                let mut f = std::fs::File::open(part_path(parts_dir, p.idx))?;
                std::io::copy(&mut f, &mut out)?;
            }
            out.flush()?;
            let f = out.into_inner().map_err(|e| LdmError::Io(e.into_error()))?;
            if fsync {
                f.sync_all()?;
            }
            Ok(())
        })();
        if let Err(e) = res {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
    }
    if total == 0 && ordered.is_empty() {
        std::fs::File::create(&tmp)?;
    }
    std::fs::rename(&tmp, dest)?;
    Ok(())
}
