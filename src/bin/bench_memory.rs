#[cfg(feature = "bench-alloc-stats")]
use std::alloc::{GlobalAlloc, Layout, System};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;

#[cfg(feature = "bench-alloc-stats")]
mod allocation_stats {
    use super::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    struct CountingAllocator;

    static LIVE_BYTES: AtomicU64 = AtomicU64::new(0);
    static OPERATION_PEAK_DELTA: AtomicU64 = AtomicU64::new(0);
    static OPERATION_BASELINE_BYTES: AtomicU64 = AtomicU64::new(0);
    static OPERATION_ACTIVE: AtomicBool = AtomicBool::new(false);
    static PEAK_LOCK: AtomicBool = AtomicBool::new(false);
    static ALLOCATION_WINDOW_LOCK: AtomicBool = AtomicBool::new(false);
    static ALLOCATION_REQUEST_BYTES: AtomicU64 = AtomicU64::new(0);

    #[cfg(not(target_env = "msvc"))]
    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    #[cfg(target_env = "msvc")]
    compile_error!("bench-alloc-stats currently requires a non-MSVC target");

    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc(layout) };
            if !ptr.is_null() {
                update_live(layout.size() as i128, layout.size() as u64);
            }
            ptr
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            let ptr = unsafe { System.alloc_zeroed(layout) };
            if !ptr.is_null() {
                update_live(layout.size() as i128, layout.size() as u64);
            }
            ptr
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            update_live(-(layout.size() as i128), 0);
            unsafe { System.dealloc(ptr, layout) };
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
            if !new_ptr.is_null() {
                let old_size = layout.size() as u64;
                let new_size = new_size as u64;
                if new_size >= old_size {
                    update_live((new_size - old_size) as i128, new_size);
                } else {
                    update_live(-((old_size - new_size) as i128), new_size);
                }
            }
            new_ptr
        }
    }

    struct FlagGuard(&'static AtomicBool);

    impl FlagGuard {
        fn acquire(flag: &'static AtomicBool) -> Self {
            while flag
                .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                std::hint::spin_loop();
            }
            Self(flag)
        }
    }

    impl Drop for FlagGuard {
        fn drop(&mut self) {
            self.0.store(false, Ordering::Release);
        }
    }

    fn update_live(delta: i128, request_bytes: u64) {
        let _guard = FlagGuard::acquire(&PEAK_LOCK);
        let current = LIVE_BYTES.load(Ordering::Relaxed);
        let live = if delta >= 0 {
            current.saturating_add(delta as u64)
        } else {
            current.saturating_sub((-delta) as u64)
        };
        LIVE_BYTES.store(live, Ordering::Relaxed);
        if OPERATION_ACTIVE.load(Ordering::Relaxed) {
            let baseline = OPERATION_BASELINE_BYTES.load(Ordering::Relaxed);
            OPERATION_PEAK_DELTA.fetch_max(live.saturating_sub(baseline), Ordering::Relaxed);
            ALLOCATION_REQUEST_BYTES.fetch_add(request_bytes, Ordering::Relaxed);
        }
    }

    pub fn start() -> Result<(), &'static str> {
        let window_guard = FlagGuard::acquire(&ALLOCATION_WINDOW_LOCK);
        let peak_guard = FlagGuard::acquire(&PEAK_LOCK);
        if OPERATION_ACTIVE.load(Ordering::Relaxed) {
            drop(peak_guard);
            drop(window_guard);
            return Err("allocator measurement windows cannot overlap");
        }
        let baseline = LIVE_BYTES.load(Ordering::SeqCst);
        OPERATION_BASELINE_BYTES.store(baseline, Ordering::SeqCst);
        OPERATION_PEAK_DELTA.store(0, Ordering::SeqCst);
        ALLOCATION_REQUEST_BYTES.store(0, Ordering::Relaxed);
        OPERATION_ACTIVE.store(true, Ordering::Release);
        drop(peak_guard);
        drop(window_guard);
        Ok(())
    }

    pub fn finish() -> Result<AllocationSnapshot, &'static str> {
        let window_guard = FlagGuard::acquire(&ALLOCATION_WINDOW_LOCK);
        let peak_guard = FlagGuard::acquire(&PEAK_LOCK);
        if !OPERATION_ACTIVE.load(Ordering::Relaxed) {
            drop(peak_guard);
            drop(window_guard);
            return Err("allocator measurement window is not active");
        }
        OPERATION_ACTIVE.store(false, Ordering::Release);
        let baseline = OPERATION_BASELINE_BYTES.load(Ordering::SeqCst);
        let snapshot = AllocationSnapshot {
            peak_live_bytes: OPERATION_PEAK_DELTA.load(Ordering::Relaxed),
            live_bytes_at_end: LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(baseline),
            allocation_request_bytes: ALLOCATION_REQUEST_BYTES.load(Ordering::Relaxed),
        };
        drop(peak_guard);
        drop(window_guard);
        Ok(snapshot)
    }

    pub struct AllocationWindow {
        active: bool,
    }

    impl AllocationWindow {
        pub fn start() -> Result<Self, &'static str> {
            start()?;
            Ok(Self { active: true })
        }

        pub fn finish(mut self) -> Result<AllocationSnapshot, &'static str> {
            let snapshot = finish()?;
            self.active = false;
            Ok(snapshot)
        }
    }

    pub(super) fn current_live_growth() -> Result<Option<u64>, &'static str> {
        let _window_guard = FlagGuard::acquire(&ALLOCATION_WINDOW_LOCK);
        let _peak_guard = FlagGuard::acquire(&PEAK_LOCK);
        if !OPERATION_ACTIVE.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let baseline = OPERATION_BASELINE_BYTES.load(Ordering::Relaxed);
        Ok(Some(
            LIVE_BYTES.load(Ordering::Relaxed).saturating_sub(baseline),
        ))
    }

    impl Drop for AllocationWindow {
        fn drop(&mut self) {
            if self.active {
                let _ = finish();
                self.active = false;
            }
        }
    }

    #[derive(Clone, Copy, Debug, serde::Serialize)]
    pub struct AllocationSnapshot {
        /// Maximum positive live requested-byte growth over the operation baseline.
        pub peak_live_bytes: u64,
        /// Live requested-byte growth still present when the operation ended.
        pub live_bytes_at_end: u64,
        /// Successful alloc/zeroed sizes plus successful realloc destination sizes.
        pub allocation_request_bytes: u64,
    }

    #[cfg(test)]
    mod tests {
        use super::AllocationWindow;

        #[test]
        fn allocator_measurement_tracks_requests_and_window_lifecycle() {
            let existing = vec![0u8; 2 * 1024 * 1024];
            let window = AllocationWindow::start().unwrap();
            let first = vec![0u8; 1024 * 1024];
            let second = vec![0u8; 512 * 1024];
            drop(first);
            let snapshot = window.finish().unwrap();
            assert!(snapshot.allocation_request_bytes >= 1536 * 1024);
            drop(second);
            drop(existing);

            let window = AllocationWindow::start().unwrap();
            let overlapping = AllocationWindow::start();
            assert!(overlapping.is_err());
            assert!(window.finish().is_ok());
            let window = AllocationWindow::start().unwrap();
            assert!(window.finish().is_ok());
        }
    }
}

#[cfg(all(
    feature = "bench-alloc-stats",
    any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
))]
compile_error!("bench-alloc-stats cannot be combined with jemalloc allocator features");

use fast_md5::Md5;
#[cfg(feature = "bench-alloc-stats")]
use futures_util::StreamExt;
use irmin_rs::api_scrape::{Compression, DownloadInfo};
use irmin_rs::bench_support::{
    apply_hdiff_fixture_with_backend, compare_hdiff_fixture, deterministic_data,
    hdiff20_step_stress_shape, make_v13_zstd_reordered_patch,
    prepare_hdiff_fixture as prepare_shared_hdiff_fixture,
    prepare_hdiff_uncovered_prefix_fixture as prepare_shared_uncovered_prefix_fixture,
    prepare_hdiff20_step_stress_fixture as prepare_shared_hdiff20_step_fixture,
    uncovered_prefix_fixture_sizes,
};
use irmin_rs::game_installer::VerifyMode;
use irmin_rs::game_installer::assembly::chunk_filename;
use irmin_rs::game_installer::cache::{VerificationCache, VerificationEntry};
use irmin_rs::game_installer::compact_manifest::{CompactManifest, StringArena};
use irmin_rs::game_installer::hdiffpatch::{HDiffSourceBackend, WindowStats};
use irmin_rs::game_installer::installer::{
    ChunkNameLookup, InstallCallbacks, InstallOptions, ResumeContext, SophonInstaller, install,
};
use irmin_rs::game_installer::{DownloadHandle, SophonError};
use irmin_rs::proto_parse::{
    SophonManifestAssetChunk, SophonManifestAssetProperty, SophonManifestProto,
};
use irmin_rs::types::CompletedFiles;
use sha2::Digest as _;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Hdiff,
    PreinstallApply,
    PreinstallDownloadPlan,
    Assembly,
    SophonInstall,
}

impl Scenario {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hdiff" => Ok(Self::Hdiff),
            "preinstall-apply" => Ok(Self::PreinstallApply),
            "preinstall-download-plan" => Ok(Self::PreinstallDownloadPlan),
            "assembly" => Ok(Self::Assembly),
            "sophon-install" => Ok(Self::SophonInstall),
            _ => Err(format!(
                "unknown scenario '{value}'; expected hdiff, preinstall-apply, preinstall-download-plan, assembly, or sophon-install"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Hdiff => "hdiff",
            Self::PreinstallApply => "preinstall-apply",
            Self::PreinstallDownloadPlan => "preinstall-download-plan",
            Self::Assembly => "assembly",
            Self::SophonInstall => "sophon-install",
        }
    }
}

#[derive(Debug)]
struct Config {
    scenario: Scenario,
    size_mib: usize,
    chunks: usize,
    assembly_verify_mode: VerifyMode,
    assembly_demand_mmap: bool,
    asset_count: usize,
    preinstall_mode: PreinstallMode,
    diagnose_hdiff_phases: bool,
    repeat: usize,
    budget_mib: Option<u64>,
    json: bool,
    keep_workdir: bool,
    source_backend: SourceBackend,
    window_size_bytes: usize,
    cover_pattern: CoverPattern,
    fixture_backing: FixtureBacking,
    sweep_sizes_mib: Option<Vec<usize>>,
    memory_max_mib: Option<u64>,
    interleave_sizes: bool,
    resume_cache: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CoverPattern {
    #[default]
    Sequential,
    Reverse,
    Thrash,
    UncoveredPrefix,
    Hdiff20Steps,
}

impl CoverPattern {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "sequential" => Ok(Self::Sequential),
            "reverse" => Ok(Self::Reverse),
            "thrash" => Ok(Self::Thrash),
            "uncovered-prefix" => Ok(Self::UncoveredPrefix),
            "hdiff20-steps" => Ok(Self::Hdiff20Steps),
            _ => Err(format!(
                "unknown cover pattern '{value}'; expected sequential, reverse, thrash, uncovered-prefix, or hdiff20-steps"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Reverse => "reverse",
            Self::Thrash => "thrash",
            Self::UncoveredPrefix => "uncovered-prefix",
            Self::Hdiff20Steps => "hdiff20-steps",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SourceBackend {
    #[default]
    Mmap,
    File,
    Windowed,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum FixtureBacking {
    #[default]
    Tempfile,
    Disk,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum PreinstallMode {
    #[default]
    Patch,
    CopyOver,
}

impl PreinstallMode {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "patch" => Ok(Self::Patch),
            "copyover" => Ok(Self::CopyOver),
            _ => Err(format!(
                "unknown preinstall mode '{value}'; expected patch or copyover"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Patch => "patch",
            Self::CopyOver => "copyover",
        }
    }
}

impl FixtureBacking {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "tempfile" => Ok(Self::Tempfile),
            "disk" => Ok(Self::Disk),
            _ => Err(format!(
                "unknown fixture backing '{value}'; expected tempfile or disk"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tempfile => "tempfile",
            Self::Disk => "disk",
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct FilesystemInfo {
    magic: u64,
    name: &'static str,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
struct MemorySnapshot {
    vm_rss_kib: u64,
    vm_hwm_kib: u64,
    rss_anon_kib: Option<u64>,
    rss_file_kib: Option<u64>,
    rss_shmem_kib: Option<u64>,
    pss_kib: Option<u64>,
    private_clean_kib: Option<u64>,
    private_dirty_kib: Option<u64>,
    shared_clean_kib: Option<u64>,
    shared_dirty_kib: Option<u64>,
    smaps_anonymous_kib: Option<u64>,
}

#[cfg(all(
    unix,
    any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
))]
#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
struct JemallocSnapshot {
    allocated_bytes: u64,
    active_bytes: u64,
    resident_bytes: u64,
    mapped_bytes: u64,
    retained_bytes: u64,
}

struct MemorySampler {
    stop: Arc<AtomicBool>,
    peak: Arc<std::sync::Mutex<MemorySnapshot>>,
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    jemalloc_peak: Arc<std::sync::Mutex<JemallocSnapshot>>,
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    jemalloc_sampled: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy, Debug, Default)]
struct CpuUsage {
    user_micros: u64,
    system_micros: u64,
    minor_faults: u64,
    major_faults: u64,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
struct CgroupMemoryEvents {
    low: u64,
    high: u64,
    max: u64,
    oom: u64,
    oom_kill: u64,
    oom_group_kill: u64,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
struct CgroupPressure {
    some_total_us: u64,
    full_total_us: u64,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
struct CgroupMemoryStat {
    anon_bytes: Option<i64>,
    file_bytes: Option<i64>,
    file_mapped_bytes: Option<i64>,
    file_dirty_bytes: Option<i64>,
    file_writeback_bytes: Option<i64>,
    shmem_bytes: Option<i64>,
    kernel_bytes: Option<i64>,
    slab_bytes: Option<i64>,
    pagetables_bytes: Option<i64>,
    pgmajfault: Option<i64>,
    workingset_refault_file: Option<i64>,
}

#[derive(Clone, Copy, Debug, Default, serde::Serialize)]
struct CgroupMemorySnapshot {
    current_bytes: Option<u64>,
    peak_bytes: Option<u64>,
    stat: CgroupMemoryStat,
}

impl CgroupPressure {
    fn delta(before: &Self, after: &Self) -> Self {
        Self {
            some_total_us: after.some_total_us.saturating_sub(before.some_total_us),
            full_total_us: after.full_total_us.saturating_sub(before.full_total_us),
        }
    }
}

impl CgroupMemorySnapshot {
    fn delta(before: &Self, after: &Self) -> CgroupMemoryStat {
        CgroupMemoryStat {
            anon_bytes: signed_delta(before.stat.anon_bytes, after.stat.anon_bytes),
            file_bytes: signed_delta(before.stat.file_bytes, after.stat.file_bytes),
            file_mapped_bytes: signed_delta(
                before.stat.file_mapped_bytes,
                after.stat.file_mapped_bytes,
            ),
            file_dirty_bytes: signed_delta(
                before.stat.file_dirty_bytes,
                after.stat.file_dirty_bytes,
            ),
            file_writeback_bytes: signed_delta(
                before.stat.file_writeback_bytes,
                after.stat.file_writeback_bytes,
            ),
            shmem_bytes: signed_delta(before.stat.shmem_bytes, after.stat.shmem_bytes),
            kernel_bytes: signed_delta(before.stat.kernel_bytes, after.stat.kernel_bytes),
            slab_bytes: signed_delta(before.stat.slab_bytes, after.stat.slab_bytes),
            pagetables_bytes: signed_delta(
                before.stat.pagetables_bytes,
                after.stat.pagetables_bytes,
            ),
            pgmajfault: signed_delta(before.stat.pgmajfault, after.stat.pgmajfault),
            workingset_refault_file: signed_delta(
                before.stat.workingset_refault_file,
                after.stat.workingset_refault_file,
            ),
        }
    }
}

fn signed_delta(before: Option<i64>, after: Option<i64>) -> Option<i64> {
    after?.checked_sub(before?)
}

impl CgroupMemoryEvents {
    fn delta(before: &Self, after: &Self) -> Self {
        Self {
            low: after.low.saturating_sub(before.low),
            high: after.high.saturating_sub(before.high),
            max: after.max.saturating_sub(before.max),
            oom: after.oom.saturating_sub(before.oom),
            oom_kill: after.oom_kill.saturating_sub(before.oom_kill),
            oom_group_kill: after.oom_group_kill.saturating_sub(before.oom_group_kill),
        }
    }
}

impl MemorySampler {
    fn start() -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let peak = Arc::new(std::sync::Mutex::new(MemorySnapshot::default()));
        let thread_stop = Arc::clone(&stop);
        let thread_peak = Arc::clone(&peak);
        #[cfg(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        ))]
        let jemalloc_peak = Arc::new(std::sync::Mutex::new(JemallocSnapshot::default()));
        #[cfg(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        ))]
        let thread_jemalloc_peak = Arc::clone(&jemalloc_peak);
        #[cfg(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        ))]
        let jemalloc_sampled = Arc::new(AtomicBool::new(false));
        #[cfg(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        ))]
        let thread_jemalloc_sampled = Arc::clone(&jemalloc_sampled);
        let thread = thread::spawn(move || {
            while !thread_stop.load(Ordering::Acquire) {
                if let Ok(snapshot) = read_memory_snapshot()
                    && let Ok(mut current_peak) = thread_peak.lock()
                    && snapshot.vm_rss_kib > current_peak.vm_rss_kib
                {
                    update_memory_peak_with_rollup(&mut current_peak, snapshot);
                }
                #[cfg(all(
                    unix,
                    any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
                ))]
                if let Some(snapshot) = jemalloc_snapshot()
                    && let Ok(mut current_peak) = thread_jemalloc_peak.lock()
                {
                    update_jemalloc_peak(&mut current_peak, snapshot);
                    thread_jemalloc_sampled.store(true, Ordering::Release);
                }
                thread::sleep(std::time::Duration::from_millis(
                    if cfg!(all(
                        unix,
                        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
                    )) {
                        10
                    } else {
                        2
                    },
                ));
            }
            if let Ok(snapshot) = read_memory_snapshot()
                && let Ok(mut current_peak) = thread_peak.lock()
                && snapshot.vm_rss_kib > current_peak.vm_rss_kib
            {
                update_memory_peak_with_rollup(&mut current_peak, snapshot);
            }
            #[cfg(all(
                unix,
                any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
            ))]
            if let Some(snapshot) = jemalloc_snapshot()
                && let Ok(mut current_peak) = thread_jemalloc_peak.lock()
            {
                update_jemalloc_peak(&mut current_peak, snapshot);
                thread_jemalloc_sampled.store(true, Ordering::Release);
            }
        });
        Self {
            stop,
            peak,
            #[cfg(all(
                unix,
                any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
            ))]
            jemalloc_peak,
            #[cfg(all(
                unix,
                any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
            ))]
            jemalloc_sampled,
            thread: Some(thread),
        }
    }

    fn finish(&mut self) -> MemorySnapshot {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.peak
            .lock()
            .map(|snapshot| snapshot.clone())
            .unwrap_or_default()
    }

    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    fn jemalloc_peak(&self) -> JemallocSnapshot {
        self.jemalloc_peak
            .lock()
            .map(|snapshot| *snapshot)
            .unwrap_or_default()
    }

    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    fn has_jemalloc_sample(&self) -> bool {
        self.jemalloc_sampled.load(Ordering::Acquire)
    }
}

fn update_memory_peak(peak: &mut MemorySnapshot, current: MemorySnapshot) {
    if current.vm_rss_kib > peak.vm_rss_kib {
        *peak = current;
    }
}

fn update_memory_peak_with_rollup(peak: &mut MemorySnapshot, current: MemorySnapshot) {
    update_memory_peak(peak, read_smaps_rollup_snapshot(current));
}

