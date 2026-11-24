//! Sophon chunk-based game installer.
mod adaptive_assembly;
/// Throughput-based controller for scaling concurrent chunk requests.
pub mod adaptive_download;
mod api;
/// Chunk validation, file assembly, and temporary-file cleanup.
pub mod assembly;
/// Batched durability for assembled files (epoch flush + rename commit).
pub mod assembly_epochs;
#[cfg(feature = "benchmark")]
pub mod assembly_opt;
#[cfg(not(feature = "benchmark"))]
mod assembly_opt;
mod assembly_workers;
/// Persistent and in-memory file verification cache.
pub mod cache;
/// Per-file SIMD batch hasher for deferred chunk-decompressed MD5 checks
/// during parallel assembly (VerifyMode::Full).
pub mod chunk_batch_hasher;
/// Disk-backed completion bitmap used to resume eager decompression.
pub mod chunk_bitmap;
/// Memory-compact columnar manifest representation.
pub mod compact_manifest;
pub(crate) mod decompress_oneshot;
mod download;
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub use download::benchmark as download_benchmark;
#[cfg(feature = "bench-diagnostics")]
#[doc(hidden)]
pub use download::{
    DownloadHashAlgorithm, DownloadPhaseSamples, DownloadPhaseTimings,
    benchmark_download_chunk_with_timings,
};
/// Streaming decompression directly into output file ranges.
pub mod eager_decompress;
mod error;
mod game_filters;
mod handle;
#[cfg(all(feature = "patch", feature = "benchmark"))]
pub mod hdiffpatch;
#[cfg(all(feature = "patch", not(feature = "benchmark")))]
mod hdiffpatch;
/// Manifest planning and download/assembly orchestration.
pub mod installer;
/// SIMD multi-buffer MD5 verification for batches of independent files.
/// Benchmark-only (not compiled into production paths); the production
/// consumer of the md5-simd kernel is the per-file chunk batch hasher.
#[cfg(feature = "bench-internals")]
pub mod md5_multibuffer;
/// Output-file preallocation and shared file-handle management.
pub mod output_allocator;
#[cfg(feature = "plugins")]
mod plugin_api;
#[cfg(feature = "plugins")]
mod plugin_install;
#[cfg(feature = "preinstall")]
pub mod preinstall;
#[cfg(not(feature = "bench-internals"))]
#[cfg(feature = "update")]
mod update;
#[cfg(feature = "bench-internals")]
#[cfg(feature = "update")]
pub mod update;

mod profiling;
mod snapshot;
/// Platform file-copy and preallocation helpers.
pub mod sysio;

#[cfg(test)]
mod eager_integration_tests;
#[cfg(test)]
mod integration_tests;

/// Maximum attempts for a failed chunk download.
pub const MAX_RETRIES: u32 = 5;
/// Maximum retries after a downloaded chunk fails integrity validation.
pub const MAX_HASH_RETRIES: u32 = 5;

/// Hard ceiling for concurrent chunk download workers.
/// Maximum download concurrency, derived from CPU count at runtime.
#[inline]
pub fn download_concurrency() -> usize {
    adaptive_download::max_concurrency()
}

use std::time::Duration;

/// Exponential retry delay with up to one second of jitter.
///
/// The exponential component doubles per attempt, caps at 30 seconds, and the
/// attempt is capped before shifting to avoid overflow.
pub fn retry_delay(attempt: u32) -> Duration {
    let exp = 1000u64.saturating_mul(1u64 << attempt.min(5));
    // Jitter prevents thundering-herd when multiple chunks fail at once.
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let jitter = (seed.wrapping_mul(1103515245)).wrapping_add(12345) % 1000;
    Duration::from_millis(exp.min(30_000) + jitter)
}

