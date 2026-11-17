//! Local-only performance measurement.
//!
//! Nothing in this crate sends data anywhere: it only aggregates samples so
//! the UI, logs and benchmarks can display them.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Percentile summary of a sample window, in microseconds.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Summary {
    pub count: usize,
    pub min: u64,
    pub p50: u64,
    pub p95: u64,
    pub p99: u64,
    pub max: u64,
    pub mean: f64,
}

/// Keeps the last `capacity` samples and computes exact percentiles over them.
/// Recording is O(1) and allocation-free once the window is full.
#[derive(Debug, Clone)]
pub struct Window {
    samples: VecDeque<u64>,
    capacity: usize,
    scratch: Vec<u64>,
}

impl Window {
    pub fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            samples: VecDeque::with_capacity(capacity),
            capacity,
            scratch: Vec::with_capacity(capacity),
        }
    }

    pub fn record(&mut self, value: u64) {
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(value);
    }

    pub fn record_duration(&mut self, d: Duration) {
        self.record(u64::try_from(d.as_micros()).unwrap_or(u64::MAX));
    }

    pub fn len(&self) -> usize {
        self.samples.len()
    }

    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    pub fn clear(&mut self) {
        self.samples.clear();
    }

    /// Nearest-rank percentiles over the current window.
    pub fn summary(&mut self) -> Summary {
        if self.samples.is_empty() {
            return Summary::default();
        }
        self.scratch.clear();
        self.scratch.extend(self.samples.iter().copied());
        self.scratch.sort_unstable();
        let s = &self.scratch;
        let rank = |p: f64| -> u64 {
            let idx = ((p / 100.0) * s.len() as f64).ceil() as usize;
            s[idx.clamp(1, s.len()) - 1]
        };
        let sum: u128 = s.iter().map(|&v| u128::from(v)).sum();
        Summary {
            count: s.len(),
            min: s[0],
            p50: rank(50.0),
            p95: rank(95.0),
            p99: rank(99.0),
            max: s[s.len() - 1],
            mean: sum as f64 / s.len() as f64,
        }
    }
}

/// Counts events and bytes over a sliding time window to report rates.
#[derive(Debug, Clone)]
pub struct RateMeter {
    window: Duration,
    events: VecDeque<(Instant, u64)>,
    total_bytes: u64,
}

impl RateMeter {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            events: VecDeque::new(),
            total_bytes: 0,
        }
    }

    pub fn record(&mut self, now: Instant, bytes: u64) {
        self.events.push_back((now, bytes));
        self.total_bytes += bytes;
        self.expire(now);
    }

    fn expire(&mut self, now: Instant) {
        while let Some(&(t, b)) = self.events.front() {
            if now.duration_since(t) > self.window {
                self.events.pop_front();
                self.total_bytes -= b;
            } else {
                break;
            }
        }
    }

    /// Events per second over the window.
    pub fn rate(&mut self, now: Instant) -> f64 {
        self.expire(now);
        self.events.len() as f64 / self.window.as_secs_f64()
    }

    /// Bits per second over the window.
    pub fn bitrate(&mut self, now: Instant) -> f64 {
        self.expire(now);
        self.total_bytes as f64 * 8.0 / self.window.as_secs_f64()
    }
}

/// Snapshot of a streaming session, shown in the UI overlay and logs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SessionStats {
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub bitrate_mbps: f64,
    pub rtt_ms: f64,
    pub lost_packets: u64,
    pub frames_dropped: u64,
    /// Client-side decode time.
    pub decode_us: Summary,
    /// Host encode time as reported by the host (if known).
    pub encode_us: Summary,
    /// Host capture timestamp → client frame ready (requires synchronized
    /// clocks; only meaningful in loopback/benchmark mode).
    pub end_to_end_us: Summary,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_nearest_rank() {
        let mut w = Window::new(1000);
        for v in 1..=100 {
            w.record(v);
        }
        let s = w.summary();
        assert_eq!(s.count, 100);
        assert_eq!(s.min, 1);
        assert_eq!(s.p50, 50);
        assert_eq!(s.p95, 95);
        assert_eq!(s.p99, 99);
        assert_eq!(s.max, 100);
        assert!((s.mean - 50.5).abs() < 1e-9);
    }

    #[test]
    fn window_is_bounded() {
        let mut w = Window::new(3);
        for v in [10, 20, 30, 40] {
            w.record(v);
        }
        assert_eq!(w.len(), 3);
        assert_eq!(w.summary().min, 20);
        assert_eq!(Window::new(3).summary(), Summary::default());
    }

    #[test]
    fn rate_meter_expires() {
        let mut m = RateMeter::new(Duration::from_secs(1));
        let t0 = Instant::now();
        for i in 0..60 {
            m.record(t0 + Duration::from_millis(i * 16), 1000);
        }
        let now = t0 + Duration::from_millis(960);
        assert!((m.rate(now) - 60.0).abs() < 1e-9);
        assert!((m.bitrate(now) - 480_000.0).abs() < 1e-6);
        assert_eq!(m.rate(t0 + Duration::from_secs(5)), 0.0);
    }
}
