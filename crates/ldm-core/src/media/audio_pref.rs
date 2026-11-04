//! Audio track preference when a master / MPD offers several audio options
//! for the same video (Apple bipbop–style: AAC + AC-3 + E-AC-3).
//! LDM defaults to AAC (the most widely playable choice) and auto-merges
//! video+audio into one file.

/// How to pick among several audio groups / representations for one video quality.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioPreference {
    /// Prefer AAC (`mp4a` / `aac`), then E-AC-3 (`ec-3`), then AC-3 (`ac-3`). Default.
    Aac,
    /// Prefer the rendition / representation with the most channels.
    MostChannels,
    /// Prefer a matching `LANGUAGE` (HLS) / AdaptationSet `lang` (DASH); within that
    /// set still prefer AAC → EC-3 → AC-3.
    Language(String),
}

impl AudioPreference {
    /// Parse a config string: `aac` (default), `most_channels`, `language:en`.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        let lower = s.to_ascii_lowercase();
        if lower == "most_channels" || lower == "channels" {
            return Self::MostChannels;
        }
        if let Some(code) = lower.strip_prefix("language:").or_else(|| lower.strip_prefix("lang:")) {
            let code = code.trim();
            if !code.is_empty() {
                return Self::Language(code.to_string());
            }
        }
        Self::Aac
    }

    /// Higher is better. Used as the primary key when ranking candidates.
    pub fn codec_tier(codecs: Option<&str>) -> u8 {
        let c = codecs.unwrap_or("").to_ascii_lowercase();
        // a single CODECS list may mix video + audio; look for audio tokens
        let parts: Vec<&str> = c.split(',').map(|x| x.trim()).collect();
        let has = |p: &str| parts.iter().any(|x| x.starts_with(p));
        if has("mp4a") || has("aac") {
            3
        } else if has("ec-3") || has("eac3") {
            2
        } else if has("ac-3") || has("ac3") {
            1
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_and_tiers() {
        assert_eq!(AudioPreference::parse("aac"), AudioPreference::Aac);
        assert_eq!(AudioPreference::parse(""), AudioPreference::Aac);
        assert_eq!(AudioPreference::parse("most_channels"), AudioPreference::MostChannels);
        assert_eq!(AudioPreference::parse("language:en"), AudioPreference::Language("en".into()));
        assert_eq!(AudioPreference::parse("lang:zh-CN"), AudioPreference::Language("zh-cn".into()));
        assert_eq!(AudioPreference::codec_tier(Some("avc1.640015,mp4a.40.2")), 3);
        assert_eq!(AudioPreference::codec_tier(Some("avc1.640015,ec-3")), 2);
        assert_eq!(AudioPreference::codec_tier(Some("avc1.640015,ac-3")), 1);
        assert_eq!(AudioPreference::codec_tier(Some("avc1.640015")), 0);
    }
}