/// Sleep for `delay`, returning `Err(())` if the handle is cancelled first.
pub async fn cancelable_sleep(
    handle: &crate::game_installer::handle::DownloadHandle,
    delay: Duration,
) -> Result<(), ()> {
    tokio::select! {
        _ = tokio::time::sleep(delay) => Ok(()),
        _ = async {
            loop {
                if handle.is_cancelled() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        } => Err(()),
    }
}
/// Assembly worker count while chunk downloads are active. Capped at 4
/// so assembly never starves download workers, regardless of CPU count.
#[inline]
pub fn assembly_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(4))
        .unwrap_or(4)
}
/// Backwards-compatible constant; retained because installer.rs is
/// codegen-sensitive and changing its call sites risks state_save
/// canary regression. New code should call [`assembly_concurrency`].
pub const ASSEMBLY_CONCURRENCY: usize = 4;
/// Parallelism for full-file or cached-chunk MD5 verification passes.
/// 2x CPU count because verification is I/O-bound; capped at 32 to
/// avoid memory pressure from concurrent file reads.
pub fn verify_concurrency() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_mul(2).min(32))
        .unwrap_or(16)
}
/// Backwards-compatible constant; retained because installer.rs is
/// codegen-sensitive. New code should call [`verify_concurrency`].
pub const VERIFY_CONCURRENCY: usize = 16;
/// Installed version marker filename.
pub const VERSION_FILE_NAME: &str = ".sophon_version";
/// MD5 verification cache filename.
pub const VERIFICATION_CACHE_FILE: &str = ".sophon_verify_cache";

/// Controls how aggressively the installer verifies data integrity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Hash compressed stream during download, decompressed data during
    /// assembly, and file-level hash where applicable. Maximum safety.
    #[default]
    Full,
    /// Skip per-chunk hashing. Verify only the file-level MD5 after assembly.
    /// All chunks use the eager path (no .zstd intermediates). Crash-resume is
    /// disabled — interrupted downloads must re-download incomplete files.
    Deferred,
    /// Skip all hash verification. Size checks only.
    None,
}

/// File write buffer during assembly (zstd streaming input + output).
pub const FILE_WRITE_BUFFER_SIZE: usize = 512 * 1024;
/// File write buffer during chunk downloads.
pub const CHUNK_WRITE_BUFFER_SIZE: usize = 512 * 1024;

/// Minimum progress update interval (ms).
pub const PROGRESS_UPDATE_INTERVAL_MS: u64 = 1000;

/// Download speed smoothing window (seconds).
pub const SPEED_SMOOTH_WINDOW_SECS: f64 = 5.0;

/// Update an EWMA-smoothed value stored as a scaled u64.
#[inline]
pub fn ewma_update(atomic: &std::sync::atomic::AtomicU64, raw_value: f64, alpha: f64) -> f64 {
    const SCALE: f64 = 1000.0;
    use std::sync::atomic::Ordering;
    let prev_raw = atomic.load(Ordering::Relaxed);
    let prev = prev_raw as f64 / SCALE;
    let new_val = if prev == 0.0 {
        raw_value
    } else {
        alpha * raw_value + (1.0 - alpha) * prev
    };
    atomic.store((new_val * SCALE) as u64, Ordering::Release);
    new_val
}

/// ETA speed sample window size.
pub const ETA_WINDOW_SAMPLES: usize = 30;
/// Minimum samples before ETA is displayed.
pub const ETA_MIN_SAMPLES: usize = 5;

/// Compute ETA speed using median filtering over recent speed samples.
/// Returns 0.0 if fewer than `ETA_MIN_SAMPLES` are available.
pub fn compute_eta_speed(
    history: &std::sync::Mutex<std::collections::VecDeque<f64>>,
    instant_speed: f64,
) -> f64 {
    let mut samples = history.lock().unwrap_or_else(|err| err.into_inner());
    samples.push_back(instant_speed);
    if samples.len() > ETA_WINDOW_SAMPLES {
        samples.pop_front();
    }
    if samples.len() < ETA_MIN_SAMPLES {
        return 0.0;
    }
    let n = samples.len();
    let mut buf = [0.0f64; ETA_WINDOW_SAMPLES];
    let slice = samples.make_contiguous();
    buf[..n].copy_from_slice(slice);
    let mid = n / 2;
    buf[..n].select_nth_unstable_by(mid, |a, b| a.total_cmp(b));
    if n.is_multiple_of(2) {
        // For even count, average the two middle elements.
        let left_max = buf[..mid].iter().copied().fold(f64::NEG_INFINITY, f64::max);
        (left_max + buf[mid]) / 2.0
    } else {
        buf[mid]
    }
}

