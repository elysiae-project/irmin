use super::*;
use crate::api_scrape::Compression;
use std::io::Write;

fn eager_context(
    root: &Path,
    download: Arc<DownloadInfo>,
    client: Arc<Client>,
    allocator: Arc<super::output_allocator::OutputAllocator>,
    chunk: SophonManifestAssetChunk,
    file_size: u64,
    file_hash: &str,
) -> Arc<InstallContext> {
    Arc::new(InstallContext {
        installer_clients: Arc::new(vec![client]),
        installer_downloads: Arc::new(vec![download]),
        chunks_dir: Arc::new(root.to_path_buf()),
        game_dir: root.to_path_buf().into(),
        all_tmp_dirs: Arc::new(vec![]),
        all_files: Arc::new(CompactManifest::from(vec![SophonManifestAssetProperty {
            asset_name: "eager.bin".to_owned(),
            asset_chunks: vec![chunk],
            asset_type: 0,
            asset_size: file_size,
            asset_hash_md5: file_hash.to_owned(),
        }])),
        downloaded_bytes: Arc::new(AtomicU64::new(0)),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: 0,
        total_files: 1,
        resume_bytes_offset: Arc::new(AtomicU64::new(0)),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: Arc::new(AtomicU64::new(now_nanos())),
        smooth_speed_bps: Arc::new(AtomicU64::new(0)),
        eta_speed_history: Arc::new(Mutex::new(VecDeque::new())),
        last_speed_bytes: Arc::new(AtomicU64::new(0)),
        last_speed_time: Arc::new(AtomicU64::new(now_nanos())),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: Arc::new(AtomicU64::new(0)),
        last_save: Arc::new(Mutex::new(None)),
        state_saver: Arc::new(|_| {}),
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(vec![AtomicBool::new(false)]),
        output_allocator: allocator,
        chunk_bitmap: Arc::new(
            super::chunk_bitmap::ChunkBitmap::create(&root.join("bitmap"), 1).unwrap(),
        ),
        verify_mode: VerifyMode::Full,
    })
}

fn eager_item() -> DownloadItem {
    DownloadItem {
        file_idx: 0,
        chunk_idx: 0,
        output_offset: 0,
        installer_idx: 0,
        is_pre_downloaded: false,
        use_eager: true,
    }
}

#[test]
fn eager_stream_threshold_preserves_small_one_shot_path() {
    assert_eq!(super::EAGER_STREAM_THRESHOLD, 64 * 1024 * 1024);
    assert!(super::EAGER_STREAM_THRESHOLD > 8 * 1024 * 1024);
    assert!(super::EAGER_STREAM_THRESHOLD <= 64 * 1024 * 1024);
    assert!(!super::should_stream_eager(
        super::EAGER_STREAM_THRESHOLD - 1,
        None
    ));
    assert!(super::should_stream_eager(
        super::EAGER_STREAM_THRESHOLD,
        None
    ));
    assert!(!super::should_stream_eager(
        super::EAGER_STREAM_THRESHOLD,
        Some(false)
    ));
}

#[cfg(feature = "bench-diagnostics")]
fn rss_high_water_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmHWM:")
                .and_then(|value| value.split_whitespace().next())
                .and_then(|value| value.parse().ok())
        })
        .unwrap()
}

#[cfg(feature = "bench-diagnostics")]
fn process_cpu_time_ms() -> u128 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(result, 0);
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec as u128 + usage.ru_stime.tv_sec as u128) * 1000
        + (usage.ru_utime.tv_usec as u128 + usage.ru_stime.tv_usec as u128) / 1000
}

#[cfg(feature = "bench-diagnostics")]
fn rss_components_kib() -> (Option<u64>, Option<u64>, Option<u64>) {
    let Ok(smaps) = std::fs::read_to_string("/proc/self/smaps_rollup") else {
        return (None, None, None);
    };
    let mut anonymous = None;
    let mut file = None;
    let mut private_dirty = None;
    for line in smaps.lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("RssAnon:") => anonymous = fields.next().and_then(|value| value.parse().ok()),
            Some("RssFile:") => file = fields.next().and_then(|value| value.parse().ok()),
            Some("Private_Dirty:") => {
                private_dirty = fields.next().and_then(|value| value.parse().ok());
            }
            _ => {}
        }
    }
    (anonymous, file, private_dirty)
}

#[cfg(feature = "bench-diagnostics")]
fn rss_status_components_kib() -> (Option<u64>, Option<u64>, Option<u64>, Option<u64>) {
    let Ok(status) = std::fs::read_to_string("/proc/self/status") else {
        return (None, None, None, None);
    };
    let mut rss = None;
    let mut anonymous = None;
    let mut file = None;
    let mut shmem = None;
    for line in status.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value
            .split_whitespace()
            .next()
            .and_then(|value| value.parse().ok());
        match key {
            "VmRSS" => rss = value,
            "RssAnon" => anonymous = value,
            "RssFile" => file = value,
            "RssShmem" => shmem = value,
            _ => {}
        }
    }
    (rss, anonymous, file, shmem)
}

#[cfg(feature = "bench-diagnostics")]
fn reset_rss_high_water() -> bool {
    std::fs::write("/proc/self/clear_refs", b"5").is_ok()
}

#[cfg(feature = "bench-diagnostics")]
fn eager_benchmark_prefault_label(mode: &str, prefault: bool) -> &'static str {
    if mode == "oneshot" {
        if prefault { "true" } else { "false" }
    } else {
        "na"
    }
}

