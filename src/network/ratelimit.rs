//! Token-bucket rate limiter — one bucket per peer id.

use std::collections::HashMap;
use std::time::Instant;

use parking_lot::Mutex;

pub struct RateLimiter {
    rpm: u32,
    buckets: Mutex<HashMap<String, Bucket>>,
}

struct Bucket {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(rpm: u32) -> Self {
        Self {
            rpm,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn allow(&self, peer_id: &str) -> bool {
        if self.rpm == 0 {
            return true;
        }
        let mut buckets = self.buckets.lock();
        let bucket = buckets
            .entry(peer_id.to_string())
            .or_insert_with(|| Bucket {
                tokens: self.rpm as f64,
                last_refill: Instant::now(),
            });
        let now = Instant::now();
        let elapsed = now.duration_since(bucket.last_refill).as_secs_f64();
        bucket.tokens = (bucket.tokens + (self.rpm as f64 / 60.0) * elapsed)
            .min(self.rpm as f64);
        bucket.last_refill = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_burst_is_allowed_up_to_the_limit_then_refused() {
        let rl = RateLimiter::new(5);
        for i in 0..5 {
            assert!(rl.allow("peer"), "request {i} of the first five");
        }
        assert!(!rl.allow("peer"), "the sixth in the same minute");
    }

    #[test]
    fn tokens_come_back_as_time_passes() {
        let rl = RateLimiter::new(60); // one per second
        for _ in 0..60 {
            assert!(rl.allow("peer"));
        }
        assert!(!rl.allow("peer"));
        // Rewind the bucket rather than sleeping: the refill is a function
        // of elapsed time, so moving `last_refill` back two seconds is
        // exactly equivalent and keeps the test instant.
        rl.buckets.lock().get_mut("peer").unwrap().last_refill -= Duration::from_secs(2);
        assert!(rl.allow("peer"), "two seconds buys two tokens at 60/min");
        assert!(rl.allow("peer"));
        assert!(!rl.allow("peer"), "but not a third");
    }

    #[test]
    fn rpm_zero_disables_the_limiter_entirely() {
        // This is WHY `/v1/invite/redeem` gets its own limiter instance
        // with a hardcoded rate: `rate_limit_rpm` is user-settable, and a
        // household that set it to 0 to stop chat throttling would
        // otherwise also switch off brute-force protection on the one
        // route that hands out credentials.
        let rl = RateLimiter::new(0);
        for _ in 0..1000 {
            assert!(rl.allow("peer"));
        }
    }

    #[test]
    fn each_key_gets_its_own_bucket() {
        let rl = RateLimiter::new(2);
        assert!(rl.allow("192.168.1.10"));
        assert!(rl.allow("192.168.1.10"));
        assert!(!rl.allow("192.168.1.10"), "that key is spent");
        assert!(rl.allow("192.168.1.11"), "a different source is unaffected");
        assert!(rl.allow("192.168.1.11"));
        assert!(!rl.allow("192.168.1.11"));
    }
}