#[cfg(all(
    unix,
    any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
))]
fn jemalloc_snapshot() -> Option<JemallocSnapshot> {
    tikv_jemalloc_ctl::epoch::advance().ok()?;
    Some(JemallocSnapshot {
        allocated_bytes: tikv_jemalloc_ctl::stats::allocated::read().ok()? as u64,
        active_bytes: tikv_jemalloc_ctl::stats::active::read().ok()? as u64,
        resident_bytes: tikv_jemalloc_ctl::stats::resident::read().ok()? as u64,
        mapped_bytes: tikv_jemalloc_ctl::stats::mapped::read().ok()? as u64,
        retained_bytes: tikv_jemalloc_ctl::stats::retained::read().ok()? as u64,
    })
}

#[cfg(all(
    unix,
    any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
))]
fn update_jemalloc_peak(peak: &mut JemallocSnapshot, current: JemallocSnapshot) {
    if current.allocated_bytes > peak.allocated_bytes {
        *peak = current;
    }
}

enum ScenarioContext {
    Hdiff { dir: PathBuf },
    PreinstallApply(Box<PreinstallApplyContext>),
    Assembly { context: Box<AssemblyContext> },
    SophonInstall(Box<InstallContext>),
}

struct AssemblyContext {
    manifest: CompactManifest,
    game_dir: PathBuf,
    chunks_dir: PathBuf,
    tmp_dir: PathBuf,
    expected_path: PathBuf,
    lookup: ChunkNameLookup,
    refcounts: Vec<AtomicU32>,
    cache: VerificationCache<String, VerificationEntry>,
    verify_mode: VerifyMode,
    demand_mmap: bool,
}

struct InstallContext {
    installer: SophonInstaller,
    game_dir: PathBuf,
    expected_path: PathBuf,
    size_mib: usize,
    resume_cache: bool,
    resume: ResumeContext,
    runtime: tokio::runtime::Runtime,
}

struct PreinstallApplyContext {
    game_dirs: Vec<PathBuf>,
    expected_path: PathBuf,
    expected_size: usize,
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
    diagnose_hdiff_phases: bool,
}

enum Verification {
    Hdiff(PathBuf),
    #[cfg(feature = "bench-diagnostics")]
    HdiffPreinstall {
        expected: PathBuf,
        actual: PathBuf,
        size: usize,
        timings: irmin_rs::game_installer::HdiffPhaseTimings,
    },
    FileCompare {
        expected: PathBuf,
        actuals: Vec<PathBuf>,
        size: usize,
        #[cfg(feature = "bench-alloc-stats")]
        asset_heap_live_growth_bytes: Vec<u64>,
    },
    Assembly {
        expected: PathBuf,
        actual: PathBuf,
        size: usize,
    },
    SophonInstall {
        expected: PathBuf,
        actual: PathBuf,
        size: usize,
    },
}

struct LocalChunkServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LocalChunkServer {
    fn start(root: PathBuf) -> Result<Self, String> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
        listener
            .set_nonblocking(true)
            .map_err(|error| error.to_string())?;
        let address = listener.local_addr().map_err(|error| error.to_string())?;
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let thread = thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => serve_chunk_request(stream, &root),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            base_url: format!("http://{address}"),
            stop,
            thread: Some(thread),
        })
    }

    fn stop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LocalChunkServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn serve_chunk_request(mut stream: TcpStream, root: &Path) {
    let mut request = [0u8; 8192];
    let read = match stream.read(&mut request) {
        Ok(read) => read,
        Err(_) => return,
    };
    let request = String::from_utf8_lossy(&request[..read]);
    let Some(requested) = request
        .lines()
        .next()
        .filter(|line| line.ends_with(" HTTP/1.1") || line.ends_with(" HTTP/1.0"))
        .and_then(|line| line.split_whitespace().nth(1))
    else {
        let _ = stream.write_all(
            b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    };
    let Some(name) = requested.strip_prefix("/chunks/") else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    if name.is_empty() || name.contains('/') || name.contains('\\') || name.contains("..") {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    }
    let path = root.join(name);
    let Ok(body) = fs::read(path) else {
        let _ = stream
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        return;
    };
    let header = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(&body);
}

fn main() {
    if let Err(error) = run_cli() {
        eprintln!("bench_memory: {error}");
        std::process::exit(2);
    }
}

fn size_run_order(sizes: &[usize], repeats: usize, interleave: bool) -> Vec<(usize, usize)> {
    if interleave {
        (0..repeats)
            .flat_map(|repeat| {
                let mut order = sizes.to_vec();
                let size_count = order.len();
                if size_count > 0 {
                    order.rotate_left(repeat % size_count);
                }
                order.into_iter().map(move |size| (size, repeat))
            })
            .collect()
    } else {
        sizes
            .iter()
            .flat_map(|&size| (0..repeats).map(move |repeat| (size, repeat)))
            .collect()
    }
}

fn run_cli() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--child") {
        return run_child(&args[1..]);
    }

    let config = parse_args(&args)?;
    let (workdir, filesystem_info) = create_benchmark_workdir(config.fixture_backing)?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let sizes = config
        .sweep_sizes_mib
        .clone()
        .unwrap_or_else(|| vec![config.size_mib]);
    let mut by_size = sizes
        .iter()
        .map(|size| (*size, Vec::with_capacity(config.repeat)))
        .collect::<std::collections::BTreeMap<_, _>>();
    for (run_index, (size_mib, _repeat)) in
        size_run_order(&sizes, config.repeat, config.interleave_sizes)
            .into_iter()
            .enumerate()
    {
        let result = run_child_once(
            &config,
            &executable,
            workdir.path(),
            filesystem_info,
            size_mib,
            run_index,
        )?;
        by_size.get_mut(&size_mib).unwrap().push(result);
    }
    let sweep_results = by_size.into_iter().collect::<Vec<_>>();
    if config.sweep_sizes_mib.is_some() {
        report_size_sweep(&config, &sweep_results)?;
    } else {
        report_results(&config, &sweep_results[0].1)?;
    }
    if config.keep_workdir {
        eprintln!("fixtures retained at {}", workdir.path().display());
        std::mem::forget(workdir);
    }
    Ok(())
}

fn run_child_once(
    config: &Config,
    executable: &Path,
    workdir: &Path,
    filesystem_info: Option<FilesystemInfo>,
    size_mib: usize,
    run_index: usize,
) -> Result<serde_json::Value, String> {
    let fixture_dir = workdir.join(format!("run-{run_index}"));
    fs::create_dir_all(&fixture_dir).map_err(|error| error.to_string())?;
    let prepare_in_child = config.memory_max_mib.is_some();
    if !prepare_in_child {
        prepare_fixture(
            config.scenario,
            &fixture_dir,
            size_mib,
            config.chunks,
            config.cover_pattern,
            config.preinstall_mode,
        )?;
    }
    let mut local_server = if !prepare_in_child
        && config.scenario == Scenario::SophonInstall
        && !config.resume_cache
    {
        Some(LocalChunkServer::start(fixture_dir.clone())?)
    } else {
        None
    };
    let server_url = local_server
        .as_ref()
        .map(|server| server.base_url.clone())
        .unwrap_or_default();
    let output = run_measured_child(
        config,
        executable,
        &fixture_dir,
        filesystem_info,
        size_mib,
        &server_url,
        prepare_in_child,
    )?;
    if !output.status.success() {
        #[cfg(unix)]
        if let Some(signal) = output.status.signal() {
            let likely_oom = config.memory_max_mib.is_some() && signal == libc::SIGKILL;
            return Err(format!(
                "{} child terminated by signal {signal}{}",
                config.scenario.name(),
                if likely_oom {
                    " under a memory limit (likely cgroup OOM)"
                } else {
                    ""
                }
            ));
        }
        return Err(format!(
            "{} child failed: {}",
            config.scenario.name(),
            output.status
        ));
    }
    let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
    let result = serde_json::from_str(stdout.trim())
        .map_err(|error| format!("invalid child JSON: {error}; output={stdout:?}"))?;
    if let Some(mut server) = local_server.take() {
        server.stop();
    }
    Ok(result)
}

fn child_command_args(
    config: &Config,
    fixture_dir: &Path,
    filesystem_info: Option<FilesystemInfo>,
    size_mib: usize,
    server_url: &str,
    prepare_in_child: bool,
) -> Vec<String> {
    vec![
        "--child".to_owned(),
        config.scenario.name().to_owned(),
        fixture_dir.to_string_lossy().into_owned(),
        size_mib.to_string(),
        config.chunks.to_string(),
        config.asset_count.to_string(),
        config.preinstall_mode.name().to_owned(),
        server_url.to_owned(),
        config.cover_pattern.name().to_owned(),
        config.fixture_backing.name().to_owned(),
        filesystem_info
            .map(|info| format!("{:x}", info.magic))
            .unwrap_or_default(),
        match config.source_backend {
            SourceBackend::Mmap => "mmap".to_owned(),
            SourceBackend::File => "file".to_owned(),
            SourceBackend::Windowed => format!("windowed:{}", config.window_size_bytes),
        },
        if prepare_in_child {
            "prepare".to_owned()
        } else {
            "prepared".to_owned()
        },
        if config.diagnose_hdiff_phases {
            "diagnose-hdiff-phases".to_owned()
        } else {
            "standard".to_owned()
        },
        match config.assembly_verify_mode {
            VerifyMode::Full => "full".to_owned(),
            VerifyMode::Deferred => "deferred".to_owned(),
            VerifyMode::None => "none".to_owned(),
        },
        if config.assembly_demand_mmap {
            "demand".to_owned()
        } else {
            "populate".to_owned()
        },
        if config.resume_cache {
            "resume-cache".to_owned()
        } else {
            "fresh".to_owned()
        },
    ]
}

fn run_measured_child(
    config: &Config,
    executable: &Path,
    fixture_dir: &Path,
    filesystem_info: Option<FilesystemInfo>,
    size_mib: usize,
    server_url: &str,
    prepare_in_child: bool,
) -> Result<std::process::Output, String> {
    let child_args = child_command_args(
        config,
        fixture_dir,
        filesystem_info,
        size_mib,
        server_url,
        prepare_in_child,
    );
    let mut command = if let Some(memory_max_mib) = config.memory_max_mib {
        #[cfg(target_os = "linux")]
        {
            let memory_max_bytes = memory_max_mib
                .checked_mul(1024 * 1024)
                .ok_or_else(|| "memory limit is too large".to_owned())?;
            let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
            if page_size <= 0 {
                return Err("failed to read system page size".to_owned());
            }
            let page_size = page_size as u64;
            let memory_max_bytes = memory_max_bytes / page_size * page_size;
            if memory_max_bytes == 0 {
                return Err("memory limit is smaller than one system page".to_owned());
            }
            let memory_max_property = format!("--property=MemoryMax={memory_max_bytes}");
            let mut command = Command::new("systemd-run");
            command
                .args(["--user", "--scope", "--quiet", &memory_max_property, "--"])
                .arg(executable)
                .args(&child_args)
                .env("IRMIN_BENCH_MEMORY_MAX_BYTES", memory_max_bytes.to_string());
            command
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = memory_max_mib;
            return Err("memory-limited benchmark scopes require Linux systemd".to_owned());
        }
    } else {
        let mut command = Command::new(executable);
        command
            .args(&child_args)
            .env_remove("IRMIN_BENCH_MEMORY_MAX_BYTES");
        command
    };
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|error| format!("failed to launch measured child: {error}"))
}

fn create_benchmark_workdir(
    backing: FixtureBacking,
) -> Result<(tempfile::TempDir, Option<FilesystemInfo>), String> {
    match backing {
        FixtureBacking::Tempfile => {
            let workdir = tempfile::Builder::new()
                .prefix("irmin-memory-bench-")
                .tempdir()
                .map_err(|error| error.to_string())?;
            let filesystem = filesystem_info(workdir.path())?;
            Ok((workdir, Some(filesystem)))
        }
        FixtureBacking::Disk => {
            #[cfg(target_os = "linux")]
            {
                let target_dir = std::env::var_os("CARGO_TARGET_DIR")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| PathBuf::from("target"));
                let root = if target_dir.is_absolute() {
                    target_dir.join("bench-memory")
                } else {
                    std::env::current_dir()
                        .map_err(|error| error.to_string())?
                        .join(target_dir)
                        .join("bench-memory")
                };
                fs::create_dir_all(&root).map_err(|error| error.to_string())?;
                let workdir = tempfile::Builder::new()
                    .prefix("irmin-memory-bench-")
                    .tempdir_in(&root)
                    .map_err(|error| error.to_string())?;
                let filesystem = filesystem_info(workdir.path())?;
                if is_memory_backed_filesystem(filesystem.magic) {
                    return Err(format!(
                        "disk-backed fixture requested, but {} is on {} (0x{:x})",
                        workdir.path().display(),
                        filesystem.name,
                        filesystem.magic
                    ));
                }
                Ok((workdir, Some(filesystem)))
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err("disk-backed fixture checks require Linux".to_owned())
            }
        }
    }
}

#[cfg(target_os = "linux")]
const TMPFS_MAGIC: u64 = 0x01021994;
#[cfg(target_os = "linux")]
const RAMFS_MAGIC: u64 = 0x858458f6;

fn filesystem_info(path: &Path) -> Result<FilesystemInfo, String> {
    #[cfg(target_os = "linux")]
    {
        let path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
            .map_err(|error| error.to_string())?;
        let mut stats = std::mem::MaybeUninit::<libc::statfs>::zeroed();
        let result = unsafe { libc::statfs(path.as_ptr(), stats.as_mut_ptr()) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let stats = unsafe { stats.assume_init() };
        let magic = stats.f_type as u64;
        let name = filesystem_name_for_magic(magic);
        Ok(FilesystemInfo { magic, name })
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        Ok(FilesystemInfo {
            magic: 0,
            name: "unknown",
        })
    }
}

fn filesystem_name_for_magic(magic: u64) -> &'static str {
    #[cfg(target_os = "linux")]
    match magic {
        TMPFS_MAGIC => "tmpfs",
        RAMFS_MAGIC => "ramfs",
        0xef53 => "ext",
        0x58465342 => "xfs",
        0x9123683e => "btrfs",
        0x794c7630 => "overlayfs",
        _ => "unknown",
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = magic;
        "unknown"
    }
}