#[cfg(feature = "bench-diagnostics")]
#[test]
#[ignore = "reports per-thread compressed buffer capacity retention"]
fn benchmark_compressed_buffer_retention() {
    use std::sync::mpsc;

    let worker_count = std::env::var("IRMIN_BUFFER_BENCH_WORKERS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4)
        .max(1);
    let sizes =
        std::env::var("IRMIN_BUFFER_BENCH_SIZES_MIB").unwrap_or_else(|_| "2,32,2".to_owned());
    let sizes_mib = sizes
        .split(',')
        .map(|value| value.parse::<usize>().expect("valid MiB size"));
    let sizes_mib = sizes_mib.collect::<Vec<_>>();
    assert!(sizes_mib.iter().all(|size| *size > 0));
    let dir = tempfile::tempdir().unwrap();
    let mut fixtures = Vec::with_capacity(sizes_mib.len());
    for (index, size_mib) in sizes_mib.iter().copied().enumerate() {
        let raw = (0..size_mib * 1024 * 1024)
            .map(|byte| {
                let value = (byte as u64)
                    .wrapping_add(index as u64 * 0x9e37_79b9)
                    .wrapping_mul(0xd6e8_feb8_6659_fd93);
                (value ^ (value >> 29) ^ (value >> 47)) as u8
            })
            .collect::<Vec<_>>();
        let expected_hash = fast_md5::digest(&raw);
        let compressed = zstd::encode_all(std::io::Cursor::new(raw), 1).unwrap();
        let path = dir.path().join(format!("chunk-{index}.zst"));
        std::fs::write(&path, &compressed).unwrap();
        let expected_size = (size_mib * 1024 * 1024) as u64;
        fixtures.push((path, expected_size, compressed.len(), expected_hash));
    }
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let mut workers = Vec::with_capacity(worker_count);
    let mut release_senders = Vec::with_capacity(worker_count);

    for worker_idx in 0..worker_count {
        let fixtures = fixtures.clone();
        let output_path = dir.path().join(format!("output-{worker_idx}.bin"));
        let tx = tx.clone();
        let ready_tx = ready_tx.clone();
        let (release_tx, release_rx) = mpsc::channel();
        release_senders.push(release_tx);
        workers.push(std::thread::spawn(move || {
            let output = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(output_path)
                .unwrap();
            let compressed_sizes = fixtures
                .iter()
                .map(|(_, _, size, _)| *size)
                .collect::<Vec<_>>();
            let mut decompression_time = std::time::Duration::ZERO;
            for (chunk_path, decoded_size, _, expected_hash) in fixtures {
                output.set_len(decoded_size).unwrap();
                let started = std::time::Instant::now();
                let written = super::super::assembly_opt::decompress_chunk_optimized(
                    &chunk_path,
                    &output,
                    0,
                    decoded_size,
                    None,
                    &hex::encode(expected_hash),
                )
                .unwrap();
                decompression_time += started.elapsed();
                assert_eq!(written, decoded_size);
                let mut hasher = fast_md5::Md5::new();
                let mut buffer = vec![0; 256 * 1024];
                let mut offset = 0u64;
                loop {
                    let count =
                        std::os::unix::fs::FileExt::read_at(&output, &mut buffer, offset).unwrap();
                    if count == 0 {
                        break;
                    }
                    hasher.update(&buffer[..count]);
                    offset += count as u64;
                }
                assert_eq!(hasher.finalize(), expected_hash);
            }
            let retained = super::super::assembly_opt::compressed_buffer_capacity();
            tx.send((retained, compressed_sizes, decompression_time.as_micros()))
                .unwrap();
            drop(output);
            ready_tx.send(()).unwrap();
            release_rx
                .recv_timeout(std::time::Duration::from_secs(30))
                .unwrap();
        }));
    }
    drop(tx);
    drop(ready_tx);
    let capacities = (0..worker_count)
        .map(|_| {
            rx.recv_timeout(std::time::Duration::from_secs(30))
                .expect("worker should report its retained buffer capacity")
        })
        .collect::<Vec<_>>();
    assert_eq!(capacities.len(), worker_count);
    drop(dir);
    for _ in 0..worker_count {
        ready_rx
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("all workers should hold their thread-local buffers");
    }
    let smaps = std::fs::read_to_string("/proc/self/smaps_rollup");
    let smaps_available = smaps.is_ok();
    let mut rss_anon_kib: Option<u64> = None;
    let mut pss_kib: Option<u64> = None;
    for line in smaps.as_deref().unwrap_or_default().lines() {
        let mut fields = line.split_whitespace();
        match fields.next() {
            Some("RssAnon:") => rss_anon_kib = fields.next().and_then(|value| value.parse().ok()),
            Some("Pss:") => pss_kib = fields.next().and_then(|value| value.parse().ok()),
            _ => {}
        }
    }
    println!(
        "compressed_buffer_retention fixture=high_entropy_worst_case policy=baseline workers={worker_count} sequence_mib={sizes_mib:?} capacities={capacities:?} smaps_available={smaps_available} rss_anon_kib={rss_anon_kib:?} pss_kib={pss_kib:?} process_hwm_kib={} retained_total_mib={:.1}",
        rss_high_water_kib(),
        capacities
            .iter()
            .map(|(capacity, _, _)| *capacity)
            .sum::<usize>() as f64
            / (1024.0 * 1024.0),
    );
    for release_tx in release_senders {
        release_tx.send(()).unwrap();
    }
    for worker in workers {
        worker.join().unwrap();
    }
}

fn chunk_download(server: &wiremock::MockServer) -> Arc<DownloadInfo> {
    Arc::new(DownloadInfo {
        encryption: 0,
        password: String::new(),
        compression: Compression::None,
        url_prefix: format!("{}/", server.uri()),
        url_suffix: "chunks".to_owned(),
    })
}

#[tokio::test]
async fn eager_streaming_download_verifies_hashes_and_marks_bitmap() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let data = vec![0x5Au8; 1024 * 1024];
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(20).unwrap();
    encoder.write_all(&data).unwrap();
    let compressed = encoder.finish().unwrap();
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&data));
    Mock::given(method("GET"))
        .and(path("chunks/eager"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(compressed.clone())
                .insert_header("content-length", compressed.len().to_string()),
        )
        .mount(&server)
        .await;

    let dir = tempfile::tempdir().unwrap();
    let allocator = Arc::new(super::output_allocator::OutputAllocator::new(dir.path()));
    allocator
        .preallocate_file(0, "eager.bin", data.len() as u64)
        .unwrap();
    let chunk = SophonManifestAssetChunk {
        chunk_name: "eager".into(),
        chunk_decompressed_hash_md5: decompressed_hash,
        chunk_on_file_offset: 0,
        chunk_size: compressed.len() as u64,
        chunk_size_decompressed: data.len() as u64,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: compressed_hash,
        chunk_old_offset: -1,
    };
    let ctx = eager_context(
        dir.path(),
        chunk_download(&server),
        Arc::new(Client::new()),
        allocator,
        chunk,
        data.len() as u64,
        &hex::encode(fast_md5::digest(&data)),
    );

    process_eager_item(
        &eager_item(),
        0,
        ctx.all_files.file_chunk(0, 0),
        &ctx,
        &[(0, 0, 0)],
        &[0, 1],
        &[AtomicU32::new(1)],
        &DownloadHandle::new(),
    )
    .await
    .unwrap();
    assert!(ctx.chunk_bitmap.is_complete(0));
    assert_eq!(fs::read(dir.path().join("eager.bin")).unwrap(), data);
}

#[cfg(feature = "bench-diagnostics")]
#[test]
#[ignore = "measures actual loopback eager installer concurrency RSS"]
fn benchmark_eager_loopback_concurrent_rss() {
    let runtime_mode =
        std::env::var("IRMIN_EAGER_BENCH_RUNTIME").unwrap_or_else(|_| "multi4".to_owned());
    match runtime_mode.as_str() {
        "current" => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(benchmark_eager_loopback_inner()),
        "multi1" | "multi2" | "multi4" => {
            let workers = match runtime_mode.as_str() {
                "multi1" => 1,
                "multi2" => 2,
                _ => 4,
            };
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(workers)
                .enable_all()
                .build()
                .unwrap()
                .block_on(benchmark_eager_loopback_inner())
        }
        _ => panic!("IRMIN_EAGER_BENCH_RUNTIME must be current, multi1, multi2, or multi4"),
    }
}