/// Service endpoint used to resolve a game id to its current branches.
pub const FRONT_DOOR_URL: &str = concat!(
    "\x68\x74\x74\x70\x73\x3a\x2f\x2f\x73\x67\x2d\x68\x79\x70\x2d\x61\x70\x69\x2e\x68\x6f\x79\x6f\x76\x65\x72\x73\x65\x2e\x63\x6f\x6d",
    "\x2f\x68\x79\x70\x2f\x68\x79\x70\x2d\x63\x6f\x6e\x6e\x65\x63\x74",
    "/api/getGameBranches?launcher_id=VYTpXlbWo8"
);
/// Base endpoint used to fetch build metadata.
pub const SOPHON_BUILD_URL_BASE: &str = concat!(
    "\x68\x74\x74\x70\x73\x3a\x2f\x2f\x73\x67\x2d\x70\x75\x62\x6c\x69\x63\x2d\x61\x70\x69\x2e\x68\x6f\x79\x6f\x76\x65\x72\x73\x65\x2e\x63\x6f\x6d",
    "/downloader/sophon_chunk/api/getBuild"
);

/// Test/bench-only endpoint overrides. When set, `front_door_url()`,
/// `sophon_build_url_base()`, and `sophon_patch_build_url_base()` return the
/// override instead of the production constant. This lets end-to-end tests
/// drive the full `Sophon` public API against a wiremock server without
/// changing the public API surface. Production builds compile this out.
#[cfg(any(test, feature = "bench-internals"))]
pub(crate) mod test_endpoints {
    use std::cell::RefCell;

    thread_local! {
        static FRONT_DOOR: RefCell<Option<String>> = const { RefCell::new(None) };
        static BUILD_BASE: RefCell<Option<String>> = const { RefCell::new(None) };
        #[cfg(feature = "preinstall")]
        static PATCH_BUILD_BASE: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub fn set_front_door(url: Option<String>) {
        FRONT_DOOR.with(|c| *c.borrow_mut() = url);
    }
    pub fn set_build_base(url: Option<String>) {
        BUILD_BASE.with(|c| *c.borrow_mut() = url);
    }
    #[cfg(feature = "preinstall")]
    pub fn set_patch_build_base(url: Option<String>) {
        PATCH_BUILD_BASE.with(|c| *c.borrow_mut() = url);
    }

    pub fn front_door() -> Option<String> {
        FRONT_DOOR.with(|c| c.borrow().clone())
    }
    pub fn build_base() -> Option<String> {
        BUILD_BASE.with(|c| c.borrow().clone())
    }
    #[cfg(feature = "preinstall")]
    pub fn patch_build_base() -> Option<String> {
        PATCH_BUILD_BASE.with(|c| c.borrow().clone())
    }
}

/// Re-export the test endpoint setters for integration tests under
/// `tests/`, gated the same way as the override mechanism itself.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod bench_test_endpoints {
    #[cfg(feature = "preinstall")]
    pub use super::test_endpoints::set_patch_build_base;
    pub use super::test_endpoints::{set_build_base, set_front_door};
}

/// Resolve the front-door URL, honoring test overrides when present.
pub(crate) fn front_door_url() -> String {
    #[cfg(any(test, feature = "bench-internals"))]
    {
        test_endpoints::front_door().unwrap_or_else(|| FRONT_DOOR_URL.to_string())
    }
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        FRONT_DOOR_URL.to_string()
    }
}

/// Resolve the build URL base, honoring test overrides when present.
pub(crate) fn sophon_build_url_base() -> String {
    #[cfg(any(test, feature = "bench-internals"))]
    {
        test_endpoints::build_base().unwrap_or_else(|| SOPHON_BUILD_URL_BASE.to_string())
    }
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        SOPHON_BUILD_URL_BASE.to_string()
    }
}

/// Base endpoint for patch-build metadata requests.
#[cfg(feature = "preinstall")]
pub const SOPHON_PATCH_BUILD_URL_BASE: &str = concat!(
    "\x68\x74\x74\x70\x73\x3a\x2f\x2f\x73\x67\x2d\x70\x75\x62\x6c\x69\x63\x2d\x61\x70\x69\x2e\x68\x6f\x79\x6f\x76\x65\x72\x73\x65\x2e\x63\x6f\x6d",
    "/downloader/sophon_chunk/api/getPatchBuild"
);

