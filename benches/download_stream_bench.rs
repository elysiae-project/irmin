use std::hint::black_box;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Instant;

use bytes::Bytes;
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use irmin_rs::game_installer::download_benchmark::{check_chunk_size, update_md5, update_xxh64};
use tokio::io::{AsyncWrite, AsyncWriteExt};

const TOTAL_MIB: usize = 64;
const TOTAL_BYTES: usize = TOTAL_MIB * 1024 * 1024;

struct VecWriter(Vec<u8>);

impl AsyncWrite for VecWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.0.extend_from_slice(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

fn make_payload() -> Bytes {
    Bytes::from(
        (0..TOTAL_BYTES)
            .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
            .collect::<Vec<_>>(),
    )
}

fn make_chunks(payload: &Bytes, chunk_size: usize) -> Vec<Bytes> {
    (0..payload.len())
        .step_by(chunk_size)
        .map(|offset| payload.slice(offset..(offset + chunk_size).min(payload.len())))
        .collect()
}

async fn run_no_hash(chunks: &[Bytes], sink: &mut VecWriter) -> u64 {
    sink.0.clear();
    let mut written = 0u64;
    for bytes in chunks {
        written = check_chunk_size(bytes, TOTAL_BYTES as u64, written).unwrap();
        sink.write_all(bytes).await.unwrap();
    }
    sink.flush().await.unwrap();
    black_box(written);
    black_box(&sink.0);
    written
}

async fn run_md5(
    chunks: &[Bytes],
    sink: &mut VecWriter,
    mut hasher: fast_md5::Md5,
) -> (u64, [u8; 16]) {
    sink.0.clear();
    let mut written = 0u64;
    for bytes in chunks {
        written = check_chunk_size(bytes, TOTAL_BYTES as u64, written).unwrap();
        sink.write_all(bytes).await.unwrap();
        update_md5(&mut hasher, bytes).unwrap();
    }
    sink.flush().await.unwrap();
    black_box(&sink.0);
    let digest = hasher.finalize();
    black_box(digest);
    (written, digest)
}

async fn run_xxh64(chunks: &[Bytes], sink: &mut VecWriter) -> (u64, u64) {
    sink.0.clear();
    let mut xxh64 = Some(xxhash_rust::xxh64::Xxh64::new(0));
    let mut written = 0u64;
    for bytes in chunks {
        written = check_chunk_size(bytes, TOTAL_BYTES as u64, written).unwrap();
        sink.write_all(bytes).await.unwrap();
        update_xxh64(xxh64.as_mut().unwrap(), bytes).unwrap();
    }
    sink.flush().await.unwrap();
    black_box(&sink.0);
    let digest = xxh64.unwrap().digest();
    black_box(digest);
    (written, digest)
}

fn bench_download_stream_core(c: &mut Criterion) {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build Tokio runtime");
    let mut group = c.benchmark_group("download_stream_core_memcopy");
    group.throughput(Throughput::Bytes(TOTAL_BYTES as u64));
    let payload = make_payload();
    let expected_md5 = fast_md5::digest(&payload);
    let mut expected_xxh64 = xxhash_rust::xxh64::Xxh64::new(0);
    expected_xxh64.update(&payload);
    let expected_xxh64 = expected_xxh64.digest();
    let mut sink = VecWriter(Vec::with_capacity(TOTAL_BYTES));

    for chunk_size in [4, 16, 64, 256].map(|kib| kib * 1024) {
        let chunks = make_chunks(&payload, chunk_size);
        assert_eq!(
            runtime.block_on(run_no_hash(&chunks, &mut sink)),
            TOTAL_BYTES as u64
        );
        assert_eq!(sink.0.len(), TOTAL_BYTES);
        let (md5_bytes, actual_md5) =
            runtime.block_on(run_md5(&chunks, &mut sink, fast_md5::Md5::new()));
        assert_eq!(md5_bytes, TOTAL_BYTES as u64);
        assert_eq!(sink.0.len(), TOTAL_BYTES);
        assert_eq!(actual_md5, expected_md5);
        let (xxh_bytes, actual_xxh64) = runtime.block_on(run_xxh64(&chunks, &mut sink));
        assert_eq!(xxh_bytes, TOTAL_BYTES as u64);
        assert_eq!(sink.0.len(), TOTAL_BYTES);
        assert_eq!(actual_xxh64, expected_xxh64);
        group.bench_function(BenchmarkId::new("write_only_kib", chunk_size / 1024), |b| {
            b.iter_custom(|iters| {
                runtime.block_on(async {
                    let start = Instant::now();
                    for _ in 0..iters {
                        let _ = run_no_hash(&chunks, &mut sink).await;
                    }
                    start.elapsed()
                })
            });
        });
        group.bench_function(BenchmarkId::new("md5_kib", chunk_size / 1024), |b| {
            b.iter_custom(|iters| {
                runtime.block_on(async {
                    let start = Instant::now();
                    for _ in 0..iters {
                        let _ = run_md5(&chunks, &mut sink, fast_md5::Md5::new()).await;
                    }
                    start.elapsed()
                })
            });
        });
        group.bench_function(BenchmarkId::new("xxh64_kib", chunk_size / 1024), |b| {
            b.iter_custom(|iters| {
                runtime.block_on(async {
                    let start = Instant::now();
                    for _ in 0..iters {
                        let _ = run_xxh64(&chunks, &mut sink).await;
                    }
                    start.elapsed()
                })
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_download_stream_core);
criterion_main!(benches);