#[cfg(feature = "bench-diagnostics")]
/// Measures fixed-work eager downloads over a synthetic loopback server.
///
/// `IRMIN_EAGER_BENCH_MODE` selects streaming, one-shot, or the production
/// threshold; `IRMIN_EAGER_BENCH_PATTERN` selects payload compressibility, and
/// `IRMIN_EAGER_BENCH_FS_LABEL` supplies the output filesystem label. The server
/// applies one response delay per request and an optional shared no-credit
/// body-rate limit. It does not model TCP windowing, loss, jitter, or a remote
/// CDN. Process RSS includes both client and server tasks, so use the reset
/// high-water mark as a relative measurement for this fixture only.
#[test]
#[ignore = "measures fixed-work loopback eager installer worker limits"]
fn benchmark_eager_loopback_fixed_work_workers() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    runtime
        .block_on(async {
            tokio::time::timeout(
                std::time::Duration::from_secs(600),
                benchmark_eager_loopback_fixed_work_inner(),
            )
            .await
        })
        .expect("fixed-work eager benchmark exceeded its timeout");
}

#[cfg(feature = "bench-diagnostics")]
async fn benchmark_eager_loopback_inner() {
    use futures_util::stream;
    use http_body_util::StreamBody;
    use hyper::body::{Frame, Incoming};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use std::convert::Infallible;
    use std::net::TcpListener as StdTcpListener;
    use std::sync::mpsc;
    use std::thread;

    let parse_env = |name: &str, default: u64| {
        std::env::var(name)
            .map(|value| {
                value
                    .parse::<u64>()
                    .unwrap_or_else(|_| panic!("{name} must be an unsigned integer"))
            })
            .unwrap_or(default)
    };
    let concurrency = usize::try_from(parse_env("IRMIN_EAGER_BENCH_CONCURRENCY", 4)).unwrap();
    let size_mib = usize::try_from(parse_env("IRMIN_EAGER_BENCH_SIZE_MIB", 32)).unwrap();
    assert!((1..=16).contains(&concurrency));
    assert!((17..=128).contains(&size_mib));
    let mode = std::env::var("IRMIN_EAGER_BENCH_MODE").unwrap_or_else(|_| "stream".to_owned());
    let force_streaming = match mode.as_str() {
        "stream" => Some(true),
        "oneshot" => Some(false),
        "threshold" => None,
        _ => panic!("IRMIN_EAGER_BENCH_MODE must be stream, oneshot, or threshold"),
    };
    let queue_capacity = usize::try_from(parse_env("IRMIN_EAGER_BENCH_QUEUE_CAPACITY", 8)).unwrap();
    assert!(queue_capacity > 0);
    let response_delay_ms = parse_env("IRMIN_EAGER_BENCH_RTT_MS", 0);
    assert!(response_delay_ms <= 60_000, "RTT delay must not exceed 60s");
    let rate_mib_s = parse_env("IRMIN_EAGER_BENCH_RATE_MIB_S", 0);
    let rate_bytes_s = rate_mib_s.checked_mul(1024 * 1024).expect("rate too large");
    let frame_size = usize::try_from(parse_env("IRMIN_EAGER_BENCH_FRAME_KIB", 64))
        .unwrap()
        .checked_mul(1024)
        .expect("frame size too large");
    assert!((1..=4096 * 1024).contains(&frame_size));
    if rate_bytes_s > 0 {
        let interval_nanos = (frame_size as u128) * 1_000_000_000 / rate_bytes_s as u128;
        assert!(
            interval_nanos >= 500_000,
            "frame pacing interval must be at least 500us"
        );
    }

    let mut state = 0xBAD5EEDu64;
    let data = (0..size_mib * 1024 * 1024)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect::<Vec<_>>();
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(25).unwrap();
    encoder.write_all(&data).unwrap();
    let compressed = bytes::Bytes::from(encoder.finish().unwrap());
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&data));
    drop(data);

    let (address_tx, address_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let pacing = Arc::new(Mutex::new(None::<Instant>));
    let response_count = Arc::new(AtomicU64::new(0));
    let frame_count = Arc::new(AtomicU64::new(0));
    let server_response_count = Arc::clone(&response_count);
    let server_frame_count = Arc::clone(&frame_count);
    let server_body = compressed.clone();
    let server_pacing = Arc::clone(&pacing);
    let server = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            address_tx.send(listener.local_addr().unwrap()).unwrap();
            let mut shutdown = Box::pin(shutdown_rx);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let body_bytes = server_body.clone();
                        let pacing = Arc::clone(&server_pacing);
                        let response_count = Arc::clone(&server_response_count);
                        let frame_count = Arc::clone(&server_frame_count);
                        tokio::spawn(async move {
                            let service = service_fn(move |request: Request<Incoming>| {
                                let body_bytes = body_bytes.clone();
                                let pacing = Arc::clone(&pacing);
                                let response_count = Arc::clone(&response_count);
                                let frame_count = Arc::clone(&frame_count);
                                async move {
                                    response_count.fetch_add(1, Ordering::Relaxed);
                                    if response_delay_ms > 0 {
                                        tokio::time::sleep(std::time::Duration::from_millis(
                                            response_delay_ms,
                                        ))
                                        .await;
                                    }
                                    let start = request
                                        .headers()
                                        .get("x-start")
                                        .and_then(|value| value.to_str().ok())
                                        .and_then(|value| value.parse::<usize>().ok())
                                        .unwrap_or(0);
                                    let length = request
                                        .headers()
                                        .get("x-length")
                                        .and_then(|value| value.to_str().ok())
                                        .and_then(|value| value.parse::<usize>().ok())
                                        .unwrap_or(body_bytes.len());
                                    let end = start.saturating_add(length).min(body_bytes.len());
                                    let response_frame_size = frame_size;
                                    let stream = stream::unfold(
                                        (start, end, body_bytes),
                                        move |(offset, end, body)| {
                                            let pacing = Arc::clone(&pacing);
                                            let frame_count = Arc::clone(&frame_count);
                                            async move {
                                                if offset >= end {
                                                    return None;
                                                }
                                                let next = (offset + response_frame_size).min(end);
                                                if rate_bytes_s > 0 {
                                                    let frame_bytes = (next - offset) as u64;
                                                    let scheduled_end = {
                                                        let mut state = pacing
                                                            .lock()
                                                            .unwrap_or_else(|e| e.into_inner());
                                                        let now = Instant::now();
                                                        let start = state
                                                            .map_or(now, |available| available.max(now));
                                                        let duration_nanos = (frame_bytes as u128)
                                                            .saturating_mul(1_000_000_000)
                                                            .div_ceil(rate_bytes_s as u128)
                                                            .min(u64::MAX as u128)
                                                            as u64;
                                                        let end = start
                                                            + std::time::Duration::from_nanos(
                                                                duration_nanos,
                                                            );
                                                        *state = Some(end);
                                                        end
                                                    };
                                                    let now = Instant::now();
                                                    if scheduled_end > now {
                                                        tokio::time::sleep_until(
                                                            scheduled_end.into(),
                                                        )
                                                        .await;
                                                    }
                                                }
                                                frame_count.fetch_add(1, Ordering::Relaxed);
                                                Some((
                                                    Ok::<_, Infallible>(Frame::data(
                                                        body.slice(offset..next),
                                                    )),
                                                    (next, end, body),
                                                ))
                                            }
                                        },
                                    );
                                    let mut response = Response::new(StreamBody::new(stream));
                                    response.headers_mut().insert(
                                        hyper::header::CONTENT_LENGTH,
                                        (end - start).to_string().parse().unwrap(),
                                    );
                                    Ok::<_, Infallible>(response)
                                }
                            });
                            let _ = http1::Builder::new()
                                .serve_connection(TokioIo::new(socket), service)
                                .await;
                        });
                    }
                }
            }
        });
    });
    struct ServerGuard {
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    let mut server_guard = ServerGuard {
        shutdown: Some(shutdown_tx),
        thread: Some(server),
    };
    let address = address_rx.recv().unwrap();
    let url_prefix = format!("http://{address}/");
    let download = Arc::new(DownloadInfo {
        encryption: 0,
        password: String::new(),
        compression: Compression::None,
        url_prefix,
        url_suffix: "chunks".to_owned(),
    });
    let client = Arc::new(Client::new());
    let dir = tempfile::tempdir().unwrap();
    let allocator = Arc::new(super::output_allocator::OutputAllocator::new(dir.path()));
    let bitmap = Arc::new(
        super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("bitmap"), concurrency).unwrap(),
    );
    let assets = (0..concurrency)
        .map(|index| {
            let chunk = SophonManifestAssetChunk {
                chunk_name: format!("chunk-{index}"),
                chunk_decompressed_hash_md5: decompressed_hash.clone(),
                chunk_on_file_offset: 0,
                chunk_size: compressed.len() as u64,
                chunk_size_decompressed: (size_mib * 1024 * 1024) as u64,
                chunk_compressed_hash_xxh: 0,
                chunk_compressed_hash_md5: compressed_hash.clone(),
                chunk_old_offset: -1,
            };
            SophonManifestAssetProperty {
                asset_name: format!("chunk-{index}.bin"),
                asset_chunks: vec![chunk],
                asset_type: 0,
                asset_size: (size_mib * 1024 * 1024) as u64,
                asset_hash_md5: decompressed_hash.clone(),
            }
        })
        .collect::<Vec<_>>();
    let manifest = Arc::new(CompactManifest::from(assets));
    for index in 0..concurrency {
        allocator
            .preallocate_file(
                index,
                &format!("chunk-{index}.bin"),
                (size_mib * 1024 * 1024) as u64,
            )
            .unwrap();
    }
    let context = Arc::new(InstallContext {
        installer_clients: Arc::new(vec![Arc::clone(&client)]),
        installer_downloads: Arc::new(vec![Arc::clone(&download)]),
        chunks_dir: Arc::new(dir.path().to_path_buf()),
        game_dir: Arc::new(dir.path().to_path_buf()),
        all_tmp_dirs: Arc::new(vec![]),
        all_files: manifest,
        downloaded_bytes: Arc::new(AtomicU64::new(0)),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: (compressed.len() * concurrency) as u64,
        total_files: concurrency as u64,
        resume_bytes_offset: Arc::new(AtomicU64::new(0)),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: Arc::new(AtomicU64::new(now_nanos())),
        smooth_speed_bps: Arc::new(AtomicU64::new(0)),
        eta_speed_history: Arc::new(Mutex::new(VecDeque::new())),
        last_speed_bytes: Arc::new(AtomicU64::new(0)),
        last_speed_time: Arc::new(AtomicU64::new(now_nanos())),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: Arc::new(AtomicU64::new(0)),
        last_save: Arc::new(Mutex::new(None)),
        state_saver: Arc::new(|_| {}),
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(
            (0..concurrency)
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>(),
        ),
        output_allocator: Arc::clone(&allocator),
        chunk_bitmap: Arc::clone(&bitmap),
        verify_mode: VerifyMode::Full,
    });
    let pending = Arc::new(
        (0..concurrency)
            .map(|_| AtomicU32::new(1))
            .collect::<Vec<_>>(),
    );
    let chunk_entries = Arc::new(
        (0..concurrency)
            .map(|index| (index as u32, 0, index as u32))
            .collect::<Vec<_>>(),
    );
    let offsets = Arc::new((0..=concurrency as u32).collect::<Vec<_>>());
    let peak_before = rss_high_water_kib();
    let cpu_before = process_cpu_time_ms();
    let started = std::time::Instant::now();
    let timings = Arc::new(super::super::eager_decompress::EagerStreamPhaseTimings::default());
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..concurrency {
        let context = Arc::clone(&context);
        let pending = Arc::clone(&pending);
        let chunk_entries = Arc::clone(&chunk_entries);
        let offsets = Arc::clone(&offsets);
        let timings = Arc::clone(&timings);
        tasks.spawn(async move {
            let mut item = eager_item();
            item.file_idx = index as u32;
            process_eager_item_with_mode(
                &item,
                index,
                context.all_files.file_chunk(index, 0),
                &context,
                chunk_entries.as_slice(),
                offsets.as_slice(),
                pending.as_slice(),
                &DownloadHandle::new(),
                force_streaming,
                Some(queue_capacity),
                Some(timings),
            )
            .await
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    println!(
        "path=loopback-installer mode={mode} size_mib={size_mib} concurrency={concurrency} queue_capacity={queue_capacity} frame_kib={} rtt_ms={response_delay_ms} server_pace_mib_s={rate_mib_s} requests={} frames={} elapsed_ms={} cpu_ms={} producer_wait_ms={} compressed_hash_ms={} send_ms={} consumer_wait_ms={} decoder_read_including_wait_ms={} decoder_thread_cpu_ms={} decoder_read_calls={} decoder_read_mib={:.1} reader_calls={} reader_requested_mib={:.1} reader_bypass_calls={} output_write_ms={} decompressed_hash_ms={} peak_before_kib={peak_before} peak_after_kib={} compressed_mib={:.1}",
        frame_size / 1024,
        response_count.load(Ordering::Relaxed),
        frame_count.load(Ordering::Relaxed),
        started.elapsed().as_millis(),
        process_cpu_time_ms().saturating_sub(cpu_before),
        timings.producer_wait_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.compressed_hash_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.channel_send_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.consumer_wait_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.decoder_read_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.decoder_thread_cpu_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.decoder_read_count.load(Ordering::Relaxed),
        timings.decoder_read_bytes.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        timings.input_read_calls.load(Ordering::Relaxed),
        timings.input_read_requested_bytes.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0),
        timings.input_read_bypass_calls.load(Ordering::Relaxed),
        timings.output_write_ns.load(Ordering::Relaxed) / 1_000_000,
        timings.output_hash_ns.load(Ordering::Relaxed) / 1_000_000,
        rss_high_water_kib(),
        compressed.len() as f64 * concurrency as f64 / (1024.0 * 1024.0),
    );
    server_guard.shutdown.take().unwrap().send(()).unwrap();
    server_guard.thread.take().unwrap().join().unwrap();
}

#[cfg(feature = "bench-diagnostics")]
async fn benchmark_eager_loopback_fixed_work_inner() {
    let workers = std::env::var("IRMIN_EAGER_BENCH_WORKERS")
        .unwrap_or_else(|_| "4".to_owned())
        .parse::<usize>()
        .expect("IRMIN_EAGER_BENCH_WORKERS must be an unsigned integer");
    let item_count = std::env::var("IRMIN_EAGER_BENCH_ITEMS")
        .unwrap_or_else(|_| "16".to_owned())
        .parse::<usize>()
        .expect("IRMIN_EAGER_BENCH_ITEMS must be an unsigned integer");
    assert!((1..=48).contains(&workers));
    assert!((1..=256).contains(&item_count));
    assert!(workers <= item_count);

    let size_mib = std::env::var("IRMIN_EAGER_BENCH_SIZE_MIB")
        .unwrap_or_else(|_| "17".to_owned())
        .parse::<usize>()
        .expect("IRMIN_EAGER_BENCH_SIZE_MIB must be an unsigned integer");
    assert!((17..=256).contains(&size_mib));
    let queue_capacity = std::env::var("IRMIN_EAGER_BENCH_QUEUE_CAPACITY")
        .unwrap_or_else(|_| "8".to_owned())
        .parse::<usize>()
        .expect("IRMIN_EAGER_BENCH_QUEUE_CAPACITY must be an unsigned integer");
    assert!(queue_capacity > 0);
    let response_delay_ms = std::env::var("IRMIN_EAGER_BENCH_RESPONSE_DELAY_MS")
        .unwrap_or_else(|_| "0".to_owned())
        .parse::<u64>()
        .expect("IRMIN_EAGER_BENCH_RESPONSE_DELAY_MS must be an unsigned integer");
    let rate_mib_s = std::env::var("IRMIN_EAGER_BENCH_RATE_MIB_S")
        .unwrap_or_else(|_| "0".to_owned())
        .parse::<u64>()
        .expect("IRMIN_EAGER_BENCH_RATE_MIB_S must be an unsigned integer");
    let frame_kib = std::env::var("IRMIN_EAGER_BENCH_FRAME_KIB")
        .unwrap_or_else(|_| "64".to_owned())
        .parse::<usize>()
        .expect("IRMIN_EAGER_BENCH_FRAME_KIB must be an unsigned integer");
    assert!((1..=4096).contains(&frame_kib));
    let mode = std::env::var("IRMIN_EAGER_BENCH_MODE").unwrap_or_else(|_| "stream".to_owned());
    let force_streaming = match mode.as_str() {
        "stream" => Some(true),
        "oneshot" => Some(false),
        "threshold" => None,
        _ => panic!("IRMIN_EAGER_BENCH_MODE must be stream, oneshot, or threshold"),
    };

    let pattern =
        std::env::var("IRMIN_EAGER_BENCH_PATTERN").unwrap_or_else(|_| "random".to_owned());
    let data_len = size_mib * 1024 * 1024;
    let data = match pattern.as_str() {
        "random" => {
            let mut state = 0xBAD5EEDu64;
            (0..data_len)
                .map(|_| {
                    state ^= state >> 12;
                    state ^= state << 25;
                    state ^= state >> 27;
                    (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
                })
                .collect::<Vec<_>>()
        }
        "low-alphabet" => {
            let mut state = 0xBAD5EEDu64;
            (0..data_len)
                .map(|_| {
                    state ^= state >> 12;
                    state ^= state << 25;
                    state ^= state >> 27;
                    (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 60) as u8
                })
                .collect::<Vec<_>>()
        }
        "repeated" => vec![0xA5; data_len],
        _ => panic!("IRMIN_EAGER_BENCH_PATTERN must be random, low-alphabet, or repeated"),
    };
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(25).unwrap();
    encoder.write_all(&data).unwrap();
    let compressed = bytes::Bytes::from(encoder.finish().unwrap());
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let expected_output_hash = fast_md5::digest(&data);
    let decompressed_hash = hex::encode(expected_output_hash);
    let should_stream = super::should_stream_eager(compressed.len() as u64, force_streaming);
    drop(data);

    let response_count = Arc::new(AtomicU64::new(0));
    let frame_count = Arc::new(AtomicU64::new(0));
    let pacing = Arc::new(Mutex::new(None::<Instant>));
    let (address_tx, address_rx) = std::sync::mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_body = compressed.clone();
    let server_response_count = Arc::clone(&response_count);
    let server_frame_count = Arc::clone(&frame_count);
    let server_pacing = Arc::clone(&pacing);
    let rate_bytes_s = rate_mib_s.checked_mul(1024 * 1024).expect("rate too large");
    let frame_size = frame_kib.checked_mul(1024).expect("frame size too large");
    let server = std::thread::spawn(move || {
        let server_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        server_runtime.block_on(async move {
            use futures_util::stream;
            use http_body_util::StreamBody;
            use hyper::body::{Frame, Incoming};
            use hyper::server::conn::http1;
            use hyper::service::service_fn;
            use hyper::{Request, Response};
            use hyper_util::rt::TokioIo;
            use std::convert::Infallible;
            use std::net::TcpListener as StdTcpListener;

            let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            address_tx.send(listener.local_addr().unwrap()).unwrap();
            let mut shutdown = Box::pin(shutdown_rx);
            loop {
                tokio::select! {
                    biased;
                    _ = &mut shutdown => break,
                    accepted = listener.accept() => {
                        let (socket, _) = accepted.unwrap();
                        let body_bytes = server_body.clone();
                        let pacing = Arc::clone(&server_pacing);
                        let response_count = Arc::clone(&server_response_count);
                        let frame_count = Arc::clone(&server_frame_count);
                        tokio::spawn(async move {
                            let service = service_fn(move |_request: Request<Incoming>| {
                                let body_bytes = body_bytes.clone();
                                let pacing = Arc::clone(&pacing);
                                let response_count = Arc::clone(&response_count);
                                let frame_count = Arc::clone(&frame_count);
                                async move {
                                    response_count.fetch_add(1, Ordering::Relaxed);
                                    if response_delay_ms > 0 {
                                        tokio::time::sleep(std::time::Duration::from_millis(response_delay_ms)).await;
                                    }
                                    let content_length = body_bytes.len();
                                    let stream = stream::unfold((0usize, body_bytes), move |(offset, body)| {
                                        let pacing = Arc::clone(&pacing);
                                        let frame_count = Arc::clone(&frame_count);
                                        async move {
                                            if offset >= body.len() {
                                                return None;
                                            }
                                            let next = (offset + frame_size).min(body.len());
                                            if rate_bytes_s > 0 {
                                                let frame_bytes = (next - offset) as u64;
                                                // A shared no-credit schedule serializes frames without bursting after idle time.
                                                let deadline = {
                                                    let mut state = pacing.lock().unwrap_or_else(|e| e.into_inner());
                                                    let now = Instant::now();
                                                    let start = state.map_or(now, |available| available.max(now));
                                                    let duration_nanos = (frame_bytes as u128)
                                                        .saturating_mul(1_000_000_000)
                                                        .div_ceil(rate_bytes_s as u128)
                                                        .min(u64::MAX as u128) as u64;
                                                    let deadline = start + std::time::Duration::from_nanos(duration_nanos);
                                                    *state = Some(deadline);
                                                    deadline
                                                };
                                                let now = Instant::now();
                                                if deadline > now {
                                                    tokio::time::sleep_until(deadline.into()).await;
                                                }
                                            }
                                            frame_count.fetch_add(1, Ordering::Relaxed);
                                            Some((Ok::<_, Infallible>(Frame::data(body.slice(offset..next))), (next, body)))
                                        }
                                    });
                                    let mut response = Response::new(StreamBody::new(stream));
                                    response.headers_mut().insert(
                                        hyper::header::CONTENT_LENGTH,
                                        content_length.to_string().parse().unwrap(),
                                    );
                                    Ok::<_, Infallible>(response)
                                }
                            });
                            let _ = http1::Builder::new().serve_connection(TokioIo::new(socket), service).await;
                        });
                    }
                }
            }
        });
    });

    struct ServerGuard {
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        thread: Option<std::thread::JoinHandle<()>>,
    }
    impl Drop for ServerGuard {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }
    let mut server_guard = ServerGuard {
        shutdown: Some(shutdown_tx),
        thread: Some(server),
    };
    let address = address_rx.recv().unwrap();
    let download = Arc::new(DownloadInfo {
        encryption: 0,
        password: String::new(),
        compression: Compression::None,
        url_prefix: format!("http://{address}/"),
        url_suffix: "chunks".to_owned(),
    });
    let client = Arc::new(Client::new());
    let temp_root = std::env::var_os("IRMIN_EAGER_BENCH_TEMP_DIR");
    let uses_custom_temp_root = temp_root.is_some();
    let dir = match temp_root {
        Some(path) => tempfile::Builder::new()
            .prefix("irmin-fixed-work-")
            .tempdir_in(path)
            .unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let output_filesystem =
        std::env::var("IRMIN_EAGER_BENCH_FS_LABEL").unwrap_or_else(|_| "unspecified".to_owned());
    let allocator = Arc::new(super::output_allocator::OutputAllocator::new(dir.path()));
    let bitmap = Arc::new(
        super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("bitmap"), item_count).unwrap(),
    );
    let assets = (0..item_count)
        .map(|index| SophonManifestAssetProperty {
            asset_name: format!("fixed-{index}.bin"),
            asset_chunks: vec![SophonManifestAssetChunk {
                chunk_name: format!("chunk-{index}"),
                chunk_decompressed_hash_md5: decompressed_hash.clone(),
                chunk_on_file_offset: 0,
                chunk_size: compressed.len() as u64,
                chunk_size_decompressed: (size_mib * 1024 * 1024) as u64,
                chunk_compressed_hash_xxh: 0,
                chunk_compressed_hash_md5: compressed_hash.clone(),
                chunk_old_offset: -1,
            }],
            asset_type: 0,
            asset_size: (size_mib * 1024 * 1024) as u64,
            asset_hash_md5: decompressed_hash.clone(),
        })
        .collect::<Vec<_>>();
    let manifest = Arc::new(CompactManifest::from(assets));
    for index in 0..item_count {
        allocator
            .preallocate_file(
                index,
                &format!("fixed-{index}.bin"),
                (size_mib * 1024 * 1024) as u64,
            )
            .unwrap();
    }
    let pending = Arc::new(
        (0..item_count)
            .map(|_| AtomicU32::new(1))
            .collect::<Vec<_>>(),
    );
    let context = Arc::new(InstallContext {
        installer_clients: Arc::new(vec![Arc::clone(&client)]),
        installer_downloads: Arc::new(vec![Arc::clone(&download)]),
        chunks_dir: Arc::new(dir.path().to_path_buf()),
        game_dir: Arc::new(dir.path().to_path_buf()),
        all_tmp_dirs: Arc::new(vec![]),
        all_files: manifest,
        downloaded_bytes: Arc::new(AtomicU64::new(0)),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: (compressed.len() * item_count) as u64,
        total_files: item_count as u64,
        resume_bytes_offset: Arc::new(AtomicU64::new(0)),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: Arc::new(AtomicU64::new(now_nanos())),
        smooth_speed_bps: Arc::new(AtomicU64::new(0)),
        eta_speed_history: Arc::new(Mutex::new(VecDeque::new())),
        last_speed_bytes: Arc::new(AtomicU64::new(0)),
        last_speed_time: Arc::new(AtomicU64::new(now_nanos())),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: Arc::new(AtomicU64::new(0)),
        last_save: Arc::new(Mutex::new(None)),
        state_saver: Arc::new(|_| {}),
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(
            (0..item_count)
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>(),
        ),
        output_allocator: Arc::clone(&allocator),
        chunk_bitmap: Arc::clone(&bitmap),
        verify_mode: VerifyMode::Full,
    });
    let chunk_entries = Arc::new(
        (0..item_count)
            .map(|index| (index as u32, 0, index as u32))
            .collect::<Vec<_>>(),
    );
    let offsets = Arc::new((0..=item_count as u32).collect::<Vec<_>>());
    let next_index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handle = DownloadHandle::new();
    let (setup_rss, setup_anon, setup_file, setup_shmem) = rss_status_components_kib();
    let hwm_reset = reset_rss_high_water();
    assert!(hwm_reset, "unable to reset process high-water RSS");
    let cpu_before = process_cpu_time_ms();
    let started = Instant::now();
    let timings = Arc::new(super::super::eager_decompress::EagerStreamPhaseTimings::default());
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..workers {
        let context = Arc::clone(&context);
        let pending = Arc::clone(&pending);
        let chunk_entries = Arc::clone(&chunk_entries);
        let offsets = Arc::clone(&offsets);
        let next_index = Arc::clone(&next_index);
        let timings = Arc::clone(&timings);
        let handle = handle.clone();
        tasks.spawn(async move {
            while let Some(index) = DownloadItem::claim_index(&next_index, item_count) {
                let mut item = eager_item();
                item.file_idx = index as u32;
                process_eager_item_with_mode(
                    &item,
                    index,
                    context.all_files.file_chunk(index, 0),
                    &context,
                    chunk_entries.as_slice(),
                    offsets.as_slice(),
                    pending.as_slice(),
                    &handle,
                    force_streaming,
                    Some(queue_capacity),
                    Some(Arc::clone(&timings)),
                )
                .await?;
            }
            Ok::<(), SophonError>(())
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    let elapsed = started.elapsed();
    let downloaded = context.downloaded_bytes.load(Ordering::Relaxed);
    assert_eq!(response_count.load(Ordering::Relaxed), item_count as u64);
    assert_eq!(downloaded, context.total_bytes);
    for index in 0..item_count {
        assert!(context.chunk_bitmap.is_complete(index));
        let output = fs::read(dir.path().join(format!("fixed-{index}.bin"))).unwrap();
        assert_eq!(output.len(), size_mib * 1024 * 1024);
        assert_eq!(fast_md5::digest(&output), expected_output_hash);
    }
    println!(
        "path=fixed-work-eager mode={mode} selected={} pattern={pattern} workers={workers} items={item_count} size_mib={size_mib} total_output_mib={:.1} compressed_mib={:.2} ratio={:.2} queue_capacity={queue_capacity} frame_kib={frame_kib} response_delay_ms={response_delay_ms} server_pace_mib_s={rate_mib_s} temp_root={} fs_label={output_filesystem} requests={} frames={} elapsed_ms={} process_cpu_ms={} hwm_reset={hwm_reset} setup_rss_kib={setup_rss:?} setup_anon_kib={setup_anon:?} setup_file_kib={setup_file:?} setup_shmem_kib={setup_shmem:?} operation_hwm_kib={} downloaded_mib={:.1} achieved_mib_s={:.2}",
        if should_stream { "stream" } else { "oneshot" },
        (size_mib * item_count) as f64,
        compressed.len() as f64 / (1024.0 * 1024.0),
        data_len as f64 / compressed.len() as f64,
        if uses_custom_temp_root {
            "custom"
        } else {
            "system"
        },
        response_count.load(Ordering::Relaxed),
        frame_count.load(Ordering::Relaxed),
        elapsed.as_millis(),
        process_cpu_time_ms().saturating_sub(cpu_before),
        rss_high_water_kib(),
        downloaded as f64 / (1024.0 * 1024.0),
        downloaded as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0),
    );
    server_guard.shutdown.take().unwrap().send(()).unwrap();
    server_guard.thread.take().unwrap().join().unwrap();
}

#[tokio::test]
#[ignore = "validates the large-chunk streaming branch"]
async fn eager_large_chunk_uses_streaming_branch() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, ResponseTemplate};

    let size = (super::EAGER_STREAM_THRESHOLD + 1024 * 1024) as usize;
    let mut state = 0xC0FFEEu64;
    let data = (0..size)
        .map(|_| {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect::<Vec<_>>();
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.window_log(25).unwrap();
    encoder.write_all(&data).unwrap();
    let compressed = encoder.finish().unwrap();
    assert!(compressed.len() as u64 >= super::EAGER_STREAM_THRESHOLD);
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&data));
    let server = wiremock::MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("chunks/large"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed.clone()))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let allocator = Arc::new(super::output_allocator::OutputAllocator::new(dir.path()));
    allocator
        .preallocate_file(0, "large.bin", size as u64)
        .unwrap();
    let chunk = SophonManifestAssetChunk {
        chunk_name: "large".into(),
        chunk_decompressed_hash_md5: decompressed_hash,
        chunk_on_file_offset: 0,
        chunk_size: compressed.len() as u64,
        chunk_size_decompressed: size as u64,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: compressed_hash,
        chunk_old_offset: -1,
    };
    let ctx = eager_context(
        dir.path(),
        chunk_download(&server),
        Arc::new(Client::new()),
        allocator,
        chunk,
        size as u64,
        &hex::encode(fast_md5::digest(&data)),
    );
    let item = eager_item();
    process_eager_item(
        &item,
        0,
        ctx.all_files.file_chunk(0, 0),
        &ctx,
        &[(0, 0, 0)],
        &[0, 1],
        &[AtomicU32::new(1)],
        &DownloadHandle::new(),
    )
    .await
    .unwrap();
    assert!(ctx.chunk_bitmap.is_complete(0));
    assert_eq!(
        fast_md5::digest(&fs::read(dir.path().join("large.bin")).unwrap()),
        fast_md5::digest(&data)
    );
}

#[test]
fn chunk_stream_reader_requires_verified_completion() {
    let (sender, receiver) = tokio::sync::mpsc::channel(1);
    sender
        .blocking_send(Ok(StreamMessage::Data(bytes::Bytes::from_static(b"abc"))))
        .unwrap();
    drop(sender);
    let mut reader = ChunkStreamReader {
        receiver,
        current: None,
        offset: 0,
        complete: false,
        #[cfg(feature = "bench-diagnostics")]
        timings: None,
    };
    let mut output = [0; 4];
    assert_eq!(std::io::Read::read(&mut reader, &mut output).unwrap(), 3);
    assert_eq!(&output[..3], b"abc");
    assert_eq!(
        std::io::Read::read(&mut reader, &mut output)
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
}

#[test]
fn stream_decompressor_rejects_decompressed_hash_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let data = vec![0x7B; 64 * 1024];
    let compressed = zstd::encode_all(data.as_slice(), 1).unwrap();
    let output = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.path().join("output.bin"))
        .unwrap();
    output.set_len(data.len() as u64).unwrap();
    let result = super::super::eager_decompress::stream_decompress_to_file(
        std::io::Cursor::new(&compressed),
        &output,
        0,
        data.len() as u64,
        "ffffffffffffffffffffffffffffffff",
    );
    assert!(matches!(result, Err(SophonError::Md5Mismatch { .. })));
}

