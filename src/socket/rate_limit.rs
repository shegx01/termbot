//! Per-connection token bucket rate limiter.
//!
//! Each connection has its own `TokenBucket`. Inbound envelopes consume
//! 1 token. When empty, the caller receives `Err(retry_after)`.

use std::time::{Duration, Instant};

/// Token bucket rate limiter.
///
/// Capacity = burst size. Refill rate = sustained requests/second.
/// Tokens are `f64` for smooth fractional refill.
#[derive(Debug)]
pub struct TokenBucket {
    capacity: f64,
    refill_per_sec: f64,
    tokens: f64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a new bucket with `capacity` (burst) tokens, refilling at
    /// `refill_per_sec` tokens per second. Starts full.
    pub fn new(capacity: f64, refill_per_sec: f64) -> Self {
        Self {
            capacity,
            refill_per_sec,
            tokens: capacity,
            last_refill: Instant::now(),
        }
    }

    /// Try to consume `cost` tokens. Returns `Ok(())` if tokens were
    /// available, `Err(retry_after)` with the duration to wait otherwise.
    pub fn try_consume(&mut self, cost: f64) -> Result<(), Duration> {
        self.refill();

        if cost > self.capacity {
            // Cost exceeds bucket capacity — will never succeed.
            return Err(Duration::from_secs(1));
        }

        if self.tokens >= cost {
            self.tokens -= cost;
            Ok(())
        } else {
            let deficit = cost - self.tokens;
            let wait_secs = deficit / self.refill_per_sec;
            Err(Duration::from_secs_f64(wait_secs))
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_refill);
        let added = elapsed.as_secs_f64() * self.refill_per_sec;
        self.tokens = (self.tokens + added).min(self.capacity);
        self.last_refill = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn fresh_bucket_allows_burst() {
        let mut bucket = TokenBucket::new(5.0, 1.0);
        for _ in 0..5 {
            assert!(bucket.try_consume(1.0).is_ok());
        }
        // 6th should fail
        assert!(bucket.try_consume(1.0).is_err());
    }

    #[test]
    fn refill_restores_tokens() {
        let mut bucket = TokenBucket::new(2.0, 100.0); // fast refill for test
        assert!(bucket.try_consume(2.0).is_ok());
        assert!(bucket.try_consume(1.0).is_err());

        // Sleep briefly for refill
        thread::sleep(Duration::from_millis(50));
        // Should have refilled ~5 tokens (100/s * 0.05s), capped at 2
        assert!(bucket.try_consume(1.0).is_ok());
    }

    #[test]
    fn cost_exceeding_capacity_always_fails() {
        let mut bucket = TokenBucket::new(3.0, 10.0);
        let err = bucket.try_consume(5.0).unwrap_err();
        assert!(err >= Duration::from_millis(1));
    }

    #[test]
    fn tokens_capped_at_capacity() {
        let mut bucket = TokenBucket::new(3.0, 1000.0);
        thread::sleep(Duration::from_millis(50));
        bucket.refill();
        // Even after generous refill time, tokens shouldn't exceed capacity
        assert!(bucket.tokens <= 3.0 + 0.01); // small float tolerance
    }

    #[test]
    fn retry_after_is_reasonable() {
        let mut bucket = TokenBucket::new(1.0, 10.0); // 10 tokens/sec
        assert!(bucket.try_consume(1.0).is_ok());
        let retry = bucket.try_consume(1.0).unwrap_err();
        // Need 1 token at 10/sec = 0.1s
        assert!(retry.as_millis() <= 150, "retry_after={:?}", retry);
        assert!(retry.as_millis() >= 50, "retry_after={:?}", retry);
    }
}
