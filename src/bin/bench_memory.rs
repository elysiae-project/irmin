use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use fast_md5::Md5;
use irmin_rs::api_scrape::{Compression, DownloadInfo};
use irmin_rs::bench_support::{
    apply_hdiff_fixture_with_backend, compare_hdiff_fixture, deterministic_data,
    make_v13_zstd_reordered_patch, prepare_hdiff_fixture as prepare_shared_hdiff_fixture,
};
use irmin_rs::game_installer::VerifyMode;
use irmin_rs::game_installer::assembly::{assemble_file, chunk_filename};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scenario {
    Hdiff,
    PreinstallApply,
    Assembly,
    SophonInstall,
}

impl Scenario {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hdiff" => Ok(Self::Hdiff),
            "preinstall-apply" => Ok(Self::PreinstallApply),
            "assembly" => Ok(Self::Assembly),
            "sophon-install" => Ok(Self::SophonInstall),
            _ => Err(format!(
                "unknown scenario '{value}'; expected hdiff, preinstall-apply, assembly, or sophon-install"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Hdiff => "hdiff",
            Self::PreinstallApply => "preinstall-apply",
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
    repeat: usize,
    budget_mib: Option<u64>,
    json: bool,
    keep_workdir: bool,
    source_backend: SourceBackend,
    window_size_bytes: usize,
    cover_pattern: CoverPattern,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum CoverPattern {
    #[default]
    Sequential,
    Reverse,
    Thrash,
}

impl CoverPattern {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "sequential" => Ok(Self::Sequential),
            "reverse" => Ok(Self::Reverse),
            "thrash" => Ok(Self::Thrash),
            _ => Err(format!(
                "unknown cover pattern '{value}'; expected sequential, reverse, or thrash"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Sequential => "sequential",
            Self::Reverse => "reverse",
            Self::Thrash => "thrash",
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

#[derive(Clone, Debug, Default, serde::Serialize)]
struct MemorySnapshot {
    vm_rss_kib: u64,
    vm_hwm_kib: u64,
    rss_anon_kib: Option<u64>,
    rss_file_kib: Option<u64>,
    rss_shmem_kib: Option<u64>,
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
                {
                    update_memory_peak(&mut current_peak, snapshot);
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
            {
                update_memory_peak(&mut current_peak, snapshot);
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
}

struct InstallContext {
    installer: SophonInstaller,
    game_dir: PathBuf,
    expected_path: PathBuf,
    size_mib: usize,
    runtime: tokio::runtime::Runtime,
}

struct PreinstallApplyContext {
    game_dir: PathBuf,
    expected_path: PathBuf,
    expected_size: usize,
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

enum Verification {
    Hdiff(PathBuf),
    FileCompare {
        expected: PathBuf,
        actual: PathBuf,
        size: usize,
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

fn run_cli() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().is_some_and(|arg| arg == "--child") {
        return run_child(&args[1..]);
    }

    let config = parse_args(&args)?;
    let workdir = tempfile::Builder::new()
        .prefix("irmin-memory-bench-")
        .tempdir()
        .map_err(|error| error.to_string())?;
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let mut results = Vec::with_capacity(config.repeat);

    for iteration in 0..config.repeat {
        let fixture_dir = workdir.path().join(format!("run-{iteration}"));
        fs::create_dir_all(&fixture_dir).map_err(|error| error.to_string())?;
        prepare_fixture(
            config.scenario,
            &fixture_dir,
            config.size_mib,
            config.chunks,
            config.cover_pattern,
        )?;
        let mut local_server = if config.scenario == Scenario::SophonInstall {
            Some(LocalChunkServer::start(fixture_dir.clone())?)
        } else {
            None
        };
        let server_url = local_server
            .as_ref()
            .map(|server| server.base_url.clone())
            .unwrap_or_default();

        let output = Command::new(&executable)
            .args(["--child", config.scenario.name()])
            .arg(&fixture_dir)
            .arg(config.size_mib.to_string())
            .arg(config.chunks.to_string())
            .arg(server_url)
            .arg(config.cover_pattern.name())
            .arg(match config.source_backend {
                SourceBackend::Mmap => "mmap".to_owned(),
                SourceBackend::File => "file".to_owned(),
                SourceBackend::Windowed => format!("windowed:{}", config.window_size_bytes),
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .output()
            .map_err(|error| error.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "{} child failed: {}",
                config.scenario.name(),
                output.status
            ));
        }
        let stdout = String::from_utf8(output.stdout).map_err(|error| error.to_string())?;
        let result: serde_json::Value = serde_json::from_str(stdout.trim())
            .map_err(|error| format!("invalid child JSON: {error}; output={stdout:?}"))?;
        results.push(result);
        if let Some(mut server) = local_server.take() {
            server.stop();
        }
    }

    report_results(&config, &results)?;
    if config.keep_workdir {
        eprintln!("fixtures retained at {}", workdir.path().display());
        std::mem::forget(workdir);
    }
    Ok(())
}

fn parse_args(args: &[String]) -> Result<Config, String> {
    let mut scenario = None;
    let mut size_mib = 64;
    let mut chunks = 8;
    let mut repeat = 3;
    let mut budget_mib = None;
    let mut json = false;
    let mut keep_workdir = false;
    let mut source_backend = SourceBackend::Mmap;
    let mut window_size_bytes = 256 * 1024;
    let mut window_size_provided = false;
    let mut cover_pattern = CoverPattern::Sequential;
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
            "--help" | "-h" => return Err(usage().to_owned()),
            other => return Err(format!("unknown argument '{other}'\n{}", usage())),
        }
        index += 1;
    }

    let scenario = scenario.ok_or_else(|| usage().to_owned())?;
    if scenario != Scenario::Hdiff && source_backend != SourceBackend::Mmap {
        return Err(
            "--source-backend overrides are only supported for the hdiff scenario".to_owned(),
        );
    }
    if !matches!(scenario, Scenario::Hdiff | Scenario::PreinstallApply)
        && cover_pattern != CoverPattern::Sequential
    {
        return Err("--cover-pattern is only supported for HDiff scenarios".to_owned());
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
    if scenario == Scenario::Assembly && (size_mib * 1024 * 1024) % chunks != 0 {
        return Err("assembly size-mib must divide evenly into chunks".to_owned());
    }
    if scenario == Scenario::SophonInstall && (size_mib * 1024 * 1024) % chunks != 0 {
        return Err("sophon-install size-mib must divide evenly into chunks".to_owned());
    }
    if source_backend == SourceBackend::Windowed && window_size_bytes > 64 * 1024 * 1024 {
        return Err("window size must not exceed 64 MiB".to_owned());
    }
    Ok(Config {
        scenario,
        size_mib,
        chunks,
        repeat,
        budget_mib,
        json,
        keep_workdir,
        source_backend,
        window_size_bytes,
        cover_pattern,
    })
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
    "Usage: bench_memory --scenario hdiff|assembly|sophon-install [--size-mib N] [--chunks N] [--repeat N] [--source-backend mmap|file|windowed] [--window-mib N|--window-kib N] [--cover-pattern sequential|reverse|thrash] [--budget-mib N] [--json] [--keep-workdir]"
}

fn run_child(args: &[String]) -> Result<(), String> {
    if args.len() != 7 {
        return Err("child expects SCENARIO FIXTURE_DIR SIZE_MIB CHUNKS SERVER_URL COVER_PATTERN SOURCE_BACKEND".to_owned());
    }
    let scenario = Scenario::parse(&args[0])?;
    let fixture_dir = PathBuf::from(&args[1]);
    let size_mib = parse_positive(&args[2], "--size-mib")?;
    let chunks = parse_positive(&args[3], "--chunks")?;
    let cover_pattern = CoverPattern::parse(&args[5])?;
    let source_backend = args[6].as_str();
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
    let context = prepare_child_context(scenario, &fixture_dir, size_mib, chunks, &args[4])?;
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
    let verification = run_operation(context, hdiff_source_backend)?;
    let elapsed_ms = started.elapsed().as_millis();
    let operation_end = read_memory_snapshot()?;
    let cpu_after = read_cpu_usage()?;
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
    let peak_rss_kib = operation_end.vm_hwm_kib.max(operation_end.vm_rss_kib);
    verify_output(verification)?;
    let mut result = serde_json::json!({
        "scenario": scenario.name(),
        "fixture_kind": if scenario == Scenario::PreinstallApply { "synthetic-preinstall" } else { "synthetic" },
        "representative": false,
        "asset_count": (scenario == Scenario::PreinstallApply).then_some(1),
        "cover_pattern": cover_pattern.name(),
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
        "baseline_rss_kib": if hwm_reset { operation_baseline.vm_rss_kib } else { baseline.vm_rss_kib },
        "baseline_rss_anon_kib": operation_baseline.rss_anon_kib,
        "baseline_rss_shmem_kib": operation_baseline.rss_shmem_kib,
        "peak_rss_kib": peak_rss_kib,
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
        "sampled_peak_is_coherent_snapshot": true,
        "hwm_reset": hwm_reset,
        "measurement": if hwm_reset { "operation_hwm" } else { "process_hwm_includes_setup" },
    });
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

fn prepare_fixture(
    scenario: Scenario,
    dir: &Path,
    size_mib: usize,
    chunks: usize,
    cover_pattern: CoverPattern,
) -> Result<(), String> {
    match scenario {
        Scenario::Hdiff | Scenario::PreinstallApply => {
            prepare_hdiff_fixture(dir, size_mib, cover_pattern)
        }
        Scenario::Assembly => prepare_assembly_fixture(dir, size_mib, chunks),
        Scenario::SophonInstall => prepare_install_fixture(dir, size_mib, chunks),
    }
}

fn prepare_hdiff_fixture(
    dir: &Path,
    size_mib: usize,
    cover_pattern: CoverPattern,
) -> Result<(), String> {
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
    };
    let (patch, new) = make_v13_zstd_reordered_patch(&old, &permutation);
    fs::write(dir.join("old.bin"), &old).map_err(|error| error.to_string())?;
    fs::write(dir.join("new.bin"), &new).map_err(|error| error.to_string())?;
    fs::write(dir.join("patch.hdiff"), patch).map_err(|error| error.to_string())
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

fn prepare_child_context(
    scenario: Scenario,
    dir: &Path,
    size_mib: usize,
    chunks: usize,
    server_url: &str,
) -> Result<ScenarioContext, String> {
    match scenario {
        Scenario::Hdiff => Ok(ScenarioContext::Hdiff {
            dir: dir.to_path_buf(),
        }),
        Scenario::PreinstallApply => {
            let game_dir = dir.join("synthetic-game");
            let (_, expected_size) = irmin_rs::game_installer::prepare_benchmark_preinstall_state(
                &game_dir,
                &dir.join("old.bin"),
                &dir.join("new.bin"),
                &dir.join("patch.hdiff"),
                "benchmark/asset.bin",
            )
            .map_err(|error| error.to_string())?;
            Ok(ScenarioContext::PreinstallApply(Box::new(
                PreinstallApplyContext {
                    game_dir,
                    expected_path: dir.join("new.bin"),
                    expected_size: usize::try_from(expected_size)
                        .map_err(|_| "benchmark output too large".to_owned())?,
                    runtime: tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())?,
                    client: reqwest::Client::new(),
                },
            )))
        }
        Scenario::Assembly => prepare_assembly_context(dir, size_mib, chunks),
        Scenario::SophonInstall => prepare_install_context(dir, size_mib, chunks, server_url),
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
            SophonManifestAssetChunk {
                chunk_name: format!("chunk-{index:04}"),
                chunk_decompressed_hash_md5: hex::encode(fast_md5::digest(&raw)),
                chunk_on_file_offset: (index * chunk_size) as u64,
                chunk_size: compressed.len() as u64,
                chunk_size_decompressed: chunk_size as u64,
                chunk_compressed_hash_xxh: 0,
                chunk_compressed_hash_md5: hex::encode(fast_md5::digest(&compressed)),
                chunk_old_offset: -1,
            }
        })
        .collect::<Vec<_>>();
    let manifest = CompactManifest::from(vec![SophonManifestAssetProperty {
        asset_name: "assembled.bin".to_owned(),
        asset_chunks: chunk_rows,
        asset_type: 0,
        asset_size: (size_mib * 1024 * 1024) as u64,
        asset_hash_md5: expected_hash,
    }]);
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
        }),
    })
}

fn prepare_install_context(
    dir: &Path,
    size_mib: usize,
    chunks: usize,
    server_url: &str,
) -> Result<ScenarioContext, String> {
    if server_url.is_empty() {
        return Err("sophon-install requires a local fixture server".to_owned());
    }
    let chunk_size = size_mib * 1024 * 1024 / chunks;
    let mut file_hasher = Md5::new();
    let chunk_rows = (0..chunks)
        .map(|index| {
            let raw = deterministic_data(0xBEEF + index as u64, chunk_size);
            file_hasher.update(&raw);
            let compressed_path = dir.join(format!("chunk-{index:04}"));
            let compressed = fs::read(&compressed_path).map_err(|error| error.to_string())?;
            Ok(SophonManifestAssetChunk {
                chunk_name: format!("chunk-{index:04}"),
                chunk_decompressed_hash_md5: hex::encode(fast_md5::digest(&raw)),
                chunk_on_file_offset: (index * chunk_size) as u64,
                chunk_size: compressed.len() as u64,
                chunk_size_decompressed: chunk_size as u64,
                chunk_compressed_hash_xxh: 0,
                chunk_compressed_hash_md5: hex::encode(fast_md5::digest(&compressed)),
                chunk_old_offset: -1,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let expected_path = dir.join("expected-install.bin");
    let mut expected_file = File::create(&expected_path).map_err(|error| error.to_string())?;
    for index in 0..chunks {
        expected_file
            .write_all(&deterministic_data(0xBEEF + index as u64, chunk_size))
            .map_err(|error| error.to_string())?;
    }
    let file_md5 = hex::encode(file_hasher.finalize());
    let manifest = SophonManifestProto {
        assets: vec![SophonManifestAssetProperty {
            asset_name: "installed.bin".to_owned(),
            asset_chunks: chunk_rows,
            asset_type: 0,
            asset_size: (size_mib * 1024 * 1024) as u64,
            asset_hash_md5: file_md5,
        }],
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
    Ok(ScenarioContext::SophonInstall(Box::new(InstallContext {
        installer,
        game_dir: dir.join("install-game"),
        expected_path,
        size_mib,
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
                game_dir,
                expected_path,
                expected_size,
                runtime,
                client,
            } = *context;
            runtime
                .block_on(irmin_rs::game_installer::apply_preinstall(
                    &client,
                    &game_dir,
                    "synthetic-target",
                    Arc::new(|_| {}),
                    &DownloadHandle::new(),
                ))
                .map_err(|error| format!("preinstall apply failed: {error}"))?;
            Ok(Verification::FileCompare {
                expected: expected_path,
                actual: game_dir.join("benchmark/asset.bin"),
                size: expected_size,
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
            } = *context;
            assemble_file(
                &manifest,
                0,
                &game_dir,
                &chunks_dir,
                &tmp_dir,
                &lookup,
                &refcounts,
                &cache,
                true,
                VerifyMode::Full,
            )
            .map_err(|error| format!("assembly failed: {error}"))?;
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
                runtime,
            } = *context;
            runtime
                .block_on(install(
                    vec![installer],
                    &game_dir,
                    Vec::new(),
                    "bench-tag",
                    ResumeContext {
                        prev_manifest_hash: String::new(),
                        prev_downloaded_chunks: Default::default(),
                        resume_seed: CompletedFiles::default(),
                    },
                    InstallOptions {
                        is_preinstall: false,
                        is_resume: false,
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
        Verification::FileCompare {
            expected,
            actual,
            size,
        } => compare_files_streaming(&expected, &actual, size),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_scenario_and_positive_values() {
        assert!(parse_args(&[]).is_err());
        assert!(parse_positive("0", "--repeat").is_err());
        assert!(parse_positive("abc", "--repeat").is_err());
        assert!(
            parse_args(&[
                "--scenario".into(),
                "assembly".into(),
                "--chunks".into(),
                "0".into(),
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
            },
        );
        assert_eq!(peak.vm_rss_kib, 900);
        assert_eq!(peak.vm_hwm_kib, 1000);
        assert_eq!(peak.rss_anon_kib, Some(700));
        assert_eq!(peak.rss_file_kib, Some(300));
        assert_eq!(peak.rss_shmem_kib, Some(100));
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
        let context = prepare_child_context(Scenario::Hdiff, dir.path(), 1, 1, "").unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn synthetic_preinstall_fixture_runs_public_apply_path() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1, CoverPattern::Sequential).unwrap();
        let context =
            prepare_child_context(Scenario::PreinstallApply, dir.path(), 1, 1, "").unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
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
        let context = prepare_child_context(Scenario::Assembly, dir.path(), 1, 2, "").unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn sophon_install_downloads_and_assembles_local_chunks() {
        let dir = tempfile::tempdir().unwrap();
        prepare_install_fixture(dir.path(), 1, 2).unwrap();
        let server = LocalChunkServer::start(dir.path().to_path_buf()).unwrap();
        let context =
            prepare_child_context(Scenario::SophonInstall, dir.path(), 1, 2, &server.base_url)
                .unwrap();
        let verification = run_operation(context, HDiffSourceBackend::Mmap).unwrap();
        verify_output(verification).unwrap();
    }
}