#[cfg(feature = "bench-diagnostics")]
#[test]
fn eager_decompress_prefault_and_demand_paths_match() {
    let dir = tempfile::tempdir().unwrap();
    let data = (0usize..256 * 1024)
        .map(|index| (index.wrapping_mul(37) >> 3) as u8)
        .collect::<Vec<_>>();
    let compressed = zstd::encode_all(data.as_slice(), 3).unwrap();
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&data));

    let mut outputs = Vec::new();
    for (index, prefault) in [true, false].into_iter().enumerate() {
        let path = dir.path().join(format!("output-{index}.bin"));
        let output = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        output.set_len(data.len() as u64).unwrap();
        let written = if prefault {
            super::super::eager_decompress::benchmark_eager_decompress_chunk_with_prefault(
                &compressed,
                &output,
                0,
                data.len() as u64,
                &compressed_hash,
                &decompressed_hash,
            )
        } else {
            super::super::eager_decompress::benchmark_eager_decompress_chunk_without_prefault(
                &compressed,
                &output,
                0,
                data.len() as u64,
                &compressed_hash,
                &decompressed_hash,
            )
        }
        .unwrap();
        assert_eq!(written, data.len() as u64);
        outputs.push(std::fs::read(path).unwrap());
    }
    assert_eq!(outputs[0], data);
    assert_eq!(outputs[1], data);
    assert_eq!(outputs[0], outputs[1]);

    let bad_output = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.path().join("bad-output.bin"))
        .unwrap();
    bad_output.set_len(data.len() as u64).unwrap();
    for prefault in [true, false] {
        let result = if prefault {
            super::super::eager_decompress::benchmark_eager_decompress_chunk_with_prefault(
                b"invalid zstd data",
                &bad_output,
                0,
                data.len() as u64,
                "",
                "",
            )
        } else {
            super::super::eager_decompress::benchmark_eager_decompress_chunk_without_prefault(
                b"invalid zstd data",
                &bad_output,
                0,
                data.len() as u64,
                "",
                "",
            )
        };
        assert!(matches!(result, Err(SophonError::Io(_))));
    }
}

