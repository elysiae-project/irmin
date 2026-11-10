//! Adaptive download concurrency controller.
//!
//! Uses gradient hill-climbing with multiplicative probing (BBR-style
//! probe/settle) and multiplicative backoff. Starts with few workers to
//! keep RSS low on slow networks, then scales up aggressively when
//! bandwidth is not saturated. Scales down when throughput plateaus or
//! decreases, shedding unnecessary workers to reduce memory.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Instant;

/// Minimum workers — ensures progress even on very slow links.
pub const MIN_CONCURRENCY: usize = 2;
/// Starting workers — low enough for slow links, high enough for fast links
/// to ramp up via doubling within a few seconds.
pub const INITIAL_CONCURRENCY: usize = 4;
/// Hard ceiling on concurrent chunk download workers.
/// Derived from available parallelism at runtime so fast multi-core machines
/// get more workers automatically. Capped at 128 to avoid excessive fd usage.
pub fn max_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| (n.get() * 4).min(128))
        .unwrap_or(48)
}
/// Backwards-compatible constant for code that needs a const value.
/// Uses the prior fixed ceiling; runtime code should call [`max_concurrency`].
pub const MAX_CONCURRENCY: usize = 48;
/// Interval between throughput measurements.
const MEASURE_INTERVAL_MS: u64 = 750;
/// Ticks to skip after a concurrency change (let throughput stabilize).
const SETTLE_TICKS: u32 = 2;
/// EWMA smoothing factor.
const ALPHA: f64 = 0.4;
/// Need >=50% of linear scaling gain to keep doubling.
const GAIN_UP: f64 = 0.5;
/// Below 10% of linear gain means too many workers.
const GAIN_HOLD: f64 = 0.1;
/// Throughput drop threshold for multiplicative backoff.
const BACKOFF_THRESHOLD: f64 = 0.95;
/// Multiplicative backoff factor.
const BACKOFF_FACTOR: f64 = 0.75;
/// Shed factor when throughput plateaus.
const SHED_FACTOR: f64 = 0.8;
/// Periodic re-probe interval (ticks of stable throughput).
const REPROBE_TICKS: u32 = 20;

/// Tracks throughput and decides when to adjust worker count.
pub struct AdaptiveDownload {
    /// Current concurrency target.
    target: AtomicUsize,
    /// Smoothed throughput (bytes/sec * 1000 for fixed-point).
    ewma: AtomicU64,
    /// Bytes counter snapshot from previous measurement.
    last_bytes: AtomicU64,
    /// Timestamp of previous measurement (nanos since epoch).
    last_time: AtomicU64,
    /// Workers at last change point (for efficiency calculation).
    prev_workers: AtomicUsize,
    /// Throughput at last change point (bytes/sec * 1000).
    prev_throughput: AtomicU64,
    /// Settle counter: skip decisions for this many ticks after a change.
    settle: AtomicU64,
    /// Stable counter: ticks since last target change.
    stable: AtomicU64,
}

impl AdaptiveDownload {
    pub fn new() -> Self {
        Self {
            target: AtomicUsize::new(INITIAL_CONCURRENCY),
            ewma: AtomicU64::new(0),
            last_bytes: AtomicU64::new(0),
            last_time: AtomicU64::new(now_nanos()),
            prev_workers: AtomicUsize::new(INITIAL_CONCURRENCY),
            prev_throughput: AtomicU64::new(0),
            settle: AtomicU64::new(SETTLE_TICKS as u64),
            stable: AtomicU64::new(0),
        }
    }

    /// Current worker count target.
    pub fn current_target(&self) -> usize {
        self.target.load(Ordering::Relaxed)
    }