/// Resolve the patch-build URL base, honoring test overrides when present.
#[cfg(feature = "preinstall")]
pub(crate) fn sophon_patch_build_url_base() -> String {
    #[cfg(any(test, feature = "bench-internals"))]
    {
        test_endpoints::patch_build_base()
            .unwrap_or_else(|| SOPHON_PATCH_BUILD_URL_BASE.to_string())
    }
    #[cfg(not(any(test, feature = "bench-internals")))]
    {
        SOPHON_PATCH_BUILD_URL_BASE.to_string()
    }
}

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

fn version_file_path(game_dir: &Path) -> PathBuf {
    game_dir.join(VERSION_FILE_NAME)
}

/// Read and trim the installed tag marker; returns `None` if it cannot be read.
pub fn read_installed_tag(game_dir: &Path) -> Option<String> {
    fs::read_to_string(version_file_path(game_dir))
        .ok()
        .map(|s| s.trim().to_owned())
}

/// Replace the installed tag marker with `tag`, returning filesystem errors.
pub fn write_installed_tag(game_dir: &Path, tag: &str) -> io::Result<()> {
    fs::write(version_file_path(game_dir), tag)
}

pub use crate::types::CompletedFiles;
pub use assembly::validate_asset_name;
pub use error::SophonError;
pub use handle::ControlState;
pub use handle::DownloadHandle;
#[cfg(feature = "verify")]
pub use installer::verify_integrity;
pub use installer::{InstallCallbacks, InstallOptions, ResumeContext, StateSaver, install};
#[cfg(feature = "update")]
pub use installer::{assign_chunk_offsets, build_update_installers, intern_old_chunk_offsets};
#[cfg(feature = "download")]
pub use installer::{build_installers, build_installers_for_tag};
#[cfg(feature = "plugins")]
pub use plugin_install::{install_channel_sdks, install_plugins};
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::benchmark_download_over_bounded_drain_stream;
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::benchmark_download_over_bounded_pending_stream;
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::benchmark_download_over_bounded_stream;
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::benchmark_download_over_chunk_future;
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::benchmark_download_over_future_batch;
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::prepare_synthetic_apply_state as prepare_benchmark_preinstall_state;
#[cfg(all(feature = "benchmark", feature = "preinstall"))]
pub use preinstall::prepare_synthetic_copyover_state as prepare_benchmark_copyover_state;
#[cfg(all(feature = "bench-diagnostics", feature = "preinstall"))]
pub use preinstall::{HdiffPhaseTimings, apply_synthetic_hdiff_for_benchmark};
#[cfg(feature = "preinstall")]
pub use preinstall::{apply_preinstall, build_preinstall_plan, preinstall_download};
#[cfg(feature = "update")]
pub use update::{UpdateInfo, check_update};

/// Advance jemalloc epoch and trigger a background decay pass. Call after
/// major sophon phase transitions (download → assembly, etc.) to reduce
/// resident memory.
pub fn reclaim_memory() {
    #[cfg(feature = "sophon-profiling")]
    {
        let _ = tikv_jemalloc_ctl::epoch::advance();
    }
    #[cfg(feature = "patch")]
    hdiffpatch::trim_idle_buffers();
    assembly_opt::trim_thread_local_buffers();
    assembly::trim_transfer_buffer();
    // Return freed glibc arena memory to the OS. Without this, freed
    // allocations stay in per-thread arenas and never get unmapped.
    unsafe {
        libc::malloc_trim(0);
    }
}

