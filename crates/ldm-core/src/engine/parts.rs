//! Part (segment) planning: initial split and dynamic mid-point splitting
//!.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PartState {
    Pending,
    Active,
    Done,
    Failed,
}

/// `[start, end)` byte range with `got` bytes already written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Part {
    pub idx: u32,
    pub start: u64,
    pub end: u64,
    pub got: u64,
    pub state: PartState,
}

impl Part {
    pub fn len(&self) -> u64 {
        self.end - self.start
    }
    pub fn is_empty(&self) -> bool {
        self.end <= self.start
    }
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.start + self.got)
    }
}

/// Initial plan: `n = min(max_conns, max(1, total / min_part))` equal parts.
pub fn split_plan(total: u64, max_conns: u32, min_part: u64) -> Vec<Part> {
    let min_part = min_part.max(1);
    let n = (max_conns.max(1) as u64).min((total / min_part).max(1));
    let chunk = total / n;
    (0..n)
        .map(|i| {
            let start = i * chunk;
            let end = if i == n - 1 { total } else { start + chunk };
            Part { idx: i as u32, start, end, got: 0, state: PartState::Pending }
        })
        .collect()
}

/// Snapshot used to decide a split. `cursor` is the first byte not yet
/// claimed by a connection (for an active part: start + claimed bytes).
#[derive(Debug, Clone, Copy)]
pub struct Span {
    pub idx: u32,
    pub state: PartState,
    pub cursor: u64,
    pub end: u64,
}

impl Span {
    pub fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.cursor)
    }
}

/// Decide which part to split and where (mid-point of its free span).
///
/// * nothing happens if `Pending + Active >= max_conns`;
/// * the largest **Pending** span is preferred (design rule);
/// * if `allow_active`, the free tail of the largest active span is used
///   otherwise (work stealing; the active connection is truncated safely);
/// * `prefer` names a slow active part that should be split first;
/// * a span is only split if its remaining length is `>= 2 * min_part`.
pub fn choose_split(spans: &[Span], min_part: u64, max_conns: u32, allow_active: bool, prefer: Option<u32>) -> Option<(u32, u64)> {
    let unfinished = spans.iter().filter(|s| matches!(s.state, PartState::Pending | PartState::Active)).count();
    if unfinished >= max_conns as usize {
        return None;
    }
    let need = 2 * min_part.max(1);
    let mid = |s: &Span| s.cursor + s.remaining() / 2;
    if let Some(p) = prefer {
        if let Some(s) = spans.iter().find(|s| s.idx == p && s.state == PartState::Active && s.remaining() >= need) {
            if allow_active {
                return Some((s.idx, mid(s)));
            }
        }
    }
    let best_of = |st: PartState| spans.iter().filter(|s| s.state == st).max_by_key(|s| (s.remaining(), u32::MAX - s.idx));
    if let Some(s) = best_of(PartState::Pending) {
        if s.remaining() >= need {
            return Some((s.idx, mid(s)));
        }
    }
    if allow_active {
        if let Some(s) = best_of(PartState::Active) {
            if s.remaining() >= need {
                return Some((s.idx, mid(s)));
            }
        }
    }
    None
}

/// Apply a split decision: the donor keeps `[start, mid)`, a new pending part gets `[mid, end)`.
pub fn apply_split(parts: &mut Vec<Part>, idx: u32, mid: u64) -> Option<Part> {
    let next = parts.iter().map(|p| p.idx).max().map(|m| m + 1).unwrap_or(0);
    let donor = parts.iter_mut().find(|p| p.idx == idx)?;
    if mid <= donor.start + donor.got || mid >= donor.end {
        return None;
    }
    let new = Part { idx: next, start: mid, end: donor.end, got: 0, state: PartState::Pending };
    donor.end = mid;
    parts.push(new.clone());
    Some(new)
}

/// Convenience wrapper with the doc's `maybe_split` signature (Pending only).
pub fn maybe_split(parts: &mut Vec<Part>, min_part: u64, max_conns: u32) -> Option<Part> {
    let spans: Vec<Span> = parts.iter().map(|p| Span { idx: p.idx, state: p.state, cursor: p.start + p.got, end: p.end }).collect();
    let (idx, mid) = choose_split(&spans, min_part, max_conns, false, None)?;
    apply_split(parts, idx, mid)
}