    /// Called periodically with cumulative downloaded bytes.
    /// Returns Some(new_target) if the target changed.
    pub fn measure(&self, total_bytes: u64) -> Option<usize> {
        let now = now_nanos();
        let prev_time = self.last_time.load(Ordering::Relaxed);
        let elapsed_ns = now.saturating_sub(prev_time);
        if elapsed_ns < MEASURE_INTERVAL_MS * 1_000_000 {
            return None;
        }

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

        let prev_smoothed = self.ewma.load(Ordering::Relaxed) as f64 / 1000.0;
        let throughput = if prev_smoothed == 0.0 {
            raw_throughput
        } else {
            ALPHA * raw_throughput + (1.0 - ALPHA) * prev_smoothed
        };
        self.ewma
            .store((throughput * 1000.0) as u64, Ordering::Relaxed);

        let settle = self.settle.load(Ordering::Relaxed);
        if settle > 0 {
            self.settle.store(settle - 1, Ordering::Relaxed);
            return None;
        }

        let current = self.target.load(Ordering::Relaxed);
        let n0 = self.prev_workers.load(Ordering::Relaxed);
        let t0 = self.prev_throughput.load(Ordering::Relaxed) as f64 / 1000.0;
        let n1 = current as f64;
        let t1 = throughput;

        if t0 < 1.0 {
            self.prev_workers.store(current, Ordering::Relaxed);
            self.prev_throughput
                .store((throughput * 1000.0) as u64, Ordering::Relaxed);
            return None;
        }

        let n0_f = n0 as f64;
        let expected = t0 * (n1 / n0_f) - t0;
        let actual = t1 - t0;
        let eff = if expected.abs() > 1.0 {
            actual / expected
        } else if n0 == current {
            // No prior change to compare against: treat high throughput as
            // signal to probe upward, low throughput as signal to hold.
            if t1 > prev_smoothed * 1.05 { 1.0 } else { 0.0 }
        } else {
            0.0
        };

        let next = if t1 < t0 * BACKOFF_THRESHOLD {
            ((n1 * BACKOFF_FACTOR) as usize)
                .max((n0 as f64 * BACKOFF_FACTOR) as usize)
                .max(MIN_CONCURRENCY)
        } else if eff >= GAIN_UP {
            ((n1 * 2.0) as usize).min(max_concurrency())
        } else if eff >= GAIN_HOLD {
            (current + (current / 4).max(1)).min(max_concurrency())
        } else {
            ((n1 * SHED_FACTOR) as usize)
                .max(n0.min(current))
                .max(MIN_CONCURRENCY)
        };

        let next = next.max(MIN_CONCURRENCY).min(max_concurrency());

        if next != current {
            self.prev_workers.store(current, Ordering::Relaxed);
            self.prev_throughput
                .store((throughput * 1000.0) as u64, Ordering::Relaxed);
            self.target.store(next, Ordering::Relaxed);
            self.settle.store(SETTLE_TICKS as u64, Ordering::Relaxed);
            self.stable.store(0, Ordering::Relaxed);
            return Some(next);
        }

        let stable = self.stable.fetch_add(1, Ordering::Relaxed) + 1;
        if stable >= REPROBE_TICKS as u64 {
            let probe = (current + current / 4 + 1).min(max_concurrency());
            self.prev_workers.store(current, Ordering::Relaxed);
            self.prev_throughput
                .store((throughput * 1000.0) as u64, Ordering::Relaxed);
            self.target.store(probe, Ordering::Relaxed);
            self.settle.store(SETTLE_TICKS as u64, Ordering::Relaxed);
            self.stable.store(0, Ordering::Relaxed);
            return Some(probe);
        }

        None
    }
}

impl Default for AdaptiveDownload {
    fn default() -> Self {
        Self::new()
    }
}

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

