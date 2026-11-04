//! Token-bucket speed limiter.
//! rate = bytes/s, burst = max(rate/10, 64 KiB). Rate 0 = unlimited.
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct TokenBucket {
    rate: AtomicU64,
    state: Mutex<(f64, Instant)>,
}

impl TokenBucket {
    pub fn new(rate: u64) -> Self {
        Self { rate: AtomicU64::new(rate), state: Mutex::new((Self::burst_for(rate), Instant::now())) }
    }

    fn burst_for(rate: u64) -> f64 {
        (rate as f64 / 10.0).max(65536.0)
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
        let mut st = self.state.lock().unwrap();
        st.0 = st.0.min(Self::burst_for(rate));
    }

    /// Reserve `n` bytes; returns how long the caller must wait.
    pub fn reserve(&self, n: usize) -> Duration {
        let rate = self.rate();
        if rate == 0 {
            return Duration::ZERO;
        }
        let burst = Self::burst_for(rate);
        let mut st = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(st.1).as_secs_f64();
        st.0 = (st.0 + elapsed * rate as f64).min(burst);
        st.1 = now;
        st.0 -= n as f64;
        if st.0 >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-st.0 / rate as f64)
        }
    }

    pub async fn acquire(&self, n: usize) {
        let d = self.reserve(n);
        if !d.is_zero() {
            tokio::time::sleep(d).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unlimited_never_waits() {
        let b = TokenBucket::new(0);
        assert!(b.reserve(10_000_000).is_zero());
    }

    #[test]
    fn limited_accumulates_debt() {
        let b = TokenBucket::new(100_000);
        // burst is 65536: the first 65536 bytes are free, then 100 kB/s
        assert!(b.reserve(65_536).is_zero());
        let d = b.reserve(100_000);
        assert!(d.as_secs_f64() > 0.9 && d.as_secs_f64() < 1.1, "{d:?}");
    }
}
