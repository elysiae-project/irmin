//! Benchmarks simulating downloads with verification/assembly load in
//! between, so regressions that couple download progress to verification
//! progress are caught before merge.
//!
//! Each scenario serves a fixed payload over loopback HTTP at a paced rate
//! (network-like), then measures wall time of `download_and_verify_md5`:
//!
//! - `healthy`: the hash consumer keeps up. Baseline throughput.
//! - `stalled_verification`: the hash consumer is stalled via the
//!   `set_bench_hash_thread_stall_ms` hook, emulating assembly/verification CPU
//!   starvation. The download pipeline must keep receiving at the paced rate;
//!   only a small fallback cost is allowed.
//! - `assembly_cpu_load`: CPU burner threads simulate parallel assembly work
//!   competing for cores while the hash consumer runs at full speed.
//!
//! Regression gate: `stalled_verification` and `assembly_cpu_load` medians
//! must stay within 25% of `healthy` throughput. Before the fix,
//! `stalled_verification` collapses toward the consumer rate (an order of
//! magnitude below healthy) because the blocking hash hand-off parks the
//! download loop.
//!
//! Run:
//!   cargo bench --features benchmark,bench-internals --bench
//! download_verify_pipeline

use std::net::TcpListener as StdTcpListener;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use irmin_rs::game_installer::download_benchmark::{
    download_and_verify_md5, set_bench_hash_thread_stall_ms,
};

const PAYLOAD_MIB: usize = 8;
const FRAME_KIB: usize = 64;
/// Pace the loopback server so the healthy scenario is network-bound, not
/// memory-bound; every scenario sees the same delivery schedule.
const PACE_BPS: u64 = 512 * 1024 * 1024; // 512 MiB/s
/// Per-frame hash stall (ms) for the stalled-verification scenario.
const STALL_MS: u64 = 1;

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
            let (socket, _) = listener.accept().await.unwrap();
            // Build the paced frame schedule once; the service only wraps
            // it per request (keeps the closure Send+Sync for hyper).
            let frame_len = FRAME_KIB * 1024;
            let started = tokio::time::Instant::now();
            let mut frames = Vec::new();
            for (i, start) in (0..payload.len()).step_by(frame_len).enumerate() {
                let end = (start + frame_len).min(payload.len());
                let slice = payload[start..end].to_vec();
                // Absolute schedule: frame i lands at i * frame_time.
                let deadline = started
                    + Duration::from_nanos(
                        (i as u64 * frame_len as u64 * 1_000_000_000) / PACE_BPS,
                    );
                frames.push((deadline, bytes::Bytes::from(slice)));
            }
            let content_length = payload.len();
            let service = service_fn(move |_: HyperRequest<Incoming>| {
                let frames = frames.clone();
                async move {
                    let stream = futures_util::stream::unfold(
                        (frames, 0usize),
                        move |(frames, idx)| async move {
                            if idx >= frames.len() {
                                return None;
                            }
                            let (deadline, data) = &frames[idx];
                            tokio::time::sleep_until(*deadline).await;
                            Some((
                                Ok::<_, Infallible>(Frame::data(data.clone())),
                                (frames, idx + 1),
                            ))
                        },
                    );
                    let body = StreamBody::new(stream);
                    let mut response = HyperResponse::new(body);
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
            let connection = http1::Builder::new().serve_connection(TokioIo::new(socket), service);
            let _ = connection.await;
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
    group.throughput(Throughput::Bytes(payload_len));
    group.sample_size(20);

    let mut scenario = |id: &str, stall_ms: u64, burners: usize| {
        set_bench_hash_thread_stall_ms(stall_ms);
        let (stop, handles) = spawn_cpu_burners(burners);

        group.bench_with_input(BenchmarkId::new("paced", id), &payload_len, |b, _| {
            b.iter_custom(|iters| {
                let mut total = Duration::ZERO;
                for _ in 0..iters {
                    // One-shot server per iteration: each download gets a
                    // fresh paced stream, so iteration timing is the
                    // download alone.
                    let (address, server) = spawn_paced_server(Arc::clone(&payload));
                    let dir = tempfile::TempDir::new().unwrap();
                    let dest = dir.path().join("payload.bin");
                    let started = Instant::now();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .unwrap();
                    runtime
                        .block_on(download_and_verify_md5(
                            &client_new(),
                            &format!("http://{address}/payload"),
                            &dest,
                            payload_len,
                            &md5_hex,
                        ))
                        .unwrap();
                    total += started.elapsed();
                    server.join().unwrap();
                }
                total
            });
        });

        stop.store(true, Ordering::Relaxed);
        for h in handles {
            h.join().unwrap();
        }
        set_bench_hash_thread_stall_ms(0);
    };

    // Healthy: consumer keeps up, no CPU contention.
    scenario("healthy", 0, 0);
    // Stalled verification: hash thread stalled 1 ms per 64 KiB frame,
    // emulating assembly/verification CPU starvation.
    scenario("stalled_verification", STALL_MS, 0);
    // Assembly CPU load: burners occupy cores while hashing runs normally.
    scenario("assembly_cpu_load", 0, 4);

    group.finish();
}

fn client_new() -> reqwest::Client {
    reqwest::Client::new()
}

criterion_group!(benches, bench_download_verify_pipeline);
criterion_main!(benches);
