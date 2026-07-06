//! Per-token request rate limiting (token-bucket). Keyed by a hash of the
//! bearer token so raw secrets aren't held as map keys. The bucket set is
//! bounded by the number of configured tokens (we key by token, not by IP).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Mutex;
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    /// Requests per minute per token. `0` disables limiting.
    per_minute: u32,
    buckets: Mutex<HashMap<u64, Bucket>>,
}

impl RateLimiter {
    pub fn new(per_minute: u32) -> Self {
        Self {
            per_minute,
            buckets: Mutex::new(HashMap::new()),
        }
    }

    pub fn disabled(&self) -> bool {
        self.per_minute == 0
    }

    /// Consume one token for `key`; returns false when the bucket is empty.
    /// `now` is injectable for tests; production callers use [`RateLimiter::allow`].
    fn allow_at(&self, key: &str, now: Instant) -> bool {
        if self.per_minute == 0 {
            return true;
        }
        let cap = self.per_minute as f64;
        let refill_per_sec = cap / 60.0;
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        let id = h.finish();

        let mut map = self.buckets.lock().unwrap_or_else(|e| e.into_inner());
        let bucket = map.entry(id).or_insert(Bucket {
            tokens: cap,
            last: now,
        });
        let elapsed = now.saturating_duration_since(bucket.last).as_secs_f64();
        bucket.tokens = (bucket.tokens + elapsed * refill_per_sec).min(cap);
        bucket.last = now;
        if bucket.tokens >= 1.0 {
            bucket.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    pub fn allow(&self, key: &str) -> bool {
        self.allow_at(key, Instant::now())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn disabled_always_allows() {
        let rl = RateLimiter::new(0);
        let t = Instant::now();
        for _ in 0..1000 {
            assert!(rl.allow_at("tok", t));
        }
    }

    #[test]
    fn burst_then_block_then_refill() {
        let rl = RateLimiter::new(60); // 60/min = 1/sec, burst 60
        let t0 = Instant::now();
        // Burst the full minute's budget at one instant.
        for _ in 0..60 {
            assert!(rl.allow_at("tok", t0), "burst should be allowed");
        }
        // 61st at the same instant is blocked.
        assert!(!rl.allow_at("tok", t0), "over-budget should block");
        // After ~2s, ~2 tokens refilled.
        let t1 = t0 + Duration::from_secs(2);
        assert!(rl.allow_at("tok", t1));
        assert!(rl.allow_at("tok", t1));
        assert!(!rl.allow_at("tok", t1));
    }

    #[test]
    fn tokens_are_independent() {
        let rl = RateLimiter::new(1);
        let t = Instant::now();
        assert!(rl.allow_at("a", t));
        assert!(!rl.allow_at("a", t));
        // A different token has its own bucket.
        assert!(rl.allow_at("b", t));
    }
}