/// Check that parts cover `[0,total)` exactly with no overlap or gap.
pub fn verify_coverage(parts: &[Part], total: u64) -> Result<(), String> {
    let mut v: Vec<&Part> = parts.iter().collect();
    v.sort_by_key(|p| p.start);
    let mut pos = 0u64;
    for p in v {
        if p.start != pos {
            return Err(format!("part {} starts at {} but expected {}", p.idx, p.start, pos));
        }
        if p.end < p.start {
            return Err(format!("part {} has negative length", p.idx));
        }
        pos = p.end;
    }
    if pos != total {
        return Err(format!("parts end at {pos} but total is {total}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans(parts: &[Part]) -> Vec<Span> {
        parts.iter().map(|p| Span { idx: p.idx, state: p.state, cursor: p.start + p.got, end: p.end }).collect()
    }

    #[test]
    fn initial_plan_covers_and_limits() {
        let p = split_plan(5 * 1024 * 1024, 4, 262144);
        assert_eq!(p.len(), 4);
        verify_coverage(&p, 5 * 1024 * 1024).unwrap();
        // small file: never smaller than min_part
        let p = split_plan(300_000, 8, 262144);
        assert_eq!(p.len(), 1);
        let p = split_plan(1_000_003, 8, 100_000);
        assert_eq!(p.len(), 8);
        verify_coverage(&p, 1_000_003).unwrap();
        assert_eq!(split_plan(0, 8, 1).len(), 1);
    }

    #[test]
    fn no_split_when_span_too_small() {
        let mut p = vec![Part { idx: 0, start: 0, end: 500, got: 0, state: PartState::Pending }];
        assert!(maybe_split(&mut p, 256, 8).is_none());
        let mut p = vec![Part { idx: 0, start: 0, end: 512, got: 0, state: PartState::Pending }];
        let n = maybe_split(&mut p, 256, 8).unwrap();
        assert_eq!((n.start, n.end), (256, 512));
        verify_coverage(&p, 512).unwrap();
    }

    #[test]
    fn respects_connection_cap() {
        let mut p = split_plan(10_000, 2, 100);
        assert_eq!(p.len(), 2);
        assert!(maybe_split(&mut p, 100, 2).is_none());
    }

    #[test]
    fn pending_only_never_touches_active() {
        let mut p = vec![
            Part { idx: 0, start: 0, end: 10_000, got: 10, state: PartState::Active },
            Part { idx: 1, start: 10_000, end: 11_000, got: 0, state: PartState::Done },
        ];
        assert!(maybe_split(&mut p, 100, 8).is_none());
        // with work stealing allowed the active tail is split at its free mid-point
        let d = choose_split(&spans(&p), 100, 8, true, None).unwrap();
        assert_eq!(d, (0, 10 + (10_000 - 10) / 2));
    }

    #[test]
    fn prefers_pending_then_largest() {
        let p = vec![
            Part { idx: 0, start: 0, end: 50_000, got: 0, state: PartState::Active },
            Part { idx: 1, start: 50_000, end: 60_000, got: 0, state: PartState::Pending },
        ];
        assert_eq!(choose_split(&spans(&p), 1000, 8, true, None).unwrap().0, 1);
        // a preferred slow part wins
        assert_eq!(choose_split(&spans(&p), 1000, 8, true, Some(0)).unwrap().0, 0);
    }

    #[test]
    fn repeated_splitting_keeps_invariants() {
        let total = 1_000_000u64;
        let mut parts = split_plan(total, 2, 1000);
        for round in 0..200u64 {
            // simulate progress on every part
            for p in parts.iter_mut() {
                let adv = (p.remaining() / 3).min(997 + round);
                p.got += adv;
                if p.remaining() == 0 {
                    p.state = PartState::Done;
                } else {
                    p.state = PartState::Active;
                }
            }
            let sp = spans(&parts);
            if let Some((i, m)) = choose_split(&sp, 1000, 16, true, None) {
                apply_split(&mut parts, i, m).unwrap();
            }
            verify_coverage(&parts, total).unwrap();
            for p in &parts {
                assert!(p.start + p.got <= p.end, "got beyond end");
            }
        }
        assert!(parts.len() > 2);
    }
}
