use std::collections::{HashMap, VecDeque};
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// Limits failed connection / pairing attempts per source address.
#[derive(Debug)]
pub struct AttemptLimiter {
    max_failures: usize,
    window: Duration,
    failures: HashMap<IpAddr, VecDeque<Instant>>,
}

impl AttemptLimiter {
    pub fn new(max_failures: usize, window: Duration) -> Self {
        Self {
            max_failures: max_failures.max(1),
            window,
            failures: HashMap::new(),
        }
    }

    fn prune(&mut self, now: Instant) {
        let window = self.window;
        self.failures.retain(|_, q| {
            while q.front().is_some_and(|t| now.duration_since(*t) > window) {
                q.pop_front();
            }
            !q.is_empty()
        });
    }

    /// Whether a new attempt from `ip` is allowed right now.
    pub fn allowed(&mut self, ip: IpAddr, now: Instant) -> bool {
        self.prune(now);
        self.failures
            .get(&ip)
            .is_none_or(|q| q.len() < self.max_failures)
    }

    pub fn record_failure(&mut self, ip: IpAddr, now: Instant) {
        self.prune(now);
        self.failures.entry(ip).or_default().push_back(now);
    }

    pub fn record_success(&mut self, ip: IpAddr) {
        self.failures.remove(&ip);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_after_failures_then_recovers() {
        let mut l = AttemptLimiter::new(3, Duration::from_secs(60));
        let ip: IpAddr = [192, 168, 1, 10].into();
        let other: IpAddr = [192, 168, 1, 11].into();
        let t0 = Instant::now();
        for _ in 0..3 {
            assert!(l.allowed(ip, t0));
            l.record_failure(ip, t0);
        }
        assert!(!l.allowed(ip, t0));
        assert!(l.allowed(other, t0));
        assert!(l.allowed(ip, t0 + Duration::from_secs(61)));
        l.record_failure(ip, t0 + Duration::from_secs(61));
        l.record_success(ip);
        assert!(l.allowed(ip, t0 + Duration::from_secs(61)));
    }
}