fn now_nanos() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn initial_target_is_small() {
        let ad = AdaptiveDownload::new();
        assert_eq!(ad.current_target(), INITIAL_CONCURRENCY);
        assert!(INITIAL_CONCURRENCY <= 8);
    }

    #[test]
    fn measure_below_interval_returns_none() {
        let ad = AdaptiveDownload::new();
        assert_eq!(ad.measure(1000), None);
    }

    #[test]
    fn scales_up_on_throughput_growth() {
        let _ = now_nanos();
        thread::sleep(Duration::from_millis(MEASURE_INTERVAL_MS + 50));
        let ad = AdaptiveDownload::new();
        ad.settle.store(0, Ordering::Relaxed);
        ad.prev_throughput
            .store(1_000_000_000_000, Ordering::Relaxed);
        ad.ewma.store(1_000_000_000_000, Ordering::Relaxed);
        ad.prev_workers
            .store(INITIAL_CONCURRENCY, Ordering::Relaxed);
        ad.last_bytes.store(0, Ordering::Relaxed);
        ad.last_time.store(0, Ordering::Relaxed);
        let result = ad.measure(10_000_000_000_000);
        assert!(
            result.is_some(),
            "expected scale-up, throughput went from 1GiB to 10GiB"
        );
        assert!(ad.current_target() > INITIAL_CONCURRENCY);
    }

    #[test]
    fn backs_off_on_throughput_drop() {
        let _ = now_nanos();
        thread::sleep(Duration::from_millis(MEASURE_INTERVAL_MS + 50));
        let ad = AdaptiveDownload::new();
        ad.target.store(16, Ordering::Relaxed);
        ad.prev_workers.store(8, Ordering::Relaxed);
        ad.prev_throughput
            .store(100_000_000_000_000, Ordering::Relaxed);
        ad.ewma.store(100_000_000_000_000, Ordering::Relaxed);
        ad.settle.store(0, Ordering::Relaxed);
        ad.last_bytes.store(0, Ordering::Relaxed);
        ad.last_time.store(0, Ordering::Relaxed);
        let result = ad.measure(1_000_000_000);
        assert!(
            result.is_some(),
            "expected backoff, throughput dropped from 100GiB to ~1GiB"
        );
        assert!(ad.current_target() < 16);
        assert!(ad.current_target() >= MIN_CONCURRENCY);
    }

    #[test]
    fn sheds_on_plateau() {
        let _ = now_nanos();
        thread::sleep(Duration::from_millis(MEASURE_INTERVAL_MS + 50));
        let ad = AdaptiveDownload::new();
        ad.target.store(32, Ordering::Relaxed);
        ad.prev_workers.store(16, Ordering::Relaxed);
        ad.prev_throughput.store(100_000_000_000, Ordering::Relaxed);
        ad.ewma.store(100_000_000_000, Ordering::Relaxed);
        ad.settle.store(0, Ordering::Relaxed);
        ad.last_bytes.store(0, Ordering::Relaxed);
        ad.last_time.store(0, Ordering::Relaxed);
        let result = ad.measure(75_000_000_000);
        if result.is_some() {
            assert!(ad.current_target() < 32);
        }
    }

    #[test]
    fn caps_at_max() {
        let _ = now_nanos();
        thread::sleep(Duration::from_millis(MEASURE_INTERVAL_MS + 50));
        let ad = AdaptiveDownload::new();
        ad.target.store(max_concurrency(), Ordering::Relaxed);
        ad.prev_workers
            .store(max_concurrency() / 2, Ordering::Relaxed);
        ad.prev_throughput.store(1_000_000_000, Ordering::Relaxed);
        ad.ewma.store(1_000_000_000_000, Ordering::Relaxed);
        ad.settle.store(0, Ordering::Relaxed);
        ad.last_bytes.store(0, Ordering::Relaxed);
        ad.last_time.store(0, Ordering::Relaxed);
        let result = ad.measure(999_999_999_999);
        if result.is_some() {
            assert!(ad.current_target() <= max_concurrency());
        }
    }

    #[test]
    fn never_goes_below_min() {
        let _ = now_nanos();
        thread::sleep(Duration::from_millis(MEASURE_INTERVAL_MS + 50));
        let ad = AdaptiveDownload::new();
        ad.target.store(MIN_CONCURRENCY, Ordering::Relaxed);
        ad.prev_workers.store(1, Ordering::Relaxed);
        ad.prev_throughput.store(100_000_000, Ordering::Relaxed);
        ad.ewma.store(1_000, Ordering::Relaxed);
        ad.settle.store(0, Ordering::Relaxed);
        ad.last_bytes.store(0, Ordering::Relaxed);
        ad.last_time.store(0, Ordering::Relaxed);
        let _ = ad.measure(0);
        assert!(ad.current_target() >= MIN_CONCURRENCY);
    }
}
