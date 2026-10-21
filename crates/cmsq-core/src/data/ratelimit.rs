//! Token bucket (per endpoint) + jittered backoff helpers.
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct TokenBucket {
    per_min: f64,
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    pub fn new(per_min: f64) -> Self {
        // burst capped at 1/10 of the minute budget so a fresh bucket cannot spike
        Self { per_min, tokens: (per_min / 10.0).max(1.0), last: Instant::now() }
    }
    pub fn per_min(&self) -> f64 {
        self.per_min
    }
    pub fn set_rate(&mut self, per_min: f64) {
        self.per_min = per_min.max(1.0);
        self.tokens = self.tokens.min(self.cap());
    }
    fn cap(&self) -> f64 {
        (self.per_min / 10.0).max(1.0)
    }
    fn refill(&mut self) {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.per_min / 60.0).min(self.cap());
        self.last = now;
    }
    /// How long to wait before a call may proceed; consumes the token if 0.
    pub fn take(&mut self) -> Duration {
        self.refill();
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            Duration::ZERO
        } else {
            Duration::from_secs_f64((1.0 - self.tokens) * 60.0 / self.per_min)
        }
    }
    /// Drain all tokens (after a server-side limit error).
    pub fn drain(&mut self) {
        self.tokens = 0.0;
        self.last = Instant::now();
    }
}

pub fn jitter(base: Duration, frac: f64) -> Duration {
    let r = u64::from_le_bytes(uuid::Uuid::new_v4().as_bytes()[..8].try_into().unwrap()) as f64 / u64::MAX as f64;
    base.mul_f64(1.0 + frac * (2.0 * r - 1.0))
}

/// Exponential backoff with ±30% jitter: base * 2^attempt.
pub fn backoff(base: Duration, attempt: u32) -> Duration {
    jitter(base * 2u32.pow(attempt.min(6)), 0.3)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bucket_limits_burst_and_rate() {
        let mut b = TokenBucket::new(600.0); // 10/s, burst 60
        let mut free = 0;
        for _ in 0..100 {
            if b.take().is_zero() {
                free += 1;
            }
        }
        assert_eq!(free, 60);
        let w = b.take();
        assert!(w > Duration::ZERO && w <= Duration::from_millis(110), "{w:?}");
        b.set_rate(60.0);
        b.drain();
        assert!(b.take() >= Duration::from_millis(900));
    }
    #[test]
    fn backoff_grows() {
        assert!(backoff(Duration::from_secs(1), 3) >= Duration::from_millis(5600));
    }
}
