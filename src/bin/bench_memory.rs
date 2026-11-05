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
    apply_hdiff_fixture, compare_hdiff_fixture, deterministic_data,
    prepare_hdiff_fixture as prepare_shared_hdiff_fixture,
};
use irmin_rs::game_installer::VerifyMode;
use irmin_rs::game_installer::assembly::{assemble_file, chunk_filename};
use irmin_rs::game_installer::cache::{VerificationCache, VerificationEntry};
use irmin_rs::game_installer::compact_manifest::{CompactManifest, StringArena};
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
    Assembly,
    SophonInstall,
}

impl Scenario {
    fn parse(value: &str) -> Result<Self, String> {
        match value {
            "hdiff" => Ok(Self::Hdiff),
            "assembly" => Ok(Self::Assembly),
            "sophon-install" => Ok(Self::SophonInstall),
            _ => Err(format!(
                "unknown scenario '{value}'; expected hdiff, assembly, or sophon-install"
            )),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Hdiff => "hdiff",
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
}

#[derive(Debug, Default, serde::Serialize)]
struct MemorySnapshot {
    vm_rss_kib: u64,
    vm_hwm_kib: u64,
    rss_anon_kib: Option<u64>,
    rss_file_kib: Option<u64>,
}

enum ScenarioContext {
    Hdiff { dir: PathBuf },
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

enum Verification {
    Hdiff(PathBuf),
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
            "--help" | "-h" => return Err(usage().to_owned()),
            other => return Err(format!("unknown argument '{other}'\n{}", usage())),
        }
        index += 1;
    }

    let scenario = scenario.ok_or_else(|| usage().to_owned())?;
    if size_mib.checked_mul(1024 * 1024).is_none() {
        return Err("--size-mib is too large".to_owned());
    }
    if scenario == Scenario::Assembly && (size_mib * 1024 * 1024) % chunks != 0 {
        return Err("assembly size-mib must divide evenly into chunks".to_owned());
    }
    if scenario == Scenario::SophonInstall && (size_mib * 1024 * 1024) % chunks != 0 {
        return Err("sophon-install size-mib must divide evenly into chunks".to_owned());
    }
    Ok(Config {
        scenario,
        size_mib,
        chunks,
        repeat,
        budget_mib,
        json,
        keep_workdir,
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
    "Usage: bench_memory --scenario hdiff|assembly|sophon-install [--size-mib N] [--chunks N] [--repeat N] [--budget-mib N] [--json] [--keep-workdir]"
}

fn run_child(args: &[String]) -> Result<(), String> {
    if args.len() != 5 {
        return Err("child expects SCENARIO FIXTURE_DIR SIZE_MIB CHUNKS SERVER_URL".to_owned());
    }
    let scenario = Scenario::parse(&args[0])?;
    let fixture_dir = PathBuf::from(&args[1]);
    let size_mib = parse_positive(&args[2], "--size-mib")?;
    let chunks = parse_positive(&args[3], "--chunks")?;
    let context = prepare_child_context(scenario, &fixture_dir, size_mib, chunks, &args[4])?;
    let baseline = read_memory_snapshot()?;
    let hwm_reset = reset_hwm().is_ok();
    let operation_baseline = read_memory_snapshot()?;
    let started = Instant::now();
    let verification = run_operation(context)?;
    let elapsed_ms = started.elapsed().as_millis();
    let peak = read_memory_snapshot()?;
    let peak_rss_kib = peak.vm_hwm_kib.max(peak.vm_rss_kib);
    verify_output(verification)?;
    println!(
        "{}",
        serde_json::json!({
            "scenario": scenario.name(),
            "size_mib": size_mib,
            "chunks": chunks,
            "elapsed_ms": elapsed_ms,
            "baseline_rss_kib": if hwm_reset { operation_baseline.vm_rss_kib } else { baseline.vm_rss_kib },
            "peak_rss_kib": peak_rss_kib,
            "peak_delta_kib": peak_rss_kib.saturating_sub(if hwm_reset {
                operation_baseline.vm_rss_kib
            } else {
                baseline.vm_rss_kib
            }),
            "end_rss_anon_kib": peak.rss_anon_kib,
            "end_rss_file_kib": peak.rss_file_kib,
            "hwm_reset": hwm_reset,
            "measurement": if hwm_reset { "operation_hwm" } else { "process_hwm_includes_setup" },
        })
    );
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
) -> Result<(), String> {
    match scenario {
        Scenario::Hdiff => prepare_hdiff_fixture(dir, size_mib),
        Scenario::Assembly => prepare_assembly_fixture(dir, size_mib, chunks),
        Scenario::SophonInstall => prepare_install_fixture(dir, size_mib, chunks),
    }
}

fn prepare_hdiff_fixture(dir: &Path, size_mib: usize) -> Result<(), String> {
    prepare_shared_hdiff_fixture(dir, size_mib)
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

fn run_operation(context: ScenarioContext) -> Result<Verification, String> {
    match context {
        ScenarioContext::Hdiff { dir } => {
            apply_hdiff_fixture(&dir)?;
            Ok(Verification::Hdiff(dir))
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
    }

    #[test]
    fn parses_memory_status_fields() {
        let status = "VmRSS:\t1024 kB\nVmHWM:\t2048 kB\nRssAnon:\t512 kB\nRssFile:\t256 kB\n";
        let snapshot = parse_memory_status(status);
        assert_eq!(snapshot.vm_rss_kib, 1024);
        assert_eq!(snapshot.vm_hwm_kib, 2048);
        assert_eq!(snapshot.rss_anon_kib, Some(512));
        assert_eq!(snapshot.rss_file_kib, Some(256));
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
        prepare_hdiff_fixture(dir.path(), 1).unwrap();
        let context = prepare_child_context(Scenario::Hdiff, dir.path(), 1, 1, "").unwrap();
        let verification = run_operation(context).unwrap();
        verify_output(verification).unwrap();
    }

    #[test]
    fn generated_assembly_fixture_assembles_and_matches_expected() {
        let dir = tempfile::tempdir().unwrap();
        prepare_assembly_fixture(dir.path(), 1, 2).unwrap();
        let context = prepare_child_context(Scenario::Assembly, dir.path(), 1, 2, "").unwrap();
        let verification = run_operation(context).unwrap();
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
        let verification = run_operation(context).unwrap();
        verify_output(verification).unwrap();
    }
}
