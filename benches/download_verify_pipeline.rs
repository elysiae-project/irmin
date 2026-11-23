//! Benchmarks simulating downloads with verification/assembly load in
//! between, so regressions that couple download progress to verification
//! progress are caught before merge.
//!
//! Each scenario serves eight fixed payloads concurrently over loopback HTTP
//! at a paced per-connection rate, then measures transfer and completion time.
//!
//! - `nominal_verification`: no artificial verifier delay.
//! - `stalled_verification`: the hash consumer is delayed 80 ms per response
//!   frame via the `set_bench_hash_thread_stall_us` hook, slower than the
//!   configured network pace and emulating verifier CPU starvation.
//! - `assembly_cpu_load`: CPU burner threads simulate parallel assembly work
//!   competing for cores while the hash consumer runs at full speed.
//!
//! The benchmark reports transfer throughput through body EOF separately
//! from verified completion latency. It also reports how often the bounded
//! MD5 queue deferred to a post-download scan. The transfer rate should stay
//! close to the configured server pace even when completion latency rises.
//!
//! Run:
//!   cargo bench --features benchmark,bench-internals,bench-diagnostics --bench
//! download_verify_pipeline

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use irmin_rs::game_installer::download_benchmark::{
    download_and_verify_md5_timed, set_bench_hash_thread_stall_us,
};

const PAYLOAD_MIB: usize = 1;
const FRAME_KIB: usize = 1024;
const CONCURRENT_DOWNLOADS: usize = 8;
/// Pace the loopback server so the healthy scenario is network-bound, not
/// memory-bound; every scenario sees the same delivery schedule.
const PACE_BPS: u64 = 16 * 1024 * 1024; // 16 MiB/s
/// Per-frame hash stall (microseconds) for the stalled-verification scenario.
const STALL_US: u64 = 80_000;

fn make_payload() -> Vec<u8> {
    // High-entropy LCG bytes: incompressible, so the hash cost is
    // representative and frames are all distinct.
    let mut state = 0x9e3779b97f4a7c15u64;
    (0..PAYLOAD_MIB * 1024 * 1024)
        .map(|_| {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        })
        .collect()
}

fn payload_md5_hex(payload: &[u8]) -> String {
    hex::encode(fast_md5::digest(payload))
}

/// One-shot loopback server: serves `payload` over a single connection,
/// paced to PACE_BPS, then exits. Returns the bound address.
fn spawn_paced_server(
    payload: Arc<Vec<u8>>,
    requests: usize,
) -> (std::net::SocketAddr, std::thread::JoinHandle<()>) {
    let (address_tx, address_rx) = std::sync::mpsc::sync_channel(1);
    let server = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            use http_body_util::StreamBody;
            use hyper::body::{Frame, Incoming};
            use hyper::server::conn::http1;
            use hyper::service::service_fn;
            use hyper::{Request as HyperRequest, Response as HyperResponse};
            use hyper_util::rt::TokioIo;
            use std::convert::Infallible;

            let listener = StdTcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            address_tx.send(listener.local_addr().unwrap()).unwrap();
            let mut connections = tokio::task::JoinSet::new();
            for _ in 0..requests {
                let (socket, _) = listener.accept().await.unwrap();
                let payload = Arc::clone(&payload);
                connections.spawn(async move {
                    let service = service_fn(move |_: HyperRequest<Incoming>| {
                        let payload = Arc::clone(&payload);
                        async move {
                            let frame_len = FRAME_KIB * 1024;
                            let mut frames = Vec::new();
                            for start in (0..payload.len()).step_by(frame_len) {
                                let end = (start + frame_len).min(payload.len());
                                let slice = bytes::Bytes::copy_from_slice(&payload[start..end]);
                                frames.push(slice);
                            }
                            let frame_period =
                                Duration::from_nanos((frame_len as u64 * 1_000_000_000) / PACE_BPS);
                            let content_length = payload.len();
                            let stream = futures_util::stream::unfold(
                                (frames, 0usize, frame_period),
                                move |(frames, idx, frame_period)| async move {
                                    if idx >= frames.len() {
                                        return None;
                                    }
                                    tokio::time::sleep(frame_period).await;
                                    let data = &frames[idx];
                                    Some((
                                        Ok::<_, Infallible>(Frame::data(data.clone())),
                                        (frames, idx + 1, frame_period),
                                    ))
                                },
                            );
                            let mut response = HyperResponse::new(StreamBody::new(stream));
                            response.headers_mut().insert(
                                hyper::header::CONTENT_LENGTH,
                                content_length.to_string().parse().unwrap(),
                            );
                            response
                                .headers_mut()
                                .insert(hyper::header::CONNECTION, "close".parse().unwrap());
                            Ok::<_, Infallible>(response)
                        }
                    });
                    let connection =
                        http1::Builder::new().serve_connection(TokioIo::new(socket), service);
                    let _ = connection.await;
                });
            }
            while connections.join_next().await.is_some() {}
        });
    });
    let address = address_rx.recv().unwrap();
    (address, server)
}

