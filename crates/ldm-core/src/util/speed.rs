//! Sliding-window speed meter.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct SpeedMeter {
    window: Duration,
    samples: Mutex<VecDeque<(Instant, u64)>>,
}

impl Default for SpeedMeter {
    fn default() -> Self {
        Self::new(Duration::from_secs(3))
    }
}

impl SpeedMeter {
    pub fn new(window: Duration) -> Self {
        Self { window, samples: Mutex::new(VecDeque::new()) }
    }

    /// Record the cumulative byte counter.
    pub fn record(&self, total: u64) {
        let now = Instant::now();
        let mut s = self.samples.lock().unwrap();
        if let Some(&(t, _)) = s.back() {
            if now.duration_since(t) < Duration::from_millis(100) {
                if let Some(last) = s.back_mut() {
                    last.1 = total;
                }
                return;
            }
        }
        s.push_back((now, total));
        while s.len() > 2 && now.duration_since(s[0].0) > self.window {
            s.pop_front();
        }
    }

    /// Bytes per second over the window.
    pub fn speed(&self) -> f64 {
        let s = self.samples.lock().unwrap();
        if s.len() < 2 {
            return 0.0;
        }
        let (t0, b0) = s[0];
        let (t1, b1) = *s.back().unwrap();
        // decay to zero if nothing was recorded recently
        let idle = Instant::now().duration_since(t1);
        let dt = t1.duration_since(t0).as_secs_f64() + idle.as_secs_f64();
        if dt <= 0.0 {
            0.0
        } else {
            b1.saturating_sub(b0) as f64 / dt
        }
    }

    /// Rate between the first and last sample, without idle decay (for control decisions).
    pub fn sampled_rate(&self) -> f64 {
        let s = self.samples.lock().unwrap();
        match (s.front(), s.back()) {
            (Some(&(t0, b0)), Some(&(t1, b1))) if t1 > t0 => b1.saturating_sub(b0) as f64 / t1.duration_since(t0).as_secs_f64(),
            _ => 0.0,
        }
    }

    pub fn reset(&self) {
        self.samples.lock().unwrap().clear();
    }
}