fn is_memory_backed_filesystem(magic: u64) -> bool {
    #[cfg(target_os = "linux")]
    {
        matches!(magic, TMPFS_MAGIC | RAMFS_MAGIC)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = magic;
        false
    }
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut scenario = None;
    let mut size_mib = 64;
    let mut chunks = 8;
    let mut assembly_verify_mode = VerifyMode::Full;
    let mut assembly_demand_mmap = false;
    let mut asset_count = 1;
    let mut preinstall_mode = PreinstallMode::Patch;
    let mut diagnose_hdiff_phases = false;
    let mut repeat = 3;
    let mut budget_mib = None;
    let mut json = false;
    let mut keep_workdir = false;
    let mut source_backend = SourceBackend::Mmap;
    let mut window_size_bytes = 256 * 1024;
    let mut window_size_provided = false;
    let mut cover_pattern = CoverPattern::Sequential;
    let mut fixture_backing = FixtureBacking::Tempfile;
    let mut sweep_sizes_mib = None;
    let mut memory_max_mib = None;
    let mut interleave_sizes = false;
    let mut resume_cache = false;
    let mut index = 0;

    while index < args.len() {
        let option = args[index].as_str();
        match option {
            "--scenario" => {
                index += 1;
                scenario = Some(Scenario::parse(value_at(args, index, option)?)?);
            }
            "--size-mib" => {
                index += 1;
                size_mib = parse_positive(value_at(args, index, option)?, option)?;
            }
            "--chunks" => {
                index += 1;
                chunks = parse_positive(value_at(args, index, option)?, option)?;
            }
            "--assembly-verify-mode" => {
                index += 1;
                assembly_verify_mode = match value_at(args, index, option)? {
                    "full" => VerifyMode::Full,
                    "deferred" => VerifyMode::Deferred,
                    value => {
                        return Err(format!(
                            "unknown assembly verification mode '{value}'; expected full or deferred"
                        ));
                    }
                };
            }
            "--assembly-demand-mmap" => assembly_demand_mmap = true,
            "--asset-count" => {
                index += 1;
                asset_count = parse_positive(value_at(args, index, option)?, option)?;
            }
            "--preinstall-mode" => {
                index += 1;
                preinstall_mode = PreinstallMode::parse(value_at(args, index, option)?)?;
            }
            "--diagnose-hdiff-phases" => diagnose_hdiff_phases = true,
            "--repeat" => {
                index += 1;
                repeat = parse_positive(value_at(args, index, option)?, option)?;
            }
            "--budget-mib" => {
                index += 1;
                budget_mib = Some(parse_positive(value_at(args, index, option)?, option)? as u64);
            }
            "--json" => json = true,
            "--keep-workdir" => keep_workdir = true,
            "--source-backend" => {
                index += 1;
                source_backend = match value_at(args, index, option)? {
                    "mmap" => SourceBackend::Mmap,
                    "file" => SourceBackend::File,
                    "windowed" => SourceBackend::Windowed,
                    value => {
                        return Err(format!(
                            "unknown source backend '{value}'; expected mmap, file, or windowed"
                        ));
                    }
                };
            }
            "--window-mib" => {
                index += 1;
                window_size_provided = true;
                window_size_bytes = parse_positive(value_at(args, index, option)?, option)?
                    .checked_mul(1024 * 1024)
                    .ok_or_else(|| "window size is too large".to_owned())?;
            }
            "--window-kib" => {
                index += 1;
                window_size_provided = true;
                window_size_bytes = parse_positive(value_at(args, index, option)?, option)?
                    .checked_mul(1024)
                    .ok_or_else(|| "window size is too large".to_owned())?;
            }
            "--cover-pattern" => {
                index += 1;
                cover_pattern = CoverPattern::parse(value_at(args, index, option)?)?;
            }
            "--fixture-backing" => {
                index += 1;
                fixture_backing = FixtureBacking::parse(value_at(args, index, option)?)?;
            }
            "--sweep-sizes-mib" => {
                index += 1;
                sweep_sizes_mib = Some(parse_size_list(value_at(args, index, option)?)?);
            }
            "--memory-max-mib" => {
                index += 1;
                memory_max_mib =
                    Some(parse_positive(value_at(args, index, option)?, option)? as u64);
            }
            "--interleave-sizes" => interleave_sizes = true,
            "--resume-cache" => resume_cache = true,
            "--help" | "-h" => return Err(usage().to_owned()),
            other => return Err(format!("unknown argument '{other}'\n{}", usage())),
        }
        index += 1;
    }

    let scenario = scenario.ok_or_else(|| usage().to_owned())?;
    if resume_cache && scenario != Scenario::SophonInstall {
        return Err("--resume-cache is only supported for sophon-install".to_owned());
    }
    if resume_cache && !(2..=5_000).contains(&chunks) {
        return Err("--resume-cache requires --chunks between 2 and 5000".to_owned());
    }
    if scenario != Scenario::Hdiff && source_backend != SourceBackend::Mmap {
        return Err(
            "--source-backend overrides are only supported for the hdiff scenario".to_owned(),
        );
    }
    if asset_count > 8 {
        return Err("--asset-count must not exceed 8".to_owned());
    }
    if asset_count != 1 && scenario != Scenario::PreinstallApply {
        return Err("--asset-count overrides are only supported for preinstall-apply".to_owned());
    }
    if preinstall_mode != PreinstallMode::Patch && scenario != Scenario::PreinstallApply {
        return Err("--preinstall-mode is only supported for preinstall-apply".to_owned());
    }
    if (assembly_verify_mode != VerifyMode::Full || assembly_demand_mmap)
        && scenario != Scenario::Assembly
    {
        return Err(
            "--assembly-verify-mode and --assembly-demand-mmap require the assembly scenario"
                .to_owned(),
        );
    }
    if assembly_demand_mmap && assembly_verify_mode != VerifyMode::Deferred {
        return Err("--assembly-demand-mmap requires --assembly-verify-mode deferred".to_owned());
    }
    if scenario == Scenario::Assembly && assembly_verify_mode == VerifyMode::None {
        return Err(
            "--assembly-verify-mode none is not supported by the assembly benchmark".to_owned(),
        );
    }
    if scenario == Scenario::Assembly && assembly_verify_mode == VerifyMode::Deferred && chunks < 4
    {
        return Err("deferred assembly mmap diagnostics require at least four chunks".to_owned());
    }
    if preinstall_mode == PreinstallMode::CopyOver && cover_pattern != CoverPattern::Sequential {
        return Err("copyover mode requires the sequential cover pattern".to_owned());
    }
    if diagnose_hdiff_phases {
        if !cfg!(feature = "bench-diagnostics") {
            return Err("--diagnose-hdiff-phases requires --features bench-diagnostics".to_owned());
        }
        if scenario != Scenario::PreinstallApply
            || preinstall_mode != PreinstallMode::Patch
            || asset_count != 1
            || sweep_sizes_mib.is_some()
        {
            return Err("--diagnose-hdiff-phases requires one preinstall-apply Patch run without size sweeps".to_owned());
        }
    }
    if !matches!(
        scenario,
        Scenario::Hdiff | Scenario::PreinstallApply | Scenario::PreinstallDownloadPlan
    ) && cover_pattern != CoverPattern::Sequential
    {
        return Err("--cover-pattern is only supported for HDiff scenarios".to_owned());
    }
    if scenario == Scenario::PreinstallDownloadPlan {
        if !cfg!(feature = "bench-alloc-stats") {
            return Err(
                "preinstall-download-plan requires --features bench-alloc-stats".to_owned(),
            );
        }
        if chunks > 100_000 {
            return Err("preinstall-download-plan supports at most 100000 chunks".to_owned());
        }
        if sweep_sizes_mib.is_some() {
            return Err("preinstall-download-plan does not support size sweeps".to_owned());
        }
        if preinstall_mode != PreinstallMode::Patch {
            return Err("preinstall-download-plan does not use --preinstall-mode".to_owned());
        }
    }
    if source_backend != SourceBackend::Windowed && window_size_provided {
        return Err("--window-mib/--window-kib require --source-backend windowed".to_owned());
    }
    #[cfg(not(unix))]
    if source_backend == SourceBackend::Windowed {
        return Err("the windowed source backend requires Unix".to_owned());
    }
    if size_mib.checked_mul(1024 * 1024).is_none() {
        return Err("--size-mib is too large".to_owned());
    }
    if cover_pattern == CoverPattern::UncoveredPrefix {
        uncovered_prefix_fixture_sizes(size_mib)?;
    }
    if cover_pattern == CoverPattern::Hdiff20Steps {
        if !matches!(scenario, Scenario::Hdiff | Scenario::PreinstallApply) {
            return Err("hdiff20-steps requires an HDiff scenario".to_owned());
        }
        hdiff20_step_stress_shape(size_mib)?;
    }
    if scenario == Scenario::Assembly && (size_mib * 1024 * 1024) % chunks != 0 {
        return Err("assembly size-mib must divide evenly into chunks".to_owned());
    }
    if scenario == Scenario::SophonInstall && (size_mib * 1024 * 1024) % chunks != 0 {
        return Err("sophon-install size-mib must divide evenly into chunks".to_owned());
    }
    if fixture_backing == FixtureBacking::Disk
        && !matches!(
            scenario,
            Scenario::PreinstallApply | Scenario::Assembly | Scenario::SophonInstall
        )
    {
        return Err(
            "--fixture-backing disk is only supported for preinstall-apply, assembly, and sophon-install"
                .to_owned(),
        );
    }
    if let Some(sizes) = &sweep_sizes_mib {
        if scenario != Scenario::PreinstallApply {
            return Err("--sweep-sizes-mib is only supported for preinstall-apply".to_owned());
        }
        if fixture_backing != FixtureBacking::Disk {
            return Err("--sweep-sizes-mib requires --fixture-backing disk".to_owned());
        }
        if source_backend != SourceBackend::Mmap {
            return Err("--sweep-sizes-mib requires the production mmap backend".to_owned());
        }
        if cfg!(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        )) {
            return Err(
                "--sweep-sizes-mib is unavailable with jemalloc instrumentation".to_owned(),
            );
        }
        if sizes
            .iter()
            .any(|size| size.checked_mul(1024 * 1024).is_none())
        {
            return Err("a sweep size is too large".to_owned());
        }
        if cover_pattern == CoverPattern::UncoveredPrefix {
            for &size in sizes {
                uncovered_prefix_fixture_sizes(size)?;
            }
        }
        if cover_pattern == CoverPattern::Hdiff20Steps {
            for &size in sizes {
                hdiff20_step_stress_shape(size)?;
            }
        }
        if memory_max_mib.is_some() && !cfg!(target_os = "linux") {
            return Err("--memory-max-mib requires Linux".to_owned());
        }
        if memory_max_mib.is_some()
            && cfg!(all(
                unix,
                any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
            ))
        {
            return Err("memory-limited sweep requires the system allocator".to_owned());
        }
    } else if interleave_sizes {
        return Err("--interleave-sizes requires --sweep-sizes-mib".to_owned());
    } else if memory_max_mib.is_some() {
        return Err("--memory-max-mib requires --sweep-sizes-mib".to_owned());
    }
    if source_backend == SourceBackend::Windowed && window_size_bytes > 64 * 1024 * 1024 {
        return Err("window size must not exceed 64 MiB".to_owned());
    }
    Ok(Config {
        scenario,
        size_mib,
        chunks,
        assembly_verify_mode,
        assembly_demand_mmap,
        asset_count,
        preinstall_mode,
        diagnose_hdiff_phases,
        repeat,
        budget_mib,
        json,
        keep_workdir,
        source_backend,
        window_size_bytes,
        cover_pattern,
        fixture_backing,
        sweep_sizes_mib,
        memory_max_mib,
        interleave_sizes,
        resume_cache,
    })
}

fn parse_size_list(value: &str) -> Result<Vec<usize>, String> {
    let mut sizes = value
        .split(',')
        .map(|size| parse_positive(size.trim(), "--sweep-sizes-mib"))
        .collect::<Result<Vec<_>, _>>()?;
    if sizes.is_empty() {
        return Err("--sweep-sizes-mib requires at least one size".to_owned());
    }
    sizes.sort_unstable();
    sizes.dedup();
    Ok(sizes)
}

fn value_at<'a>(args: &'a [String], index: usize, option: &str) -> Result<&'a str, String> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| format!("missing value for {option}"))
}

fn parse_positive(value: &str, option: &str) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|_| format!("{option} expects a positive integer"))?;
    if parsed == 0 {
        return Err(format!("{option} expects a positive integer"));
    }
    Ok(parsed)
}

fn usage() -> &'static str {
    "Usage: bench_memory --scenario hdiff|preinstall-apply|preinstall-download-plan|assembly|sophon-install [--size-mib N] [--sweep-sizes-mib N,N,...] [--memory-max-mib N] [--chunks N] [--asset-count N] [--preinstall-mode patch|copyover] [--resume-cache] [--assembly-verify-mode full|deferred] [--assembly-demand-mmap] [--diagnose-hdiff-phases] [--repeat N] [--source-backend mmap|file|windowed] [--window-mib N|--window-kib N] [--cover-pattern sequential|reverse|thrash|uncovered-prefix|hdiff20-steps] [--fixture-backing tempfile|disk] [--budget-mib N] [--json] [--keep-workdir]"
}

