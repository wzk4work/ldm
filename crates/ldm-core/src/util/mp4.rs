//! Minimal ISO-BMFF sanity check (original work): does a finished `.mp4` have a movie header?
//! A capture of one byte range of a segmented stream (`moof`/`mdat` only) downloads "fine" but
//! is not playable; such a result is reported as "not media" (eligible for the yt-dlp fallback).
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const KNOWN: &[&[u8; 4]] = &[
    b"ftyp", b"styp", b"moov", b"moof", b"mdat", b"sidx", b"free", b"skip", b"wide", b"pdin", b"uuid", b"emsg", b"prft", b"meta", b"mfra",
];

/// `Some(true)`: MP4 with `moov`; `Some(false)`: ISO-BMFF boxes but no `moov` (a fragment);
/// `None`: not recognisable as ISO-BMFF (no judgement).
pub fn has_movie_header(path: &Path) -> Option<bool> {
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let mut pos = 0u64;
    let mut boxes = 0;
    while pos + 8 <= len && boxes < 100_000 {
        let mut h = [0u8; 16];
        f.seek(SeekFrom::Start(pos)).ok()?;
        f.read_exact(&mut h[..8]).ok()?;
        let mut size = u32::from_be_bytes([h[0], h[1], h[2], h[3]]) as u64;
        let typ: [u8; 4] = [h[4], h[5], h[6], h[7]];
        if boxes == 0 && !KNOWN.iter().any(|k| **k == typ) {
            return None;
        }
        if &typ == b"moov" {
            return Some(true);
        }
        if size == 1 {
            f.read_exact(&mut h[8..16]).ok()?;
            size = u64::from_be_bytes([h[8], h[9], h[10], h[11], h[12], h[13], h[14], h[15]]);
        } else if size == 0 {
            size = len - pos;
        }
        if size < 8 {
            break;
        }
        pos += size;
        boxes += 1;
    }
    (boxes > 0).then_some(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bx(t: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(t);
        v.extend_from_slice(body);
        v
    }

    #[test]
    fn fragments_vs_complete_files() {
        let d = tempfile::tempdir().unwrap();
        let w = |n: &str, parts: &[Vec<u8>]| {
            let p = d.path().join(n);
            std::fs::write(&p, parts.concat()).unwrap();
            p
        };
        // faststart and moov-at-end files
        assert_eq!(has_movie_header(&w("a.mp4", &[bx(b"ftyp", b"isom0000"), bx(b"moov", &[0; 20]), bx(b"mdat", &[1; 100])])), Some(true));
        assert_eq!(has_movie_header(&w("b.mp4", &[bx(b"ftyp", b"isom0000"), bx(b"mdat", &[1; 5000]), bx(b"moov", &[0; 20])])), Some(true));
        // one range of a DASH/fMP4 stream
        assert_eq!(has_movie_header(&w("c.mp4", &[bx(b"moof", &[0; 30]), bx(b"mdat", &[1; 500])])), Some(false));
        assert_eq!(has_movie_header(&w("d.mp4", &[bx(b"sidx", &[0; 30]), bx(b"moof", &[0; 30]), bx(b"mdat", &[1; 50])])), Some(false));
        // mid-box cut (no box header at offset 0) and other formats: no judgement
        assert_eq!(has_movie_header(&w("e.mp4", &[vec![7u8; 300]])), None);
        assert_eq!(has_movie_header(&w("f.webm", &[vec![0x1a, 0x45, 0xdf, 0xa3, 0, 0, 0, 0]])), None);
        assert_eq!(has_movie_header(&w("g.mp4", &[])), None);
    }
}
