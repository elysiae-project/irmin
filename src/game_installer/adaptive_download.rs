//! Dynamic download concurrency controller.
//!
//! Starts with a small worker pool and spawns additional workers when
//! throughput is still increasing. Stops scaling when bandwidth saturates
//! or the ceiling is reached. Workers die naturally when the queue empties.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

/// Initial number of download workers.
pub const INITIAL_CONCURRENCY: usize = 8;
/// Maximum download workers (hard ceiling).
pub const MAX_CONCURRENCY: usize = 48;
/// Interval between throughput checks.
const MEASURE_INTERVAL_MS: u64 = 2000;
/// Scale up when throughput grew by at least this fraction.
const GROWTH_THRESHOLD: f64 = 0.05;
/// Workers to add per scale-up event.
const SCALE_STEP: usize = 4;

/// Tracks throughput and decides when to spawn more workers.
pub struct AdaptiveDownload {
    /// Current concurrency target (workers alive or to be spawned).
    target: AtomicUsize,
    /// Smoothed throughput (bytes/sec * 1000 for fixed-point).
    last_throughput: AtomicU64,
    /// Bytes counter snapshot from previous window.
    last_bytes: AtomicU64,
    /// Timestamp of previous measurement (nanos since epoch).
    last_time: AtomicU64,
}

impl AdaptiveDownload {
    pub fn new() -> Self {
        Self {
            target: AtomicUsize::new(INITIAL_CONCURRENCY),
            last_throughput: AtomicU64::new(0),
            last_bytes: AtomicU64::new(0),
            last_time: AtomicU64::new(now_nanos()),
        }
    }

    /// Current number of workers that should be alive.
    pub fn current_target(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    /// Called periodically with the total downloaded bytes. Returns the new
    /// target if it changed (signals the caller to spawn more workers).
    pub fn measure(&self, total_bytes: u64) -> Option<usize> {
        let now = now_nanos();
        let prev_time = self.last_time.load(Ordering::Relaxed);
        let elapsed_ns = now.saturating_sub(prev_time);
        if elapsed_ns < (MEASURE_INTERVAL_MS as u64) * 1_000_000 {
            return None;
        }

        // Swap timestamps
        if self
            .last_time
            .compare_exchange(prev_time, now, Ordering::AcqRel, Ordering::Relaxed)
            .is_err()
        {
            return None;
        }

        let prev_bytes = self.last_bytes.swap(total_bytes, Ordering::Relaxed);
        let delta_bytes = total_bytes.saturating_sub(prev_bytes);
        let elapsed_secs = elapsed_ns as f64 / 1_000_000_000.0;
        let raw_throughput = delta_bytes as f64 / elapsed_secs.max(0.001);

        // EWMA smooth
        let prev_smoothed = self.last_throughput.load(Ordering::Relaxed) as f64 / 1000.0;
        let throughput = if prev_smoothed == 0.0 {
            raw_throughput
        } else {
            0.3 * raw_throughput + 0.7 * prev_smoothed
        };
        self.last_throughput
            .store((throughput * 1000.0) as u64, Ordering::Relaxed);

        // First measurement: just record baseline, don't scale yet.
        if prev_smoothed == 0.0 {
            return None;
        }

        let current = self.target.load(Ordering::Relaxed);
        if current >= MAX_CONCURRENCY {
            return None;
        }

        // Scale up if throughput grew (bandwidth not yet saturated).
        if throughput > prev_smoothed * (1.0 + GROWTH_THRESHOLD) {
            let new_target = (current + SCALE_STEP).min(MAX_CONCURRENCY);
            self.target.store(new_target, Ordering::Relaxed);
            return Some(new_target);
        }

        None
    }
}

fn now_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_target() {
        let ad = AdaptiveDownload::new();
        assert_eq!(ad.current_target(), INITIAL_CONCURRENCY);
    }

    #[test]
    fn measure_below_interval_returns_none() {
        let ad = AdaptiveDownload::new();
        assert_eq!(ad.measure(1000), None);
    }

    #[test]
    fn measure_scales_up_on_growth() {
        let ad = AdaptiveDownload::new();
        // Simulate first measurement window
        ad.last_time.store(
            now_nanos() - (MEASURE_INTERVAL_MS as u64 + 100) * 1_000_000,
            Ordering::Relaxed,
        );
        ad.last_bytes.store(0, Ordering::Relaxed);
        // First call sets baseline
        assert_eq!(ad.measure(100_000_000), None);

        // Simulate second window with growth
        ad.last_time.store(
            now_nanos() - (MEASURE_INTERVAL_MS as u64 + 100) * 1_000_000,
            Ordering::Relaxed,
        );
        let result = ad.measure(250_000_000);
        assert!(result.is_some());
        assert!(ad.current_target() > INITIAL_CONCURRENCY);
    }

    #[test]
    fn measure_caps_at_max() {
        let ad = AdaptiveDownload::new();
        ad.target.store(MAX_CONCURRENCY, Ordering::Relaxed);
        ad.last_time.store(
            now_nanos() - (MEASURE_INTERVAL_MS as u64 + 100) * 1_000_000,
            Ordering::Relaxed,
        );
        ad.last_throughput.store(1000 * 1000, Ordering::Relaxed);
        ad.last_bytes.store(0, Ordering::Relaxed);
        assert_eq!(ad.measure(999_999_999), None);
    }
}