fn run_child(args: &[String]) -> Result<(), String> {
    if args.len() != 16 {
        return Err("child expects SCENARIO FIXTURE_DIR SIZE_MIB CHUNKS ASSET_COUNT PREINSTALL_MODE SERVER_URL COVER_PATTERN FIXTURE_BACKING FS_MAGIC SOURCE_BACKEND PREPARE_MODE DIAGNOSTIC_MODE ASSEMBLY_VERIFY_MODE ASSEMBLY_MMAP_ADVICE RESUME_MODE".to_owned());
    }
    let scenario = Scenario::parse(&args[0])?;
    let fixture_dir = PathBuf::from(&args[1]);
    let size_mib = parse_positive(&args[2], "--size-mib")?;
    let chunks = parse_positive(&args[3], "--chunks")?;
    let asset_count = parse_positive(&args[4], "--asset-count")?;
    let preinstall_mode = PreinstallMode::parse(&args[5])?;
    if asset_count > 8 || (asset_count != 1 && scenario != Scenario::PreinstallApply) {
        return Err("child asset count is unsupported for this scenario".to_owned());
    }
    let cover_pattern = CoverPattern::parse(&args[7])?;
    let fixture_backing = FixtureBacking::parse(&args[8])?;
    let reported_fs_magic = if args[9].is_empty() {
        None
    } else {
        Some(
            u64::from_str_radix(&args[9], 16)
                .map_err(|_| "invalid fixture filesystem magic".to_owned())?,
        )
    };
    let source_backend = args[10].as_str();
    let prepare_in_child = match args[11].as_str() {
        "prepare" => true,
        "prepared" => false,
        _ => return Err("invalid child fixture preparation mode".to_owned()),
    };
    let diagnose_hdiff_phases = match args[12].as_str() {
        "diagnose-hdiff-phases" => true,
        "standard" => false,
        _ => return Err("invalid child diagnostic mode".to_owned()),
    };
    let assembly_verify_mode = match args[13].as_str() {
        "full" => VerifyMode::Full,
        "deferred" => VerifyMode::Deferred,
        "none" => VerifyMode::None,
        _ => return Err("invalid child assembly verification mode".to_owned()),
    };
    let assembly_demand_mmap = match args[14].as_str() {
        "populate" => false,
        "demand" => true,
        _ => return Err("invalid child assembly mmap advice".to_owned()),
    };
    let resume_cache = match args[15].as_str() {
        "resume-cache" => true,
        "fresh" => false,
        _ => return Err("invalid child resume mode".to_owned()),
    };
    if resume_cache && scenario != Scenario::SophonInstall {
        return Err("resume cache is only supported for SophonInstall".to_owned());
    }
    let download_over_hash_strategy = if scenario == Scenario::Assembly {
        std::env::var("IRMIN_BENCH_DOWNLOAD_OVER_HASH")
            .ok()
            .filter(|strategy| matches!(strategy.as_str(), "current" | "fused"))
    } else {
        None
    };
    if scenario != Scenario::Assembly
        && (assembly_verify_mode != VerifyMode::Full || assembly_demand_mmap)
    {
        return Err("assembly diagnostics are only valid for assembly scenario".to_owned());
    }
    if scenario == Scenario::Assembly && assembly_verify_mode == VerifyMode::None {
        return Err("assembly benchmark verification mode cannot be none".to_owned());
    }
    if assembly_demand_mmap && assembly_verify_mode != VerifyMode::Deferred {
        return Err("demand mmap advice requires deferred assembly verification".to_owned());
    }
    if scenario == Scenario::Assembly && assembly_verify_mode == VerifyMode::Deferred && chunks < 4
    {
        return Err("deferred assembly mmap diagnostics require at least four chunks".to_owned());
    }
    if cover_pattern == CoverPattern::UncoveredPrefix {
        uncovered_prefix_fixture_sizes(size_mib)?;
    }
    if cover_pattern == CoverPattern::Hdiff20Steps {
        hdiff20_step_stress_shape(size_mib)?;
    }
    let expected_memory_max = std::env::var("IRMIN_BENCH_MEMORY_MAX_BYTES")
        .ok()
        .filter(|value| !value.is_empty())
        .map(|value| {
            value
                .parse::<u64>()
                .map_err(|_| "invalid requested cgroup memory limit".to_owned())
        })
        .transpose()?;
    let cgroup_memory_before_setup = if let Some(expected) = expected_memory_max {
        let (actual, path) = read_cgroup_memory_limit()?;
        if actual != expected {
            return Err(format!(
                "requested memory.max {expected} bytes but child cgroup reports {actual} bytes"
            ));
        }
        let events = read_cgroup_memory_events(&path)?;
        let pressure = read_cgroup_memory_pressure(&path)?;
        let memory = read_cgroup_memory_snapshot(&path);
        Some((actual, path, events, pressure, memory))
    } else {
        None
    };
    let filesystem_info = filesystem_info(&fixture_dir)?;
    if reported_fs_magic != Some(filesystem_info.magic) {
        return Err("fixture filesystem changed between setup and child run".to_owned());
    }
    if prepare_in_child {
        prepare_fixture(
            scenario,
            &fixture_dir,
            size_mib,
            chunks,
            cover_pattern,
            preinstall_mode,
        )?;
    }
    let window_stats = Arc::new(WindowStats::default());
    let hdiff_source_backend = match source_backend {
        "mmap" => HDiffSourceBackend::Mmap,
        "file" => HDiffSourceBackend::File,
        value if value.starts_with("windowed:") => {
            let window_size = value["windowed:".len()..]
                .parse::<usize>()
                .map_err(|_| "invalid child window size".to_owned())?;
            if window_size == 0 || window_size > 64 * 1024 * 1024 {
                return Err("child window size must be between 1 byte and 64 MiB".to_owned());
            }
            HDiffSourceBackend::Windowed {
                window_size,
                stats: Arc::clone(&window_stats),
            }
        }
        _ => return Err("invalid child source backend".to_owned()),
    };
    if scenario == Scenario::PreinstallDownloadPlan {
        return run_download_plan_child(&fixture_dir, chunks, size_mib, filesystem_info);
    }
    let mut context = prepare_child_context(
        scenario,
        &fixture_dir,
        size_mib,
        chunks,
        asset_count,
        preinstall_mode,
        &args[6],
        resume_cache,
    )?;
    match &mut context {
        ScenarioContext::PreinstallApply(context) => {
            context.diagnose_hdiff_phases = diagnose_hdiff_phases;
        }
        ScenarioContext::Assembly { context } => {
            context.verify_mode = assembly_verify_mode;
            context.demand_mmap = assembly_demand_mmap;
        }
        _ => {}
    }
    let cgroup_after_setup = if let Some((_, path, _, _, _)) = &cgroup_memory_before_setup {
        Some((
            read_cgroup_memory_events(path)?,
            read_cgroup_memory_pressure(path)?,
            read_cgroup_memory_snapshot(path),
        ))
    } else {
        None
    };
    let mut sampler = MemorySampler::start();
    thread::sleep(std::time::Duration::from_millis(5));
    let baseline = read_memory_snapshot()?;
    let hwm_reset = reset_hwm().is_ok();
    let operation_baseline = read_memory_snapshot()?;
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    let jemalloc_baseline = jemalloc_snapshot();
    let cpu_before = read_cpu_usage()?;
    let started = Instant::now();
    #[cfg(feature = "bench-alloc-stats")]
    let allocation_window = allocation_stats::AllocationWindow::start().map_err(str::to_owned)?;
    #[cfg(feature = "bench-alloc-stats")]
    if matches!(scenario, Scenario::Hdiff | Scenario::PreinstallApply)
        && cover_pattern == CoverPattern::Hdiff20Steps
    {
        irmin_rs::game_installer::hdiffpatch::reset_step_buffer_diagnostics();
    }
    let verification = run_operation(context, hdiff_source_backend)?;
    #[cfg(feature = "bench-alloc-stats")]
    let allocation_snapshot = allocation_window.finish().map_err(str::to_owned)?;
    #[cfg(feature = "bench-alloc-stats")]
    let step_buffer_diagnostics = (matches!(scenario, Scenario::Hdiff | Scenario::PreinstallApply)
        && cover_pattern == CoverPattern::Hdiff20Steps)
        .then(irmin_rs::game_installer::hdiffpatch::step_buffer_diagnostics);
    let elapsed_ms = started.elapsed().as_millis();
    let operation_end = read_smaps_rollup_snapshot(read_memory_snapshot()?);
    let cpu_after = read_cpu_usage()?;
    let cgroup_after_operation = if let Some((_, path, _, _, _)) = &cgroup_memory_before_setup {
        Some((
            read_cgroup_memory_events(path)?,
            read_cgroup_memory_pressure(path)?,
            read_cgroup_memory_snapshot(path),
        ))
    } else {
        None
    };
    let cgroup_setup_events = cgroup_memory_before_setup
        .as_ref()
        .zip(cgroup_after_setup.as_ref())
        .map(|((_, _, before, _, _), (after, _, _))| CgroupMemoryEvents::delta(before, after));
    let cgroup_operation_events = cgroup_after_setup
        .as_ref()
        .zip(cgroup_after_operation.as_ref())
        .map(|((before, _, _), (after, _, _))| CgroupMemoryEvents::delta(before, after));
    let cgroup_total_events = cgroup_memory_before_setup
        .as_ref()
        .zip(cgroup_after_operation.as_ref())
        .map(|((_, _, before, _, _), (after, _, _))| CgroupMemoryEvents::delta(before, after));
    let cgroup_setup_pressure = cgroup_memory_before_setup
        .as_ref()
        .zip(cgroup_after_setup.as_ref())
        .map(|((_, _, _, before, _), (_, after, _))| CgroupPressure::delta(before, after));
    let cgroup_operation_pressure = cgroup_after_setup
        .as_ref()
        .zip(cgroup_after_operation.as_ref())
        .map(|((_, before, _), (_, after, _))| CgroupPressure::delta(before, after));
    let cgroup_total_pressure = cgroup_memory_before_setup
        .as_ref()
        .zip(cgroup_after_operation.as_ref())
        .map(|((_, _, _, before, _), (_, after, _))| CgroupPressure::delta(before, after));
    let cgroup_memory_setup_delta = cgroup_memory_before_setup
        .as_ref()
        .zip(cgroup_after_setup.as_ref())
        .map(|((_, _, _, _, before), (_, _, after))| CgroupMemorySnapshot::delta(before, after));
    let cgroup_memory_operation_delta = cgroup_after_setup
        .as_ref()
        .zip(cgroup_after_operation.as_ref())
        .map(|((_, _, before), (_, _, after))| CgroupMemorySnapshot::delta(before, after));
    let cgroup_memory_total_delta = cgroup_memory_before_setup
        .as_ref()
        .zip(cgroup_after_operation.as_ref())
        .map(|((_, _, _, _, before), (_, _, after))| CgroupMemorySnapshot::delta(before, after));
    let sampled_peak = sampler.finish();
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    let jemalloc_sampled_peak = sampler.jemalloc_peak();
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    let jemalloc_peak_available = sampler.has_jemalloc_sample();
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    let jemalloc_end = jemalloc_snapshot();
    let kernel_hwm_peak_rss_kib = operation_end.vm_hwm_kib.max(operation_end.vm_rss_kib);
    let peak_rss_kib = kernel_hwm_peak_rss_kib.max(sampled_peak.vm_rss_kib);
    #[cfg(feature = "bench-alloc-stats")]
    let asset_heap_live_growth_bytes = match &verification {
        Verification::FileCompare {
            asset_heap_live_growth_bytes,
            ..
        } => Some(asset_heap_live_growth_bytes.clone()),
        _ => None,
    };
    #[cfg(feature = "bench-diagnostics")]
    let phase_timings = verification_phase_timings(&verification);
    verify_output(verification)?;
    let fixture_shape = if cover_pattern == CoverPattern::UncoveredPrefix {
        let (prefix_size, suffix_size) = uncovered_prefix_fixture_sizes(size_mib)?;
        Some(serde_json::json!({
            "uncovered_prefix_bytes": prefix_size,
            "covered_suffix_bytes": suffix_size,
            "output_bytes": prefix_size + suffix_size,
        }))
    } else if cover_pattern == CoverPattern::Hdiff20Steps {
        let shape = hdiff20_step_stress_shape(size_mib)?;
        Some(serde_json::json!({
            "output_bytes": shape.output_bytes,
            "cover_count": shape.cover_count,
            "step_sizes_bytes": shape.step_sizes_bytes,
            "covers_per_output_byte": 1,
            "step_sizes_include_cover_and_rle_sections": true,
            "step_layout_is_synthetic_stress": true,
        }))
    } else if scenario == Scenario::PreinstallApply && preinstall_mode == PreinstallMode::CopyOver {
        Some(serde_json::json!({
            "output_bytes": size_mib * 1024 * 1024,
            "raw_copyover_payload_bytes": size_mib * 1024 * 1024,
        }))
    } else {
        None
    };
    let mut result = serde_json::json!({
        "scenario": scenario.name(),
        "fixture_kind": if scenario == Scenario::PreinstallApply { "synthetic-preinstall" } else { "synthetic" },
        "operation_mode": if resume_cache {
            "resume_cache".to_owned()
        } else if let Some(strategy) = &download_over_hash_strategy {
            format!("download_over_{strategy}")
        } else if diagnose_hdiff_phases {
            "single_asset_hdiff_phase_diagnostics".to_owned()
        } else {
            "standard_scenario".to_owned()
        },
        "representative": false,
        "asset_count": (scenario == Scenario::PreinstallApply).then_some(asset_count),
        "preinstall_mode": (scenario == Scenario::PreinstallApply)
            .then_some(preinstall_mode.name()),
        "resume_cache": (scenario == Scenario::SophonInstall).then_some(resume_cache),
        "cover_pattern": cover_pattern.name(),
        "fixture_shape": fixture_shape,
        "fixture_backing": fixture_backing.name(),
        "fixture_fs_magic": format!("0x{:x}", filesystem_info.magic),
        "fixture_fs_type": filesystem_info.name,
        "fixture_cache_state": "warm",
        "fixture_prepared_inside_memory_limit": prepare_in_child,
        "source_backend": source_backend,
        "window_cache_hits": window_stats.hits.load(Ordering::Relaxed),
        "window_cache_misses": window_stats.misses.load(Ordering::Relaxed),
        "window_cache_bytes_read": window_stats.bytes_read.load(Ordering::Relaxed),
        "size_mib": size_mib,
        "chunks": if matches!(scenario, Scenario::Assembly | Scenario::SophonInstall) {
            Some(chunks)
        } else {
            None
        },
        "elapsed_ms": elapsed_ms,
        "user_cpu_ms": cpu_after.user_micros.saturating_sub(cpu_before.user_micros) / 1000,
        "system_cpu_ms": cpu_after.system_micros.saturating_sub(cpu_before.system_micros) / 1000,
        "minor_faults": cpu_after.minor_faults.saturating_sub(cpu_before.minor_faults),
        "major_faults": cpu_after.major_faults.saturating_sub(cpu_before.major_faults),
        "baseline_rss_kib": if hwm_reset { operation_baseline.vm_rss_kib } else { baseline.vm_rss_kib },
        "baseline_rss_anon_kib": operation_baseline.rss_anon_kib,
        "baseline_rss_shmem_kib": operation_baseline.rss_shmem_kib,
        "peak_rss_kib": peak_rss_kib,
        "kernel_hwm_peak_rss_kib": kernel_hwm_peak_rss_kib,
        "peak_delta_kib": peak_rss_kib.saturating_sub(if hwm_reset {
            operation_baseline.vm_rss_kib
        } else {
            baseline.vm_rss_kib
        }),
        "operation_end_rss_anon_kib": operation_end.rss_anon_kib,
        "operation_end_rss_file_kib": operation_end.rss_file_kib,
        "operation_end_rss_shmem_kib": operation_end.rss_shmem_kib,
        "sampled_max_vm_rss_kib": sampled_peak.vm_rss_kib,
        "sampled_max_rss_anon_kib": sampled_peak.rss_anon_kib,
        "sampled_max_rss_file_kib": sampled_peak.rss_file_kib,
        "sampled_max_rss_shmem_kib": sampled_peak.rss_shmem_kib,
        "cpu_scope": "process_including_sampler",
        "hwm_reset": hwm_reset,
        "measurement": if hwm_reset { "operation_hwm" } else { "process_hwm_includes_setup" },
    });
    result.as_object_mut().unwrap().insert(
        "assembly_verify_mode".to_owned(),
        serde_json::json!(
            (scenario == Scenario::Assembly).then_some(match assembly_verify_mode {
                VerifyMode::Full => "full",
                VerifyMode::Deferred => "deferred",
                VerifyMode::None => "none",
            })
        ),
    );
    result.as_object_mut().unwrap().insert(
        "assembly_mmap_advice".to_owned(),
        serde_json::json!(
            (scenario == Scenario::Assembly).then_some(if assembly_demand_mmap {
                "demand-sequential"
            } else {
                "populate-read-default"
            })
        ),
    );
    result.as_object_mut().unwrap().insert(
        "download_over_hash_strategy".to_owned(),
        serde_json::json!(download_over_hash_strategy),
    );
    #[cfg(feature = "bench-alloc-stats")]
    {
        if let Some(checkpoints) = asset_heap_live_growth_bytes {
            result.as_object_mut().unwrap().insert(
                "allocator_live_growth_after_asset_bytes".to_owned(),
                serde_json::json!(checkpoints),
            );
        }
        result.as_object_mut().unwrap().insert(
            "allocator_net_live_growth_peak_bytes".to_owned(),
            serde_json::json!(allocation_snapshot.peak_live_bytes),
        );
        result.as_object_mut().unwrap().insert(
            "allocator_heap_live_delta_end_bytes".to_owned(),
            serde_json::json!(allocation_snapshot.live_bytes_at_end),
        );
        result.as_object_mut().unwrap().insert(
            "allocator_allocation_request_bytes".to_owned(),
            serde_json::json!(allocation_snapshot.allocation_request_bytes),
        );
        result.as_object_mut().unwrap().insert(
            "allocator_heap_measurement".to_owned(),
            serde_json::json!("process_wide_system_requested_bytes_net_growth"),
        );
        result.as_object_mut().unwrap().insert(
            "allocator_peak_is_approximate".to_owned(),
            serde_json::json!(true),
        );
        if let Some((capacity_bytes, max_step_end_bytes)) = step_buffer_diagnostics {
            result.as_object_mut().unwrap().insert(
                "hdiff20_max_retained_step_buffer_capacity_bytes".to_owned(),
                serde_json::json!(capacity_bytes),
            );
            result.as_object_mut().unwrap().insert(
                "hdiff20_max_requested_step_length_bytes".to_owned(),
                serde_json::json!(max_step_end_bytes),
            );
            result.as_object_mut().unwrap().insert(
                "hdiff20_buffer_diagnostic_scope".to_owned(),
                serde_json::json!("process_wide_maxima_during_benchmark_operation"),
            );
            result.as_object_mut().unwrap().insert(
                "hdiff20_buffer_diagnostic_note".to_owned(),
                serde_json::json!("These maxima are correlated observations, not time-aligned or additive components of the allocator peak."),
            );
        }
    }
    for (key, value) in [
        ("operation_end_pss_kib", operation_end.pss_kib),
        (
            "operation_end_private_clean_kib",
            operation_end.private_clean_kib,
        ),
        (
            "operation_end_private_dirty_kib",
            operation_end.private_dirty_kib,
        ),
        (
            "operation_end_shared_clean_kib",
            operation_end.shared_clean_kib,
        ),
        (
            "operation_end_shared_dirty_kib",
            operation_end.shared_dirty_kib,
        ),
        (
            "operation_end_smaps_anonymous_kib",
            operation_end.smaps_anonymous_kib,
        ),
        ("sampled_peak_pss_kib", sampled_peak.pss_kib),
        (
            "sampled_peak_private_clean_kib",
            sampled_peak.private_clean_kib,
        ),
        (
            "sampled_peak_private_dirty_kib",
            sampled_peak.private_dirty_kib,
        ),
        (
            "sampled_peak_shared_clean_kib",
            sampled_peak.shared_clean_kib,
        ),
        (
            "sampled_peak_shared_dirty_kib",
            sampled_peak.shared_dirty_kib,
        ),
        (
            "sampled_peak_smaps_anonymous_kib",
            sampled_peak.smaps_anonymous_kib,
        ),
    ] {
        result
            .as_object_mut()
            .unwrap()
            .insert(key.to_owned(), serde_json::json!(value));
    }
    result.as_object_mut().unwrap().insert(
        "smaps_rollup_sampled_only_at_new_rss_high_water".to_owned(),
        serde_json::json!(true),
    );
    result.as_object_mut().unwrap().insert(
        "sampled_peak_components_share_rss_high_water_sample".to_owned(),
        serde_json::json!(true),
    );
    result.as_object_mut().unwrap().insert(
        "smaps_rollup_sampling_note".to_owned(),
        serde_json::json!(
            "/proc/self/status and /proc/self/smaps_rollup are read sequentially, not atomically."
        ),
    );
    #[cfg(feature = "bench-diagnostics")]
    if let Some(timings) = phase_timings {
        result.as_object_mut().unwrap().insert(
            "hdiff_phase_wall_time_ns".to_owned(),
            serde_json::json!({
                "original_metadata_and_hash_validation": timings.original_metadata_and_hash_validation.as_nanos(),
                "patch_application": timings.patch_application.as_nanos(),
                "output_verification": timings.output_verification.as_nanos(),
            }),
        );
        result.as_object_mut().unwrap().insert(
            "hdiff_phase_timing_scope".to_owned(),
            serde_json::json!("synchronous_single_asset_worker_wall_time"),
        );
        result.as_object_mut().unwrap().insert(
            "hdiff_phase_timing_notes".to_owned(),
            serde_json::json!([
                "Original timing covers existing-file metadata and hash checks; path setup and synthetic state loading are excluded.",
                "Patch timing includes HDiff apply, temporary-output sync, atomic persist, and directory sync.",
                "Output verification timing covers post-apply output size/hash checks and the final rename to the target path.",
                "This diagnostic bypasses async preinstall orchestration and is synthetic-only; it does not explain the reported production memory peak."
            ]),
        );
    }
    let result = result.as_object_mut().unwrap();
    result.insert(
        "sampling_interval_ms".to_owned(),
        serde_json::json!(if cfg!(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        )) {
            10
        } else {
            2
        }),
    );
    result.insert(
        "cgroup_memory_max_bytes".to_owned(),
        serde_json::to_value(
            cgroup_memory_before_setup
                .as_ref()
                .map(|(limit, _, _, _, _)| *limit),
        )
        .unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_events_setup".to_owned(),
        serde_json::to_value(cgroup_setup_events).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_events_operation".to_owned(),
        serde_json::to_value(cgroup_operation_events).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_events_total".to_owned(),
        serde_json::to_value(cgroup_total_events).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_pressure_setup".to_owned(),
        serde_json::to_value(cgroup_setup_pressure).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_pressure_operation".to_owned(),
        serde_json::to_value(cgroup_operation_pressure).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_pressure_total".to_owned(),
        serde_json::to_value(cgroup_total_pressure).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_snapshot_before_setup".to_owned(),
        serde_json::to_value(
            cgroup_memory_before_setup
                .as_ref()
                .map(|(_, _, _, _, snapshot)| snapshot),
        )
        .unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_snapshot_after_setup".to_owned(),
        serde_json::to_value(cgroup_after_setup.as_ref().map(|(_, _, snapshot)| snapshot))
            .unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_snapshot_after_operation".to_owned(),
        serde_json::to_value(
            cgroup_after_operation
                .as_ref()
                .map(|(_, _, snapshot)| snapshot),
        )
        .unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_stat_delta_setup".to_owned(),
        serde_json::to_value(cgroup_memory_setup_delta).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_stat_delta_operation".to_owned(),
        serde_json::to_value(cgroup_memory_operation_delta).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_stat_delta_total".to_owned(),
        serde_json::to_value(cgroup_memory_total_delta).unwrap_or_default(),
    );
    result.insert(
        "cgroup_memory_snapshot_note".to_owned(),
        serde_json::json!("These optional readings are cgroup-wide boundary snapshots, not process-only values. memory.current and memory.peak are reported as snapshot values; signed endpoint deltas apply to selected memory.stat fields. Byte-valued memory.stat gauges can decrease; memory.stat updates, including counters, can be batched. pgmajfault and workingset_refault_file are counters. Endpoint deltas can include cleanup and are not reclaim or peak attribution. memory.peak is the raw cgroup high-water since cgroup creation."),
    );
    result.insert(
        "allocator".to_owned(),
        serde_json::json!(if cfg!(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        )) {
            "jemalloc-instrumented"
        } else {
            "system"
        }),
    );
    result.insert(
        "jemalloc_sampled_peak_is_approximate".to_owned(),
        serde_json::json!(cfg!(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        ))),
    );
    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    {
        result.insert(
            "jemalloc_sampled_peak_available".to_owned(),
            serde_json::json!(jemalloc_peak_available),
        );
        result.insert(
            "jemalloc_baseline".to_owned(),
            serde_json::to_value(jemalloc_baseline).unwrap_or_default(),
        );
        result.insert(
            "jemalloc_end".to_owned(),
            serde_json::to_value(jemalloc_end).unwrap_or_default(),
        );
        result.insert(
            "jemalloc_sampled_peak".to_owned(),
            if jemalloc_peak_available {
                serde_json::to_value(jemalloc_sampled_peak).unwrap_or_default()
            } else {
                serde_json::Value::Null
            },
        );
    }
    println!("{}", serde_json::to_string(result).unwrap());
    Ok(())
}

fn run_download_plan_child(
    fixture_dir: &Path,
    chunk_count: usize,
    size_mib: usize,
    filesystem: FilesystemInfo,
) -> Result<(), String> {
    #[cfg(not(feature = "bench-alloc-stats"))]
    {
        let _ = (fixture_dir, chunk_count, size_mib, filesystem);
        Err("preinstall-download-plan requires --features bench-alloc-stats".to_owned())
    }
    #[cfg(feature = "bench-alloc-stats")]
    {
        let total_bytes = size_mib
            .checked_mul(1024 * 1024)
            .ok_or_else(|| "download-plan size is too large".to_owned())?;
        let base_chunk_size = total_bytes / chunk_count;
        let larger_chunks = total_bytes % chunk_count;
        let mut chunk_offset = 0usize;
        let mut chunks = Vec::with_capacity(chunk_count);
        for index in 0..chunk_count {
            let chunk_size = base_chunk_size + usize::from(index < larger_chunks);
            let chunk_offset_u64 = u64::try_from(chunk_offset)
                .map_err(|_| "download-plan chunk offset does not fit u64".to_owned())?;
            chunks.push(SophonManifestAssetChunk::from_parts(
                format!("download-plan-{index:08}.zstd"),
                bytes::Bytes::from("0123456789abcdef0123456789abcdef"),
                chunk_offset_u64,
                chunk_size as u64,
                chunk_size as u64,
                0,
                bytes::Bytes::from("0123456789abcdef0123456789abcdef"),
                -1,
            ));
            chunk_offset = chunk_offset
                .checked_add(chunk_size)
                .ok_or_else(|| "download-plan chunk offset overflow".to_owned())?;
        }
        let client = reqwest::Client::new();
        let plan_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?;
        let chunk_download = DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: Compression::None,
            url_prefix: "http://127.0.0.1:0/unused".to_owned(),
            url_suffix: String::new(),
        };
        let handle = DownloadHandle::new();
        let chunks_dir = fixture_dir.join("chunks");
        let mut sampler = MemorySampler::start();
        thread::sleep(std::time::Duration::from_millis(5));
        let baseline = read_memory_snapshot()?;
        let hwm_reset = reset_hwm().is_ok();
        let operation_baseline = read_memory_snapshot()?;
        let started = Instant::now();
        let eager_started = Instant::now();
        let eager_window = allocation_stats::AllocationWindow::start().map_err(str::to_owned)?;
        let eager_batch = irmin_rs::game_installer::benchmark_download_over_future_batch(
            &chunks,
            &chunks_dir,
            &client,
            &chunk_download,
            &handle,
        );
        let eager_inline_future_bytes = std::mem::size_of_val(&eager_batch);
        drop(eager_batch);
        let eager_allocation = eager_window.finish().map_err(str::to_owned)?;
        let eager_materialization_ms = eager_started.elapsed().as_millis();
        let rss_after_eager = read_memory_snapshot()?;

        let single_started = Instant::now();
        let single_window = allocation_stats::AllocationWindow::start().map_err(str::to_owned)?;
        let single_future = irmin_rs::game_installer::benchmark_download_over_chunk_future(
            &chunks[0],
            &chunks_dir,
            &client,
            &chunk_download,
            &handle,
        );
        let single_inline_future_bytes = std::mem::size_of_val(&single_future);
        let single_future_growth_bytes = allocation_stats::current_live_growth()
            .map_err(str::to_owned)?
            .unwrap_or(0);
        drop(single_future);
        let single_allocation = single_window.finish().map_err(str::to_owned)?;
        let single_future_construction_ms = single_started.elapsed().as_millis();
        let bounded_in_flight_estimate_bytes = single_future_growth_bytes
            .saturating_mul(irmin_rs::game_installer::download_concurrency() as u64);
        let bounded_future_state_estimate_bytes = (single_future_growth_bytes
            .saturating_add(single_inline_future_bytes as u64))
        .saturating_mul(irmin_rs::game_installer::download_concurrency() as u64);

        let pending_started = Instant::now();
        let pending_window = allocation_stats::AllocationWindow::start().map_err(str::to_owned)?;
        let pending_future_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pending_stream =
            irmin_rs::game_installer::benchmark_download_over_bounded_pending_stream(
                &chunks,
                &chunks_dir,
                &client,
                &chunk_download,
                &handle,
                std::sync::Arc::clone(&pending_future_count),
            );
        let pending_stream_inline_size_bytes = std::mem::size_of_val(&pending_stream);
        let mut pending_stream = Box::pin(pending_stream);
        let waker = futures_util::task::noop_waker_ref();
        let mut context = std::task::Context::from_waker(waker);
        let poll_result = futures_util::Stream::poll_next(pending_stream.as_mut(), &mut context);
        if !matches!(poll_result, std::task::Poll::Pending) {
            return Err("bounded pending stream completed unexpectedly".to_owned());
        }
        let pending_future_count = pending_future_count.load(std::sync::atomic::Ordering::Relaxed);
        let expected_pending_future_count =
            chunk_count.min(irmin_rs::game_installer::download_concurrency());
        if pending_future_count != expected_pending_future_count {
            return Err(format!(
                "bounded pending stream retained {pending_future_count} futures; expected {expected_pending_future_count}"
            ));
        }
        let pending_live_growth = allocation_stats::current_live_growth()
            .map_err(str::to_owned)?
            .unwrap_or(0);
        drop(pending_stream);
        let pending_allocation = pending_window.finish().map_err(str::to_owned)?;
        let rss_after_pending = read_memory_snapshot()?;

        let drain_started = Instant::now();
        let drain_window = allocation_stats::AllocationWindow::start().map_err(str::to_owned)?;
        let drain_stream = irmin_rs::game_installer::benchmark_download_over_bounded_drain_stream(
            &chunks,
            &chunks_dir,
            &client,
            &chunk_download,
            &handle,
        );
        let drained_chunks = plan_runtime.block_on(async move {
            let mut drain_stream = Box::pin(drain_stream);
            let mut completed = 0usize;
            while let Some(result) = drain_stream.next().await {
                result.map_err(|error| error.to_string())?;
                completed += 1;
            }
            Ok::<_, String>(completed)
        })?;
        let drain_allocation = drain_window.finish().map_err(str::to_owned)?;
        let drain_elapsed_ms = drain_started.elapsed().as_millis();
        let rss_after_drain = read_memory_snapshot()?;
        let elapsed_ms = started.elapsed().as_millis();
        let operation_end = read_smaps_rollup_snapshot(read_memory_snapshot()?);
        let sampled_peak = sampler.finish();
        let kernel_hwm_peak_rss_kib = operation_end.vm_hwm_kib.max(operation_end.vm_rss_kib);
        let peak_rss_kib = kernel_hwm_peak_rss_kib.max(sampled_peak.vm_rss_kib);
        let rss_baseline_kib = if hwm_reset {
            operation_baseline.vm_rss_kib
        } else {
            baseline.vm_rss_kib
        };
        let eager_metrics = serde_json::json!({
            "heap_peak_bytes": eager_allocation.peak_live_bytes,
            "heap_live_delta_end_bytes": eager_allocation.live_bytes_at_end,
            "allocation_request_bytes": eager_allocation.allocation_request_bytes,
            "inline_future_size_bytes": eager_inline_future_bytes,
            "rss_after_drop_kib": rss_after_eager.vm_rss_kib,
            "rss_anon_after_drop_kib": rss_after_eager.rss_anon_kib,
            "rss_file_after_drop_kib": rss_after_eager.rss_file_kib,
        });
        let bounded_pending_metrics = serde_json::json!({
            "concurrency": irmin_rs::game_installer::download_concurrency(),
            "pending_future_count": pending_future_count,
            "pending_live_growth_bytes": pending_live_growth,
            "allocator_peak_bytes": pending_allocation.peak_live_bytes,
            "pending_stream_inline_size_bytes": pending_stream_inline_size_bytes,
            "rss_after_poll_kib": rss_after_pending.vm_rss_kib,
            "rss_anon_after_poll_kib": rss_after_pending.rss_anon_kib,
            "rss_file_after_poll_kib": rss_after_pending.rss_file_kib,
        });
        let bounded_drain_metrics = serde_json::json!({
            "concurrency": irmin_rs::game_installer::download_concurrency(),
            "stream_items_drained": drained_chunks,
            "dedup_entry_count": chunk_count,
            "dedup_metadata_scales_with_unique_chunks": true,
            "in_flight_heap_estimate_bytes": bounded_in_flight_estimate_bytes,
            "bounded_stream_allocator_peak_bytes": drain_allocation.peak_live_bytes,
            "bounded_stream_allocator_live_end_bytes": drain_allocation.live_bytes_at_end,
            "rss_after_drop_kib": rss_after_drain.vm_rss_kib,
            "rss_anon_after_drop_kib": rss_after_drain.rss_anon_kib,
            "rss_file_after_drop_kib": rss_after_drain.rss_file_kib,
        });
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "scenario": Scenario::PreinstallDownloadPlan.name(),
                "fixture_kind": "synthetic-download-future-plan",
                "representative": false,
                "size_mib": size_mib,
                "unique_chunk_count": chunk_count,
                "chunk_count_distribution": "even_with_remainder_in_early_chunks",
                "fixture_backing": "none",
                "fixture_fs_magic": format!("0x{:x}", filesystem.magic),
                "fixture_fs_type": filesystem.name,
                "operation": "compare_eager_batch_with_bounded_pending_and_drain_phases",
                "eager": eager_metrics,
                "eager_materialization_ms": eager_materialization_ms,
                "single_production_future_inline_size_bytes": single_inline_future_bytes,
                "single_production_future_heap_peak_bytes": single_allocation.peak_live_bytes,
                "single_future_construction_ms": single_future_construction_ms,
                "bounded_future_state_estimate_bytes": bounded_future_state_estimate_bytes,
                "bounded_pending": bounded_pending_metrics,
                "bounded_pending_poll_elapsed_ns": pending_started.elapsed().as_nanos(),
                "bounded_drain": bounded_drain_metrics,
                "bounded_future_drain_ms": drain_elapsed_ms,
                "allocator_metric_scope": "process_wide_system_requested_bytes_net_growth",
                "rss_baseline_kib": rss_baseline_kib,
                "peak_rss_kib": peak_rss_kib,
                "peak_delta_kib": peak_rss_kib.saturating_sub(rss_baseline_kib),
                "operation_end_rss": operation_end,
                "sampled_peak_rss": sampled_peak,
                "elapsed_ms": elapsed_ms,
                "sampling_interval_ms": 2,
                "notes": [
                    "Eager batch uses the production try_join_all builder and is dropped without polling or network I/O.",
                    "The bounded_pending phase polls the production buffer_unordered stream once; each future holds the actual production download future behind a pending gate before network I/O.",
                    "The bounded_drain phase drains all items by yielding once and dropping the inner download future; it measures O(N) dedup metadata and scheduler bookkeeping, not HTTP buffers.",
                    "The single-future estimate multiplies one actual production chunk-future allocation by DOWNLOAD_CONCURRENCY.",
                    "Bounded phases follow the eager phase in this child; allocator net live deltas are phase scoped, but process VmHWM remains eager-inclusive.",
                    "Excludes base manifest metadata and active HTTP buffers; isolates retained chunk-future materialization.",
                    "Heap values are requested-byte net growth, not RSS or physical residency."
                ]
            }))
            .unwrap()
        );
        Ok(())
    }
}