/// CPU burner threads that compete for cores, emulating assembly workers
/// running concurrently with downloads. Returns a stop flag and handles.
fn spawn_cpu_burners(count: usize) -> (Arc<AtomicBool>, Vec<std::thread::JoinHandle<()>>) {
    let stop = Arc::new(AtomicBool::new(false));
    let handles = (0..count)
        .map(|_| {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut x = 0x1234_5678_9abc_def0u64;
                while !stop.load(Ordering::Relaxed) {
                    // Hash-like scalar work: mixing rounds, no syscalls.
                    for _ in 0..4096 {
                        x ^= x << 13;
                        x ^= x >> 7;
                        x ^= x << 17;
                    }
                    std::hint::black_box(x);
                }
            })
        })
        .collect();
    (stop, handles)
}

fn bench_download_verify_pipeline(c: &mut Criterion) {
    let payload = Arc::new(make_payload());
    let md5_hex = payload_md5_hex(&payload);
    let payload_len = payload.len() as u64;

    let mut group = c.benchmark_group("download_verify_pipeline");
    group.throughput(Throughput::Bytes(payload_len * CONCURRENT_DOWNLOADS as u64));
    group.sample_size(20);

    let mut scenario = |id: &str, stall_us: u64, burners: usize| {
        let require_deferred = stall_us > 0;
        set_bench_hash_thread_stall_us(stall_us);
        let (stop, handles) = spawn_cpu_burners(burners);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let client = reqwest::Client::new();

        group.bench_with_input(BenchmarkId::new("paced", id), &payload_len, |b, _| {
            b.iter_custom(|iters| {
                let mut transfer_total = Duration::ZERO;
                let mut verified_total = Duration::ZERO;
                let mut deferred_hashes = 0u64;
                for _ in 0..iters {
                    // Each sample downloads several chunks concurrently,
                    // matching the install path's shared runtime and worker
                    // contention rather than timing a single isolated chunk.
                    let (address, server) =
                        spawn_paced_server(Arc::clone(&payload), CONCURRENT_DOWNLOADS);
                    let dir = tempfile::TempDir::new().unwrap();
                    let started = Instant::now();
                    let results = runtime.block_on(async {
                        let downloads = (0..CONCURRENT_DOWNLOADS).map(|index| {
                            let client = &client;
                            let url = format!("http://{address}/payload");
                            let dest = dir.path().join(format!("payload-{index}.bin"));
                            let md5_hex = &md5_hex;
                            async move {
                                download_and_verify_md5_timed(
                                    client,
                                    &url,
                                    &dest,
                                    payload_len,
                                    md5_hex,
                                )
                                .await
                                .unwrap()
                            }
                        });
                        futures_util::future::join_all(downloads).await
                    });
                    transfer_total += results
                        .iter()
                        .map(|timings| timings.download)
                        .max()
                        .unwrap_or_default();
                    verified_total += started.elapsed();
                    deferred_hashes += results
                        .iter()
                        .filter(|timings| timings.md5_deferred)
                        .count() as u64;
                    server.join().unwrap();
                }
                eprintln!(
                    "scenario={id} iterations={iters} transfer_ms={:.2} verified_ms={:.2} deferred_hashes={deferred_hashes}",
                    transfer_total.as_secs_f64() * 1000.0,
                    verified_total.as_secs_f64() * 1000.0,
                );
                if id == "nominal_verification" {
                    assert_eq!(deferred_hashes, 0, "nominal verifier must keep up");
                } else if require_deferred {
                    assert_eq!(deferred_hashes, iters * CONCURRENT_DOWNLOADS as u64);
                }
                black_box(verified_total);
                transfer_total
            });
        });

        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        set_bench_hash_thread_stall_us(0);
    };

    // Healthy: consumer keeps up, no CPU contention.
    scenario("nominal_verification", 0, 0);
    // Stalled verification: hash thread stalled 80 ms per response frame,
    // emulating assembly/verification CPU starvation.
    scenario("stalled_verification", STALL_US, 0);
    // Assembly CPU load: burners occupy cores while hashing runs normally.
    scenario("assembly_cpu_load", 0, 4);

    group.finish();
}

use std::hint::black_box;

criterion_group!(benches, bench_download_verify_pipeline);
criterion_main!(benches);
