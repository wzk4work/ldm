//! Web-video pipeline: manifest → plan → segment download → (decrypt) → mux
//!.
pub mod adscore;
pub mod audio_pref;
pub mod dash;
pub mod hls;
pub mod job;
pub mod mux;

pub use audio_pref::AudioPreference;
pub use job::{plan_media, probe_media, run_media_job, MediaJobSpec, MediaOutcome, MediaPlan, QualityRow};