fn reset_hwm() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        fs::write("/proc/self/clear_refs", b"5")
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "Linux required",
        ))
    }
}

fn read_cgroup_memory_limit() -> Result<(u64, PathBuf), String> {
    #[cfg(target_os = "linux")]
    {
        let membership = fs::read_to_string("/proc/self/cgroup")
            .map_err(|error| format!("failed reading cgroup membership: {error}"))?;
        let relative = membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))
            .ok_or_else(|| "process is not in a unified cgroup v2 hierarchy".to_owned())?;
        let membership_path = Path::new(relative);
        if !membership_path.is_absolute()
            || membership_path
                .components()
                .any(|component| component == std::path::Component::ParentDir)
        {
            return Err("invalid cgroup v2 membership path".to_owned());
        }
        let mountinfo = fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("failed reading cgroup mountinfo: {error}"))?;
        let (mount_root, mount_point) = find_cgroup2_mount(&mountinfo, membership_path)?;
        let relative_to_mount = membership_path
            .strip_prefix(&mount_root)
            .map_err(|_| "cgroup membership is outside the mounted cgroup2 root".to_owned())?;
        if relative_to_mount
            .components()
            .any(|component| component == std::path::Component::ParentDir)
        {
            return Err("cgroup membership escapes the mounted cgroup2 root".to_owned());
        }
        let path = mount_point.join(relative_to_mount);
        let value = fs::read_to_string(path.join("memory.max"))
            .map_err(|error| format!("failed reading child cgroup memory.max: {error}"))?;
        let limit = value.trim().parse::<u64>().map_err(|_| {
            format!("child cgroup memory.max is not a finite byte limit: {value:?}")
        })?;
        Ok((limit, path))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("cgroup memory limits require Linux".to_owned())
    }
}

fn read_cgroup_memory_events(path: &Path) -> Result<CgroupMemoryEvents, String> {
    let contents = fs::read_to_string(path.join("memory.events"))
        .map_err(|error| format!("failed reading cgroup memory.events: {error}"))?;
    let mut events = CgroupMemoryEvents::default();
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(key) = parts.next() else {
            continue;
        };
        let Some(value) = parts.next() else {
            return Err(format!("malformed cgroup memory.events line: {line:?}"));
        };
        let value = value
            .parse::<u64>()
            .map_err(|_| format!("invalid cgroup memory.events counter: {line:?}"))?;
        match key {
            "low" => events.low = value,
            "high" => events.high = value,
            "max" => events.max = value,
            "oom" => events.oom = value,
            "oom_kill" => events.oom_kill = value,
            "oom_group_kill" => events.oom_group_kill = value,
            _ => {}
        }
    }
    Ok(events)
}

fn read_cgroup_memory_pressure(path: &Path) -> Result<CgroupPressure, String> {
    let contents = fs::read_to_string(path.join("memory.pressure"))
        .map_err(|error| format!("failed reading cgroup memory.pressure: {error}"))?;
    let mut some_total_us = None;
    let mut full_total_us = None;
    for line in contents.lines() {
        let mut parts = line.split_whitespace();
        let Some(kind) = parts.next() else {
            continue;
        };
        let mut total = None;
        for field in parts {
            if let Some(value) = field.strip_prefix("total=") {
                total = Some(
                    value
                        .parse::<u64>()
                        .map_err(|_| format!("invalid cgroup memory.pressure total: {line:?}"))?,
                );
            }
        }
        let total = total.ok_or_else(|| format!("missing total in memory.pressure: {line:?}"))?;
        match kind {
            "some" => some_total_us = Some(total),
            "full" => full_total_us = Some(total),
            _ => {}
        }
    }
    Ok(CgroupPressure {
        some_total_us: some_total_us
            .ok_or_else(|| "missing some line in memory.pressure".to_owned())?,
        full_total_us: full_total_us
            .ok_or_else(|| "missing full line in memory.pressure".to_owned())?,
    })
}

fn read_cgroup_memory_snapshot(path: &Path) -> CgroupMemorySnapshot {
    let current_bytes = fs::read_to_string(path.join("memory.current"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let peak_bytes = fs::read_to_string(path.join("memory.peak"))
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok());
    let stat = fs::read_to_string(path.join("memory.stat"))
        .ok()
        .map(|contents| parse_cgroup_memory_stat(&contents))
        .unwrap_or_default();
    CgroupMemorySnapshot {
        current_bytes,
        peak_bytes,
        stat,
    }
}

fn parse_cgroup_memory_stat(contents: &str) -> CgroupMemoryStat {
    let mut stat = CgroupMemoryStat::default();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(key) = fields.next() else {
            continue;
        };
        let Some(value) = fields.next().and_then(|value| value.parse::<i64>().ok()) else {
            continue;
        };
        match key {
            "anon" => stat.anon_bytes = Some(value),
            "file" => stat.file_bytes = Some(value),
            "file_mapped" => stat.file_mapped_bytes = Some(value),
            "file_dirty" => stat.file_dirty_bytes = Some(value),
            "file_writeback" => stat.file_writeback_bytes = Some(value),
            "shmem" => stat.shmem_bytes = Some(value),
            "kernel" => stat.kernel_bytes = Some(value),
            "slab" => stat.slab_bytes = Some(value),
            "pagetables" => stat.pagetables_bytes = Some(value),
            "pgmajfault" => stat.pgmajfault = Some(value),
            "workingset_refault_file" => stat.workingset_refault_file = Some(value),
            _ => {}
        }
    }
    stat
}

#[cfg(test)]
mod cgroup_memory_stat_tests {
    use super::{CgroupMemorySnapshot, parse_cgroup_memory_stat, read_cgroup_memory_snapshot};
    use std::fs;

    #[test]
    fn parses_cgroup_memory_stat_gauges_and_counters() {
        let stat = parse_cgroup_memory_stat(
            "anon 1024\nfile 2048\nfile_mapped 512\nfile_dirty 64\nfile_writeback 32\nshmem 16\nkernel 4096\nslab 128\npagetables 256\npgmajfault 7\nworkingset_refault_file 9\nunknown 11\n",
        );
        assert_eq!(stat.anon_bytes, Some(1024));
        assert_eq!(stat.file_bytes, Some(2048));
        assert_eq!(stat.file_mapped_bytes, Some(512));
        assert_eq!(stat.file_dirty_bytes, Some(64));
        assert_eq!(stat.file_writeback_bytes, Some(32));
        assert_eq!(stat.shmem_bytes, Some(16));
        assert_eq!(stat.kernel_bytes, Some(4096));
        assert_eq!(stat.slab_bytes, Some(128));
        assert_eq!(stat.pagetables_bytes, Some(256));
        assert_eq!(stat.pgmajfault, Some(7));
        assert_eq!(stat.workingset_refault_file, Some(9));
    }

    #[test]
    fn cgroup_gauge_deltas_preserve_decreases_and_require_both_samples() {
        let before = CgroupMemorySnapshot {
            current_bytes: Some(1000),
            peak_bytes: Some(2000),
            stat: parse_cgroup_memory_stat("anon 10\nfile 20\npgmajfault 5\n"),
        };
        let after = CgroupMemorySnapshot {
            current_bytes: Some(900),
            peak_bytes: Some(2500),
            stat: parse_cgroup_memory_stat("anon 8\nfile 24\npgmajfault 8\n"),
        };
        let delta = CgroupMemorySnapshot::delta(&before, &after);
        assert_eq!(delta.anon_bytes, Some(-2));
        assert_eq!(delta.file_bytes, Some(4));
        assert_eq!(delta.pgmajfault, Some(3));
        assert_eq!(delta.file_mapped_bytes, None);
    }

    #[test]
    fn absent_cgroup_memory_files_produce_empty_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let snapshot = read_cgroup_memory_snapshot(dir.path());
        assert_eq!(snapshot.current_bytes, None);
        assert_eq!(snapshot.peak_bytes, None);
        assert_eq!(snapshot.stat.anon_bytes, None);
        assert_eq!(snapshot.stat.file_bytes, None);
        fs::write(dir.path().join("memory.current"), "123\n").unwrap();
        assert_eq!(
            read_cgroup_memory_snapshot(dir.path()).current_bytes,
            Some(123)
        );
    }
}

#[cfg(target_os = "linux")]
fn find_cgroup2_mount(
    mountinfo: &str,
    membership_path: &Path,
) -> Result<(PathBuf, PathBuf), String> {
    let mut selected: Option<(PathBuf, PathBuf)> = None;
    for line in mountinfo.lines() {
        let Some((before_separator, after_separator)) = line.split_once(" - ") else {
            continue;
        };
        let before = before_separator.split_whitespace().collect::<Vec<_>>();
        let after = after_separator.split_whitespace().collect::<Vec<_>>();
        if after.first() != Some(&"cgroup2") || before.len() < 5 {
            continue;
        }
        let root = unescape_mountinfo_path(before[3])?;
        let mount_point = unescape_mountinfo_path(before[4])?;
        let root = PathBuf::from(root);
        if membership_path.strip_prefix(&root).is_ok()
            && selected.as_ref().is_none_or(|(current_root, _)| {
                root.components().count() > current_root.components().count()
            })
        {
            selected = Some((root, PathBuf::from(mount_point)));
        }
    }
    selected.ok_or_else(|| "no cgroup2 mount covers the process membership path".to_owned())
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo_path(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            if index + 3 >= bytes.len()
                || !bytes[index + 1..index + 4]
                    .iter()
                    .all(|digit| (b'0'..=b'7').contains(digit))
            {
                return Err("invalid escape in cgroup mountinfo path".to_owned());
            }
            let value = (bytes[index + 1] - b'0') * 64
                + (bytes[index + 2] - b'0') * 8
                + (bytes[index + 3] - b'0');
            decoded.push(value);
            index += 4;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded).map_err(|error| error.to_string())
}

fn read_cpu_usage() -> Result<CpuUsage, String> {
    #[cfg(target_os = "linux")]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if result != 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let usage = unsafe { usage.assume_init() };
        let timeval_micros = |time: libc::timeval| {
            (time.tv_sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(time.tv_usec as u64)
        };
        Ok(CpuUsage {
            user_micros: timeval_micros(usage.ru_utime),
            system_micros: timeval_micros(usage.ru_stime),
            minor_faults: usage.ru_minflt.max(0) as u64,
            major_faults: usage.ru_majflt.max(0) as u64,
        })
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err("CPU usage sampling requires Linux".to_owned())
    }
}

fn read_memory_snapshot() -> Result<MemorySnapshot, String> {
    let status = fs::read_to_string("/proc/self/status").map_err(|error| error.to_string())?;
    Ok(parse_memory_status(&status))
}

fn read_smaps_rollup_snapshot(mut snapshot: MemorySnapshot) -> MemorySnapshot {
    #[cfg(target_os = "linux")]
    if let Ok(contents) = fs::read_to_string("/proc/self/smaps_rollup") {
        parse_smaps_rollup(&contents, &mut snapshot);
    }
    snapshot
}

fn parse_memory_status(status: &str) -> MemorySnapshot {
    let mut snapshot = MemorySnapshot::default();
    for line in status.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<u64>().ok());
        match (key, value) {
            ("VmRSS", Some(value)) => snapshot.vm_rss_kib = value,
            ("VmHWM", Some(value)) => snapshot.vm_hwm_kib = value,
            ("RssAnon", Some(value)) => snapshot.rss_anon_kib = Some(value),
            ("RssFile", Some(value)) => snapshot.rss_file_kib = Some(value),
            ("RssShmem", Some(value)) => snapshot.rss_shmem_kib = Some(value),
            _ => {}
        }
    }
    snapshot
}

fn parse_smaps_rollup(contents: &str, snapshot: &mut MemorySnapshot) {
    for line in contents.lines() {
        let Some((key, rest)) = line.split_once(':') else {
            continue;
        };
        let value = rest
            .split_whitespace()
            .next()
            .and_then(|value| value.parse::<u64>().ok());
        match (key, value) {
            ("Pss", Some(value)) => snapshot.pss_kib = Some(value),
            ("Private_Clean", Some(value)) => snapshot.private_clean_kib = Some(value),
            ("Private_Dirty", Some(value)) => snapshot.private_dirty_kib = Some(value),
            ("Shared_Clean", Some(value)) => snapshot.shared_clean_kib = Some(value),
            ("Shared_Dirty", Some(value)) => snapshot.shared_dirty_kib = Some(value),
            ("Anonymous", Some(value)) => snapshot.smaps_anonymous_kib = Some(value),
            _ => {}
        }
    }
}

fn prepare_fixture(
    scenario: Scenario,
    dir: &Path,
    size_mib: usize,
    chunks: usize,
    cover_pattern: CoverPattern,
    preinstall_mode: PreinstallMode,
) -> Result<(), String> {
    match scenario {
        Scenario::Hdiff | Scenario::PreinstallApply => {
            if scenario == Scenario::PreinstallApply && preinstall_mode == PreinstallMode::CopyOver
            {
                prepare_copyover_fixture(dir, size_mib)
            } else {
                prepare_hdiff_fixture(dir, size_mib, cover_pattern)
            }
        }
        Scenario::PreinstallDownloadPlan => Ok(()),
        Scenario::Assembly => prepare_assembly_fixture(dir, size_mib, chunks),
        Scenario::SophonInstall => prepare_install_fixture(dir, size_mib, chunks),
    }
}

fn prepare_hdiff_fixture(
    dir: &Path,
    size_mib: usize,
    cover_pattern: CoverPattern,
) -> Result<(), String> {
    if cover_pattern == CoverPattern::UncoveredPrefix {
        prepare_shared_uncovered_prefix_fixture(dir, size_mib)?;
        return Ok(());
    }
    if cover_pattern == CoverPattern::Hdiff20Steps {
        prepare_shared_hdiff20_step_fixture(dir, size_mib)?;
        return Ok(());
    }
    if cover_pattern == CoverPattern::Sequential {
        return prepare_shared_hdiff_fixture(dir, size_mib);
    }
    const BLOCK_SIZE: usize = 64 * 1024;
    let block_count = size_mib * 1024 * 1024 / BLOCK_SIZE;
    let old = deterministic_data(0xDEAD, block_count * BLOCK_SIZE);
    let permutation = match cover_pattern {
        CoverPattern::Reverse => (0..block_count).rev().collect::<Vec<_>>(),
        CoverPattern::Thrash => {
            let step = (5..block_count)
                .step_by(2)
                .find(|step| gcd(*step, block_count) == 1)
                .unwrap_or(1);
            (0..block_count)
                .map(|index| (index * step) % block_count)
                .collect::<Vec<_>>()
        }
        CoverPattern::Sequential => unreachable!(),
        CoverPattern::UncoveredPrefix => unreachable!(),
        CoverPattern::Hdiff20Steps => unreachable!(),
    };
    let (patch, new) = make_v13_zstd_reordered_patch(&old, &permutation);
    fs::write(dir.join("old.bin"), &old).map_err(|error| error.to_string())?;
    fs::write(dir.join("new.bin"), &new).map_err(|error| error.to_string())?;
    fs::write(dir.join("patch.hdiff"), patch).map_err(|error| error.to_string())
}

fn prepare_copyover_fixture(dir: &Path, size_mib: usize) -> Result<(), String> {
    let size = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "CopyOver fixture size is too large".to_owned())?;
    let old = deterministic_data(0xC0_0A, size);
    let mut new = deterministic_data(0xC0_0B, size);
    if new.starts_with(b"HDIFF") {
        new[0] ^= 0xff;
    }
    fs::write(dir.join("old.bin"), old).map_err(|error| error.to_string())?;
    fs::write(dir.join("new.bin"), &new).map_err(|error| error.to_string())?;
    fs::write(dir.join("patch.hdiff"), new).map_err(|error| error.to_string())
}

