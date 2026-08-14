//! Per-IP token-bucket rate limiting.
//!
//! Hand-rolled rather than pulled in as a dependency: the whole thing is a
//! HashMap and some arithmetic, and it keeps the deploy surface small.
//!
//! Buckets refill continuously rather than resetting on a window boundary, so
//! a client cannot burst twice by straddling the boundary.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::Instant;

struct Bucket {
    tokens: f64,
    last: Instant,
}

pub struct RateLimiter {
    buckets: Mutex<HashMap<IpAddr, Bucket>>,
    capacity: f64,
    per_sec: f64,
}

/// Evict idle buckets once the map gets big, rather than running a timer.
const SWEEP_AT: usize = 10_000;

impl RateLimiter {
    /// `burst` requests immediately available, refilling at `per_sec`.
    pub fn new(burst: f64, per_sec: f64) -> Self {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
            capacity: burst,
            per_sec,
        }
    }

    /// True if the request is allowed, consuming one token.
    pub fn allow(&self, ip: IpAddr) -> bool {
        let now = Instant::now();
        let mut map = self.buckets.lock().unwrap();

        if map.len() > SWEEP_AT {
            let cap = self.capacity;
            let rate = self.per_sec;
            // A bucket that has had time to refill completely carries no
            // information, so dropping it is free.
            map.retain(|_, b| {
                b.tokens + now.duration_since(b.last).as_secs_f64() * rate < cap
            });
        }

        let b = map.entry(ip).or_insert(Bucket { tokens: self.capacity, last: now });
        let refill = now.duration_since(b.last).as_secs_f64() * self.per_sec;
        b.tokens = (b.tokens + refill).min(self.capacity);
        b.last = now;

        if b.tokens >= 1.0 {
            b.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn burst_is_allowed_then_refused() {
        let rl = RateLimiter::new(3.0, 0.0001);
        assert!(rl.allow(ip(1)));
        assert!(rl.allow(ip(1)));
        assert!(rl.allow(ip(1)));
        assert!(!rl.allow(ip(1)), "fourth request must be refused");
    }

    #[test]
    fn buckets_are_per_ip() {
        let rl = RateLimiter::new(1.0, 0.0001);
        assert!(rl.allow(ip(1)));
        assert!(!rl.allow(ip(1)));
        assert!(rl.allow(ip(2)), "one IP must not exhaust another's budget");
    }

    #[test]
    fn tokens_refill_over_time() {
        // 100/sec refill: a 10 ms sleep is worth a token.
        let rl = RateLimiter::new(1.0, 100.0);
        assert!(rl.allow(ip(3)));
        assert!(!rl.allow(ip(3)));
        std::thread::sleep(std::time::Duration::from_millis(30));
        assert!(rl.allow(ip(3)), "bucket should have refilled");
    }

    #[test]
    fn refill_never_exceeds_capacity() {
        let rl = RateLimiter::new(2.0, 1000.0);
        std::thread::sleep(std::time::Duration::from_millis(20));
        assert!(rl.allow(ip(4)));
        assert!(rl.allow(ip(4)));
        assert!(!rl.allow(ip(4)), "capacity must cap the refill");
    }
}
