//! MPEG-DASH MPD parser (ISO/IEC 23009-1 subset) – own implementation
//!.
use crate::error::{LdmError, Result};
use crate::media::hls::resolve;

/// A segment reference; `range` is an inclusive byte range.
#[derive(Debug, Clone, PartialEq)]
pub struct SegRef {
    pub url: String,
    pub range: Option<(u64, u64)>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Representation {
    pub id: String,
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
    pub mime: Option<String>,
    pub init: Option<SegRef>,
    pub segments: Vec<SegRef>,
}

impl Representation {
    pub fn label(&self) -> String {
        let mut s = match self.height {
            Some(h) => format!("{h}p"),
            None => "audio".to_string(),
        };
        s.push_str(&format!(" · {} kbps", self.bandwidth / 1000));
        if let Some(c) = &self.codecs {
            s.push_str(&format!(" · {c}"));
        }
        s
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct AdaptationSet {
    /// `video` | `audio` | `text` | other
    pub content_type: String,
    pub lang: Option<String>,
    pub protected: bool,
    pub representations: Vec<Representation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Period {
    pub duration: Option<f64>,
    pub adaptation_sets: Vec<AdaptationSet>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Mpd {
    pub dynamic: bool,
    pub duration: Option<f64>,
    pub periods: Vec<Period>,
}

/// ISO-8601 duration (`PT1H2M3.5S`, `P1DT2H`) → seconds.
pub fn parse_duration(s: &str) -> Option<f64> {
    let s = s.trim().strip_prefix('P')?;
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, t),
        None => (s, ""),
    };
    let mut total = 0.0;
    let mut num = String::new();
    for c in date.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().ok()?;
            num.clear();
            total += v * match c {
                'Y' => 365.0 * 86400.0,
                'M' => 30.0 * 86400.0,
                'W' => 7.0 * 86400.0,
                'D' => 86400.0,
                _ => return None,
            };
        }
    }
    for c in time.chars() {
        if c.is_ascii_digit() || c == '.' {
            num.push(c);
        } else {
            let v: f64 = num.parse().ok()?;
            num.clear();
            total += v * match c {
                'H' => 3600.0,
                'M' => 60.0,
                'S' => 1.0,
                _ => return None,
            };
        }
    }
    Some(total)
}

/// Expand `$RepresentationID$`, `$Bandwidth$`, `$Number$`, `$Number%05d$`, `$Time$`, `$$`.
pub fn expand_template(t: &str, rep_id: &str, bandwidth: u64, number: Option<u64>, time: Option<u64>) -> String {
    let mut out = String::new();
    let mut rest = t;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        let Some(j) = after.find('$') else {
            out.push_str(&rest[i..]);
            return out;
        };
        let ident = &after[..j];
        rest = &after[j + 1..];
        if ident.is_empty() {
            out.push('$');
            continue;
        }
        let (name, fmt) = match ident.split_once('%') {
            Some((n, f)) => (n, Some(f)),
            None => (ident, None),
        };
        let val: Option<String> = match name {
            "RepresentationID" => Some(rep_id.to_string()),
            "Bandwidth" => Some(bandwidth.to_string()),
            "Number" => number.map(|n| n.to_string()),
            "Time" => time.map(|n| n.to_string()),
            _ => None,
        };
        match val {
            Some(v) => {
                let width = fmt
                    .and_then(|f| f.strip_suffix('d'))
                    .map(|f| f.trim_start_matches('0'))
                    .and_then(|w| w.parse::<usize>().ok())
                    .unwrap_or(0);
                if v.len() < width && v.chars().all(|c| c.is_ascii_digit()) {
                    out.push_str(&"0".repeat(width - v.len()));
                }
                out.push_str(&v);
            }
            None => {
                out.push('$');
                out.push_str(ident);
                out.push('$');
            }
        }
    }
    out.push_str(rest);
    out
}

#[derive(Debug, Clone, Default)]
struct Template {
    media: Option<String>,
    init: Option<String>,
    start_number: Option<u64>,
    timescale: Option<u64>,
    duration: Option<u64>,
    /// (t, d, r)
    timeline: Option<Vec<(Option<u64>, u64, i64)>>,
}