fn gcd(mut left: usize, mut right: usize) -> usize {
    while right != 0 {
        (left, right) = (right, left % right);
    }
    left
}

fn prepare_assembly_fixture(dir: &Path, size_mib: usize, chunks: usize) -> Result<(), String> {
    let chunk_size = size_mib * 1024 * 1024 / chunks;
    let chunk_dir = dir.join("chunks");
    fs::create_dir_all(&chunk_dir).map_err(|error| error.to_string())?;
    for index in 0..chunks {
        let data = deterministic_data(0x9e37_79b9_7f4a_7c15 + index as u64, chunk_size);
        let compressed = zstd::encode_all(data.as_slice(), 3).map_err(|error| error.to_string())?;
        fs::write(
            chunk_dir.join(chunk_filename(&format!("chunk-{index:04}"))),
            compressed,
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn prepare_install_fixture(dir: &Path, size_mib: usize, chunks: usize) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|error| error.to_string())?;
    let chunk_size = size_mib * 1024 * 1024 / chunks;
    for index in 0..chunks {
        let raw = deterministic_data(0xBEEF + index as u64, chunk_size);
        let compressed = zstd::encode_all(raw.as_slice(), 3).map_err(|error| error.to_string())?;
        fs::write(dir.join(format!("chunk-{index:04}")), compressed)
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn prepare_child_context(
    scenario: Scenario,
    dir: &Path,
    size_mib: usize,
    chunks: usize,
    asset_count: usize,
    preinstall_mode: PreinstallMode,
    server_url: &str,
    resume_cache: bool,
) -> Result<ScenarioContext, String> {
    if asset_count == 0 || asset_count > 8 {
        return Err("asset_count must be between 1 and 8".to_owned());
    }
    match scenario {
        Scenario::PreinstallDownloadPlan => {
            Err("preinstall-download-plan uses its dedicated operation".to_owned())
        }
        Scenario::Hdiff => Ok(ScenarioContext::Hdiff {
            dir: dir.to_path_buf(),
        }),
        Scenario::PreinstallApply => {
            let mut game_dirs = Vec::with_capacity(asset_count);
            let mut expected_size = None;
            for index in 0..asset_count {
                let game_dir = if asset_count == 1 {
                    dir.join("synthetic-game")
                } else {
                    dir.join(format!("synthetic-game-{index:02}"))
                };
                let state_builder = match preinstall_mode {
                    PreinstallMode::Patch => {
                        irmin_rs::game_installer::prepare_benchmark_preinstall_state
                    }
                    PreinstallMode::CopyOver => {
                        irmin_rs::game_installer::prepare_benchmark_copyover_state
                    }
                };
                let (_, size) = state_builder(
                    &game_dir,
                    &dir.join("old.bin"),
                    &dir.join("new.bin"),
                    &dir.join("patch.hdiff"),
                    "benchmark/asset.bin",
                )
                .map_err(|error| error.to_string())?;
                expected_size.get_or_insert(size);
                game_dirs.push(game_dir);
            }
            Ok(ScenarioContext::PreinstallApply(Box::new(
                PreinstallApplyContext {
                    game_dirs,
                    expected_path: dir.join("new.bin"),
                    expected_size: usize::try_from(expected_size.unwrap_or_default())
                        .map_err(|_| "benchmark output too large".to_owned())?,
                    runtime: tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())?,
                    client: reqwest::Client::new(),
                    diagnose_hdiff_phases: false,
                },
            )))
        }
        Scenario::Assembly => prepare_assembly_context(dir, size_mib, chunks),
        Scenario::SophonInstall => {
            prepare_install_context(dir, size_mib, chunks, server_url, resume_cache)
        }
    }
}

fn prepare_assembly_context(
    dir: &Path,
    size_mib: usize,
    chunks: usize,
) -> Result<ScenarioContext, String> {
    let chunk_size = size_mib * 1024 * 1024 / chunks;
    let chunks_dir = dir.join("chunks");
    for index in 0..chunks {
        let chunk_path = chunks_dir.join(chunk_filename(&format!("chunk-{index:04}")));
        if !chunk_path.is_file() {
            return Err(format!("missing benchmark chunk {}", chunk_path.display()));
        }
    }
    let game_dir = dir.join("game");
    let tmp_dir = dir.join("tmp");
    fs::create_dir_all(&game_dir).map_err(|error| error.to_string())?;
    fs::create_dir_all(&tmp_dir).map_err(|error| error.to_string())?;
    let mut expected_hasher = Md5::new();
    for index in 0..chunks {
        expected_hasher.update(&deterministic_data(
            0x9e37_79b9_7f4a_7c15 + index as u64,
            chunk_size,
        ));
    }
    let expected_hash = hex::encode(expected_hasher.finalize());
    let expected_path = dir.join("expected.bin");
    let mut expected_file = File::create(&expected_path).map_err(|error| error.to_string())?;
    for index in 0..chunks {
        expected_file
            .write_all(&deterministic_data(
                0x9e37_79b9_7f4a_7c15 + index as u64,
                chunk_size,
            ))
            .map_err(|error| error.to_string())?;
    }
    let chunk_rows = (0..chunks)
        .map(|index| {
            let raw = deterministic_data(0x9e37_79b9_7f4a_7c15 + index as u64, chunk_size);
            let compressed_path = chunks_dir.join(chunk_filename(&format!("chunk-{index:04}")));
            let compressed = fs::read(&compressed_path).expect("generated chunk exists");
            SophonManifestAssetChunk::from_parts(
                format!("chunk-{index:04}").into(),
                bytes::Bytes::from(hex::encode(fast_md5::digest(&raw))),
                (index * chunk_size) as u64,
                compressed.len() as u64,
                chunk_size as u64,
                0,
                bytes::Bytes::from(hex::encode(fast_md5::digest(&compressed))),
                -1,
            )
        })
        .collect::<Vec<_>>();
    let manifest = CompactManifest::from(vec![SophonManifestAssetProperty::from_parts(
        bytes::Bytes::from("assembled.bin"),
        chunk_rows,
        0,
        (size_mib * 1024 * 1024) as u64,
        expected_hash.into(),
    )]);
    let names = (0..chunks)
        .map(|index| Box::leak(format!("chunk-{index:04}").into_boxed_str()) as &str)
        .collect::<Vec<_>>();
    let lookup = ChunkNameLookup::from_arena(StringArena::from(names.as_slice()));
    let refcounts = (0..chunks).map(|_| AtomicU32::new(1)).collect::<Vec<_>>();
    let cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();
    Ok(ScenarioContext::Assembly {
        context: Box::new(AssemblyContext {
            manifest,
            game_dir,
            chunks_dir,
            tmp_dir,
            expected_path,
            lookup,
            refcounts,
            cache,
            verify_mode: VerifyMode::Full,
            demand_mmap: false,
        }),
    })
}

fn prepare_install_context(
    dir: &Path,
    size_mib: usize,
    chunks: usize,
    server_url: &str,
    resume_cache: bool,
) -> Result<ScenarioContext, String> {
    if server_url.is_empty() && !resume_cache {
        return Err("sophon-install requires a local fixture server".to_owned());
    }
    if resume_cache && !(2..=5_000).contains(&chunks) {
        return Err("resume-cache requires between 2 and 5000 chunks".to_owned());
    }
    let game_dir = dir.join("install-game");
    let fixture_chunk_index = |index: usize| {
        if resume_cache && index + 1 == chunks {
            0
        } else {
            index
        }
    };
    let chunk_size = size_mib * 1024 * 1024 / chunks;
    let mut file_hasher = Md5::new();
    let chunk_rows = (0..chunks)
        .map(|index| {
            let fixture_index = fixture_chunk_index(index);
            let raw = deterministic_data(0xBEEF + fixture_index as u64, chunk_size);
            file_hasher.update(&raw);
            let compressed_path = dir.join(format!("chunk-{fixture_index:04}"));
            let compressed = fs::read(&compressed_path).map_err(|error| error.to_string())?;
            Ok(SophonManifestAssetChunk::from_parts(
                format!("chunk-{fixture_index:04}").into(),
                bytes::Bytes::from(hex::encode(fast_md5::digest(&raw))),
                (index * chunk_size) as u64,
                compressed.len() as u64,
                chunk_size as u64,
                0,
                bytes::Bytes::from(hex::encode(fast_md5::digest(&compressed))),
                -1,
            ))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let expected_path = dir.join("expected-install.bin");
    let mut expected_file = File::create(&expected_path).map_err(|error| error.to_string())?;
    for index in 0..chunks {
        let fixture_index = fixture_chunk_index(index);
        expected_file
            .write_all(&deterministic_data(
                0xBEEF + fixture_index as u64,
                chunk_size,
            ))
            .map_err(|error| error.to_string())?;
    }
    let file_md5 = hex::encode(file_hasher.finalize());
    let manifest = SophonManifestProto {
        assets: vec![SophonManifestAssetProperty::from_parts(
            bytes::Bytes::from("installed.bin"),
            chunk_rows,
            0,
            (size_mib * 1024 * 1024) as u64,
            bytes::Bytes::from(file_md5),
        )],
    };
    let installer = SophonInstaller {
        manifest,
        chunk_download: DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: Compression::None,
            url_prefix: server_url.to_owned(),
            url_suffix: "chunks".to_owned(),
        },
        label: "memory-bench".to_owned(),
        matching_field: "bench".to_owned(),
        manifest_hash: "memory-bench-manifest".to_owned(),
    };
    let mut resume = ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: Default::default(),
        resume_seed: CompletedFiles::default(),
    };
    if resume_cache {
        let chunks_dir = game_dir.join("chunks");
        fs::create_dir_all(&chunks_dir).map_err(|error| error.to_string())?;
        let cache = VerificationCache::new();
        for chunk in installer.manifest.assets[0].asset_chunks[..chunks - 1].iter() {
            let cached_path = chunks_dir.join(chunk_filename(chunk.chunk_name_str()));
            fs::hard_link(dir.join(chunk.chunk_name_str()), &cached_path)
                .map_err(|error| error.to_string())?;
            let valid = irmin_rs::game_installer::cache::check_file_md5_cached_with_size(
                &cached_path,
                chunk.chunk_size,
                irmin_rs::game_installer::assembly_opt::hex_to_bytes(chunk.chunk_compressed_hash_md5_str()).unwrap_or_default().as_slice(),
                &game_dir,
                &cache,
            )
            .map_err(|error| error.to_string())?;
            if !valid.0 {
                return Err("failed to seed resume verification cache".to_owned());
            }
            resume
                .prev_downloaded_chunks
                .insert(chunk.chunk_name_str().to_string(), chunk.chunk_size);
        }
        irmin_rs::game_installer::cache::save_verification_cache(&game_dir, &cache)
            .map_err(|error| error.to_string())?;
        let mut hasher = sha2::Sha256::new();
        hasher.update(installer.manifest_hash.as_bytes());
        resume.prev_manifest_hash = hex::encode(&hasher.finalize()[..8]);
    }
    Ok(ScenarioContext::SophonInstall(Box::new(InstallContext {
        installer,
        game_dir,
        expected_path,
        size_mib,
        resume_cache,
        resume,
        runtime: tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| error.to_string())?,
    })))
}

fn run_operation(
    context: ScenarioContext,
    hdiff_source_backend: HDiffSourceBackend,
) -> Result<Verification, String> {
    match context {
        ScenarioContext::Hdiff { dir } => {
            apply_hdiff_fixture_with_backend(&dir, hdiff_source_backend)?;
            Ok(Verification::Hdiff(dir))
        }
        ScenarioContext::PreinstallApply(context) => {
            let PreinstallApplyContext {
                game_dirs,
                expected_path,
                expected_size,
                runtime,
                client,
                #[cfg(feature = "bench-diagnostics")]
                diagnose_hdiff_phases,
                ..
            } = *context;
            #[cfg(feature = "bench-alloc-stats")]
            let mut asset_heap_live_growth_bytes = Vec::with_capacity(game_dirs.len());
            #[cfg(feature = "bench-diagnostics")]
            let mut hdiff_phase_timings = None;
            for game_dir in &game_dirs {
                #[cfg(feature = "bench-diagnostics")]
                if diagnose_hdiff_phases {
                    let target_relative_path = "benchmark/asset.bin";
                    let timings = irmin_rs::game_installer::apply_synthetic_hdiff_for_benchmark(
                        game_dir,
                        "synthetic-target",
                        target_relative_path,
                    )
                    .map_err(|error| format!("diagnostic preinstall apply failed: {error}"))?;
                    hdiff_phase_timings = Some(timings);
                } else {
                    runtime
                        .block_on(irmin_rs::game_installer::apply_preinstall(
                            &client,
                            game_dir,
                            "synthetic-target",
                            Arc::new(|_| {}),
                            &DownloadHandle::new(),
                        ))
                        .map_err(|error| format!("preinstall apply failed: {error}"))?;
                }
                #[cfg(not(feature = "bench-diagnostics"))]
                runtime
                    .block_on(irmin_rs::game_installer::apply_preinstall(
                        &client,
                        game_dir,
                        "synthetic-target",
                        Arc::new(|_| {}),
                        &DownloadHandle::new(),
                    ))
                    .map_err(|error| format!("preinstall apply failed: {error}"))?;
                #[cfg(feature = "bench-alloc-stats")]
                if let Some(checkpoint) =
                    allocation_stats::current_live_growth().map_err(str::to_owned)?
                {
                    asset_heap_live_growth_bytes.push(checkpoint);
                }
            }
            #[cfg(feature = "bench-diagnostics")]
            if let Some(timings) = hdiff_phase_timings {
                return Ok(Verification::HdiffPreinstall {
                    expected: expected_path,
                    actual: game_dirs[0].join("benchmark/asset.bin"),
                    size: expected_size,
                    timings,
                });
            }
            Ok(Verification::FileCompare {
                expected: expected_path,
                actuals: game_dirs
                    .into_iter()
                    .map(|game_dir| game_dir.join("benchmark/asset.bin"))
                    .collect(),
                size: expected_size,
                #[cfg(feature = "bench-alloc-stats")]
                asset_heap_live_growth_bytes,
            })
        }
        ScenarioContext::Assembly { context } => {
            let AssemblyContext {
                manifest,
                game_dir,
                chunks_dir,
                tmp_dir,
                expected_path,
                lookup,
                refcounts,
                cache,
                verify_mode,
                demand_mmap,
            } = *context;
            match std::env::var("IRMIN_BENCH_DOWNLOAD_OVER_HASH").as_deref() {
                Ok("fused") => {
                    let verified =
                        irmin_rs::game_installer::assembly::assemble_file_with_benchmark_file_hash(
                            &manifest,
                            0,
                            &game_dir,
                            &chunks_dir,
                            &tmp_dir,
                            &lookup,
                            &refcounts,
                            &cache,
                        )
                        .map_err(|error| format!("fused assembly failed: {error}"))?;
                    if !verified {
                        return Err("fused assembly did not verify the file hash".to_owned());
                    }
                }
                Ok("current") => {
                    irmin_rs::game_installer::assembly::assemble_file_with_benchmark_mmap_advice(
                        &manifest,
                        0,
                        &game_dir,
                        &chunks_dir,
                        &tmp_dir,
                        &lookup,
                        &refcounts,
                        &cache,
                        true,
                        verify_mode,
                        demand_mmap,
                    )
                    .map_err(|error| format!("assembly failed: {error}"))?;
                    let actual = irmin_rs::game_installer::cache::file_md5_digest(
                        &game_dir.join("assembled.bin"),
                    )
                    .map_err(|error| format!("DownloadOver output hash failed: {error}"))?;
                    if !hex::encode(actual).eq_ignore_ascii_case(
                        &irmin_rs::game_installer::assembly_opt::bytes_to_hex(
                            manifest.file_hash_md5(0),
                        ),
                    ) {
                        return Err("DownloadOver output hash mismatch".to_owned());
                    }
                }
                _ => {
                    irmin_rs::game_installer::assembly::assemble_file_with_benchmark_mmap_advice(
                        &manifest,
                        0,
                        &game_dir,
                        &chunks_dir,
                        &tmp_dir,
                        &lookup,
                        &refcounts,
                        &cache,
                        true,
                        verify_mode,
                        demand_mmap,
                    )
                    .map_err(|error| format!("assembly failed: {error}"))?;
                }
            }
            Ok(Verification::Assembly {
                expected: expected_path,
                actual: game_dir.join("assembled.bin"),
                size: manifest.file_size(0) as usize,
            })
        }
        ScenarioContext::SophonInstall(context) => {
            let InstallContext {
                installer,
                game_dir,
                expected_path,
                size_mib,
                resume_cache,
                resume,
                runtime,
            } = *context;
            runtime
                .block_on(install(
                    vec![installer],
                    &game_dir,
                    Vec::new(),
                    "bench-tag",
                    resume,
                    InstallOptions {
                        is_preinstall: false,
                        is_resume: resume_cache,
                        handle: DownloadHandle::new(),
                        verify_mode: VerifyMode::Full,
                    },
                    InstallCallbacks {
                        updater: Arc::new(|_| {}),
                        state_saver: Arc::new(|_| {}),
                        completion_state: Arc::new(OnceLock::new()),
                    },
                    "bench",
                    &[],
                ))
                .map_err(|error: SophonError| format!("Sophon install failed: {error}"))?;
            Ok(Verification::SophonInstall {
                expected: expected_path,
                actual: game_dir.join("installed.bin"),
                size: size_mib * 1024 * 1024,
            })
        }
    }
}

fn verify_output(verification: Verification) -> Result<(), String> {
    match verification {
        Verification::Hdiff(dir) => compare_hdiff_fixture(&dir),
        #[cfg(feature = "bench-diagnostics")]
        Verification::HdiffPreinstall {
            expected,
            actual,
            size,
            ..
        } => compare_files_streaming(&expected, &actual, size),
        Verification::FileCompare {
            expected,
            actuals,
            size,
            ..
        } => {
            for actual in actuals {
                compare_files_streaming(&expected, &actual, size)?;
            }
            Ok(())
        }
        Verification::Assembly {
            expected,
            actual,
            size,
        }
        | Verification::SophonInstall {
            expected,
            actual,
            size,
        } => compare_files_streaming(&expected, &actual, size),
    }
}

#[cfg(feature = "bench-diagnostics")]
fn verification_phase_timings(
    verification: &Verification,
) -> Option<irmin_rs::game_installer::HdiffPhaseTimings> {
    match verification {
        Verification::HdiffPreinstall { timings, .. } => Some(*timings),
        _ => None,
    }
}

fn compare_files_streaming(
    expected: &Path,
    actual: &Path,
    expected_len: usize,
) -> Result<(), String> {
    use std::io::Read;

    let mut expected_file = File::open(expected).map_err(|error| error.to_string())?;
    let mut actual_file = File::open(actual).map_err(|error| error.to_string())?;
    if expected_file.metadata().map_err(|e| e.to_string())?.len() != expected_len as u64
        || actual_file.metadata().map_err(|e| e.to_string())?.len() != expected_len as u64
    {
        return Err("benchmark output size mismatch".to_owned());
    }
    let mut left = [0u8; 64 * 1024];
    let mut right = [0u8; 64 * 1024];
    loop {
        let left_len = expected_file.read(&mut left).map_err(|e| e.to_string())?;
        let right_len = actual_file.read(&mut right).map_err(|e| e.to_string())?;
        if left_len != right_len || left[..left_len] != right[..right_len] {
            return Err("benchmark output validation failed".to_owned());
        }
        if left_len == 0 {
            break;
        }
    }
    Ok(())
}

fn report_results(config: &Config, results: &[serde_json::Value]) -> Result<(), String> {
    let mut peaks = results
        .iter()
        .filter_map(|result| result["peak_rss_kib"].as_u64())
        .collect::<Vec<_>>();
    peaks.sort_unstable();
    let median = peaks[peaks.len() / 2];
    let maximum = *peaks.last().unwrap();
    if config.json {
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "scenario": config.scenario.name(),
                "runs": results,
                "median_peak_rss_kib": median,
                "max_peak_rss_kib": maximum,
            }))
            .unwrap()
        );
    } else {
        println!(
            "{}: median peak {} KiB; max peak {} KiB",
            config.scenario.name(),
            median,
            maximum
        );
        for result in results {
            println!("{}", serde_json::to_string(result).unwrap());
        }
    }
    if let Some(budget) = config.budget_mib
        && maximum > budget.saturating_mul(1024)
    {
        return Err(format!("peak {maximum} KiB exceeds {budget} MiB budget"));
    }
    Ok(())
}

