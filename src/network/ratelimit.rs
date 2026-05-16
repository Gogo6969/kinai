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
