//! Token-bucket rate limiting per credential (or per client address when anonymous).
//!
//! Buckets are capped so that a flood of distinct keys cannot exhaust memory.

use std::collections::BTreeMap;

/// Maximum number of tracked buckets.
pub const MAX_BUCKETS: usize = 16_384;

#[derive(Debug, Clone)]
struct Bucket {
    tokens: f64,
    last_refill_ms: i64,
}

/// A simple in-process rate limiter.
#[derive(Debug)]
pub struct RateLimiter {
    per_second: f64,
    burst: f64,
    buckets: BTreeMap<String, Bucket>,
}

impl RateLimiter {
    pub fn new(per_second: u32, burst: u32) -> RateLimiter {
        RateLimiter {
            per_second: per_second as f64,
            burst: burst.max(per_second) as f64,
            buckets: BTreeMap::new(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.per_second > 0.0
    }

    /// Consume one token for `key`. Returns `false` when the limit is exceeded.
    pub fn check(&mut self, key: &str, now_ms: i64) -> bool {
        if !self.enabled() {
            return true;
        }
        if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(key) {
            self.prune(now_ms);
            if self.buckets.len() >= MAX_BUCKETS && !self.buckets.contains_key(key) {
                return false; // refuse rather than grow without bound
            }
        }
        let entry = self.buckets.entry(key.to_string()).or_insert(Bucket {
            tokens: self.burst,
            last_refill_ms: now_ms,
        });
        let elapsed = (now_ms - entry.last_refill_ms).max(0) as f64 / 1000.0;
        entry.tokens = (entry.tokens + elapsed * self.per_second).min(self.burst);
        entry.last_refill_ms = now_ms;
        if entry.tokens >= 1.0 {
            entry.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    /// Drop buckets that have fully refilled (they carry no information).
    pub fn prune(&mut self, now_ms: i64) {
        let per_second = self.per_second;
        let burst = self.burst;
        self.buckets.retain(|_, bucket| {
            let elapsed = (now_ms - bucket.last_refill_ms).max(0) as f64 / 1000.0;
            bucket.tokens + elapsed * per_second < burst
        });
    }

    pub fn tracked(&self) -> usize {
        self.buckets.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_burst_then_limits() {
        let mut limiter = RateLimiter::new(10, 10);
        for _ in 0..10 {
            assert!(limiter.check("token-a", 1000));
        }
        assert!(!limiter.check("token-a", 1000));
        // Independent buckets.
        assert!(limiter.check("token-b", 1000));
        // One second later enough tokens have refilled for a few requests.
        let mut allowed = 0;
        while limiter.check("token-a", 2000) {
            allowed += 1;
            if allowed > 100 {
                break;
            }
        }
        assert!((9..=11).contains(&allowed), "refilled {allowed} tokens");
    }

    #[test]
    fn disabled_limiter_allows_everything() {
        let mut limiter = RateLimiter::new(0, 0);
        assert!(!limiter.enabled());
        for _ in 0..1000 {
            assert!(limiter.check("any", 0));
        }
        assert_eq!(limiter.tracked(), 0);
    }

    #[test]
    fn prune_drops_idle_buckets() {
        let mut limiter = RateLimiter::new(1, 1);
        assert!(limiter.check("k", 0));
        assert_eq!(limiter.tracked(), 1);
        limiter.prune(60_000);
        assert_eq!(limiter.tracked(), 0);
    }

    #[test]
    fn refuses_new_keys_at_the_bucket_bound() {
        let mut limiter = RateLimiter::new(1, 1);
        for i in 0..MAX_BUCKETS {
            assert!(limiter.check(&format!("client-{i}"), 0));
        }
        assert_eq!(limiter.tracked(), MAX_BUCKETS);
        assert!(!limiter.check("one-too-many", 0));
        assert_eq!(limiter.tracked(), MAX_BUCKETS);
        // Existing keys remain usable under the cap.
        assert!(!limiter.check("client-0", 0));
        assert_eq!(limiter.tracked(), MAX_BUCKETS);
    }
}