impl Template {
    fn merged(&self, child: &Template) -> Template {
        Template {
            media: child.media.clone().or(self.media.clone()),
            init: child.init.clone().or(self.init.clone()),
            start_number: child.start_number.or(self.start_number),
            timescale: child.timescale.or(self.timescale),
            duration: child.duration.or(self.duration),
            timeline: child.timeline.clone().or(self.timeline.clone()),
        }
    }
}

fn child<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &str) -> Option<roxmltree::Node<'a, 'i>> {
    n.children().find(|c| c.is_element() && c.tag_name().name() == name)
}

fn children<'a, 'i>(n: roxmltree::Node<'a, 'i>, name: &'static str) -> impl Iterator<Item = roxmltree::Node<'a, 'i>> {
    n.children().filter(move |c| c.is_element() && c.tag_name().name() == name)
}

fn base_of(n: roxmltree::Node, parent: &str) -> String {
    match child(n, "BaseURL").and_then(|b| b.text()) {
        Some(t) => resolve(parent, t.trim()),
        None => parent.to_string(),
    }
}

fn parse_range(s: &str) -> Option<(u64, u64)> {
    let (a, b) = s.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

fn read_template(n: Option<roxmltree::Node>) -> Template {
    let Some(n) = n else { return Template::default() };
    let num = |a: &str| n.attribute(a).and_then(|v| v.parse::<u64>().ok());
    let timeline = child(n, "SegmentTimeline").map(|tl| {
        children(tl, "S")
            .map(|s| {
                (
                    s.attribute("t").and_then(|v| v.parse().ok()),
                    s.attribute("d").and_then(|v| v.parse().ok()).unwrap_or(0),
                    s.attribute("r").and_then(|v| v.parse().ok()).unwrap_or(0),
                )
            })
            .collect()
    });
    Template {
        media: n.attribute("media").map(String::from),
        init: n.attribute("initialization").map(String::from),
        start_number: num("startNumber"),
        timescale: num("timescale"),
        duration: num("duration"),
        timeline,
    }
}

/// Expand a SegmentTimeline: every `<S d r>` contributes `r+1` segments;
/// `r = -1` repeats until the end of the period.
pub fn timeline_entries(tl: &[(Option<u64>, u64, i64)], period_ticks: Option<u64>) -> Vec<u64> {
    let mut out = Vec::new();
    let mut t = 0u64;
    for (i, &(st, d, r)) in tl.iter().enumerate() {
        if let Some(st) = st {
            t = st;
        }
        if d == 0 {
            continue;
        }
        let reps: u64 = if r >= 0 {
            r as u64 + 1
        } else {
            let end = tl.get(i + 1).and_then(|n| n.0).or(period_ticks).unwrap_or(t + d);
            end.saturating_sub(t).div_ceil(d)
        };
        for _ in 0..reps {
            out.push(t);
            t += d;
        }
    }
    out
}

pub fn parse(xml: &str, mpd_url: &str) -> Result<Mpd> {
    let doc = roxmltree::Document::parse(xml).map_err(|e| LdmError::Format(format!("MPD XML: {e}")))?;
    let root = doc.root_element();
    if root.tag_name().name() != "MPD" {
        return Err(LdmError::Format("root element is not MPD".into()));
    }
    let dynamic = root.attribute("type") == Some("dynamic");
    // live: segments exist up to "now" (availabilityStartTime + elapsed)
    let live_elapsed = root
        .attribute("availabilityStartTime")
        .filter(|_| dynamic)
        .and_then(|a| chrono::DateTime::parse_from_rfc3339(a).ok())
        .map(|a| (chrono::Utc::now() - a.with_timezone(&chrono::Utc)).num_milliseconds() as f64 / 1000.0);
    let mpd_dur = root.attribute("mediaPresentationDuration").and_then(parse_duration);
    // live window for $Number$/@duration templates: only the segments still in the time-shift buffer
    // (a 1970 availabilityStartTime would otherwise mean ~10^9 segment URLs)
    let live_window = dynamic.then(|| root.attribute("timeShiftBufferDepth").and_then(parse_duration).unwrap_or(LIVE_DEFAULT_WINDOW));
    let base0 = base_of(root, mpd_url);
    let mut periods = Vec::new();
    let period_nodes: Vec<_> = children(root, "Period").collect();
    for (pi, pn) in period_nodes.iter().enumerate() {
        let pbase = base_of(*pn, &base0);
        let pstart = pn.attribute("start").and_then(parse_duration).unwrap_or(0.0);
        let pdur = pn
            .attribute("duration")
            .and_then(parse_duration)
            .or(if period_nodes.len() == 1 || pi + 1 == period_nodes.len() { mpd_dur.map(|d| d - pstart) } else { None })
            .or(live_elapsed.map(|e| (e - pstart).max(0.0)));
        let ptmpl = read_template(child(*pn, "SegmentTemplate"));
        let mut sets = Vec::new();
        for an in children(*pn, "AdaptationSet") {
            let abase = base_of(an, &pbase);
            let atmpl = ptmpl.merged(&read_template(child(an, "SegmentTemplate")));
            let amime = an.attribute("mimeType").map(String::from);
            let protected = child(an, "ContentProtection").is_some();
            let mut ctype = an.attribute("contentType").map(String::from);
            let mut reps = Vec::new();
            let mut rep_protected = false;
            for rn in children(an, "Representation") {
                rep_protected |= child(rn, "ContentProtection").is_some();
                let id = rn.attribute("id").unwrap_or("").to_string();
                let bandwidth = rn.attribute("bandwidth").and_then(|v| v.parse().ok()).unwrap_or(0);
                let mime = rn.attribute("mimeType").map(String::from).or(amime.clone());
                if ctype.is_none() {
                    ctype = mime.as_deref().and_then(|m| m.split('/').next()).map(String::from);
                }
                let rbase = base_of(rn, &abase);
                let tmpl = atmpl.merged(&read_template(child(rn, "SegmentTemplate")));
                let (init, segments) = if tmpl.media.is_some() {
                    expand_rep_template(&tmpl, &id, bandwidth, &rbase, pdur, live_window)?
                } else if let Some(sl) = child(rn, "SegmentList").or_else(|| child(an, "SegmentList")) {
                    let init = child(sl, "Initialization").map(|i| SegRef {
                        url: i.attribute("sourceURL").map(|u| resolve(&rbase, u)).unwrap_or_else(|| rbase.clone()),
                        range: i.attribute("range").and_then(parse_range),
                    });
                    let segs = children(sl, "SegmentURL")
                        .map(|s| SegRef {
                            url: s.attribute("media").map(|u| resolve(&rbase, u)).unwrap_or_else(|| rbase.clone()),
                            range: s.attribute("mediaRange").and_then(parse_range),
                        })
                        .collect();
                    (init, segs)
                } else {
                    // SegmentBase or plain BaseURL: one self-contained file
                    (None, vec![SegRef { url: rbase.clone(), range: None }])
                };
                reps.push(Representation {
                    id,
                    bandwidth,
                    width: rn.attribute("width").and_then(|v| v.parse().ok()),
                    height: rn.attribute("height").and_then(|v| v.parse().ok()),
                    codecs: rn.attribute("codecs").or(an.attribute("codecs")).map(String::from),
                    mime,
                    init,
                    segments,
                });
            }
            sets.push(AdaptationSet {
                content_type: ctype.unwrap_or_else(|| "unknown".into()),
                lang: an.attribute("lang").map(String::from),
                protected: protected || rep_protected,
                representations: reps,
            });
        }
        periods.push(Period { duration: pdur, adaptation_sets: sets });
    }
    Ok(Mpd { dynamic, duration: mpd_dur, periods })
}

/// Time-shift window assumed for a dynamic MPD without `timeShiftBufferDepth`.
const LIVE_DEFAULT_WINDOW: f64 = 120.0;
/// Refuse absurd segment counts instead of building millions of URLs.
const MAX_TEMPLATE_SEGMENTS: u64 = 200_000;

fn expand_rep_template(
    t: &Template,
    id: &str,
    bw: u64,
    base: &str,
    pdur: Option<f64>,
    live_window: Option<f64>,
) -> Result<(Option<SegRef>, Vec<SegRef>)> {
    let media = t.media.as_deref().unwrap_or("");
    let timescale = t.timescale.unwrap_or(1).max(1);
    let start = t.start_number.unwrap_or(1);
    let init = t.init.as_deref().map(|i| SegRef { url: resolve(base, &expand_template(i, id, bw, None, None)), range: None });
    let mut segs = Vec::new();
    if let Some(tl) = &t.timeline {
        let pticks = pdur.map(|d| (d * timescale as f64).round() as u64);
        for (k, time) in timeline_entries(tl, pticks).into_iter().enumerate() {
            segs.push(SegRef { url: resolve(base, &expand_template(media, id, bw, Some(start + k as u64), Some(time))), range: None });
        }
    } else if let Some(d) = t.duration {
        let total = pdur.ok_or_else(|| LdmError::Format("SegmentTemplate@duration without a period duration".into()))?;
        let per = d as f64 / timescale as f64;
        let (first, count) = match live_window {
            // live: only fully published segments inside the time-shift buffer
            Some(w) => {
                let count = (total / per + 1e-9).floor().max(0.0) as u64;
                (count.saturating_sub(((w / per).ceil() as u64).max(1)), count)
            }
            None => (0, ((total * timescale as f64) / d as f64 - 1e-9).ceil().max(0.0) as u64),
        };
        if count - first > MAX_TEMPLATE_SEGMENTS {
            return Err(LdmError::Format(format!("SegmentTemplate expands to {} segments", count - first)));
        }
        for k in first..count {
            let time = k * d;
            segs.push(SegRef { url: resolve(base, &expand_template(media, id, bw, Some(start + k), Some(time))), range: None });
        }
    } else {
        segs.push(SegRef { url: resolve(base, &expand_template(media, id, bw, Some(start), Some(0))), range: None });
    }
    Ok((init, segs))
}

/// Rank an audio representation under the preference (higher is better).
fn audio_rep_rank(a: &AdaptationSet, r: &Representation, pref: &crate::media::AudioPreference) -> (u8, u64) {
    use crate::media::AudioPreference as P;
    let tier = crate::media::AudioPreference::codec_tier(r.codecs.as_deref());
    let lang_hit = match pref {
        P::Language(code) => a.lang.as_deref().is_some_and(|l| l.eq_ignore_ascii_case(code)) as u8,
        _ => 0,
    };
    match pref {
        P::Aac => (tier, r.bandwidth),
        P::MostChannels => (tier, r.bandwidth), // CHANNELS not always in MPD; bandwidth as secondary
        P::Language(_) => (lang_hit.saturating_mul(4).saturating_add(tier), r.bandwidth),
    }
}

/// Pick the representation to download for each content type: highest
/// bandwidth (video limited to `max_height` / `preferred`); audio follows `audio` preference.
pub fn select<'a>(
    p: &'a Period,
    max_height: u32,
    preferred: Option<u32>,
    audio: &crate::media::AudioPreference,
) -> Vec<(&'a AdaptationSet, &'a Representation)> {
    let mut out = Vec::new();
    for want in ["video", "audio"] {
        let sets: Vec<&AdaptationSet> = p.adaptation_sets.iter().filter(|a| a.content_type == want).collect();
        let cands: Vec<(&AdaptationSet, &Representation)> =
            sets.iter().flat_map(|a| a.representations.iter().map(move |r| (*a, r))).collect();
        if cands.is_empty() {
            continue;
        }
        let cap = preferred.unwrap_or(max_height);
        let pick = if want == "video" {
            cands.iter().filter(|(_, r)| cap == 0 || r.height.unwrap_or(0) <= cap).max_by_key(|(_, r)| r.bandwidth).or_else(|| {
                if preferred.is_some() {
                    // explicit choice below every rendition: closest is the smallest
                    cands.iter().min_by_key(|(_, r)| (r.height.unwrap_or(0), r.bandwidth))
                } else {
                    cands.iter().max_by_key(|(_, r)| r.bandwidth)
                }
            })
        } else {
            cands.iter().max_by_key(|(a, r)| audio_rep_rank(a, r, audio))
        };
        if let Some(x) = pick {
            out.push(*x);
        }
    }
    if out.is_empty() {
        // unknown content types: take the best representation of the first set
        if let Some(a) = p.adaptation_sets.first() {
            if let Some(r) = a.representations.iter().max_by_key(|r| r.bandwidth) {
                out.push((a, r));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_number_template_lists_only_the_time_shift_window() {
        // DASH-IF livesim style: availabilityStartTime 1970, 2 s segments, 60 s buffer
        let x = r#"<MPD type="dynamic" availabilityStartTime="1970-01-01T00:00:00Z" timeShiftBufferDepth="PT1M"><Period start="PT0S">
          <AdaptationSet contentType="video" mimeType="video/mp4"><SegmentTemplate media="$RepresentationID$/$Number$.m4s" initialization="$RepresentationID$/init.mp4" duration="2" startNumber="0"/>
          <Representation id="V300" bandwidth="300000" width="640" height="480"/></AdaptationSet></Period></MPD>"#;
        let t0 = std::time::Instant::now();
        let m = parse(x, "https://h/livesim2/testpic_2s/Manifest.mpd").unwrap();
        assert!(t0.elapsed() < std::time::Duration::from_secs(1));
        let segs = &m.periods[0].adaptation_sets[0].representations[0].segments;
        assert_eq!(segs.len(), 30, "60 s / 2 s");
        let now_num = (chrono::Utc::now().timestamp() / 2) as u64;
        let last: u64 = segs.last().unwrap().url.rsplit('/').next().unwrap().trim_end_matches(".m4s").parse().unwrap();
        assert!(last + 2 >= now_num && last < now_num, "newest published segment {last} vs now {now_num}");
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT10S"), Some(10.0));
        assert_eq!(parse_duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(parse_duration("P1DT1S"), Some(86401.0));
        assert_eq!(parse_duration("PT0H0M9.960S"), Some(9.96));
        assert!(parse_duration("10S").is_none());
    }

    #[test]
    fn templates() {
        assert_eq!(expand_template("chunk-$RepresentationID$-$Number%05d$.m4s", "v0", 1, Some(7), None), "chunk-v0-00007.m4s");
        assert_eq!(expand_template("$Bandwidth$/$Time$.m4s", "a", 128000, None, Some(90000)), "128000/90000.m4s");
        assert_eq!(expand_template("a$$b", "x", 0, None, None), "a$b");
        assert_eq!(expand_template("x$Unknown$y", "x", 0, None, None), "x$Unknown$y");
    }

    #[test]
    fn timeline_r_expansion() {
        // regression from the prototype: r must contribute r+1 segments
        let tl = vec![(Some(0), 2000, 3), (None, 1000, 0)];
        assert_eq!(timeline_entries(&tl, None), vec![0, 2000, 4000, 6000, 8000]);
        let tl = vec![(Some(0), 1000, -1)];
        assert_eq!(timeline_entries(&tl, Some(4500)).len(), 5);
    }

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10S">
  <BaseURL>media/</BaseURL>
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <SegmentTemplate timescale="1000" initialization="init-$RepresentationID$.m4s" media="chunk-$RepresentationID$-$Number%05d$.m4s" startNumber="1">
        <SegmentTimeline><S t="0" d="2000" r="4"/></SegmentTimeline>
      </SegmentTemplate>
      <Representation id="v0" bandwidth="500000" width="640" height="360" codecs="avc1.64001e"/>
      <Representation id="v1" bandwidth="3000000" width="1920" height="1080"/>
      <Representation id="v2" bandwidth="9000000" width="3840" height="2160"/>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4" lang="en">
      <SegmentTemplate timescale="48000" duration="96000" initialization="a-init.mp4" media="a-$Number$.m4s"/>
      <Representation id="a0" bandwidth="64000"/>
      <Representation id="a1" bandwidth="128000"/>
    </AdaptationSet>
    <AdaptationSet mimeType="video/mp4">
      <Representation id="list" bandwidth="1">
        <SegmentList><Initialization sourceURL="l-init.mp4"/><SegmentURL media="l1.m4s"/><SegmentURL media="l2.m4s" mediaRange="0-99"/></SegmentList>
      </Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn parse_and_select() {
        let m = parse(MPD, "http://h/v/stream.mpd").unwrap();
        assert!(!m.dynamic);
        let p = &m.periods[0];
        let v1 = &p.adaptation_sets[0].representations[1];
        assert_eq!(v1.segments.len(), 5);
        assert_eq!(v1.segments[0].url, "http://h/v/media/chunk-v1-00001.m4s");
        assert_eq!(v1.segments[4].url, "http://h/v/media/chunk-v1-00005.m4s");
        assert_eq!(v1.init.as_ref().unwrap().url, "http://h/v/media/init-v1.m4s");
        let a = &p.adaptation_sets[1].representations[0];
        assert_eq!(a.segments.len(), 5); // 10 s / 2 s
        assert_eq!(a.segments[1].url, "http://h/v/media/a-2.m4s");
        let l = &p.adaptation_sets[2].representations[0];
        assert_eq!(l.segments[1].range, Some((0, 99)));
        assert_eq!(l.init.as_ref().unwrap().url, "http://h/v/media/l-init.mp4");
        let sel = select(p, 1080, None, &crate::media::AudioPreference::Aac);
        assert_eq!(sel.len(), 2);
        assert_eq!(sel[0].1.id, "v1");
        assert_eq!(sel[1].1.id, "a1");
        assert_eq!(select(p, 1080, Some(360), &crate::media::AudioPreference::Aac)[0].1.id, "v0");
    }

    #[test]
    fn audio_prefers_aac_over_ac3() {
        let x = r#"<?xml version="1.0"?>
<MPD type="static" mediaPresentationDuration="PT10S">
  <Period>
    <AdaptationSet contentType="video" mimeType="video/mp4">
      <Representation id="v0" bandwidth="500000" width="640" height="360" codecs="avc1.64001e">
        <BaseURL>v.mp4</BaseURL>
      </Representation>
    </AdaptationSet>
    <AdaptationSet contentType="audio" mimeType="audio/mp4" lang="en">
      <Representation id="ac3" bandwidth="384000" codecs="ac-3"><BaseURL>ac3.mp4</BaseURL></Representation>
      <Representation id="eac3" bandwidth="256000" codecs="ec-3"><BaseURL>eac3.mp4</BaseURL></Representation>
      <Representation id="aac" bandwidth="128000" codecs="mp4a.40.2"><BaseURL>aac.mp4</BaseURL></Representation>
    </AdaptationSet>
  </Period>
</MPD>"#;
        let m = parse(x, "http://h/a.mpd").unwrap();
        let aac = crate::media::AudioPreference::Aac;
        let sel = select(&m.periods[0], 1080, None, &aac);
        assert_eq!(sel.iter().find(|(a, _)| a.content_type == "audio").unwrap().1.id, "aac");
        // only AC-3 present
        let x2 = x.replace(
            r#"<Representation id="eac3" bandwidth="256000" codecs="ec-3"><BaseURL>eac3.mp4</BaseURL></Representation>
      <Representation id="aac" bandwidth="128000" codecs="mp4a.40.2"><BaseURL>aac.mp4</BaseURL></Representation>"#,
            "",
        );
        let m2 = parse(&x2, "http://h/a.mpd").unwrap();
        assert_eq!(select(&m2.periods[0], 1080, None, &aac).iter().find(|(a, _)| a.content_type == "audio").unwrap().1.id, "ac3");
    }

    #[test]
    fn protection_and_errors() {
        let x = r#"<MPD type="dynamic"><Period><AdaptationSet contentType="video"><ContentProtection schemeIdUri="urn:x"/><Representation id="a" bandwidth="1"><BaseURL>v.mp4</BaseURL></Representation></AdaptationSet></Period></MPD>"#;
        let m = parse(x, "http://h/a.mpd").unwrap();
        assert!(m.dynamic);
        assert!(m.periods[0].adaptation_sets[0].protected);
        assert_eq!(m.periods[0].adaptation_sets[0].representations[0].segments[0].url, "http://h/v.mp4");
        assert!(parse("<html/>", "http://h/").is_err());
        assert!(parse("not xml", "http://h/").is_err());
    }
}