/// Raise RLIMIT_NOFILE to the hard limit. The eager decompression path may
/// open many output files concurrently. Call early in process startup.
pub fn raise_fd_limit() {
    unsafe {
        let mut rlim: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rlim) == 0 && rlim.rlim_cur < rlim.rlim_max {
            rlim.rlim_cur = rlim.rlim_max;
            libc::setrlimit(libc::RLIMIT_NOFILE, &rlim);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn read_installed_tag_present() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(VERSION_FILE_NAME), "1.2.3").unwrap();
        assert_eq!(read_installed_tag(dir.path()), Some("1.2.3".to_string()));
    }

    #[test]
    fn read_installed_tag_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_installed_tag(dir.path()), None);
    }

    #[test]
    fn write_read_installed_tag_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        write_installed_tag(dir.path(), "2.0.0").unwrap();
        assert_eq!(read_installed_tag(dir.path()), Some("2.0.0".to_string()));
    }

    #[test]
    fn retry_delay_exponential() {
        // Base delays with jitter (0-999ms added)
        assert!(retry_delay(0) >= Duration::from_millis(1000));
        assert!(retry_delay(0) < Duration::from_millis(2000));
        assert!(retry_delay(1) >= Duration::from_millis(2000));
        assert!(retry_delay(1) < Duration::from_millis(3000));
        assert!(retry_delay(2) >= Duration::from_millis(4000));
        assert!(retry_delay(2) < Duration::from_millis(5000));
        assert!(retry_delay(3) >= Duration::from_millis(8000));
        assert!(retry_delay(3) < Duration::from_millis(9000));
        assert!(retry_delay(4) >= Duration::from_millis(16000));
        assert!(retry_delay(4) < Duration::from_millis(17000));
    }

    #[test]
    fn retry_delay_capped_at_30s() {
        // Capped at 30000 + jitter (up to ~30999ms)
        assert!(retry_delay(5) >= Duration::from_millis(30000));
        assert!(retry_delay(5) < Duration::from_millis(31000));
        assert!(retry_delay(10) >= Duration::from_millis(30000));
        assert!(retry_delay(10) < Duration::from_millis(31000));
        assert!(retry_delay(100) >= Duration::from_millis(30000));
        assert!(retry_delay(100) < Duration::from_millis(31000));
    }

    #[tokio::test]
    async fn cancelable_sleep_completes_normally() {
        let handle = DownloadHandle::new();
        let result = cancelable_sleep(&handle, Duration::from_millis(10)).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cancelable_sleep_returns_err_on_cancel() {
        let handle = DownloadHandle::new();
        handle.cancel();
        let result = cancelable_sleep(&handle, Duration::from_secs(30)).await;
        assert!(result.is_err());
    }

    #[test]
    fn ewma_update_first_sample_is_raw() {
        let atomic = std::sync::atomic::AtomicU64::new(0);
        let val = ewma_update(&atomic, 1000.0, 0.2);
        assert!((val - 1000.0).abs() < 0.01);
    }

    #[test]
    fn ewma_update_smoothing_converges() {
        let atomic = std::sync::atomic::AtomicU64::new(0);
        ewma_update(&atomic, 1000.0, 0.2);
        let val = ewma_update(&atomic, 500.0, 0.2);
        assert!((val - 900.0).abs() < 0.01);
        let val = ewma_update(&atomic, 500.0, 0.2);
        assert!(val < 900.0 && val > 500.0);
    }

    #[test]
    fn ewma_update_speed_window_alpha() {
        let alpha = 1.0 / (SPEED_SMOOTH_WINDOW_SECS * 1000.0 / PROGRESS_UPDATE_INTERVAL_MS as f64);
        assert!((alpha - 0.2).abs() < 0.001);
    }

    #[test]
    fn compute_eta_speed_returns_zero_with_few_samples() {
        let history = std::sync::Mutex::new(std::collections::VecDeque::new());
        assert_eq!(compute_eta_speed(&history, 1000.0), 0.0);
        assert_eq!(compute_eta_speed(&history, 2000.0), 0.0);
        assert_eq!(compute_eta_speed(&history, 3000.0), 0.0);
        assert_eq!(compute_eta_speed(&history, 4000.0), 0.0);
    }

    #[test]
    fn compute_eta_speed_median_with_enough_samples() {
        let history = std::sync::Mutex::new(std::collections::VecDeque::new());
        for _ in 0..4 {
            compute_eta_speed(&history, 1000.0);
        }
        let result = compute_eta_speed(&history, 1000.0);
        assert!((result - 1000.0).abs() < 0.01);
    }

    #[test]
    fn compute_eta_speed_median_rejects_outlier() {
        let history = std::sync::Mutex::new(std::collections::VecDeque::new());
        for _ in 0..4 {
            compute_eta_speed(&history, 100.0);
        }
        let result = compute_eta_speed(&history, 10000.0);
        assert!((result - 100.0).abs() < 0.01);
    }

    #[test]
    fn compute_eta_speed_window_bounds() {
        let history = std::sync::Mutex::new(std::collections::VecDeque::new());
        for i in 0..ETA_WINDOW_SAMPLES + 5 {
            compute_eta_speed(&history, i as f64 * 100.0);
        }
        let guard = history.lock().unwrap();
        assert!(guard.len() <= ETA_WINDOW_SAMPLES);
    }
}