fn report_size_sweep(
    config: &Config,
    results: &[(usize, Vec<serde_json::Value>)],
) -> Result<(), String> {
    let points = results
        .iter()
        .map(|(size_mib, runs)| {
            let median = |field: &str| median_optional_u64(runs, field);
            serde_json::json!({
                "size_mib": size_mib,
                "repeat_count": runs.len(),
                "median_peak_rss_kib": median("peak_rss_kib"),
                "median_peak_delta_kib": median("peak_delta_kib"),
                "median_sampled_peak_anon_kib": median("sampled_max_rss_anon_kib"),
                "median_sampled_peak_file_kib": median("sampled_max_rss_file_kib"),
                "median_sampled_peak_shmem_kib": median("sampled_max_rss_shmem_kib"),
                "median_major_faults": median("major_faults"),
                "median_operation_cgroup_max_events": median_nested_u64(
                    runs,
                    "cgroup_memory_events_operation",
                    "max",
                ),
                "median_operation_cgroup_high_events": median_nested_u64(
                    runs,
                    "cgroup_memory_events_operation",
                    "high",
                ),
                "median_operation_psi_some_us": median_nested_u64(
                    runs,
                    "cgroup_memory_pressure_operation",
                    "some_total_us",
                ),
                "median_operation_psi_full_us": median_nested_u64(
                    runs,
                    "cgroup_memory_pressure_operation",
                    "full_total_us",
                ),
                "runs": runs,
            })
        })
        .collect::<Vec<_>>();
    let fit = |field: &str| fit_sweep_points(&points, field);
    let summary = serde_json::json!({
        "scenario": config.scenario.name(),
        "fixture_kind": "synthetic-preinstall",
        "representative": false,
        "fixture_backing": config.fixture_backing.name(),
        "fixture_cache_state": "warm",
        "allocator": "system",
        "interleave_sizes": config.interleave_sizes,
        "run_order": if config.interleave_sizes {
            "deterministic_rotation"
        } else {
            "grouped_by_size"
        },
        "fit_kind": "ordinary_least_squares",
        "points": points,
        "fits": {
            "peak_rss": fit("median_peak_rss_kib"),
            "peak_delta": fit("median_peak_delta_kib"),
            "sampled_peak_anon": fit("median_sampled_peak_anon_kib"),
            "sampled_peak_file": fit("median_sampled_peak_file_kib"),
            "sampled_peak_shmem": fit("median_sampled_peak_shmem_kib"),
        },
        "notes": [
            "Warm-cache disk-backed synthetic measurements; not representative preinstall validation.",
            "RSS components come from a coherent sampled peak snapshot; fits are descriptive linear approximations.",
            "Major/minor faults are process-wide; cgroup memory.events and PSI memory totals are split into setup, operation, and total deltas when memory-limited.",
            "Interleaving rotates sizes by repeat; balanced rotation requires repeat_count to be a multiple of size_count.",
        ],
    });
    if config.json {
        println!("{}", serde_json::to_string(&summary).unwrap());
    } else {
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    }
    Ok(())
}

