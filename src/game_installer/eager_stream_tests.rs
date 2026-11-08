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

fn process_cpu_time_ms() -> u128 {
    let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
    let result = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
    assert_eq!(result, 0);
    let usage = unsafe { usage.assume_init() };
    (usage.ru_utime.tv_sec as u128 + usage.ru_stime.tv_sec as u128) * 1000
        + (usage.ru_utime.tv_usec as u128 + usage.ru_stime.tv_usec as u128) / 1000
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

#[tokio::test]
#[ignore = "measures actual loopback eager installer concurrency RSS"]
async fn benchmark_eager_loopback_concurrent_rss() {
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

    let concurrency = std::env::var("IRMIN_EAGER_BENCH_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(4);
    let size_mib = std::env::var("IRMIN_EAGER_BENCH_SIZE_MIB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(32);
    assert!((1..=16).contains(&concurrency));
    assert!((17..=128).contains(&size_mib));
    let mode = std::env::var("IRMIN_EAGER_BENCH_MODE").unwrap_or_else(|_| "stream".to_owned());
    let force_streaming = match mode.as_str() {
        "stream" => Some(true),
        "oneshot" => Some(false),
        "threshold" => None,
        _ => panic!("IRMIN_EAGER_BENCH_MODE must be stream, oneshot, or threshold"),
    };
    let queue_capacity = std::env::var("IRMIN_EAGER_BENCH_QUEUE_CAPACITY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8);
    assert!(queue_capacity > 0);

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
    let server_body = compressed.clone();
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
                        tokio::spawn(async move {
                            let service = service_fn(move |request: Request<Incoming>| {
                            let body_bytes = body_bytes.clone();
                            async move {
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
                                let frame_size = 64 * 1024;
                                let stream = stream::unfold(
                                    (start, end, body_bytes),
                                    move |(offset, end, body)| async move {
                                        if offset >= end {
                                            return None;
                                        }
                                        let next = (offset + frame_size).min(end);
                                        Some((
                                            Ok::<_, Infallible>(Frame::data(body.slice(offset..next))),
                                            (next, end, body),
                                        ))
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
    let mut tasks = tokio::task::JoinSet::new();
    for index in 0..concurrency {
        let context = Arc::clone(&context);
        let pending = Arc::clone(&pending);
        let chunk_entries = Arc::clone(&chunk_entries);
        let offsets = Arc::clone(&offsets);
        let force_streaming = force_streaming;
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
            )
            .await
        });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    println!(
        "path=loopback-installer mode={mode} size_mib={size_mib} concurrency={concurrency} queue_capacity={queue_capacity} elapsed_ms={} cpu_ms={} peak_before_kib={peak_before} peak_after_kib={} compressed_mib={:.1}",
        started.elapsed().as_millis(),
        process_cpu_time_ms().saturating_sub(cpu_before),
        rss_high_water_kib(),
        compressed.len() as f64 * concurrency as f64 / (1024.0 * 1024.0),
    );
    let _ = shutdown_tx.send(());
    server.join().unwrap();
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
    let mut random_state = 0xA11C_E5EED_u64;
    let data = (0..size_mib * 1024 * 1024)
        .map(|_| {
            random_state ^= random_state >> 12;
            random_state ^= random_state << 25;
            random_state ^= random_state >> 27;
            (random_state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 56) as u8
        })
        .collect::<Vec<_>>();
    let compressed = zstd::encode_all(data.as_slice(), 3).unwrap();
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&data));
    let dir = tempfile::tempdir().unwrap();
    let mode = std::env::var("IRMIN_EAGER_BENCH_MODE").unwrap_or_else(|_| "stream".to_owned());
    let before = rss_high_water_kib();
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
            output.set_len(data.len() as u64).unwrap();
            let compressed = &compressed;
            let compressed_hash = &compressed_hash;
            let decompressed_hash = &decompressed_hash;
            let mode = mode.as_str();
            let size = data.len() as u64;
            workers.push(scope.spawn(move || match mode {
                "oneshot" => super::super::eager_decompress::eager_decompress_chunk(
                    compressed,
                    &output,
                    0,
                    size,
                    compressed_hash,
                    decompressed_hash,
                ),
                "stream" => super::super::eager_decompress::stream_decompress_to_file(
                    std::io::Cursor::new(compressed),
                    &output,
                    0,
                    size,
                    decompressed_hash,
                ),
                _ => panic!("IRMIN_EAGER_BENCH_MODE must be oneshot or stream"),
            }));
        }
        for worker in workers {
            worker.join().unwrap().unwrap();
        }
    });
    println!(
        "mode={mode} size_mib={size_mib} concurrency={concurrency} compressed_mib={:.1} elapsed_ms={} before_kib={before} after_kib={}",
        compressed.len() as f64 / (1024.0 * 1024.0),
        started.elapsed().as_millis(),
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