#[cfg(feature = "bench-diagnostics")]
#[test]
fn eager_benchmark_prefault_label_matches_mode() {
    assert_eq!(eager_benchmark_prefault_label("oneshot", true), "true");
    assert_eq!(eager_benchmark_prefault_label("oneshot", false), "false");
    assert_eq!(eager_benchmark_prefault_label("stream", true), "na");
    assert_eq!(eager_benchmark_prefault_label("stream", false), "na");
}

#[cfg(feature = "bench-diagnostics")]
#[test]
#[ignore = "reports single-worker eager decompression timing and VmHWM"]
fn benchmark_eager_decompression_rss() {
    use std::thread;
    use std::time::Instant;

    let size_mib = std::env::var("IRMIN_EAGER_BENCH_SIZE_MIB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64);
    assert!((1..=512).contains(&size_mib));
    let concurrency = std::env::var("IRMIN_EAGER_BENCH_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1);
    assert!((1..=32).contains(&concurrency));
    let prefault = std::env::var("IRMIN_EAGER_BENCH_PREFAULT")
        .map(|value| value != "0")
        .unwrap_or(true);
    let pattern =
        std::env::var("IRMIN_EAGER_BENCH_PATTERN").unwrap_or_else(|_| "random".to_owned());
    let data_len = size_mib * 1024 * 1024;
    let data = match pattern.as_str() {
        "random" => {
            let mut random_state = 0xA11C_E5EED_u64;
            (0..data_len)
                .map(|_| {
                    random_state ^= random_state >> 12;
                    random_state ^= random_state << 25;
                    random_state ^= random_state >> 27;
                    (random_state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
                })
                .collect::<Vec<_>>()
        }
        "low-alphabet" => {
            let mut random_state = 0xA11C_E5EED_u64;
            (0..data_len)
                .map(|_| {
                    random_state ^= random_state >> 12;
                    random_state ^= random_state << 25;
                    random_state ^= random_state >> 27;
                    (random_state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 60) as u8
                })
                .collect::<Vec<_>>()
        }
        "repeated" => vec![0xA5; data_len],
        _ => panic!("IRMIN_EAGER_BENCH_PATTERN must be random, low-alphabet, or repeated"),
    };
    let compressed = zstd::encode_all(data.as_slice(), 3).unwrap();
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&data));
    drop(data);
    let dir = match std::env::var_os("IRMIN_EAGER_BENCH_TEMP_DIR") {
        Some(path) => tempfile::tempdir_in(path).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let mode = std::env::var("IRMIN_EAGER_BENCH_MODE").unwrap_or_else(|_| "stream".to_owned());
    let (setup_rss, setup_anon, setup_file, setup_shmem) = rss_status_components_kib();
    let (setup_smaps_anon, setup_smaps_file, setup_private_dirty) = rss_components_kib();
    let hwm_reset = reset_rss_high_water();
    let before = rss_high_water_kib();
    let cpu_before = process_cpu_time_ms();
    let started = Instant::now();
    thread::scope(|scope| {
        let mut workers = Vec::with_capacity(concurrency);
        for index in 0..concurrency {
            let output = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(dir.path().join(format!("output-{index}.bin")))
                .unwrap();
            output.set_len(data_len as u64).unwrap();
            let compressed = &compressed;
            let compressed_hash = &compressed_hash;
            let decompressed_hash = &decompressed_hash;
            let mode = mode.as_str();
            let size = data_len as u64;
            workers.push(scope.spawn(move || {
                match (mode, prefault) {
                ("oneshot", true) => {
                    super::super::eager_decompress::benchmark_eager_decompress_chunk_with_prefault(
                        compressed,
                        &output,
                        0,
                        size,
                        compressed_hash,
                        decompressed_hash,
                    )
                }
                ("oneshot", false) => super::super::eager_decompress::
                    benchmark_eager_decompress_chunk_without_prefault(
                    compressed,
                    &output,
                    0,
                    size,
                    compressed_hash,
                    decompressed_hash,
                ),
                ("stream", _) => super::super::eager_decompress::stream_decompress_to_file(
                    std::io::Cursor::new(compressed),
                    &output,
                    0,
                    size,
                    decompressed_hash,
                ),
                _ => panic!("IRMIN_EAGER_BENCH_MODE must be oneshot or stream"),
            }
            }));
        }
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
    });
    let elapsed = started.elapsed();
    let prefault_label = eager_benchmark_prefault_label(&mode, prefault);
    let (after_rss, after_anon, after_file, after_shmem) = rss_status_components_kib();
    let (after_smaps_anon, after_smaps_file, after_dirty) = rss_components_kib();
    println!(
        "mode={mode} prefault={prefault_label} pattern={pattern} size_mib={size_mib} concurrency={concurrency} compressed_mib={:.2} ratio={:.2} elapsed_us={} cpu_ms={} hwm_reset={hwm_reset} setup_hwm_kib={before} setup_rss_kib={setup_rss:?} setup_anon_kib={setup_anon:?} setup_file_kib={setup_file:?} setup_shmem_kib={setup_shmem:?} setup_smaps_anon_kib={setup_smaps_anon:?} setup_smaps_file_kib={setup_smaps_file:?} setup_private_dirty_kib={setup_private_dirty:?} operation_hwm_kib={} end_rss_kib={after_rss:?} end_anon_kib={after_anon:?} end_file_kib={after_file:?} end_shmem_kib={after_shmem:?} end_smaps_anon_kib={after_smaps_anon:?} end_smaps_file_kib={after_smaps_file:?} end_private_dirty_kib={after_dirty:?}",
        compressed.len() as f64 / (1024.0 * 1024.0),
        data_len as f64 / compressed.len() as f64,
        elapsed.as_micros(),
        process_cpu_time_ms().saturating_sub(cpu_before),
        rss_high_water_kib(),
    );
}

#[tokio::test]
async fn eager_stream_rejects_unsupported_hash_metadata_without_downloading() {
    use wiremock::MockServer;

    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let allocator = Arc::new(super::output_allocator::OutputAllocator::new(dir.path()));
    allocator.preallocate_file(0, "eager.bin", 1024).unwrap();
    let chunk = SophonManifestAssetChunk {
        chunk_name: "unsupported-hash".into(),
        chunk_decompressed_hash_md5: String::new(),
        chunk_on_file_offset: 0,
        chunk_size: 100,
        chunk_size_decompressed: 1024,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: "invalid-hash-size".into(),
        chunk_old_offset: -1,
    };
    let ctx = eager_context(
        dir.path(),
        chunk_download(&server),
        Arc::new(Client::new()),
        allocator,
        chunk,
        1024,
        "",
    );
    let result = process_eager_item(
        &eager_item(),
        0,
        ctx.all_files.file_chunk(0, 0),
        &ctx,
        &[(0, 0, 0)],
        &[0, 1],
        &[AtomicU32::new(1)],
        &DownloadHandle::new(),
    )
    .await;
    assert!(matches!(result, Err(SophonError::Io(_))));
    assert!(!ctx.chunk_bitmap.is_complete(0));
}