fn median_optional_u64(runs: &[serde_json::Value], field: &str) -> Option<u64> {
    let mut values = runs
        .iter()
        .filter_map(|run| run.get(field).and_then(serde_json::Value::as_u64))
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

fn median_nested_u64(runs: &[serde_json::Value], object: &str, field: &str) -> Option<u64> {
    let mut values = runs
        .iter()
        .filter_map(|run| run.get(object)?.get(field)?.as_u64())
        .collect::<Vec<_>>();
    if values.is_empty() {
        return None;
    }
    values.sort_unstable();
    Some(values[values.len() / 2])
}

fn linear_fit(samples: &[(f64, f64)]) -> Option<(f64, f64, f64)> {
    if samples.len() < 3 {
        return None;
    }
    let count = samples.len() as f64;
    let mean_x = samples.iter().map(|(x, _)| x).sum::<f64>() / count;
    let mean_y = samples.iter().map(|(_, y)| y).sum::<f64>() / count;
    let variance = samples
        .iter()
        .map(|(x, _)| (x - mean_x).powi(2))
        .sum::<f64>();
    if variance == 0.0 {
        return None;
    }
    let covariance = samples
        .iter()
        .map(|(x, y)| (x - mean_x) * (y - mean_y))
        .sum::<f64>();
    let slope = covariance / variance;
    let intercept = mean_y - slope * mean_x;
    let residual_sum_squares = samples
        .iter()
        .map(|(x, y)| (y - (slope * x + intercept)).powi(2))
        .sum::<f64>();
    let total_sum_squares = samples
        .iter()
        .map(|(_, y)| (y - mean_y).powi(2))
        .sum::<f64>();
    let r_squared = if total_sum_squares == 0.0 {
        1.0
    } else {
        1.0 - residual_sum_squares / total_sum_squares
    };
    Some((slope, intercept, r_squared))
}

fn fit_sweep_points(points: &[serde_json::Value], field: &str) -> Option<serde_json::Value> {
    let samples = points
        .iter()
        .filter_map(|point| {
            let size = point.get("size_mib")?.as_u64()?;
            let value = point.get(field)?.as_u64()?;
            Some((size as f64, value as f64))
        })
        .collect::<Vec<_>>();
    let (slope, intercept, r_squared) = linear_fit(&samples)?;
    Some(serde_json::json!({
        "slope_kib_per_mib": slope,
        "intercept_kib": intercept,
        "r_squared": r_squared,
        "point_count": samples.len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_scenario_and_positive_values() {
        assert!(usage().contains("--asset-count N"));
        assert!(usage().contains("--preinstall-mode patch|copyover"));
        assert!(parse_args(&[]).is_err());
        assert!(parse_positive("0", "--repeat").is_err());
        assert!(parse_positive("abc", "--repeat").is_err());
        let default_assembly = parse_args(&["--scenario".into(), "assembly".into()]).unwrap();
        assert_eq!(default_assembly.assembly_verify_mode, VerifyMode::Full);
        assert!(!default_assembly.assembly_demand_mmap);
        let deferred_demand = parse_args(&[
            "--scenario".into(),
            "assembly".into(),
            "--assembly-verify-mode".into(),
            "deferred".into(),
            "--assembly-demand-mmap".into(),
            "--chunks".into(),
            "4".into(),
        ])
        .unwrap();
        assert_eq!(deferred_demand.assembly_verify_mode, VerifyMode::Deferred);
        assert!(deferred_demand.assembly_demand_mmap);
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--assembly-demand-mmap".into(),
            ])
            .is_err()
        );
        let full_single_chunk = parse_args(&[
            "--scenario".into(),
            "assembly".into(),
            "--chunks".into(),
            "1".into(),
        ])
        .unwrap();
        assert_eq!(full_single_chunk.assembly_verify_mode, VerifyMode::Full);
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--assembly-verify-mode".into(),
                "none".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--assembly-verify-mode".into(),
                "deferred".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--assembly-verify-mode".into(),
                "deferred".into(),
                "--chunks".into(),
                "3".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--source-backend".into(),
                "file".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "sophon-install".into(),
                "--size-mib".into(),
                "1".into(),
                "--chunks".into(),
                "3".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--size-mib".into(),
                usize::MAX.to_string(),
            ])
            .is_err()
        );
        let uncovered_sweep = parse_args(&[
            "--scenario".into(),
            "preinstall-apply".into(),
            "--cover-pattern".into(),
            "uncovered-prefix".into(),
            "--fixture-backing".into(),
            "disk".into(),
            "--sweep-sizes-mib".into(),
            "16,32,64".into(),
        ])
        .unwrap();
        assert_eq!(uncovered_sweep.cover_pattern, CoverPattern::UncoveredPrefix);
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--cover-pattern".into(),
                "hdiff20-steps".into(),
                "--fixture-backing".into(),
                "disk".into(),
                "--sweep-sizes-mib".into(),
                "4,9".into(),
            ])
            .is_err()
        );
        let hdiff20_steps = parse_args(&[
            "--scenario".into(),
            "hdiff".into(),
            "--cover-pattern".into(),
            "hdiff20-steps".into(),
            "--size-mib".into(),
            "7".into(),
        ])
        .unwrap();
        assert_eq!(hdiff20_steps.cover_pattern, CoverPattern::Hdiff20Steps);
        let sequence = parse_args(&[
            "--scenario".into(),
            "preinstall-apply".into(),
            "--size-mib".into(),
            "4".into(),
            "--cover-pattern".into(),
            "hdiff20-steps".into(),
            "--asset-count".into(),
            "3".into(),
        ])
        .unwrap();
        assert_eq!(sequence.asset_count, 3);
        let copyover = parse_args(&[
            "--scenario".into(),
            "preinstall-apply".into(),
            "--preinstall-mode".into(),
            "copyover".into(),
        ])
        .unwrap();
        assert_eq!(copyover.preinstall_mode, PreinstallMode::CopyOver);
        let copyover_sequence = parse_args(&[
            "--scenario".into(),
            "preinstall-apply".into(),
            "--preinstall-mode".into(),
            "copyover".into(),
            "--asset-count".into(),
            "3".into(),
        ])
        .unwrap();
        assert_eq!(copyover_sequence.asset_count, 3);
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--preinstall-mode".into(),
                "copyover".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--preinstall-mode".into(),
                "copyover".into(),
                "--cover-pattern".into(),
                "reverse".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--asset-count".into(),
                "9".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--asset-count".into(),
                "2".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--sweep-sizes-mib".into(),
                "16,32,128".into(),
                "--fixture-backing".into(),
                "disk".into(),
            ])
            .is_ok()
        );
        assert_eq!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--cover-pattern".into(),
                "uncovered-prefix".into(),
            ])
            .unwrap()
            .cover_pattern,
            CoverPattern::UncoveredPrefix
        );
        assert_eq!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--cover-pattern".into(),
                "uncovered-prefix".into(),
            ])
            .unwrap()
            .cover_pattern,
            CoverPattern::UncoveredPrefix
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--cover-pattern".into(),
                "uncovered-prefix".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--size-mib".into(),
                "1".into(),
                "--cover-pattern".into(),
                "uncovered-prefix".into(),
            ])
            .is_ok()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--size-mib".into(),
                "0".into(),
                "--cover-pattern".into(),
                "uncovered-prefix".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--sweep-sizes-mib".into(),
                "16,0,128".into(),
                "--fixture-backing".into(),
                "disk".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--sweep-sizes-mib".into(),
                "16,32".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--window-kib".into(),
                "256".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--cover-pattern".into(),
                "reverse".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--cover-pattern".into(),
                "reverse".into(),
            ])
            .is_ok()
        );
        let tempfile_default =
            parse_args(&["--scenario".into(), "preinstall-apply".into()]).unwrap();
        assert_eq!(tempfile_default.fixture_backing, FixtureBacking::Tempfile);
        let disk = parse_args(&[
            "--scenario".into(),
            "preinstall-apply".into(),
            "--fixture-backing".into(),
            "disk".into(),
        ])
        .unwrap();
        assert_eq!(disk.fixture_backing, FixtureBacking::Disk);
        let disk_assembly = parse_args(&[
            "--scenario".into(),
            "assembly".into(),
            "--fixture-backing".into(),
            "disk".into(),
        ])
        .unwrap();
        assert_eq!(disk_assembly.fixture_backing, FixtureBacking::Disk);
        let disk_sophon_install = parse_args(&[
            "--scenario".into(),
            "sophon-install".into(),
            "--fixture-backing".into(),
            "disk".into(),
        ])
        .unwrap();
        assert_eq!(disk_sophon_install.fixture_backing, FixtureBacking::Disk);
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-apply".into(),
                "--fixture-backing".into(),
                "invalid".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "hdiff".into(),
                "--fixture-backing".into(),
                "disk".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn phase_diagnostic_flag_requires_supported_single_patch_run() {
        let base = [
            "--scenario".to_owned(),
            "preinstall-apply".to_owned(),
            "--diagnose-hdiff-phases".to_owned(),
        ];
        if cfg!(feature = "bench-diagnostics") {
            let config = parse_args(&base).unwrap();
            assert!(config.diagnose_hdiff_phases);
            assert_eq!(config.asset_count, 1);
            assert_eq!(config.preinstall_mode, PreinstallMode::Patch);

            let mut copyover = base.to_vec();
            copyover.extend(["--preinstall-mode".to_owned(), "copyover".to_owned()]);
            assert!(parse_args(&copyover).is_err());

            let mut multiple_assets = base.to_vec();
            multiple_assets.extend(["--asset-count".to_owned(), "2".to_owned()]);
            assert!(parse_args(&multiple_assets).is_err());

            let mut sweep = base.to_vec();
            sweep.extend([
                "--fixture-backing".to_owned(),
                "disk".to_owned(),
                "--sweep-sizes-mib".to_owned(),
                "4,8".to_owned(),
            ]);
            assert!(parse_args(&sweep).is_err());
        } else {
            let error = parse_args(&base).unwrap_err();
            assert!(error.contains("bench-diagnostics"));
        }
    }

    #[test]
    fn download_plan_rejects_cgroup_memory_limit_configuration() {
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-download-plan".into(),
                "--memory-max-mib".into(),
                "64".into(),
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--scenario".into(),
                "preinstall-download-plan".into(),
                "--sweep-sizes-mib".into(),
                "4,8".into(),
            ])
            .is_err()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn filesystem_classifier_identifies_memory_and_disk_filesystems() {
        assert_eq!(filesystem_name_for_magic(TMPFS_MAGIC), "tmpfs");
        assert_eq!(filesystem_name_for_magic(RAMFS_MAGIC), "ramfs");
        assert_eq!(filesystem_name_for_magic(0xef53), "ext");
        assert_eq!(filesystem_name_for_magic(0xdeadbeef), "unknown");
        assert!(is_memory_backed_filesystem(TMPFS_MAGIC));
        assert!(is_memory_backed_filesystem(RAMFS_MAGIC));
        assert!(!is_memory_backed_filesystem(0xef53));
        assert!(!is_memory_backed_filesystem(0x9123683e));
    }

    #[test]
    fn parses_size_list_sorts_and_deduplicates() {
        assert_eq!(parse_size_list("128,16,32,16").unwrap(), [16, 32, 128]);
        assert!(parse_size_list("").is_err());
        assert!(parse_size_list("16,nope").is_err());
    }

    #[test]
    fn size_run_order_matches_grouped_and_interleaved_labels() {
        let sizes = [16, 32, 64];
        assert_eq!(
            size_run_order(&sizes, 2, false),
            [(16, 0), (16, 1), (32, 0), (32, 1), (64, 0), (64, 1)]
        );
        assert_eq!(
            size_run_order(&sizes, 2, true),
            [(16, 0), (32, 0), (64, 0), (32, 1), (64, 1), (16, 1)]
        );
    }

    #[test]
    fn child_protocol_preserves_assembly_diagnostic_settings() {
        let config = parse_args(&[
            "--scenario".into(),
            "assembly".into(),
            "--assembly-verify-mode".into(),
            "deferred".into(),
            "--assembly-demand-mmap".into(),
            "--chunks".into(),
            "4".into(),
        ])
        .unwrap();
        let args = child_command_args(&config, Path::new("fixture"), None, 1, "", false);
        assert_eq!(args.len(), 17);
        assert_eq!(args[14], "deferred");
        assert_eq!(args[15], "demand");
        assert_eq!(args[16], "fresh");
        let mut invalid_mode = args[1..].to_vec();
        invalid_mode[13] = "none".to_owned();
        assert!(
            run_child(&invalid_mode)
                .unwrap_err()
                .contains("cannot be none")
        );
        let mut demand_without_deferred = args[1..].to_vec();
        demand_without_deferred[13] = "full".to_owned();
        assert!(
            run_child(&demand_without_deferred)
                .unwrap_err()
                .contains("requires deferred")
        );
    }

    #[test]
    fn sweep_requires_disk_backing_and_system_allocator() {
        let base = [
            "--scenario",
            "preinstall-apply",
            "--sweep-sizes-mib",
            "16,32",
            "--fixture-backing",
            "disk",
        ]
        .map(str::to_owned);
        if cfg!(all(
            unix,
            any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
        )) {
            assert!(
                parse_args(&base)
                    .unwrap_err()
                    .contains("jemalloc instrumentation")
            );
        } else {
            assert!(parse_args(&base).is_ok());
        }
        let without_disk = [
            "--scenario",
            "preinstall-apply",
            "--sweep-sizes-mib",
            "16,32",
        ]
        .map(str::to_owned);
        assert!(parse_args(&without_disk).is_err());
    }

    #[test]
    fn linear_fit_recovers_slope_and_intercept() {
        let (slope, intercept, r_squared) =
            linear_fit(&[(16.0, 42.0), (32.0, 74.0), (64.0, 138.0)])
                .expect("three distinct samples fit a line");
        assert!((slope - 2.0).abs() < f64::EPSILON);
        assert!((intercept - 10.0).abs() < f64::EPSILON);
        assert!((r_squared - 1.0).abs() < f64::EPSILON);
        assert!(linear_fit(&[(16.0, 1.0)]).is_none());
        assert!(linear_fit(&[(16.0, 1.0), (16.0, 2.0)]).is_none());
        assert!(linear_fit(&[(16.0, 1.0), (32.0, 2.0)]).is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup2_mountinfo_resolves_namespace_root_and_escaped_mountpoint() {
        let mountinfo = "33 22 0:29 /user.slice/user-1000.slice /sys/fs/cgroup/user.slice rw,nosuid - cgroup2 cgroup rw";
        let membership =
            Path::new("/user.slice/user-1000.slice/user@1000.service/app.slice/test.scope");
        let (root, point) = find_cgroup2_mount(mountinfo, membership).unwrap();
        assert_eq!(root, PathBuf::from("/user.slice/user-1000.slice"));
        assert_eq!(point, PathBuf::from("/sys/fs/cgroup/user.slice"));

        let escaped = "33 22 0:29 / /sys/fs/cgroup/my\\040cg rw,nosuid - cgroup2 cgroup rw";
        let (root, point) = find_cgroup2_mount(escaped, Path::new("/child.scope")).unwrap();
        assert_eq!(root, PathBuf::from("/"));
        assert_eq!(point, PathBuf::from("/sys/fs/cgroup/my cg"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup2_mountinfo_prefers_most_specific_covering_mount() {
        let mountinfo = concat!(
            "31 22 0:29 / /sys/fs/cgroup rw - cgroup2 cgroup rw\n",
            "33 22 0:29 /user.slice /sys/fs/cgroup/user.slice rw - cgroup2 cgroup rw\n",
        );
        let membership = Path::new("/user.slice/user-1000.slice/test.scope");
        let (root, point) = find_cgroup2_mount(mountinfo, membership).unwrap();
        assert_eq!(root, PathBuf::from("/user.slice"));
        assert_eq!(point, PathBuf::from("/sys/fs/cgroup/user.slice"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup2_mountinfo_rejects_invalid_escapes() {
        let mountinfo = "33 22 0:29 / /sys/fs/cgroup/my\\999 cg rw,nosuid - cgroup2 cgroup rw";
        assert!(find_cgroup2_mount(mountinfo, Path::new("/child.scope")).is_err());
    }

    #[test]
    fn cgroup_memory_event_deltas_track_limit_pressure() {
        let before = CgroupMemoryEvents {
            low: 1,
            high: 2,
            max: 3,
            oom: 4,
            oom_kill: 5,
            oom_group_kill: 6,
        };
        let after = CgroupMemoryEvents {
            low: 1,
            high: 4,
            max: 7,
            oom: 4,
            oom_kill: 6,
            oom_group_kill: 6,
        };
        let delta = CgroupMemoryEvents::delta(&before, &after);
        assert_eq!(delta.high, 2);
        assert_eq!(delta.max, 4);
        assert_eq!(delta.oom, 0);
        assert_eq!(delta.oom_kill, 1);
    }

    #[test]
    fn parses_cgroup_memory_events_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("memory.events"),
            "low 0\nhigh 4\nmax 7\noom 2\noom_kill 1\n",
        )
        .unwrap();
        let events = read_cgroup_memory_events(dir.path()).unwrap();
        assert_eq!(events.high, 4);
        assert_eq!(events.max, 7);
        assert_eq!(events.oom, 2);
        assert_eq!(events.oom_kill, 1);
        assert_eq!(events.oom_group_kill, 0);
    }

    #[test]
    fn parses_cgroup_memory_pressure_totals() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("memory.pressure"),
            "some avg10=0.25 avg60=0.10 avg300=0.01 total=12345\nfull avg10=0.05 avg60=0.01 avg300=0.00 total=6789\n",
        )
        .unwrap();
        let pressure = read_cgroup_memory_pressure(dir.path()).unwrap();
        assert_eq!(pressure.some_total_us, 12345);
        assert_eq!(pressure.full_total_us, 6789);
        let after = CgroupPressure {
            some_total_us: 13000,
            full_total_us: 7000,
        };
        let delta = CgroupPressure::delta(&pressure, &after);
        assert_eq!(delta.some_total_us, 655);
        assert_eq!(delta.full_total_us, 211);
        fs::write(dir.path().join("memory.pressure"), "some avg10=0 total=5\n").unwrap();
        assert!(read_cgroup_memory_pressure(dir.path()).is_err());
    }

    #[test]
    fn sweep_fit_uses_point_medians_and_skips_missing_values() {
        let points = [
            serde_json::json!({"size_mib": 16, "median_peak_delta_kib": 42}),
            serde_json::json!({"size_mib": 32, "median_peak_delta_kib": 74}),
            serde_json::json!({"size_mib": 64, "median_peak_delta_kib": 138}),
            serde_json::json!({"size_mib": 128, "median_peak_delta_kib": null}),
        ];
        let fit = fit_sweep_points(&points, "median_peak_delta_kib").unwrap();
        assert_eq!(fit["point_count"], 3);
        assert!((fit["slope_kib_per_mib"].as_f64().unwrap() - 2.0).abs() < f64::EPSILON);
        assert!((fit["intercept_kib"].as_f64().unwrap() - 10.0).abs() < f64::EPSILON);
    }

    #[test]
    fn median_ignores_missing_samples_and_uses_upper_middle() {
        let runs = vec![
            serde_json::json!({"peak": 9}),
            serde_json::json!({"other": 3}),
            serde_json::json!({"peak": 2}),
            serde_json::json!({"peak": 5}),
        ];
        assert_eq!(median_optional_u64(&runs, "peak"), Some(5));
        assert_eq!(median_optional_u64(&runs, "missing"), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn disk_workdir_rejects_memory_backed_target_when_present() {
        match create_benchmark_workdir(FixtureBacking::Disk) {
            Ok((workdir, Some(filesystem))) => {
                assert!(!matches!(filesystem.magic, TMPFS_MAGIC | RAMFS_MAGIC));
                assert!(workdir.path().exists());
            }
            Ok((_, None)) => panic!("disk workdir must report filesystem information"),
            Err(error) if error.contains("on tmpfs") || error.contains("on ramfs") => {
                eprintln!("skipping disk workdir assertion: {error}");
            }
            Err(error) => panic!("disk workdir setup failed unexpectedly: {error}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn tempfile_workdir_reports_actual_filesystem() {
        let (workdir, filesystem) = create_benchmark_workdir(FixtureBacking::Tempfile).unwrap();
        let filesystem = filesystem.expect("tempfile backing reports its filesystem");
        assert!(workdir.path().exists());
        assert_eq!(
            filesystem_info(workdir.path()).unwrap().magic,
            filesystem.magic
        );
    }

    #[test]
    fn parses_memory_status_fields() {
        let status = "VmRSS:\t1024 kB\nVmHWM:\t2048 kB\nRssAnon:\t512 kB\nRssFile:\t256 kB\nRssShmem:\t128 kB\n";
        let snapshot = parse_memory_status(status);
        assert_eq!(snapshot.vm_rss_kib, 1024);
        assert_eq!(snapshot.vm_hwm_kib, 2048);
        assert_eq!(snapshot.rss_anon_kib, Some(512));
        assert_eq!(snapshot.rss_file_kib, Some(256));
        assert_eq!(snapshot.rss_shmem_kib, Some(128));
    }

    #[test]
    fn parses_smaps_rollup_private_and_shared_fields() {
        let mut snapshot = MemorySnapshot::default();
        parse_smaps_rollup(
            "1000-2000 ---p 00000000 00:00 0 [rollup]\nRss: 2048 kB\nPss: 1536 kB\nShared_Clean: 700 kB\nShared_Dirty: 50 kB\nPrivate_Clean: 100 kB\nPrivate_Dirty: 900 kB\nAnonymous: 920 kB\n",
            &mut snapshot,
        );
        assert_eq!(snapshot.pss_kib, Some(1536));
        assert_eq!(snapshot.shared_clean_kib, Some(700));
        assert_eq!(snapshot.shared_dirty_kib, Some(50));
        assert_eq!(snapshot.private_clean_kib, Some(100));
        assert_eq!(snapshot.private_dirty_kib, Some(900));
        assert_eq!(snapshot.smaps_anonymous_kib, Some(920));
    }

    #[test]
    fn smaps_rollup_parser_ignores_missing_and_malformed_values() {
        let mut snapshot = MemorySnapshot::default();
        parse_smaps_rollup("Pss: unavailable kB\nPrivate_Dirty: 12 kB\n", &mut snapshot);
        assert_eq!(snapshot.pss_kib, None);
        assert_eq!(snapshot.private_dirty_kib, Some(12));
        assert_eq!(snapshot.shared_clean_kib, None);
    }

    #[test]
    fn memory_peak_tracks_components_independently() {
        let mut peak = MemorySnapshot::default();
        update_memory_peak(
            &mut peak,
            MemorySnapshot {
                vm_rss_kib: 900,
                vm_hwm_kib: 1000,
                rss_anon_kib: Some(700),
                rss_file_kib: Some(300),
                rss_shmem_kib: Some(100),
                pss_kib: Some(800),
                private_clean_kib: Some(50),
                private_dirty_kib: Some(650),
                shared_clean_kib: Some(250),
                shared_dirty_kib: Some(50),
                smaps_anonymous_kib: Some(675),
            },
        );
        update_memory_peak(
            &mut peak,
            MemorySnapshot {
                vm_rss_kib: 800,
                vm_hwm_kib: 1100,
                rss_anon_kib: Some(400),
                rss_file_kib: Some(600),
                rss_shmem_kib: Some(800),
                pss_kib: Some(700),
                private_clean_kib: Some(100),
                private_dirty_kib: Some(300),
                shared_clean_kib: Some(500),
                shared_dirty_kib: Some(100),
                smaps_anonymous_kib: Some(400),
            },
        );
        assert_eq!(peak.vm_rss_kib, 900);
        assert_eq!(peak.vm_hwm_kib, 1000);
        assert_eq!(peak.rss_anon_kib, Some(700));
        assert_eq!(peak.rss_file_kib, Some(300));
        assert_eq!(peak.rss_shmem_kib, Some(100));
        assert_eq!(peak.pss_kib, Some(800));
        assert_eq!(peak.private_clean_kib, Some(50));
        assert_eq!(peak.private_dirty_kib, Some(650));
        assert_eq!(peak.shared_clean_kib, Some(250));
        assert_eq!(peak.shared_dirty_kib, Some(50));
        assert_eq!(peak.smaps_anonymous_kib, Some(675));
    }

    #[cfg(all(
        unix,
        any(feature = "benchmark-jemalloc", feature = "sophon-profiling")
    ))]
    #[test]
    fn jemalloc_peak_keeps_coherent_snapshot_with_highest_allocation() {
        let mut peak = JemallocSnapshot::default();
        update_jemalloc_peak(
            &mut peak,
            JemallocSnapshot {
                allocated_bytes: 100,
                active_bytes: 120,
                resident_bytes: 140,
                mapped_bytes: 160,
                retained_bytes: 180,
            },
        );
        update_jemalloc_peak(
            &mut peak,
            JemallocSnapshot {
                allocated_bytes: 90,
                active_bytes: 300,
                resident_bytes: 400,
                mapped_bytes: 500,
                retained_bytes: 600,
            },
        );

        assert_eq!(peak.allocated_bytes, 100);
        assert_eq!(peak.active_bytes, 120);
        assert_eq!(peak.resident_bytes, 140);
        assert_eq!(peak.mapped_bytes, 160);
        assert_eq!(peak.retained_bytes, 180);
    }

    #[test]
    fn hwm_reset_fallback_uses_process_baseline() {
        let baseline = MemorySnapshot {
            vm_rss_kib: 500,
            vm_hwm_kib: 900,
            ..MemorySnapshot::default()
        };
        let peak = MemorySnapshot {
            vm_rss_kib: 700,
            vm_hwm_kib: 900,
            ..MemorySnapshot::default()
        };
        let hwm_reset = false;
        let peak_rss_kib = peak.vm_hwm_kib.max(peak.vm_rss_kib);
        let baseline_rss_kib = if hwm_reset {
            peak.vm_rss_kib
        } else {
            baseline.vm_rss_kib
        };
        assert_eq!(peak_rss_kib, 900);
        assert_eq!(peak_rss_kib.saturating_sub(baseline_rss_kib), 400);
        assert!(!hwm_reset);
    }

    #[test]
    fn generated_hdiff_fixture_applies_to_expected_output() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::Sequential).unwrap();
        let context = prepare_child_context(
            Scenario::Hdiff,
            dir.path(),
            1,
            1,
            1,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn hdiff20_step_fixture_runs_through_direct_apply_verification() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 4, CoverPattern::Hdiff20Steps).unwrap();
        let context = prepare_child_context(
            Scenario::Hdiff,
            dir.path(),
            4,
            1,
            1,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn hdiff20_step_fixture_runs_through_preinstall_apply_verification() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 4, CoverPattern::Hdiff20Steps).unwrap();
        let context = prepare_child_context(
            Scenario::PreinstallApply,
            dir.path(),
            4,
            1,
            1,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[cfg(feature = "bench-alloc-stats")]
    #[test]
    fn sequential_preinstall_assets_report_retained_heap_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 4, CoverPattern::Hdiff20Steps).unwrap();
        let context = prepare_child_context(
            Scenario::PreinstallApply,
            dir.path(),
            4,
            1,
            2,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let window = allocation_stats::AllocationWindow::start().unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        let snapshot = window.finish().unwrap();
        assert!(snapshot.peak_live_bytes > 0);
        if let Verification::FileCompare {
            actuals,
            asset_heap_live_growth_bytes,
            ..
        } = &verification
        {
            assert_eq!(actuals.len(), 2);
            assert_eq!(asset_heap_live_growth_bytes.len(), 2);
        } else {
            panic!("expected preinstall file verification");
        }
        verify_output(verification).unwrap();
    }

    #[cfg(feature = "bench-alloc-stats")]
    #[test]
    fn copyover_mode_runs_through_preinstall_apply() {
        let dir = tempfile::tempdir().unwrap();
        prepare_copyover_fixture(dir.path(), 4).unwrap();
        let old = fs::read(dir.path().join("old.bin")).unwrap();
        let new = fs::read(dir.path().join("new.bin")).unwrap();
        let patch = fs::read(dir.path().join("patch.hdiff")).unwrap();
        assert_ne!(old, new);
        assert_eq!(patch, new);
        assert!(!patch.starts_with(b"HDIFF"));
        let context = prepare_child_context(
            Scenario::PreinstallApply,
            dir.path(),
            4,
            1,
            1,
            PreinstallMode::CopyOver,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn uncovered_prefix_fixture_applies_with_each_direct_source_backend() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::UncoveredPrefix).unwrap();
        for backend in [HDiffSourceBackend::Mmap, HDiffSourceBackend::File] {
            let context = prepare_child_context(
                Scenario::Hdiff,
                dir.path(),
                1,
                1,
                1,
                PreinstallMode::Patch,
                "",
                false,
            )
            .unwrap();
            let verification = run_operation(context, backend).unwrap();
            verify_output(verification).unwrap();
        }
    }

    #[test]
    fn uncovered_prefix_fixture_runs_through_preinstall_apply() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::UncoveredPrefix).unwrap();
        let context = prepare_child_context(
            Scenario::PreinstallApply,
            dir.path(),
            1,
            1,
            1,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn synthetic_preinstall_fixture_runs_public_apply_path() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::Sequential).unwrap();
        let context = prepare_child_context(
            Scenario::PreinstallApply,
            dir.path(),
            1,
            1,
            1,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    fn hdiff_phase_diagnostics_match_synthetic_output() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::Sequential).unwrap();
        let game_dir = dir.path().join("timed-game");
        irmin_rs::game_installer::prepare_benchmark_preinstall_state(
            &game_dir,
            &dir.path().join("old.bin"),
            &dir.path().join("new.bin"),
            &dir.path().join("patch.hdiff"),
            "benchmark/asset.bin",
        )
        .unwrap();

        let started = Instant::now();
        let timings = irmin_rs::game_installer::apply_synthetic_hdiff_for_benchmark(
            &game_dir,
            "synthetic-target",
            "benchmark/asset.bin",
        )
        .unwrap();
        let elapsed = started.elapsed();
        assert!(!timings.original_metadata_and_hash_validation.is_zero());
        assert!(!timings.patch_application.is_zero());
        assert!(!timings.output_verification.is_zero());
        assert!(
            timings.original_metadata_and_hash_validation
                + timings.patch_application
                + timings.output_verification
                <= elapsed
        );
        compare_files_streaming(
            &dir.path().join("new.bin"),
            &game_dir.join("benchmark/asset.bin"),
            1024 * 1024,
        )
        .unwrap();
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    fn hdiff_phase_diagnostics_preserve_target_on_patch_failure() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::Sequential).unwrap();
        let game_dir = dir.path().join("timed-failure-game");
        irmin_rs::game_installer::prepare_benchmark_preinstall_state(
            &game_dir,
            &dir.path().join("old.bin"),
            &dir.path().join("new.bin"),
            &dir.path().join("patch.hdiff"),
            "benchmark/asset.bin",
        )
        .unwrap();
        let target_path = game_dir.join("benchmark/asset.bin");
        let original = fs::read(&target_path).unwrap();
        fs::write(
            game_dir.join("patching/chunk/synthetic.hdiff"),
            b"invalid hdiff data",
        )
        .unwrap();

        assert!(
            irmin_rs::game_installer::apply_synthetic_hdiff_for_benchmark(
                &game_dir,
                "synthetic-target",
                "benchmark/asset.bin",
            )
            .is_err()
        );
        assert_eq!(fs::read(target_path).unwrap(), original);
    }

    #[test]
    fn synthetic_preinstall_rejects_changed_installed_tag_before_apply() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::Sequential).unwrap();
        let game_dir = dir.path().join("synthetic-game");
        irmin_rs::game_installer::prepare_benchmark_preinstall_state(
            &game_dir,
            &dir.path().join("old.bin"),
            &dir.path().join("new.bin"),
            &dir.path().join("patch.hdiff"),
            "benchmark/asset.bin",
        )
        .unwrap();
        irmin_rs::game_installer::write_installed_tag(&game_dir, "wrong-source-tag").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let result = runtime.block_on(irmin_rs::game_installer::apply_preinstall(
            &reqwest::Client::new(),
            &game_dir,
            "synthetic-target",
            Arc::new(|_| {}),
            &DownloadHandle::new(),
        ));
        assert!(result.is_err());
        assert!(
            compare_files_streaming(
                &dir.path().join("old.bin"),
                &game_dir.join("benchmark/asset.bin"),
                1024 * 1024,
            )
            .is_ok()
        );
    }

    #[test]
    fn synthetic_preinstall_rejects_unsafe_fixture_path_before_writing() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("not-created");
        let source = dir.path().join("source.bin");
        let target = dir.path().join("target.bin");
        let patch = dir.path().join("patch.hdiff");
        fs::write(&source, b"source").unwrap();
        fs::write(&target, b"target").unwrap();
        fs::write(&patch, b"patch").unwrap();
        let result = irmin_rs::game_installer::prepare_benchmark_preinstall_state(
            &game_dir,
            &source,
            &target,
            &patch,
            "../escape.bin",
        );
        assert!(result.is_err());
        assert!(!game_dir.exists());
    }

    #[test]
    fn reordered_hdiff_patterns_apply_with_each_source_backend() {
        for pattern in [CoverPattern::Reverse, CoverPattern::Thrash] {
            let dir = tempfile::tempdir().unwrap();
            prepare_hdiff_fixture(dir.path(), 1, pattern).unwrap();
            for backend in [
                HDiffSourceBackend::Mmap,
                HDiffSourceBackend::File,
                HDiffSourceBackend::Windowed {
                    window_size: 256 * 1024,
                    stats: Arc::new(WindowStats::default()),
                },
            ] {
                apply_hdiff_fixture_with_backend(dir.path(), backend).unwrap();
                compare_hdiff_fixture(dir.path()).unwrap();
            }
        }
    }

    #[test]
    fn generated_assembly_fixture_assembles_and_matches_expected() {
        let dir = tempfile::tempdir().unwrap();
        prepare_assembly_fixture(dir.path(), 1, 2).unwrap();
        let context = prepare_child_context(
            Scenario::Assembly,
            dir.path(),
            1,
            2,
            1,
            PreinstallMode::Patch,
            "",
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn deferred_parallel_hash_matches_with_and_without_prefault() {
        let mut outputs = Vec::new();
        for demand_mmap in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            prepare_assembly_fixture(dir.path(), 1, 4).unwrap();
            let context = prepare_assembly_context(dir.path(), 1, 4).unwrap();
            let ScenarioContext::Assembly { mut context } = context else {
                panic!("assembly fixture returned a different scenario");
            };
            context.verify_mode = VerifyMode::Deferred;
            context.demand_mmap = demand_mmap;
            let context = ScenarioContext::Assembly { context };
            let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
            verify_output(verification).unwrap();
            outputs.push(fs::read(dir.path().join("game/assembled.bin")).unwrap());
        }
        assert_eq!(outputs[0], outputs[1]);
    }

    #[test]
    fn sophon_install_downloads_and_assembles_local_chunks() {
        let dir = tempfile::tempdir().unwrap();
        prepare_install_fixture(dir.path(), 1, 2).unwrap();
        let server = LocalChunkServer::start(dir.path().to_path_buf()).unwrap();
        let context = prepare_child_context(
            Scenario::SophonInstall,
            dir.path(),
            1,
            2,
            1,
            PreinstallMode::Patch,
            &server.base_url,
            false,
        )
        .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn sophon_install_resumes_cached_assembly_chunks() {
        let dir = tempfile::tempdir().unwrap();
        prepare_install_fixture(dir.path(), 1, 4).unwrap();
        let context = prepare_child_context(
            Scenario::SophonInstall,
            dir.path(),
            1,
            4,
            1,
            PreinstallMode::Patch,
            "",
            true,
        )
        .unwrap();
        let cache = irmin_rs::game_installer::cache::load_verification_cache(
            &dir.path().join("install-game"),
        );
        assert_eq!(cache.len(), 3);
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }
}
