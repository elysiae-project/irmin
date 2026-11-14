//! Real measurement benchmarks for the Sophon installer hot paths.
//!
//! Each benchmark measures a component of the actual implementation — no
//! comparisons against unused alternatives. Use criterion baselines to compare
//! before/after an optimization:
//!
//! Run:                cargo bench
//! Run one group:      cargo bench -- io_copy
//! Save a baseline:    cargo bench -- --save-baseline before
//! Compare to baseline: cargo bench -- --baseline before

use std::fs;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;

use criterion::{BatchSize, BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use fast_md5;

use irmin_rs::game_installer::{
    VerifyMode,
    assembly::{assemble_file, assemble_file_with_benchmark_epochs, chunk_filename},
    assembly_epochs::{AssemblyEpochs, EPOCH_MAX_BYTES, EPOCH_MAX_FILES},
    cache::{VerificationCache, VerificationEntry},
    compact_manifest::{CompactManifest, StringArena},
    installer::{ChunkNameLookup, SophonInstaller},
    sysio,
};
use irmin_rs::manifest::compute_content_manifest_hash;
use irmin_rs::proto_parse::{
    SophonManifestAssetChunk, SophonManifestAssetProperty, decode_manifest_from_bytes,
};

/// Fill a file with `mib` MiB of byte `fill`.
fn fill_file(path: &std::path::Path, mib: usize, fill: u8) {
    let block = vec![fill; 1024 * 1024];
    let mut f = fs::File::create(path).expect("create");
    for _ in 0..mib {
        f.write_all(&block).expect("write");
    }
    f.sync_all().expect("sync");
}

/// Generate pseudo-random data with realistic compression ratio (~2-3:1 at zstd level 3).
/// Uses xoshiro256** seeded deterministically. Every 4th 1 KiB block is a repeat of the
/// previous block, giving zstd something to match without collapsing to trivial ratios.
fn gen_prng_data(seed: u64, len: usize) -> Vec<u8> {
    let mut s = [
        seed,
        seed.wrapping_mul(6364136223846793005).wrapping_add(1),
        seed.wrapping_mul(1442695040888963407).wrapping_add(3),
        seed ^ 0xdeadbeefcafe1234,
    ];
    let mut out = Vec::with_capacity(len);
    let mut block_idx = 0u64;
    while out.len() < len {
        let chunk_size = 1024.min(len - out.len());
        if block_idx % 4 == 3 && out.len() >= 1024 {
            // Repeat previous block for compressibility
            let start = out.len() - 1024;
            let repeated: Vec<u8> = out[start..start + chunk_size].to_vec();
            out.extend_from_slice(&repeated);
        } else {
            for _ in 0..chunk_size {
                // xoshiro256** next
                let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
                let t = s[1] << 17;
                s[2] ^= s[0];
                s[3] ^= s[1];
                s[1] ^= s[2];
                s[0] ^= s[3];
                s[2] ^= t;
                s[3] = s[3].rotate_left(45);
                out.push(result as u8);
            }
        }
        block_idx += 1;
    }
    out.truncate(len);
    out
}

/// Fill a file with PRNG data (realistic entropy for zstd benchmarks).
#[allow(dead_code)]
fn fill_file_prng(path: &std::path::Path, mib: usize, seed: u64) {
    let data = gen_prng_data(seed, mib * 1024 * 1024);
    fs::write(path, &data).expect("write");
}

/// Advise the kernel to drop a file's pages so the next bench run re-faults it.
fn evict_page_cache(path: &std::path::Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    unsafe {
        let fd = libc::open(c.as_ptr(), libc::O_RDONLY);
        if fd >= 0 {
            libc::posix_fadvise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
            libc::close(fd);
        }
    }
}

const COPY_MIB: u64 = 64;

/// IO throughput for the actual assembly hot paths.
fn bench_io_copy(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let src = dir.path().join("src.bin");
    fill_file(&src, COPY_MIB as usize, 0xAB);
    let dst = dir.path().join("dst.bin");
    let bytes = COPY_MIB * 1024 * 1024;

    let mut group = c.benchmark_group("io_copy");
    group.throughput(Throughput::Bytes(bytes));

    group.bench_function(BenchmarkId::new("copy_file_range", COPY_MIB), |b| {
        b.iter(|| {
            evict_page_cache(&src);
            let d = fs::File::create(&dst).unwrap();
            d.set_len(bytes).unwrap();
            sysio::copy_file_region_to(&src, 0, &d, 0, bytes).unwrap();
            d.sync_all().unwrap();
        });
    });

    // `pread` + `pwrite` + MD5 is the actual assembly hot path (`write_all_at`).
    group.bench_function(BenchmarkId::new("pread_pwrite_md5", COPY_MIB), |b| {
        b.iter(|| {
            evict_page_cache(&src);
            let s = fs::File::open(&src).unwrap();
            let d = fs::File::create(&dst).unwrap();
            d.set_len(bytes).unwrap();
            let mut buf = vec![0u8; 256 * 1024];
            let mut h = fast_md5::Md5::new();
            let mut off = 0u64;
            loop {
                let n = s.read_at(&mut buf, off).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                d.write_all_at(&buf[..n], off).unwrap();
                off += n as u64;
            }
            d.sync_all().unwrap();
            let _ = h.finalize();
        });
    });

    group.finish();
}

const MD5_MIB: u64 = 50;

/// MD5 throughput on N independent same-size messages: sequential fast_md5
/// (per-message scalar) vs the md5-simd crate's AVX2 multi-buffer `hash_many`,
/// one core each. Measures the ceiling of multi-buffer SIMD MD5 on this CPU.
/// Benchmark-only (not compiled into production paths).
#[cfg(feature = "bench-internals")]
fn bench_md5_multibuffer(c: &mut Criterion) {
    use md5_simd::{Md5Engine, digest as simd_digest};

    const MSG_MIB: usize = 64;
    const N_MSG: usize = 8;
    let msgs: Vec<Vec<u8>> = (0..N_MSG)
        .map(|i| gen_prng_data(0x5EED + i as u64, MSG_MIB * 1024 * 1024))
        .collect();
    let inputs: Vec<&[u8]> = msgs.iter().map(|m| m.as_slice()).collect();
    let total = (MSG_MIB * N_MSG) as u64 * 1024 * 1024;

    let mut group = c.benchmark_group("md5_multibuffer");
    group.throughput(Throughput::Bytes(total));

    group.bench_function("sequential_x8", |b| {
        b.iter(|| {
            for m in &msgs {
                let mut hasher = fast_md5::Md5::new();
                hasher.update(m);
                std::hint::black_box(hasher.finalize());
            }
        });
    });

    group.bench_function("hash_many_avx2", |b| {
        b.iter(|| {
            let mut engine = Md5Engine::new();
            let mut outputs = [[0u8; 16]; N_MSG];
            engine.hash_many(&inputs, &mut outputs);
            std::hint::black_box(outputs);
        });
    });

    // Correctness cross-check against fast_md5 (runs once per sample batch).
    let mut outputs = [[0u8; 16]; N_MSG];
    Md5Engine::new().hash_many(&inputs, &mut outputs);
    for (i, m) in msgs.iter().enumerate() {
        let expect = fast_md5::digest(m);
        assert_eq!(outputs[i], expect, "lane {i} digest mismatch");
        let _ = simd_digest; // keep import honest: also compare crate one-shot
        assert_eq!(simd_digest(&inputs[i]), expect, "lane {i} one-shot mismatch");
    }

    group.finish();
}

fn bench_md5_resident(c: &mut Criterion) {
    const SIZE: usize = 64 * 1024 * 1024;
    let data = gen_prng_data(0x5EED, SIZE);
    let mut group = c.benchmark_group("md5_resident");
    group.throughput(Throughput::Bytes(SIZE as u64));

    for block_size in [64 * 1024, 256 * 1024, 1024 * 1024, SIZE] {
        group.bench_function(BenchmarkId::from_parameter(block_size / 1024), |b| {
            b.iter(|| {
                let mut hasher = fast_md5::Md5::new();
                for block in data.chunks(block_size) {
                    hasher.update(block);
                }
                std::hint::black_box(hasher.finalize());
            });
        });
    }
    group.finish();
}

/// MD5 throughput of a 50 MiB file via the `md-5` crate (the verification hasher).
fn bench_md5(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("md5.bin");
    fill_file(&path, MD5_MIB as usize, 0xAB);
    let bytes = MD5_MIB * 1024 * 1024;

    let mut group = c.benchmark_group("md5");
    group.throughput(Throughput::Bytes(bytes));

    group.bench_function(BenchmarkId::new("crate", MD5_MIB), |b| {
        b.iter(|| {
            evict_page_cache(&path);
            let f = fs::File::open(&path).unwrap();
            let mut h = fast_md5::Md5::new();
            let mut buf = vec![0u8; 256 * 1024];
            let mut off = 0u64;
            loop {
                let n = f.read_at(&mut buf, off).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                off += n as u64;
            }
            let _ = hex::encode(h.finalize());
        });
    });

    group.finish();
}

const ZSTD_MIB: u64 = 8;

/// zstd decompression throughput with varying input (BufReader) buffer sizes
/// while keeping a fixed 1 MiB output buffer to match the production path in
/// `assembly_opt.rs` (`ASSEMBLY_BUFFER_SIZE` = 1 MiB output, `FILE_WRITE_BUFFER_SIZE`
/// = 256 KiB input).
fn bench_zstd_decompress(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let raw = gen_prng_data(0xBEEF, ZSTD_MIB as usize * 1024 * 1024);
    let comp_path = dir.path().join("chunk.zst");
    {
        let f = fs::File::create(&comp_path).unwrap();
        let mut enc = zstd::Encoder::new(f, 3).unwrap();
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        enc.finish().unwrap();
    }
    let out = dir.path().join("out.bin");
    let bytes = ZSTD_MIB * 1024 * 1024;
    const OUTPUT_BUF: usize = 1024 * 1024;

    let mut group = c.benchmark_group("zstd_decompress");
    group.throughput(Throughput::Bytes(bytes));

    for input_kib in [64usize, 128, 256, 512] {
        let input_bytes = input_kib * 1024;
        group.bench_function(BenchmarkId::new("input_kib", input_kib), |b| {
            b.iter(|| {
                evict_page_cache(&comp_path);
                let o = fs::File::create(&out).unwrap();
                o.set_len(raw.len() as u64).unwrap();
                let cf = fs::File::open(&comp_path).unwrap();
                let br = std::io::BufReader::with_capacity(input_bytes, cf);
                let mut dec = zstd::Decoder::new(br).unwrap();
                let mut buf = vec![0u8; OUTPUT_BUF];
                let mut woff = 0u64;
                loop {
                    let n = dec.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    o.write_all_at(&buf[..n], woff).unwrap();
                    woff += n as u64;
                }
                o.sync_all().unwrap();
            });
        });
    }
    group.finish();
}

/// lz4 and deflate decompression throughput with varying input (BufReader) buffer sizes.
/// Measures the hdiffpatch decompression path for lz4- and deflate-compressed clips.
fn bench_lz4_deflate_decompress(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let raw = gen_prng_data(0xBEEF, ZSTD_MIB as usize * 1024 * 1024);
    let bytes = ZSTD_MIB * 1024 * 1024;
    const OUTPUT_BUF: usize = 1024 * 1024;

    // Compress with lz4.
    let lz4_path = dir.path().join("chunk.lz4");
    {
        let f = fs::File::create(&lz4_path).unwrap();
        let mut enc = lz4::EncoderBuilder::new().level(3).build(f).unwrap();
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        let (_f, result) = enc.finish();
        result.unwrap();
    }

    // Compress with deflate (raw, no zlib header).
    let deflate_path = dir.path().join("chunk.deflate");
    {
        let f = fs::File::create(&deflate_path).unwrap();
        let mut enc = flate2::write::DeflateEncoder::new(f, flate2::Compression::default());
        std::io::Write::write_all(&mut enc, &raw).unwrap();
        enc.finish().unwrap();
    }

    let out = dir.path().join("out.bin");

    let mut group = c.benchmark_group("lz4_deflate_decompress");
    group.throughput(Throughput::Bytes(bytes));

    for input_kib in [64usize, 128, 256] {
        let input_bytes = input_kib * 1024;

        group.bench_function(BenchmarkId::new("lz4_input_kib", input_kib), |b| {
            b.iter(|| {
                evict_page_cache(&lz4_path);
                let o = fs::File::create(&out).unwrap();
                o.set_len(raw.len() as u64).unwrap();
                let cf = fs::File::open(&lz4_path).unwrap();
                let br = std::io::BufReader::with_capacity(input_bytes, cf);
                let mut dec = lz4::Decoder::new(br).unwrap();
                let mut buf = vec![0u8; OUTPUT_BUF];
                let mut woff = 0u64;
                loop {
                    let n = dec.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    o.write_all_at(&buf[..n], woff).unwrap();
                    woff += n as u64;
                }
                o.sync_all().unwrap();
            });
        });

        group.bench_function(BenchmarkId::new("deflate_input_kib", input_kib), |b| {
            b.iter(|| {
                evict_page_cache(&deflate_path);
                let o = fs::File::create(&out).unwrap();
                o.set_len(raw.len() as u64).unwrap();
                let cf = fs::File::open(&deflate_path).unwrap();
                let br = std::io::BufReader::with_capacity(input_bytes, cf);
                let mut dec = flate2::read::DeflateDecoder::new(br);
                let mut buf = vec![0u8; OUTPUT_BUF];
                let mut woff = 0u64;
                loop {
                    let n = dec.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    o.write_all_at(&buf[..n], woff).unwrap();
                    woff += n as u64;
                }
                o.sync_all().unwrap();
            });
        });
    }

    group.finish();
}

/// Build a single-file `CompactManifest` over `num_chunks` equal decompressed
/// chunks of `chunk_mib` MiB each, zstd-compressed on disk. When
/// `with_chunk_hashes` is true, each chunk's `chunk_decompressed_hash_md5` is
/// populated, enabling chunk-level verification and file-hash elision.
/// Uses PRNG data with realistic entropy for meaningful zstd timings.
fn build_assembly_fixture(
    chunks_dir: &std::path::Path,
    chunk_mib: usize,
    num_chunks: usize,
    with_chunk_hashes: bool,
) -> (CompactManifest, Vec<u8>) {
    let mut raw = Vec::with_capacity(chunk_mib * 1024 * 1024 * num_chunks);
    let chunks: Vec<SophonManifestAssetChunk> = (0..num_chunks)
        .map(|i| {
            let name = format!("ck{i:02}");
            let data = gen_prng_data(42 + i as u64, chunk_mib * 1024 * 1024);
            let comp = zstd::encode_all(&data[..], 3).unwrap();
            fs::write(chunks_dir.join(chunk_filename(&name)), &comp).unwrap();
            raw.extend_from_slice(&data);
            let offset = (i * chunk_mib * 1024 * 1024) as u64;
            let chunk_hash = if with_chunk_hashes {
                hex::encode(fast_md5::digest(&data))
            } else {
                String::new()
            };
            SophonManifestAssetChunk::from_parts(
                name.into(),
                chunk_hash.into(),
                offset,
                0,
                (chunk_mib * 1024 * 1024) as u64,
                0,
                bytes::Bytes::new(),
                -1,
            )
        })
        .collect();
    let file_md5 = hex::encode(fast_md5::digest(&raw));
    let file = SophonManifestAssetProperty::from_parts(
        "assembled.bin".into(),
        chunks,
        0,
        raw.len() as u64,
        file_md5.into(),
    );
    (CompactManifest::from(vec![file]), raw)
}

/// End-to-end `assemble_file` throughput: chunk reads, zstd decompress, pwrite,
/// file-level MD5, sync, and rename. This is what the user actually waits on.
fn bench_assembly_e2e(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let game_dir = dir.path().join("game");
    let chunks_dir = dir.path().join("chunks");
    let tmp_dir = dir.path().join("tmp");
    fs::create_dir_all(&game_dir).unwrap();
    fs::create_dir_all(&chunks_dir).unwrap();
    fs::create_dir_all(&tmp_dir).unwrap();

    let chunk_mib = 8;
    let num_chunks = 8;
    let total_bytes = (chunk_mib * num_chunks) as u64 * 1024 * 1024;

    let (manifest, raw) = build_assembly_fixture(&chunks_dir, chunk_mib, num_chunks, false);

    let name_refs: Vec<&str> = (0..num_chunks)
        .map(|i| Box::leak(format!("ck{i:02}").into_boxed_str()) as &str)
        .collect();
    let lookup = ChunkNameLookup::from_arena(StringArena::from(name_refs.as_slice()));
    let refcounts: Vec<AtomicU32> = (0..num_chunks).map(|_| AtomicU32::new(1000)).collect();
    let cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();

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
    .expect("warmup assemble");
    let assembled = fs::read(game_dir.join("assembled.bin")).unwrap();
    assert_eq!(assembled, raw, "assemble_file bytes must match raw input");

    let mut group = c.benchmark_group("assembly_e2e");
    group.throughput(Throughput::Bytes(total_bytes));
    group.bench_function(BenchmarkId::new("assemble_file", num_chunks), |b| {
        let game_dir = game_dir.clone();
        let tmp_dir = tmp_dir.clone();
        b.iter(|| {
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
            .expect("assemble");
        });
    });

    let (manifest_ch, raw_ch) = build_assembly_fixture(&chunks_dir, chunk_mib, num_chunks, true);
    let game_dir2 = dir.path().join("game_ch");
    let tmp_dir2 = dir.path().join("tmp_ch");
    fs::create_dir_all(&game_dir2).unwrap();
    fs::create_dir_all(&tmp_dir2).unwrap();
    assemble_file(
        &manifest_ch,
        0,
        &game_dir2,
        &chunks_dir,
        &tmp_dir2,
        &lookup,
        &refcounts,
        &cache,
        true,
        VerifyMode::Full,
    )
    .expect("warmup assemble (chunk hashes)");
    let assembled_ch = fs::read(game_dir2.join("assembled.bin")).unwrap();
    assert_eq!(
        assembled_ch, raw_ch,
        "assemble_file (chunk hashes) bytes must match raw input"
    );

    group.bench_function(
        BenchmarkId::new("assemble_file_chunk_verified", num_chunks),
        |b| {
            let game_dir2 = game_dir2.clone();
            let tmp_dir2 = tmp_dir2.clone();
            b.iter(|| {
                assemble_file(
                    &manifest_ch,
                    0,
                    &game_dir2,
                    &chunks_dir,
                    &tmp_dir2,
                    &lookup,
                    &refcounts,
                    &cache,
                    true,
                    VerifyMode::Full,
                )
                .expect("assemble");
            });
        },
    );

    let expected_hash = hex::encode(fast_md5::digest(&raw_ch));
    cache.clear();
    let _ = fs::remove_file(game_dir2.join("assembled.bin"));
    let fused_warmup = irmin_rs::game_installer::assembly::assemble_file_with_benchmark_file_hash(
        &manifest_ch,
        0,
        &game_dir2,
        &chunks_dir,
        &tmp_dir2,
        &lookup,
        &refcounts,
        &cache,
    )
    .expect("warmup fused DownloadOver output");
    assert!(fused_warmup);
    assert_eq!(
        hex::encode(
            irmin_rs::game_installer::cache::file_md5_digest(&game_dir2.join("assembled.bin"))
                .expect("verify warmup fused DownloadOver output")
        ),
        expected_hash
    );
    group.bench_function(
        BenchmarkId::new(
            "download_over_current_chunk_verify_plus_file_hash",
            num_chunks,
        ),
        |b| {
            let game_dir = game_dir2.clone();
            let tmp_dir = tmp_dir2.clone();
            b.iter_batched(
                || {
                    cache.clear();
                    let _ = fs::remove_file(game_dir.join("assembled.bin"));
                },
                |_| {
                    assemble_file(
                        &manifest_ch,
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
                    .expect("assemble DownloadOver output");
                    let actual = irmin_rs::game_installer::cache::file_md5_digest(
                        &game_dir.join("assembled.bin"),
                    )
                    .expect("verify assembled DownloadOver output");
                    assert_eq!(hex::encode(actual), expected_hash);
                },
                BatchSize::PerIteration,
            );
        },
    );
    group.bench_function(
        BenchmarkId::new("download_over_fused_chunk_and_file_hash", num_chunks),
        |b| {
            let game_dir = game_dir2.clone();
            let tmp_dir = tmp_dir2.clone();
            b.iter_batched(
                || {
                    cache.clear();
                    let _ = fs::remove_file(game_dir.join("assembled.bin"));
                },
                |_| {
                    let file_hash_verified =
                        irmin_rs::game_installer::assembly::assemble_file_with_benchmark_file_hash(
                            &manifest_ch,
                            0,
                            &game_dir,
                            &chunks_dir,
                            &tmp_dir,
                            &lookup,
                            &refcounts,
                            &cache,
                        )
                        .expect("assemble fused DownloadOver output");
                    assert!(file_hash_verified);
                },
                BatchSize::PerIteration,
            );
        },
    );
    group.finish();
}


/// Assembly with many small chunks — exercises the worker loop's per-chunk
/// accessor calls, hash lookups, and sync_and_evict_range overhead. Uses
/// 1000 chunks of 64 KiB to amplify per-chunk costs relative to I/O.
fn bench_assembly_many_chunks(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let game_dir = dir.path().join("game");
    let chunks_dir = dir.path().join("chunks");
    let tmp_dir = dir.path().join("tmp");
    fs::create_dir_all(&game_dir).unwrap();
    fs::create_dir_all(&chunks_dir).unwrap();
    fs::create_dir_all(&tmp_dir).unwrap();

    let chunk_mib = 1;
    let num_chunks = 100;
    let total_bytes = (chunk_mib * 1024 * 1024 * num_chunks) as u64;

    let (manifest, raw) = build_assembly_fixture(&chunks_dir, chunk_mib, num_chunks, false);

    let name_refs: Vec<&str> = (0..num_chunks)
        .map(|i| Box::leak(format!("ck{i:02}").into_boxed_str()) as &str)
        .collect();
    let lookup = ChunkNameLookup::from_arena(StringArena::from(name_refs.as_slice()));
    let refcounts: Vec<AtomicU32> = (0..num_chunks).map(|_| AtomicU32::new(1000)).collect();
    let cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();

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
    .expect("warmup assemble");
    let assembled = fs::read(game_dir.join("assembled.bin")).unwrap();
    assert_eq!(assembled, raw, "assemble_file bytes must match raw input");

    let mut group = c.benchmark_group("assembly_many_chunks");
    group.throughput(Throughput::Bytes(total_bytes));
    group.bench_function(BenchmarkId::from_parameter(num_chunks), |b| {
        b.iter(|| {
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
            .expect("assemble");
        });
    });
    group.finish();
}


/// Many-small-files fixture: `files` assets, each `chunks_per_file` zstd
/// chunks of `chunk_kib` KiB — the shape where per-file durability barriers
/// dominate assembly wall time. Returns the manifest plus file 0's raw bytes
/// for a post-assembly content assert.
fn build_small_files_fixture(
    chunks_dir: &Path,
    files: usize,
    chunks_per_file: usize,
    chunk_kib: usize,
) -> (CompactManifest, Vec<u8>) {
    let chunk_bytes = chunk_kib * 1024;
    let mut assets = Vec::with_capacity(files);
    let mut raw0 = Vec::new();
    for fi in 0..files {
        let mut raw = Vec::with_capacity(chunks_per_file * chunk_bytes);
        let chunks = (0..chunks_per_file)
            .map(|ci| {
                let name = format!("sfc{fi:04}_{ci}");
                let data = gen_prng_data((fi * 16 + ci) as u64, chunk_bytes);
                let comp = zstd::encode_all(&data[..], 3).unwrap();
                fs::write(chunks_dir.join(chunk_filename(&name)), &comp).unwrap();
                let hash = hex::encode(fast_md5::digest(&data));
                let offset = (ci * chunk_bytes) as u64;
                raw.extend_from_slice(&data);
                SophonManifestAssetChunk::from_parts(
                    name.into(),
                    hash.into(),
                    offset,
                    0,
                    chunk_bytes as u64,
                    0,
                    bytes::Bytes::new(),
                    -1,
                )
            })
            .collect();
        let file_md5 = hex::encode(fast_md5::digest(&raw));
        if fi == 0 {
            raw0 = raw;
        }
        assets.push(SophonManifestAssetProperty::from_parts(
            format!("sfa{fi:04}.bin").into(),
            chunks,
            0,
            (chunks_per_file * chunk_bytes) as u64,
            file_md5.into(),
        ));
    }
    (CompactManifest::from(assets), raw0)
}

/// Entire-batch durability barriers for small-file assembly, at the
/// coordinator's four-strand concurrency. `per_file_sync` is the classic
/// tail (one fdatasync + rename per file inside the worker); `staged_epochs`
/// defers flush, rename, and chunk release to one epoch commit per 64 files
/// (or 256 MiB) plus a final commit — the production install topology.
fn bench_assembly_epoch_durability(c: &mut Criterion) {
    const FILES: usize = 128;
    const CHUNKS_PER_FILE: usize = 4;
    const CHUNK_KIB: usize = 64;
    const STRANDS: usize = 4;
    let chunk_bytes = CHUNK_KIB * 1024;
    let total_bytes = (FILES * CHUNKS_PER_FILE * chunk_bytes) as u64;

    let dir = tempfile::tempdir().expect("tempdir");
    let game_dir = dir.path().join("game");
    let chunks_dir = dir.path().join("chunks");
    let tmp_dir = dir.path().join("tmp");
    fs::create_dir_all(&game_dir).unwrap();
    fs::create_dir_all(&chunks_dir).unwrap();
    fs::create_dir_all(&tmp_dir).unwrap();

    let (manifest_raw, raw0) =
        build_small_files_fixture(&chunks_dir, FILES, CHUNKS_PER_FILE, CHUNK_KIB);
    let manifest = Arc::new(manifest_raw);
    let names: Vec<Box<str>> = (0..FILES)
        .flat_map(|fi| (0..CHUNKS_PER_FILE).map(move |ci| format!("sfc{fi:04}_{ci}")))
        .map(|n| n.into_boxed_str())
        .collect();
    let name_refs: Vec<&str> = names.iter().map(|n| n.as_ref()).collect();
    let lookup = Arc::new(ChunkNameLookup::from_arena(StringArena::from(name_refs.as_slice())));
    let shared_refcounts: Arc<Vec<AtomicU32>> = Arc::new(
        (0..FILES * CHUNKS_PER_FILE)
            .map(|_| AtomicU32::new(1000))
            .collect(),
    );
    let cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();
    let epochs = Arc::new(AssemblyEpochs::new(
        Arc::clone(&lookup),
        Arc::clone(&shared_refcounts),
        Arc::new(chunks_dir.clone()),
        EPOCH_MAX_FILES,
        EPOCH_MAX_BYTES,
    ));

    // Warmup + correctness gate for the staged path: assemble everything
    // once, commit, and confirm bytes and completeness.
    {
        std::thread::scope(|sc| {
            for w in 0..STRANDS {
                let manifest = &manifest;
                let epochs = &epochs;
                let game_dir = &game_dir;
                let chunks_dir = &chunks_dir;
                let tmp_dir = &tmp_dir;
                let lookup = &lookup;
                let shared_refcounts = &shared_refcounts;
                let cache = &cache;
                sc.spawn(move || {
                    for fi in (w..FILES).step_by(STRANDS) {
                        assemble_file_with_benchmark_epochs(
                            manifest,
                            fi,
                            epochs,
                            game_dir,
                            chunks_dir,
                            tmp_dir,
                            lookup,
                            shared_refcounts,
                            cache,
                        )
                        .expect("warmup staged assemble");
                    }
                });
            }
        });
        epochs.commit().expect("warmup final commit");
        for fi in 0..FILES {
            let path = game_dir.join(format!("sfa{fi:04}.bin"));
            assert!(path.exists(), "staged file {fi} missing after final commit");
        }
        assert_eq!(
            fs::read(game_dir.join("sfa0000.bin")).unwrap(),
            raw0,
            "staged assembly bytes must match raw input"
        );
    }

    let mut group = c.benchmark_group("assembly_epoch_durability");
    group.throughput(Throughput::Bytes(total_bytes));
    group.sample_size(10);
    group.measurement_time(std::time::Duration::from_secs(20));

    group.bench_function("per_file_sync", |b| {
        b.iter(|| {
            std::thread::scope(|sc| {
                for w in 0..STRANDS {
                    let manifest = &manifest;
                    let game_dir = &game_dir;
                    let chunks_dir = &chunks_dir;
                    let tmp_dir = &tmp_dir;
                    let lookup = &lookup;
                    let shared_refcounts = &shared_refcounts;
                    let cache = &cache;
                    sc.spawn(move || {
                        for fi in (w..FILES).step_by(STRANDS) {
                            assemble_file(
                                manifest,
                                fi,
                                game_dir,
                                chunks_dir,
                                tmp_dir,
                                lookup,
                                shared_refcounts,
                                cache,
                                true,
                                VerifyMode::Full,
                            )
                            .expect("assemble");
                        }
                    });
                }
            });
        });
    });

    group.bench_function("staged_epochs", |b| {
        b.iter(|| {
            std::thread::scope(|sc| {
                for w in 0..STRANDS {
                    let manifest = &manifest;
                    let epochs = &epochs;
                    let game_dir = &game_dir;
                    let chunks_dir = &chunks_dir;
                    let tmp_dir = &tmp_dir;
                    let lookup = &lookup;
                    let shared_refcounts = &shared_refcounts;
                    let cache = &cache;
                    sc.spawn(move || {
                        for fi in (w..FILES).step_by(STRANDS) {
                            assemble_file_with_benchmark_epochs(
                                manifest,
                                fi,
                                epochs,
                                game_dir,
                                chunks_dir,
                                tmp_dir,
                                lookup,
                                shared_refcounts,
                                cache,
                            )
                            .expect("assemble");
                        }
                    });
                }
            });
            epochs.commit().expect("final commit");
        });
    });
    group.finish();
}
fn bench_assembly_old_chunk_reuse(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let game_dir = dir.path().join("game");
    let chunks_dir = dir.path().join("chunks");
    let tmp_dir = dir.path().join("tmp");
    fs::create_dir_all(&game_dir).unwrap();
    fs::create_dir_all(&chunks_dir).unwrap();
    fs::create_dir_all(&tmp_dir).unwrap();

    let chunk_mib = 8;
    let num_chunks = 8;
    let total_bytes = (chunk_mib * num_chunks) as u64 * 1024 * 1024;
    let (_, raw) = build_assembly_fixture(&chunks_dir, chunk_mib, num_chunks, true);
    let expected_output: Vec<u8> = (0..num_chunks)
        .flat_map(|index| {
            let source_index = (index * 3) % num_chunks;
            let start = source_index * chunk_mib * 1024 * 1024;
            let end = start + chunk_mib * 1024 * 1024;
            raw[start..end].iter().copied()
        })
        .collect();
    let file_md5 = hex::encode(fast_md5::digest(&expected_output));
    let asset_name = format!("reused-{}.bin", std::process::id());
    let reuse_manifest = CompactManifest::from(vec![SophonManifestAssetProperty::from_parts(
        asset_name.clone().into(),
        (0..num_chunks)
            .map(|index| {
                let source_index = (index * 3) % num_chunks;
                let source_start = source_index * chunk_mib * 1024 * 1024;
                let source_end = source_start + chunk_mib * 1024 * 1024;
                SophonManifestAssetChunk::from_parts(
                    format!("ck{index:02}").into(),
                    bytes::Bytes::new(),
                    (index * chunk_mib * 1024 * 1024) as u64,
                    0,
                    (source_end - source_start) as u64,
                    0,
                    bytes::Bytes::new(),
                    source_start as i64,
                )
            })
            .collect(),
        0,
        total_bytes,
        file_md5.clone().into(),
    )]);
    for index in 0..num_chunks {
        let chunk = reuse_manifest.chunk(index);
        let expected_source_offset = ((index * 3) % num_chunks * chunk_mib * 1024 * 1024) as i64;
        let expected_destination_offset = (index * chunk_mib * 1024 * 1024) as u64;
        assert_eq!(chunk.chunk_old_offset, expected_source_offset);
        assert_eq!(
            reuse_manifest.file_chunk_offset(0, index),
            expected_destination_offset
        );
        assert!(chunk.chunk_decompressed_hash_md5.is_empty());
    }
    let names: Vec<String> = (0..num_chunks).map(|i| format!("ck{i:02}")).collect();
    let name_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let lookup = ChunkNameLookup::from_arena(StringArena::from(name_refs.as_slice()));
    let refcounts: Vec<AtomicU32> = (0..num_chunks).map(|_| AtomicU32::new(1000)).collect();
    let cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();
    let old_data = raw.clone();
    let make_fixture = || {
        let fixture = tempfile::tempdir_in(dir.path()).expect("fixture tempdir");
        let game_dir = fixture.path().join("game");
        let tmp_dir = fixture.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&tmp_dir).unwrap();
        let target = game_dir.join(&asset_name);
        let mut old_file = fs::File::create(&target).unwrap();
        old_file.write_all(&old_data).unwrap();
        old_file.sync_all().unwrap();
        drop(old_file);
        for chunk_idx in reuse_manifest.file_chunk_range(0) {
            let chunk = reuse_manifest.chunk(chunk_idx as usize);
            let start = chunk.chunk_old_offset as usize;
            let end = start + chunk.chunk_size_decompressed as usize;
            assert_eq!(
                end - start,
                chunk_mib * 1024 * 1024,
                "old source length mismatch for chunk {chunk_idx}"
            );
            assert_eq!(
                start,
                ((chunk_idx as usize * 3) % num_chunks) * chunk_mib * 1024 * 1024,
                "old source offset mismatch for chunk {chunk_idx}"
            );
        }
        (fixture, game_dir, tmp_dir, target)
    };
    let verify_output = |target: &Path| {
        let output = fs::read(target).expect("read assembled file");
        assert_eq!(
            output.len(),
            expected_output.len(),
            "reused output size mismatch"
        );
        for (index, chunk) in output.chunks(chunk_mib * 1024 * 1024).enumerate() {
            let manifest_chunk = reuse_manifest.chunk(index);
            let source_start = manifest_chunk.chunk_old_offset as usize;
            let source_end = source_start + manifest_chunk.chunk_size_decompressed as usize;
            assert_eq!(
                fast_md5::digest(chunk),
                fast_md5::digest(&old_data[source_start..source_end]),
                "reused output chunk digest mismatch at index {index}"
            );
            assert_eq!(chunk, &old_data[source_start..source_end]);
        }
        assert_eq!(hex::encode(fast_md5::digest(&output)), file_md5);
    };
    let run_unhashed_assembly = |game_dir: &Path, tmp_dir: &Path| {
        assemble_file(
            &reuse_manifest,
            0,
            game_dir,
            &chunks_dir,
            tmp_dir,
            &lookup,
            &refcounts,
            &cache,
            true,
            VerifyMode::None,
        )
        .expect("assemble reused chunks");
    };
    let run_hashed_assembly = |game_dir: &Path, tmp_dir: &Path| {
        assert!(
            irmin_rs::game_installer::assembly::assemble_file_with_benchmark_file_hash(
                &reuse_manifest,
                0,
                game_dir,
                &chunks_dir,
                tmp_dir,
                &lookup,
                &refcounts,
                &cache,
            )
            .expect("assemble reused chunks with file hash")
        );
    };

    let mut group = c.benchmark_group("assembly_old_chunk_reuse");
    group.throughput(Throughput::Bytes(total_bytes));
    let bench_reuse = |b: &mut criterion::Bencher<'_>, run: &dyn Fn(&Path, &Path)| {
        b.iter_custom(|iterations| {
            let mut elapsed = std::time::Duration::ZERO;
            for _ in 0..iterations {
                let (fixture, game_dir, tmp_dir, target) = make_fixture();
                let started = std::time::Instant::now();
                run(&game_dir, &tmp_dir);
                elapsed += started.elapsed();
                verify_output(&target);
                drop(fixture);
            }
            elapsed
        });
    };
    group.bench_function(BenchmarkId::new("kernel_copy_no_hash", num_chunks), |b| {
        bench_reuse(b, &run_unhashed_assembly);
    });
    group.bench_function(BenchmarkId::new("file_hash_end_to_end", num_chunks), |b| {
        bench_reuse(b, &run_hashed_assembly)
    });
    group.finish();
}

const VERIFY_MIB: u64 = 50;

/// `check_file_md5_cached` cache-miss path: full read + MD5 of a 50 MiB file.
/// Also benchmarks cache-hit path (metadata lookup only, no file read).
fn bench_verify_file_md5(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let game_dir = dir.path().to_path_buf();
    let path = game_dir.join("verify.bin");
    fill_file(&path, VERIFY_MIB as usize, 0xAB);
    let md5 = hex::encode(fast_md5::digest(&fs::read(&path).unwrap()));
    let bytes = VERIFY_MIB * 1024 * 1024;
    let cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();

    let mut group = c.benchmark_group("verify_file_md5");
    group.throughput(Throughput::Bytes(bytes));
    group.bench_function("miss", |b| {
        let game_dir = game_dir.clone();
        b.iter(|| {
            cache.clear();
            evict_page_cache(&path);
            let valid = irmin_rs::game_installer::cache::check_file_md5_cached(
                &path,
                bytes,
                irmin_rs::game_installer::assembly_opt::hex_to_bytes(&md5)
                    .unwrap_or_default()
                    .as_slice(),
                &game_dir,
                &cache,
            )
            .unwrap();
            assert!(valid);
            assert!(cache.contains_key("verify.bin"));
        });
    });

    group.bench_function("warm_start_miss_setup_excluded", |b| {
        let game_dir = game_dir.clone();
        b.iter_batched(
            || {
                cache.clear();
                let mut file = fs::File::open(&path).unwrap();
                std::io::copy(&mut file, &mut std::io::sink()).unwrap();
                cache.clear();
            },
            |_| {
                let valid = irmin_rs::game_installer::cache::check_file_md5_cached(
                    &path,
                    bytes,
                    irmin_rs::game_installer::assembly_opt::hex_to_bytes(&md5)
                        .unwrap_or_default()
                        .as_slice(),
                    &game_dir,
                    &cache,
                )
                .unwrap();
                assert!(valid);
                assert!(cache.contains_key("verify.bin"));
            },
            BatchSize::PerIteration,
        );
    });

    group.bench_function("eviction_advised_miss_setup_excluded", |b| {
        let game_dir = game_dir.clone();
        b.iter_batched(
            || {
                cache.clear();
                evict_page_cache(&path);
            },
            |_| {
                let valid = irmin_rs::game_installer::cache::check_file_md5_cached(
                    &path,
                    bytes,
                    irmin_rs::game_installer::assembly_opt::hex_to_bytes(&md5)
                        .unwrap_or_default()
                        .as_slice(),
                    &game_dir,
                    &cache,
                )
                .unwrap();
                assert!(valid);
                assert!(cache.contains_key("verify.bin"));
            },
            BatchSize::PerIteration,
        );
    });

    // Populate cache for hit benchmark
    {
        cache.clear();
        let result = irmin_rs::game_installer::cache::check_file_md5_cached(
            &path,
            bytes,
            irmin_rs::game_installer::assembly_opt::hex_to_bytes(&md5)
                .unwrap_or_default()
                .as_slice(),
            &game_dir,
            &cache,
        )
        .unwrap();
        assert!(result, "cache population should succeed");
    }

    group.bench_function("hit", |b| {
        let game_dir = game_dir.clone();
        b.iter(|| {
            let result = irmin_rs::game_installer::cache::check_file_md5_cached(
                &path,
                bytes,
                irmin_rs::game_installer::assembly_opt::hex_to_bytes(&md5)
                    .unwrap_or_default()
                    .as_slice(),
                &game_dir,
                &cache,
            )
            .unwrap();
            assert!(result);
        });
    });

    group.finish();

    const SMALL_VERIFY_MIB: usize = 1;
    let small_path = game_dir.join("verify-small.bin");
    fill_file(&small_path, SMALL_VERIFY_MIB, 0x5A);
    let small_md5 = hex::encode(fast_md5::digest(&fs::read(&small_path).unwrap()));
    let small_bytes = (SMALL_VERIFY_MIB * 1024 * 1024) as u64;
    let small_cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();
    let mut small_group = c.benchmark_group("verify_file_md5_small");
    small_group.throughput(Throughput::Bytes(small_bytes));
    small_group.bench_function("eviction_advised_miss_setup_excluded", |b| {
        let game_dir = game_dir.clone();
        b.iter_batched(
            || {
                small_cache.clear();
                evict_page_cache(&small_path);
            },
            |_| {
                let valid = irmin_rs::game_installer::cache::check_file_md5_cached(
                    &small_path,
                    small_bytes,
                    irmin_rs::game_installer::assembly_opt::hex_to_bytes(&small_md5)
                        .unwrap_or_default()
                        .as_slice(),
                    &game_dir,
                    &small_cache,
                )
                .unwrap();
                assert!(valid);
                assert!(small_cache.contains_key("verify-small.bin"));
            },
            BatchSize::PerIteration,
        );
    });
    small_group.finish();

    // Many tiny files: isolates the remaining per-file cold-verify costs
    // (open, metadata, cache lookup, hash setup) from large sequential reads.
    let tiny_dir = game_dir.join("tiny");
    fs::create_dir_all(&tiny_dir).unwrap();
    let tiny_files: Vec<(std::path::PathBuf, String)> = (0..1024usize)
        .map(|index| {
            let path = tiny_dir.join(format!("tiny-{index:03}.bin"));
            let data = vec![index as u8; 1024];
            fs::write(&path, &data).unwrap();
            (path, hex::encode(fast_md5::digest(&data)))
        })
        .collect();
    let tiny_bytes: u64 = 1024;
    let mut tiny_group = c.benchmark_group("verify_file_md5_tiny");
    tiny_group.throughput(Throughput::Bytes(tiny_bytes * tiny_files.len() as u64));
    tiny_group.bench_function(BenchmarkId::new("miss", tiny_files.len()), |b| {
        let game_dir = game_dir.clone();
        let tiny_files = &tiny_files;
        let tiny_cache: VerificationCache<String, VerificationEntry> = VerificationCache::new();
        b.iter(|| {
            tiny_cache.clear();
            for (path, md5) in tiny_files {
                let valid = irmin_rs::game_installer::cache::check_file_md5_cached(
                    path,
                    tiny_bytes,
                    irmin_rs::game_installer::assembly_opt::hex_to_bytes(&md5)
                        .unwrap_or_default()
                        .as_slice(),
                    &game_dir,
                    &tiny_cache,
                )
                .unwrap();
                assert!(valid);
            }
        });
    });
    // Same files, but only open + read + hash: excludes cache bookkeeping so the
    // read-buffer allocation is the dominant per-file variable.
    tiny_group.bench_function(BenchmarkId::new("digest_only", tiny_files.len()), |b| {
        let tiny_files = &tiny_files;
        b.iter(|| {
            for (path, md5) in tiny_files {
                let digest = irmin_rs::game_installer::cache::file_md5_digest(path).unwrap();
                assert_eq!(hex::encode(digest), *md5);
            }
        });
    });
    tiny_group.finish();
}

#[cfg(feature = "bench-internals")]
fn bench_file_md5_buffer_capacity(c: &mut Criterion) {
    use irmin_rs::game_installer::cache::benchmark::file_md5_digest_with_options;

    const FILE_MIB: usize = 50;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("md5-buffer-sweep.bin");
    fill_file_prng(&path, FILE_MIB, 0xD1CE_BA5E);
    let expected = fast_md5::digest(&fs::read(&path).expect("read fixture"));
    let bytes = (FILE_MIB * 1024 * 1024) as u64;

    let mut group = c.benchmark_group("file_md5_buffer_capacity");
    group.throughput(Throughput::Bytes(bytes));
    for advise_page_cache in [false, true] {
        let advice_label = if advise_page_cache {
            "with_fadvise"
        } else {
            "no_fadvise"
        };
        for capacity in [64, 128, 256, 512, 1024, 4096].map(|kib| kib * 1024) {
            let digest = file_md5_digest_with_options(&path, capacity, advise_page_cache)
                .expect("validate digest capacity");
            assert_eq!(digest, expected, "capacity={capacity}");
            group.bench_function(BenchmarkId::new(advice_label, capacity / 1024), |b| {
                b.iter(|| {
                    let digest = file_md5_digest_with_options(&path, capacity, advise_page_cache)
                        .expect("hash with selected reader capacity");
                    std::hint::black_box(digest);
                });
            });
        }
    }
    group.finish();
}

#[cfg(feature = "bench-internals")]
// The fixture is read during setup, so digest cases exercise warm page-cache data.
fn bench_file_metadata_cost(c: &mut Criterion) {
    use irmin_rs::game_installer::cache::benchmark::{
        file_md5_digest_with_file, file_md5_digest_with_options,
    };

    const FILE_MIB: usize = 1;
    const READ_BUFFER_BYTES: usize = 256 * 1024;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("metadata-cost.bin");
    fill_file_prng(&path, FILE_MIB, 0xF11E_0F57);
    let expected = fast_md5::digest(&fs::read(&path).expect("read fixture"));
    let bytes = (FILE_MIB * 1024 * 1024) as u64;

    assert_eq!(fs::metadata(&path).expect("path metadata").len(), bytes);
    {
        let file = fs::File::open(&path).expect("open fixture");
        assert_eq!(file.metadata().expect("file metadata").len(), bytes);
    }
    assert_eq!(
        file_md5_digest_with_options(&path, READ_BUFFER_BYTES, false).expect("path digest"),
        expected
    );
    assert_eq!(
        file_md5_digest_with_file(
            fs::File::open(&path).expect("open digest fixture"),
            READ_BUFFER_BYTES,
            false,
        )
        .expect("open-file digest"),
        expected
    );
    let file = fs::File::open(&path).expect("open metadata fixture");

    let mut group = c.benchmark_group("file_metadata_cost");
    group.sample_size(100);
    group.bench_function("path_metadata", |b| {
        b.iter(|| {
            let metadata = fs::metadata(&path).expect("path metadata");
            std::hint::black_box(metadata.len());
        });
    });
    group.bench_function("open_then_file_metadata", |b| {
        b.iter(|| {
            let file = fs::File::open(&path).expect("open fixture");
            let metadata = file.metadata().expect("file metadata");
            std::hint::black_box(metadata.len());
        });
    });
    group.bench_function("open_close", |b| {
        b.iter(|| {
            let file = fs::File::open(&path).expect("open fixture");
            std::hint::black_box(file);
        });
    });
    group.bench_function("already_open_file_metadata", |b| {
        b.iter(|| {
            let metadata = file.metadata().expect("file metadata");
            std::hint::black_box(metadata.len());
        });
    });
    group.bench_function("path_digest_1mib", |b| {
        b.iter(|| {
            let digest =
                file_md5_digest_with_options(&path, READ_BUFFER_BYTES, false).expect("path digest");
            std::hint::black_box(digest);
        });
    });
    group.bench_function("open_excluded_digest_1mib_close_timed", |b| {
        b.iter_batched(
            || fs::File::open(&path).expect("open digest fixture"),
            |file| {
                let digest = file_md5_digest_with_file(file, READ_BUFFER_BYTES, false)
                    .expect("open-file digest");
                std::hint::black_box(digest);
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

const HASH_MIB: u64 = 128;

/// XXH64 throughput on a 128 MiB file via pread. This is the compressed-chunk
/// hash verification path used during downloads.
fn bench_xxh64(c: &mut Criterion) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("hash.bin");
    fill_file(&path, HASH_MIB as usize, 0xCD);
    let bytes = HASH_MIB * 1024 * 1024;

    let mut group = c.benchmark_group("xxh64_throughput");
    group.throughput(Throughput::Bytes(bytes));

    group.bench_function("128m", |b| {
        b.iter(|| {
            evict_page_cache(&path);
            let f = fs::File::open(&path).unwrap();
            let mut h = xxhash_rust::xxh64::Xxh64::new(0);
            let mut buf = vec![0u8; 256 * 1024];
            let mut off = 0u64;
            loop {
                let n = f.read_at(&mut buf, off).unwrap();
                if n == 0 {
                    break;
                }
                h.update(&buf[..n]);
                off += n as u64;
            }
            let _ = h.digest();
        });
    });

    group.finish();
}

const ONESHOT_MIB: u64 = 8;

/// One-shot zstd decompression (mmap input + single ZSTD_decompressDCtx) vs
/// streaming decompression (BufReader + Decoder). The one-shot path is what
/// `decompress_chunk_oneshot` uses for chunks <= 8 MiB.
/// Production oneshot chunk-decompression path: per-chunk open + pread +
/// output mmap + madvise + one-shot decode + sync_file_range + DONTNEED +
/// munmap. This is what assembly workers execute per downloaded chunk;
/// unlike zstd_oneshot, it includes all syscall and page-cache costs.
fn bench_chunk_decompress_production(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly_opt::bench_internals::decompress_chunk_optimized;

    const CHUNK_MIB: usize = 8;
    const NUM_CHUNKS: usize = 8;

    let dir = tempfile::tempdir().expect("tempdir");
    let chunks_dir = dir.path().join("chunks");
    fs::create_dir_all(&chunks_dir).unwrap();
    let out_path = dir.path().join("out.bin");

    let mut chunk_paths = Vec::with_capacity(NUM_CHUNKS);
    for i in 0..NUM_CHUNKS {
        let raw = gen_prng_data(0xBEEF + i as u64, CHUNK_MIB * 1024 * 1024);
        let comp = zstd::encode_all(&raw[..], 3).unwrap();
        let p = chunks_dir.join(format!("c{i}.zstd"));
        fs::write(&p, &comp).unwrap();
        chunk_paths.push(p);
    }

    let out_file = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .read(true)
        .truncate(true)
        .open(&out_path)
        .unwrap();
    out_file
        .set_len((CHUNK_MIB * 1024 * 1024 * NUM_CHUNKS) as u64)
        .unwrap();

    let bytes = (CHUNK_MIB * NUM_CHUNKS) as u64 * 1024 * 1024;
    let mut group = c.benchmark_group("chunk_decompress_production");
    group.throughput(Throughput::Bytes(bytes));
    group.bench_function(BenchmarkId::from_parameter(NUM_CHUNKS), |b| {
        b.iter(|| {
            let mut hasher = irmin_rs::game_installer::assembly_opt::Md5::new().unwrap();
            for (i, path) in chunk_paths.iter().enumerate() {
                let offset = (i * CHUNK_MIB * 1024 * 1024) as u64;
                let written = decompress_chunk_optimized(
                    path,
                    &out_file,
                    offset,
                    (CHUNK_MIB * 1024 * 1024) as u64,
                    Some(&mut hasher),
                    b"",
                    false,
                )
                .expect("decompress");
                assert_eq!(written, (CHUNK_MIB * 1024 * 1024) as u64);
            }
            let digest = hasher.finish().unwrap();
            criterion::black_box(digest);
        });
    });
    group.finish();
}

fn bench_zstd_oneshot(c: &mut Criterion) {    let dir = tempfile::tempdir().expect("tempdir");
    let raw = gen_prng_data(0xCAFE, ONESHOT_MIB as usize * 1024 * 1024);
    let comp = zstd::encode_all(&raw[..], 3).unwrap();
    let comp_path = dir.path().join("oneshot.zst");
    fs::write(&comp_path, &comp).unwrap();
    let out = dir.path().join("out.bin");
    let bytes = ONESHOT_MIB * 1024 * 1024;

    let mut group = c.benchmark_group("zstd_oneshot");
    group.throughput(Throughput::Bytes(bytes));

    group.bench_function("streaming", |b| {
        b.iter(|| {
            evict_page_cache(&comp_path);
            let o = fs::File::create(&out).unwrap();
            o.set_len(raw.len() as u64).unwrap();
            let cf = fs::File::open(&comp_path).unwrap();
            let br = std::io::BufReader::with_capacity(256 * 1024, cf);
            let mut dec = zstd::Decoder::new(br).unwrap();
            let mut buf = vec![0u8; 256 * 1024];
            let mut woff = 0u64;
            loop {
                let n = dec.read(&mut buf).unwrap();
                if n == 0 {
                    break;
                }
                o.write_all_at(&buf[..n], woff).unwrap();
                woff += n as u64;
            }
            o.sync_all().unwrap();
        });
    });

    group.bench_function("oneshot_heap", |b| {
        b.iter(|| {
            let comp_data = fs::read(&comp_path).unwrap();
            let mut out_buf = vec![0u8; raw.len()];
            let mut dec = zstd::Decoder::new(comp_data.as_slice()).unwrap();
            std::io::Read::read_to_end(&mut dec, &mut out_buf).unwrap();
        });
    });

    group.finish();
}

const MANIFEST_FILES: usize = 5000;
const MANIFEST_CHUNKS_PER_FILE: usize = 50;

/// CompactManifest build from a large synthetic manifest. Measures the arena
/// interning and column vec construction cost.
fn bench_compact_manifest_build(c: &mut Criterion) {
    let hash_pool: Vec<String> = (0..MANIFEST_FILES).map(|i| format!("{i:032x}")).collect();
    let assets: Vec<SophonManifestAssetProperty> = (0..MANIFEST_FILES)
        .map(|f| {
            let chunks: Vec<SophonManifestAssetChunk> = (0..MANIFEST_CHUNKS_PER_FILE)
                .map(|c| {
                    let h = &hash_pool[(f + c) % MANIFEST_FILES];
                    SophonManifestAssetChunk::from_parts(
                        format!("f{f}c{c}").into(),
                        h.clone().into(),
                        (c * 1024 * 1024) as u64,
                        0,
                        0,
                        0,
                        bytes::Bytes::new(),
                        -1,
                    )
                })
                .collect();
            let size = chunks.iter().map(|c| c.chunk_size_decompressed).sum();
            SophonManifestAssetProperty::from_parts(
                format!("asset_{f}.pak").into(),
                chunks,
                0,
                size,
                format!("{f:032x}").into(),
            )
        })
        .collect();

    let mut group = c.benchmark_group("compact_manifest");
    group.throughput(Throughput::Elements(MANIFEST_FILES as u64));

    // Measure the clone cost separately so the build-only timing excludes it.
    group.bench_function("clone_5000x50", |b| {
        b.iter(|| {
            let assets = assets.clone();
            std::hint::black_box(&assets);
        });
    });

    // Build-only: clone once per iteration outside the timed region, then
    // move the cloned assets into the builder inside the timer.
    group.bench_function("build_5000x50", |b| {
        b.iter_batched(
            || assets.clone(),
            |assets| {
                let manifest = CompactManifest::from(assets);
                std::hint::black_box(manifest.num_files());
            },
            criterion::BatchSize::LargeInput,
        );
    });

    group.finish();
}

const LOOKUP_CHUNKS: usize = 10000;

/// ChunkNameLookup binary search throughput (the actual chunk resolution path).
fn write_eytzinger(sorted: &[u32], layout: &mut [u32], source: &mut usize, node: usize) {
    if node >= layout.len() {
        return;
    }
    write_eytzinger(sorted, layout, source, node * 2 + 1);
    layout[node] = sorted[*source];
    *source += 1;
    write_eytzinger(sorted, layout, source, node * 2 + 2);
}

fn eytzinger_lookup_by<'a>(
    layout: &[u32],
    query: &str,
    name_at: impl Fn(usize) -> &'a str,
) -> Option<usize> {
    let mut node = 0usize;
    while node < layout.len() {
        let index = layout[node] as usize;
        match name_at(index).cmp(query) {
            std::cmp::Ordering::Equal => return Some(index),
            std::cmp::Ordering::Less => node = node * 2 + 2,
            std::cmp::Ordering::Greater => node = node * 2 + 1,
        }
    }
    None
}

fn bench_chunk_lookup(c: &mut Criterion) {
    for count in [0usize, 1, 2, 3, 7, 8, 9, 15, 16, 17, 31, 32, 33] {
        let owned: Vec<String> = (0..count)
            .map(|index| format!("name_{:04}", (index / 2) * 4))
            .collect();
        let names: Vec<&str> = owned.iter().map(String::as_str).collect();
        let interior_miss = if count > 1 {
            "name_0002".to_owned()
        } else {
            "name_9999".to_owned()
        };
        let mut indices: Vec<u32> = (0..count as u32).collect();
        indices.sort_by(|&a, &b| names[a as usize].cmp(names[b as usize]));
        let mut layout = vec![0u32; count];
        let mut source = 0usize;
        write_eytzinger(&indices, &mut layout, &mut source, 0);
        for query in names
            .iter()
            .copied()
            .chain(["", "name_9999", interior_miss.as_str()])
        {
            let expected = names.iter().position(|name| *name == query);
            assert_eq!(
                eytzinger_lookup_by(&layout, query, |index| names[index]).map(|index| names[index]),
                expected.map(|index| names[index])
            );
        }
    }

    let names: Vec<String> = (0..LOOKUP_CHUNKS)
        .map(|i| format!("chunk_{i:08}"))
        .collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let arena = StringArena::from(name_refs.as_slice());
    let lookup = ChunkNameLookup::from_arena(arena);
    let mut indices: Vec<u32> = (0..LOOKUP_CHUNKS as u32).collect();
    indices.sort_by(|&a, &b| lookup.get(a as usize).cmp(lookup.get(b as usize)));
    let mut eytzinger = vec![0u32; indices.len()];
    let mut source = 0usize;
    write_eytzinger(&indices, &mut eytzinger, &mut source, 0);

    let mut shuffled_indices: Vec<usize> = (0..LOOKUP_CHUNKS).collect();
    let mut state = 0x9e37_79b9u32;
    for i in (1..shuffled_indices.len()).rev() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        shuffled_indices.swap(i, state as usize % (i + 1));
    }
    let shuffled_hits: Vec<&str> = shuffled_indices
        .iter()
        .map(|&index| names[index].as_str())
        .collect();
    let mut mixed_queries: Vec<String> = shuffled_indices
        .iter()
        .enumerate()
        .map(|(i, &index)| {
            if i % 2 == 0 {
                names[index].clone()
            } else if index % 2 == 0 {
                format!("chunk!{index:08}")
            } else {
                format!("chunk~{index:08}")
            }
        })
        .collect();
    for i in (1..mixed_queries.len()).rev() {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        mixed_queries.swap(i, state as usize % (i + 1));
    }

    let mut group = c.benchmark_group("chunk_lookup");
    group.throughput(Throughput::Elements(LOOKUP_CHUNKS as u64));

    group.bench_function("binary_search_10000", |b| {
        b.iter(|| {
            let mut found = 0usize;
            for n in &names {
                if lookup.lookup(n).is_some() {
                    found += 1;
                }
            }
            std::hint::black_box(found);
        });
    });

    assert!(
        shuffled_hits
            .iter()
            .all(|name| lookup.lookup(name).is_some())
    );
    group.bench_function("shuffled_hits_10000", |b| {
        b.iter(|| {
            let mut found = 0usize;
            for name in &shuffled_hits {
                if lookup.lookup(name).is_some() {
                    found += 1;
                }
            }
            std::hint::black_box(found);
        });
    });

    assert_eq!(
        mixed_queries
            .iter()
            .filter(|name| lookup.lookup(name).is_some())
            .count(),
        LOOKUP_CHUNKS / 2
    );
    assert!(mixed_queries.iter().any(|name| name < &names[0]));
    assert!(
        mixed_queries
            .iter()
            .any(|name| name > names.last().expect("lookup names are non-empty"))
    );
    group.bench_function("shuffled_mixed_hits_boundary_misses_10000", |b| {
        b.iter(|| {
            let mut found = 0usize;
            for name in &mixed_queries {
                if lookup.lookup(name).is_some() {
                    found += 1;
                }
            }
            std::hint::black_box(found);
        });
    });

    assert_eq!(
        mixed_queries
            .iter()
            .filter(|name| {
                eytzinger_lookup_by(&eytzinger, name, |index| lookup.get(index)).is_some()
            })
            .count(),
        LOOKUP_CHUNKS / 2
    );
    assert_eq!(
        shuffled_hits
            .iter()
            .filter(
                |name| eytzinger_lookup_by(&eytzinger, name, |index| lookup.get(index)).is_some()
            )
            .count(),
        LOOKUP_CHUNKS
    );
    assert!(shuffled_hits.iter().all(|name| {
        eytzinger_lookup_by(&eytzinger, name, |index| lookup.get(index))
            .is_some_and(|index| lookup.get(index) == *name)
    }));
    group.bench_function("eytzinger_shuffled_hits_10000", |b| {
        b.iter(|| {
            let mut found = 0usize;
            for name in &shuffled_hits {
                if eytzinger_lookup_by(&eytzinger, name, |index| lookup.get(index)).is_some() {
                    found += 1;
                }
            }
            std::hint::black_box(found);
        });
    });
    group.bench_function("eytzinger_mixed_hits_boundary_misses_10000", |b| {
        b.iter(|| {
            let mut found = 0usize;
            for name in &mixed_queries {
                if eytzinger_lookup_by(&eytzinger, name, |index| lookup.get(index)).is_some() {
                    found += 1;
                }
            }
            std::hint::black_box(found);
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Varint decode benchmark
// ---------------------------------------------------------------------------

use irmin_rs::game_installer::hdiffpatch::read_long_7bit_from_slice;

/// Encode a non-negative i64 as a 7-bit varint (tag_bit=0 format).
fn encode_7bit_varint(v: i64) -> Vec<u8> {
    debug_assert!(v >= 0);
    if v < 0x80 {
        return vec![v as u8];
    }
    let bits = 64 - (v as u64).leading_zeros() as usize;
    let groups = (bits + 6) / 7;
    let mut out = Vec::with_capacity(groups);
    for i in (0..groups).rev() {
        let chunk = ((v >> (i * 7)) & 0x7F) as u8;
        if i > 0 {
            out.push(chunk | 0x80);
        } else {
            out.push(chunk);
        }
    }
    // Set continuation bit on first byte (bit 7 signals multi-byte)
    out[0] |= 0x80;
    out
}

/// Generate a buffer of `count` varints with mixed 1-8 byte encodings.
fn gen_varint_buffer(count: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(count * 4);
    for i in 0..count {
        let v = match i % 8 {
            0 => (i % 64) as i64,                   // 1 byte (0-63)
            1 => 100 + (i % 28) as i64,             // 1 byte (100-127)
            2 => 200 + (i % 1000) as i64,           // 2 bytes
            3 => 20_000 + (i % 10_000) as i64,      // 3 bytes
            4 => 2_000_000 + (i as i64),            // 4 bytes
            5 => 300_000_000 + (i as i64),          // 5 bytes
            6 => 40_000_000_000i64 + (i as i64),    // 6 bytes
            _ => 5_000_000_000_000i64 + (i as i64), // 7+ bytes
        };
        buf.extend_from_slice(&encode_7bit_varint(v));
    }
    buf
}

const VARINT_COUNT: usize = 100_000;

/// Varint decoding throughput: read_long_7bit_from_slice over 100k varints
/// of varying encoded lengths (1-8 bytes).
fn bench_varint_decode(c: &mut Criterion) {
    let buf = gen_varint_buffer(VARINT_COUNT);

    let mut group = c.benchmark_group("varint_decode");
    group.throughput(Throughput::Elements(VARINT_COUNT as u64));

    group.bench_function("mixed_100k", |b| {
        b.iter(|| {
            let mut offset = 0usize;
            for _ in 0..VARINT_COUNT {
                std::hint::black_box(read_long_7bit_from_slice(&buf, &mut offset, 0, 0).unwrap());
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Cover stream decode benchmark
// ---------------------------------------------------------------------------

use irmin_rs::game_installer::hdiffpatch::enumerate_cover_headers_checked;

use irmin_rs::proto_parse::SophonManifestProto;
use prost::Message;
/// Encode a tagged varint (tag_bit=1) into a p_sign byte + continuation bytes.
/// Returns (p_sign_byte, continuation_bytes). `sign` is 0 (positive) or 1 (negative).
fn encode_cover_header_entry(
    sign: u8,
    inc_old_pos: i64,
    copy_length: i64,
    cover_length: i64,
) -> Vec<u8> {
    let mut out = Vec::new();
    // p_sign byte: bit 7 = sign, bit 6 = continuation, bits 0-5 = first 6 bits of inc_old_pos
    if inc_old_pos < 64 {
        out.push((sign << 7) | (inc_old_pos as u8));
    } else {
        // Need continuation bytes. First byte: sign | 0x40 (continuation set) | top 6 bits
        let bits = 64 - (inc_old_pos as u64).leading_zeros() as usize;
        let groups = (bits.saturating_sub(6) + 6) / 7 + 1; // 1 for first 6-bit group
        let shift = (groups - 1) * 7;
        let first_payload = ((inc_old_pos >> shift) & 0x3F) as u8;
        out.push((sign << 7) | 0x40 | first_payload);
        // Remaining groups (7 bits each, MSB-first)
        for g in (0..groups - 1).rev() {
            let chunk = ((inc_old_pos >> (g * 7)) & 0x7F) as u8;
            if g > 0 {
                out.push(chunk | 0x80);
            } else {
                out.push(chunk);
            }
        }
    }
    // copy_length and cover_length as standard varints
    out.extend_from_slice(&encode_7bit_varint(copy_length));
    out.extend_from_slice(&encode_7bit_varint(cover_length));
    out
}

/// Generate a cover header buffer with `count` entries for benchmarking.
/// Produces realistic cover sequences: monotonically advancing positions.
fn gen_cover_buffer(count: usize) -> (Vec<u8>, i64) {
    let mut buf = Vec::with_capacity(count * 6);
    for i in 0..count {
        // Mix of small and medium increments
        let inc = match i % 10 {
            0..=5 => 4 + (i % 60) as i64,     // single-byte (< 64)
            6..=8 => 100 + (i % 1000) as i64, // two-byte
            _ => 10_000 + (i % 5000) as i64,  // three-byte
        };
        let copy_len = (i % 32) as i64;
        let cover_len = 64 + (i % 128) as i64;
        buf.extend_from_slice(&encode_cover_header_entry(0, inc, copy_len, cover_len));
    }
    let size = buf.len() as i64;
    (buf, size)
}

const COVER_COUNT: usize = 10_000;

/// Cover header enumeration throughput via the streaming iterator.
fn bench_cover_stream_decode(c: &mut Criterion) {
    let (buf, buf_size) = gen_cover_buffer(COVER_COUNT);

    let mut group = c.benchmark_group("cover_stream_decode");
    group.throughput(Throughput::Elements(COVER_COUNT as u64));

    group.bench_function("checked_10k", |b| {
        b.iter(|| {
            let mut cursor = std::io::Cursor::new(&buf);
            let iter =
                enumerate_cover_headers_checked(&mut cursor, buf_size, COVER_COUNT as i64).unwrap();
            for h in iter {
                std::hint::black_box(h.unwrap());
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// Protobuf manifest decode benchmark
// ---------------------------------------------------------------------------

const PROTO_FILES: usize = 5000;
const PROTO_CHUNKS_PER_FILE: usize = 50;

/// Generate a prost-encoded SophonManifestProto with `files` assets, each with `chunks` chunks.
fn gen_protobuf_manifest(files: usize, chunks: usize) -> Vec<u8> {
    let assets: Vec<SophonManifestAssetProperty> = (0..files)
        .map(|f| {
            let asset_chunks: Vec<SophonManifestAssetChunk> = (0..chunks)
                .map(|c| {
                    SophonManifestAssetChunk::from_parts(
                        format!("f{f}c{c}").into(),
                        format!("{:032x}", f * chunks + c).into(),
                        (c * 1024 * 1024) as u64,
                        512 * 1024,
                        1024 * 1024,
                        (f * chunks + c) as u64,
                        format!("{:032x}", f * chunks + c + 1).into(),
                        -1,
                    )
                })
                .collect();
            SophonManifestAssetProperty::from_parts(
                format!("data/assets/bundle_{f:04}.pak").into(),
                asset_chunks,
                0,
                (chunks * 1024 * 1024) as u64,
                format!("{f:032x}").into(),
            )
        })
        .collect();
    let manifest = SophonManifestProto { assets };
    manifest.encode_to_vec()
}

/// Protobuf manifest decode throughput: prost decode of 5000 assets x 50 chunks.
fn bench_protobuf_decode(c: &mut Criterion) {
    let encoded = gen_protobuf_manifest(PROTO_FILES, PROTO_CHUNKS_PER_FILE);
    let encoded_bytes = bytes::Bytes::from(encoded.clone());

    let mut group = c.benchmark_group("protobuf_manifest");
    group.throughput(Throughput::Elements(PROTO_FILES as u64));

    // Slice path: copies each string field into a new String allocation.
    group.bench_function("decode_slice_5000x50", |b| {
        b.iter(|| {
            let manifest = SophonManifestProto::decode(encoded.as_slice()).unwrap();
            std::hint::black_box(manifest.assets.len());
        });
    });

    // Bytes path: prost stores string fields as refcounted slices of the
    // input buffer, avoiding per-field allocation and copy.
    group.bench_function("decode_bytes_5000x50", |b| {
        b.iter(|| {
            let manifest = SophonManifestProto::decode(encoded_bytes.clone()).unwrap();
            std::hint::black_box(manifest.assets.len());
        });
    });

    // Production path: zero-copy decode plus the mandatory UTF-8 validation
    // pass over every string field. This is what install startup runs.
    group.bench_function("decode_bytes_validated_5000x50", |b| {
        b.iter(|| {
            let manifest = decode_manifest_from_bytes(encoded_bytes.clone()).unwrap();
            std::hint::black_box(manifest.assets.len());
        });
    });

    group.finish();
}

fn bench_manifest_content_hash(c: &mut Criterion) {
    let encoded = gen_protobuf_manifest(PROTO_FILES, PROTO_CHUNKS_PER_FILE);
    let encoded_bytes = bytes::Bytes::from(encoded.clone());
    let manifest = SophonManifestProto::decode(encoded.as_slice()).unwrap();
    let mut group = c.benchmark_group("manifest_content_hash");
    group.throughput(Throughput::Elements(
        (PROTO_FILES * PROTO_CHUNKS_PER_FILE) as u64,
    ));
    group.bench_function("hash_5000x50", |b| {
        b.iter(|| std::hint::black_box(compute_content_manifest_hash(&manifest)));
    });
    group.bench_function("decode_and_hash_5000x50", |b| {
        b.iter(|| {
            let decoded = SophonManifestProto::decode(encoded.as_slice()).unwrap();
            std::hint::black_box(compute_content_manifest_hash(&decoded))
        });
    });
    group.bench_function("decode_only_5000x50", |b| {
        b.iter(|| {
            let decoded = SophonManifestProto::decode(encoded.as_slice()).unwrap();
            std::hint::black_box(decoded);
        });
    });
    // Zero-copy path: decode from Bytes so string fields are refcounted
    // slices of the input buffer instead of copied String allocations.
    group.bench_function("decode_bytes_and_hash_5000x50", |b| {
        b.iter(|| {
            let decoded = SophonManifestProto::decode(encoded_bytes.clone()).unwrap();
            std::hint::black_box(compute_content_manifest_hash(&decoded))
        });
    });
    group.bench_function("decode_bytes_only_5000x50", |b| {
        b.iter(|| {
            let decoded = SophonManifestProto::decode(encoded_bytes.clone()).unwrap();
            std::hint::black_box(decoded);
        });
    });
    group.finish();
}

// ---------------------------------------------------------------------------
// Buffer pool benchmark
// ---------------------------------------------------------------------------

use irmin_rs::game_installer::hdiffpatch::HDiffPublic as HDiff;
use irmin_rs::game_installer::hdiffpatch::{BENCH_MAX_ARRAY_POOL_LEN, BufferPool};

const POOL_OPS: u64 = 1000;

/// BufferPool take/return cycle throughput (the actual allocation path in hdiffpatch).
fn bench_buffer_pool(c: &mut Criterion) {
    let mut group = c.benchmark_group("buffer_pool");
    group.throughput(Throughput::Elements(POOL_OPS));

    group.bench_function("pooled_take_return", |b| {
        let pool = BufferPool::new(4);
        // Seed pool with one buffer
        pool.return_buf(Vec::with_capacity(BENCH_MAX_ARRAY_POOL_LEN));
        b.iter(|| {
            for _ in 0..POOL_OPS {
                let buf = pool.take(BENCH_MAX_ARRAY_POOL_LEN);
                std::hint::black_box(buf.capacity());
                pool.return_buf(buf);
            }
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// HDiffPatch E2E benchmark
// ---------------------------------------------------------------------------

/// Encode a tagged varint with tag_bit=1 (used for cover header delta encoding in v20/PatchSF).
/// Returns bytes for the first byte (with sign in bit 7) + continuation.
fn encode_tagged_1bit(sign: u8, value: i64) -> Vec<u8> {
    debug_assert!(value >= 0);
    // tag_bit=1: bit 7 = sign, bit 6 = continuation flag, bits 0-5 = first 6 bits of value
    let mut out = Vec::new();
    if value < 64 {
        out.push((sign << 7) | (value as u8));
    } else {
        let bits = 64 - (value as u64).leading_zeros() as usize;
        // First 6 bits in byte 0, then 7-bit groups
        let remaining_bits = bits - 6;
        let extra_groups = (remaining_bits + 6) / 7;
        let total_shift = extra_groups * 7;
        let first_payload = ((value >> total_shift) & 0x3F) as u8;
        out.push((sign << 7) | 0x40 | first_payload);
        for g in (0..extra_groups).rev() {
            let chunk = ((value >> (g * 7)) & 0x7F) as u8;
            if g > 0 {
                out.push(chunk | 0x80);
            } else {
                out.push(chunk);
            }
        }
    }
    out
}

/// Generate a valid HDIFF20&nocomp patch file.
/// Creates a patch where ~10% of 4 KiB blocks are "modified" (replaced with new data).
/// The rest are copied from old via covers.
fn gen_hdiff_v20_patch(old: &[u8], new: &[u8]) -> Vec<u8> {
    let block_size = 4096usize;
    let num_blocks = old.len() / block_size;

    // Determine which blocks differ
    let mut covers: Vec<(u64, u64, u64)> = Vec::new(); // (old_pos, new_pos, length)
    let mut run_start: Option<usize> = None;

    for i in 0..num_blocks {
        let off = i * block_size;
        let same = &old[off..off + block_size] == &new[off..off + block_size];
        if same {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else {
            if let Some(start) = run_start {
                let pos = (start * block_size) as u64;
                let len = ((i - start) * block_size) as u64;
                covers.push((pos, pos, len));
                run_start = None;
            }
        }
    }
    if let Some(start) = run_start {
        let pos = (start * block_size) as u64;
        let len = ((num_blocks - start) * block_size) as u64;
        covers.push((pos, pos, len));
    }

    let cover_count = covers.len();
    let new_data_size = new.len() as i64;
    let old_data_size = old.len() as i64;

    // Encode covers for PatchSF format (same as PatchSingle cover encoding: sign+delta, gap, length)
    let mut cover_buf = Vec::new();
    let mut last_old_end = 0u64;
    let mut last_new_end = 0u64;
    for &(old_pos, new_pos, length) in &covers {
        let delta = if old_pos >= last_old_end {
            let d = old_pos - last_old_end;
            cover_buf.extend_from_slice(&encode_tagged_1bit(0, d as i64));
            d
        } else {
            let d = last_old_end - old_pos;
            cover_buf.extend_from_slice(&encode_tagged_1bit(1, d as i64));
            d
        };
        let _ = delta;
        let new_pos_gap = new_pos - last_new_end;
        cover_buf.extend_from_slice(&encode_7bit_varint(new_pos_gap as i64));
        cover_buf.extend_from_slice(&encode_7bit_varint(length as i64));
        last_old_end = old_pos + length;
        last_new_end = new_pos + length;
    }

    // RLE buffer: for each cover, (len0=cover_length, lenv=0) — no XOR modification
    let mut rle_buf = Vec::new();
    for &(_, _, length) in &covers {
        rle_buf.extend_from_slice(&encode_7bit_varint(length as i64)); // len0
        rle_buf.extend_from_slice(&encode_7bit_varint(0)); // lenv
    }

    // Gap data: bytes from `new` that are between covers
    let mut gap_data = Vec::new();
    let mut prev_new_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_new_end {
            gap_data.extend_from_slice(&new[prev_new_end as usize..new_pos as usize]);
        }
        prev_new_end = new_pos + length;
    }
    // Tail
    if prev_new_end < new.len() as u64 {
        gap_data.extend_from_slice(&new[prev_new_end as usize..]);
    }

    // Build the diff stream: [buf_cover_size][buf_rle_size][covers][rle][gap_data_interleaved]
    // For simplicity: single step with all covers, then all gap/tail data at the end.
    // The gap data must be interspersed at the right points in the stream.
    // PatchSF reads gaps between covers inline from the diff stream.
    // So: step header + step data, then gap bytes interleaved per cover.

    // Actually, patch_loop reads: step_header, step_data (covers+rle), then for each cover
    // it checks if there's a gap and reads from diff. So the stream must be:
    // [step_header][step_data][gap0_data][gap1_data]...[tail_data]
    // where gapN appears between covers where new_pos > prev_new_end.

    let mut diff_stream = Vec::new();
    // Step header
    diff_stream.extend_from_slice(&encode_7bit_varint(cover_buf.len() as i64));
    diff_stream.extend_from_slice(&encode_7bit_varint(rle_buf.len() as i64));
    // Step data
    diff_stream.extend_from_slice(&cover_buf);
    diff_stream.extend_from_slice(&rle_buf);
    // Gap data interspersed (same order as covers processed)
    let mut prev_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_end {
            diff_stream.extend_from_slice(&new[prev_end as usize..new_pos as usize]);
        }
        prev_end = new_pos + length;
    }
    if prev_end < new.len() as u64 {
        diff_stream.extend_from_slice(&new[prev_end as usize..]);
    }

    let uncompressed_size = diff_stream.len() as i64;
    let step_mem_size = (cover_buf.len() + rle_buf.len()) as i64;

    // Build the full patch file
    let mut patch = Vec::new();
    // Header line (null-terminated)
    patch.extend_from_slice(b"HDIFF20&nocomp\0");
    // Header fields as varints
    patch.extend_from_slice(&encode_7bit_varint(new_data_size));
    patch.extend_from_slice(&encode_7bit_varint(old_data_size));
    patch.extend_from_slice(&encode_7bit_varint(cover_count as i64));
    patch.extend_from_slice(&encode_7bit_varint(step_mem_size));
    patch.extend_from_slice(&encode_7bit_varint(uncompressed_size));
    patch.extend_from_slice(&encode_7bit_varint(0)); // compressed_size = 0 (nocomp)
    // Diff data
    patch.extend_from_slice(&diff_stream);

    patch
}

fn gen_hdiff_v20_pattern_patch(
    old: &[u8],
    new: &[u8],
    block_size: usize,
    cover_period_blocks: usize,
    covered_blocks_per_period: usize,
) -> (Vec<u8>, Vec<u8>, usize) {
    assert_eq!(old.len(), new.len());
    assert!(block_size > 0);
    assert_eq!(old.len() % block_size, 0);
    assert!(cover_period_blocks > 0);
    assert!(covered_blocks_per_period > 0);
    assert!(covered_blocks_per_period < cover_period_blocks);
    let blocks = old.len() / block_size;
    let unchanged = |block: usize| block % cover_period_blocks < covered_blocks_per_period;
    let mut prepared_new = new.to_vec();
    for block in 0..blocks {
        if !unchanged(block) {
            let offset = block * block_size;
            for (index, byte) in prepared_new[offset..offset + block_size]
                .iter_mut()
                .enumerate()
            {
                *byte = byte.wrapping_add((index as u8).wrapping_mul(17) | 1);
            }
        } else {
            prepared_new[block * block_size..(block + 1) * block_size]
                .copy_from_slice(&old[block * block_size..(block + 1) * block_size]);
        }
    }

    let mut covers = Vec::<(u64, u64, u64)>::new();
    let mut run_start = None;
    for block in 0..blocks {
        if unchanged(block) {
            run_start.get_or_insert(block);
        } else if let Some(start) = run_start.take() {
            let position = (start * block_size) as u64;
            covers.push((position, position, ((block - start) * block_size) as u64));
        }
    }
    if let Some(start) = run_start {
        let position = (start * block_size) as u64;
        covers.push((position, position, ((blocks - start) * block_size) as u64));
    }

    let mut cover_data = Vec::new();
    let mut last_old_end = 0u64;
    let mut last_new_end = 0u64;
    let mut rle_data = Vec::new();
    let mut diff_stream = Vec::new();
    for &(old_pos, new_pos, length) in &covers {
        let old_delta = old_pos.abs_diff(last_old_end);
        let old_sign = u8::from(old_pos < last_old_end);
        cover_data.extend_from_slice(&encode_tagged_1bit(old_sign, old_delta as i64));
        cover_data.extend_from_slice(&encode_7bit_varint((new_pos - last_new_end) as i64));
        cover_data.extend_from_slice(&encode_7bit_varint(length as i64));
        rle_data.extend_from_slice(&encode_7bit_varint(length as i64));
        rle_data.extend_from_slice(&encode_7bit_varint(0));
        last_old_end = old_pos + length;
        last_new_end = new_pos + length;
    }
    diff_stream.extend_from_slice(&encode_7bit_varint(cover_data.len() as i64));
    diff_stream.extend_from_slice(&encode_7bit_varint(rle_data.len() as i64));
    diff_stream.extend_from_slice(&cover_data);
    diff_stream.extend_from_slice(&rle_data);
    let mut previous_new_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > previous_new_end {
            diff_stream
                .extend_from_slice(&prepared_new[previous_new_end as usize..new_pos as usize]);
        }
        previous_new_end = new_pos + length;
    }
    if previous_new_end < prepared_new.len() as u64 {
        diff_stream.extend_from_slice(&prepared_new[previous_new_end as usize..]);
    }

    let mut patch = b"HDIFF20&nocomp\0".to_vec();
    for value in [
        prepared_new.len() as i64,
        old.len() as i64,
        covers.len() as i64,
        (cover_data.len() + rle_data.len()) as i64,
        diff_stream.len() as i64,
        0,
    ] {
        patch.extend_from_slice(&encode_7bit_varint(value));
    }
    patch.extend_from_slice(&diff_stream);
    (patch, prepared_new, covers.len())
}

fn hash_file_xxh64(path: &Path) -> u64 {
    let mut file = fs::File::open(path).unwrap();
    let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
    let mut buffer = [0u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            return hasher.digest();
        }
        hasher.update(&buffer[..read]);
    }
}

fn bench_v20_cover_shapes(c: &mut Criterion) {
    let mut group = c.benchmark_group("hdiffpatch_v20_cover_shapes");
    group.sample_size(10);
    let size_mib = 4usize;
    let size_bytes = size_mib * 1024 * 1024;
    let old = gen_prng_data(0xC0FFEE, size_bytes);
    for (label, block_size, period, cover_run) in [
        ("covers_16", 4096, 64, 32),
        ("covers_256", 4096, 4, 2),
        ("covers_2048", 1024, 2, 1),
        ("covers_4096", 512, 2, 1),
    ] {
        let (patch, expected, cover_count) =
            gen_hdiff_v20_pattern_patch(&old, &old, block_size, period, cover_run);
        let expected_count = (size_bytes / block_size).div_ceil(period);
        assert_eq!(cover_count, expected_count);
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old.bin");
        let patch_path = dir.path().join("patch.hdiff");
        let out_path = dir.path().join("out.bin");
        fs::write(&old_path, &old).unwrap();
        fs::write(&patch_path, &patch).unwrap();
        let expected_hash = xxhash_rust::xxh64::xxh64(&expected, 0);
        let mut hdiff = HDiff::new(
            old_path.to_string_lossy().into_owned(),
            patch_path.to_string_lossy().into_owned(),
            out_path.to_string_lossy().into_owned(),
        );
        assert!(hdiff.apply(None), "{label} patch failed");
        assert_eq!(fs::metadata(&out_path).unwrap().len(), size_bytes as u64);
        assert_eq!(hash_file_xxh64(&out_path), expected_hash);

        group.throughput(Throughput::Bytes(size_bytes as u64));
        group.bench_function(BenchmarkId::new(label, size_mib), |bencher| {
            bencher.iter(|| {
                let mut hdiff = HDiff::new(
                    old_path.to_string_lossy().into_owned(),
                    patch_path.to_string_lossy().into_owned(),
                    out_path.to_string_lossy().into_owned(),
                );
                assert!(hdiff.apply(None));
            });
        });
    }
    group.finish();
}

fn bench_v20_step_buffer_stress(c: &mut Criterion) {
    use irmin_rs::bench_support::prepare_hdiff20_step_stress_fixture;

    let size_mib = 7;
    let dir = tempfile::tempdir().expect("tempdir");
    let shape = prepare_hdiff20_step_stress_fixture(dir.path(), size_mib).unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("patch.hdiff");
    let new_path = dir.path().join("new.bin");
    let out_path = dir.path().join("out.bin");
    let expected_hash = hash_file_xxh64(&new_path);

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        patch_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(hdiff.apply(None), "HDIFF20 step-stress fixture failed");
    assert_eq!(hash_file_xxh64(&out_path), expected_hash);
    assert_eq!(
        fs::metadata(&out_path).unwrap().len(),
        shape.output_bytes as u64
    );

    let mut group = c.benchmark_group("hdiffpatch_v20_step_buffer");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(shape.output_bytes as u64));
    group.bench_function(BenchmarkId::new("large_small_large", size_mib), |bencher| {
        bencher.iter(|| {
            let mut hdiff = HDiff::new(
                old_path.to_string_lossy().into_owned(),
                patch_path.to_string_lossy().into_owned(),
                out_path.to_string_lossy().into_owned(),
            );
            assert!(hdiff.apply(None));
        });
    });
    group.finish();
}

#[test]
fn v20_cover_pattern_generator_handles_partial_period() {
    let block_size = 512;
    let blocks = 11;
    let old = gen_prng_data(0x1234, blocks * block_size);
    let (patch, expected, cover_count) = gen_hdiff_v20_pattern_patch(&old, &old, block_size, 4, 2);
    assert_eq!(cover_count, 3);
    for block in 0..blocks {
        let expected_block = &expected[block * block_size..(block + 1) * block_size];
        let source_block = &old[block * block_size..(block + 1) * block_size];
        if block % 4 < 2 {
            assert_eq!(expected_block, source_block);
        } else {
            assert!(
                expected_block
                    .iter()
                    .zip(source_block)
                    .all(|(actual, source)| actual == &source.wrapping_add(1))
            );
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("partial.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();
    let mut patcher = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        patch_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(patcher.apply(None));
    assert_eq!(
        fs::metadata(&out_path).unwrap().len(),
        expected.len() as u64
    );
    assert_eq!(
        hash_file_xxh64(&out_path),
        xxhash_rust::xxh64::xxh64(&expected, 0)
    );
}

/// Generate a valid HDIFF20&zstd patch file (zstd-compressed diff stream).
fn gen_hdiff_v20_patch_zstd(old: &[u8], new: &[u8]) -> Vec<u8> {
    let block_size = 4096usize;
    let num_blocks = old.len() / block_size;

    let mut covers: Vec<(u64, u64, u64)> = Vec::new();
    let mut run_start: Option<usize> = None;
    for i in 0..num_blocks {
        let off = i * block_size;
        let same = &old[off..off + block_size] == &new[off..off + block_size];
        if same {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else {
            if let Some(start) = run_start {
                covers.push((
                    (start * block_size) as u64,
                    (start * block_size) as u64,
                    ((i - start) * block_size) as u64,
                ));
                run_start = None;
            }
        }
    }
    if let Some(start) = run_start {
        covers.push((
            (start * block_size) as u64,
            (start * block_size) as u64,
            ((num_blocks - start) * block_size) as u64,
        ));
    }

    let cover_count = covers.len();
    let new_data_size = new.len() as i64;
    let old_data_size = old.len() as i64;

    // Encode covers (same as v20 nocomp)
    let mut cover_buf = Vec::new();
    let mut last_old_end = 0u64;
    let mut last_new_end = 0u64;
    for &(old_pos, new_pos, length) in &covers {
        let delta = if old_pos >= last_old_end {
            old_pos - last_old_end
        } else {
            last_old_end - old_pos
        };
        let sign = if old_pos >= last_old_end { 0 } else { 1 };
        cover_buf.extend_from_slice(&encode_tagged_1bit(sign, delta as i64));
        cover_buf.extend_from_slice(&encode_7bit_varint((new_pos - last_new_end) as i64));
        cover_buf.extend_from_slice(&encode_7bit_varint(length as i64));
        last_old_end = old_pos + length;
        last_new_end = new_pos + length;
    }

    // RLE buffer
    let mut rle_buf = Vec::new();
    for &(_, _, length) in &covers {
        rle_buf.extend_from_slice(&encode_7bit_varint(length as i64));
        rle_buf.extend_from_slice(&encode_7bit_varint(0));
    }

    // Build raw diff stream
    let mut diff_stream = Vec::new();
    diff_stream.extend_from_slice(&encode_7bit_varint(cover_buf.len() as i64));
    diff_stream.extend_from_slice(&encode_7bit_varint(rle_buf.len() as i64));
    diff_stream.extend_from_slice(&cover_buf);
    diff_stream.extend_from_slice(&rle_buf);
    let mut prev_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_end {
            diff_stream.extend_from_slice(&new[prev_end as usize..new_pos as usize]);
        }
        prev_end = new_pos + length;
    }
    if prev_end < new.len() as u64 {
        diff_stream.extend_from_slice(&new[prev_end as usize..]);
    }

    let uncompressed_size = diff_stream.len() as i64;
    let step_mem_size = (cover_buf.len() + rle_buf.len()) as i64;
    let compressed = zstd::encode_all(diff_stream.as_slice(), 3).unwrap();
    let compressed_size = compressed.len() as i64;

    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF20&zstd\0");
    patch.extend_from_slice(&encode_7bit_varint(new_data_size));
    patch.extend_from_slice(&encode_7bit_varint(old_data_size));
    patch.extend_from_slice(&encode_7bit_varint(cover_count as i64));
    patch.extend_from_slice(&encode_7bit_varint(step_mem_size));
    patch.extend_from_slice(&encode_7bit_varint(uncompressed_size));
    patch.extend_from_slice(&encode_7bit_varint(compressed_size));
    patch.extend_from_slice(&compressed);

    patch
}

/// Mutate ~10% of 4 KiB blocks in `data` with PRNG replacement.
fn mutate_blocks(data: &[u8], mutation_rate_pct: usize) -> Vec<u8> {
    let block_size = 4096;
    let mut out = data.to_vec();
    let num_blocks = out.len() / block_size;
    for i in 0..num_blocks {
        if (i * 7 + 3) % 100 < mutation_rate_pct {
            let replacement = gen_prng_data(1000 + i as u64, block_size);
            out[i * block_size..(i + 1) * block_size].copy_from_slice(&replacement);
        }
    }
    out
}

/// Encode a tagged varint with tag_bit=2 (used for RLE ctrl entries in v13 PatchSingle).
/// rle_type occupies bits 7-6 of the first byte.
fn encode_tagged_2bit(rle_type: u8, value: i64) -> Vec<u8> {
    debug_assert!(value >= 0);
    debug_assert!(rle_type < 4);
    // tag_bit=2: bit 5 = continuation flag, bits 0-4 = first 5 bits of value
    let mut out = Vec::new();
    if value < 32 {
        out.push((rle_type << 6) | (value as u8));
    } else {
        let bits = 64 - (value as u64).leading_zeros() as usize;
        let remaining_bits = bits - 5;
        let extra_groups = (remaining_bits + 6) / 7;
        let total_shift = extra_groups * 7;
        let first_payload = ((value >> total_shift) & 0x1F) as u8;
        out.push((rle_type << 6) | 0x20 | first_payload);
        for g in (0..extra_groups).rev() {
            let chunk = ((value >> (g * 7)) & 0x7F) as u8;
            if g > 0 {
                out.push(chunk | 0x80);
            } else {
                out.push(chunk);
            }
        }
    }
    out
}

/// Generate a valid HDIFF13&nocomp patch file (PatchSingle format with 4 streams).
fn gen_hdiff_v13_patch(old: &[u8], new: &[u8]) -> Vec<u8> {
    let block_size = 4096usize;
    let num_blocks = old.len() / block_size;

    // Determine which blocks are identical vs modified
    let mut covers: Vec<(u64, u64, u64)> = Vec::new(); // (old_pos, new_pos, length)
    let mut run_start: Option<usize> = None;

    for i in 0..num_blocks {
        let off = i * block_size;
        let same = &old[off..off + block_size] == &new[off..off + block_size];
        if same {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start {
            let pos = (start * block_size) as u64;
            let len = ((i - start) * block_size) as u64;
            covers.push((pos, pos, len));
            run_start = None;
        }
    }
    if let Some(start) = run_start {
        let pos = (start * block_size) as u64;
        let len = ((num_blocks - start) * block_size) as u64;
        covers.push((pos, pos, len));
    }

    let cover_count = covers.len() as i64;
    let new_data_size = new.len() as i64;
    let old_data_size = old.len() as i64;

    // Build cover buffer (same encoding as used in write_cover_stream_to_output)
    // Each cover: p_sign (tagged varint for inc_old_pos with sign), copy_length varint, cover_length varint
    let mut cover_buf = Vec::new();
    let mut last_old_pos_back = 0i64;
    let mut last_new_pos_back = 0i64;
    for &(old_pos, new_pos, length) in &covers {
        let old_pos = old_pos as i64;
        let new_pos = new_pos as i64;
        let length = length as i64;

        let inc_old_pos = (old_pos - last_old_pos_back).unsigned_abs() as i64;
        let sign: u8 = if old_pos >= last_old_pos_back { 0 } else { 1 };
        cover_buf.extend_from_slice(&encode_tagged_1bit(sign, inc_old_pos));

        let copy_length = new_pos - last_new_pos_back;
        cover_buf.extend_from_slice(&encode_7bit_varint(copy_length));
        cover_buf.extend_from_slice(&encode_7bit_varint(length));

        last_old_pos_back = old_pos + length;
        last_new_pos_back = new_pos + length;
    }

    // Build rle_ctrl buffer: for each gap + cover, one type-0 entry covering full length
    // Order: gap0_rle, cover0_rle, gap1_rle, cover1_rle, ...
    let mut rle_ctrl_buf = Vec::new();
    let mut prev_new_end = 0i64;
    for &(_, new_pos, length) in &covers {
        let new_pos = new_pos as i64;
        let length = length as i64;
        let gap = new_pos - prev_new_end;
        if gap > 0 {
            // RLE type 0, raw_length = gap - 1
            rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, gap - 1));
        }
        // RLE type 0, raw_length = cover_length - 1
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, length - 1));
        prev_new_end = new_pos + length;
    }
    // Tail gap
    let tail_gap = new_data_size - prev_new_end;
    if tail_gap > 0 {
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, tail_gap - 1));
    }

    // rle_code is empty (type 0 doesn't read from rle_code)
    let rle_code_buf: Vec<u8> = Vec::new();

    // new_data_diff: bytes from new that fill the gaps between covers + tail
    let mut new_data_diff = Vec::new();
    let mut prev_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_end {
            new_data_diff.extend_from_slice(&new[prev_end as usize..new_pos as usize]);
        }
        prev_end = new_pos + length;
    }
    if prev_end < new.len() as u64 {
        new_data_diff.extend_from_slice(&new[prev_end as usize..]);
    }

    // Build patch file
    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF13&nocomp\0");

    // Header varints (read_non_single_file_header)
    patch.extend_from_slice(&encode_7bit_varint(new_data_size));
    patch.extend_from_slice(&encode_7bit_varint(old_data_size));
    // DiffChunkInfo fields
    patch.extend_from_slice(&encode_7bit_varint(cover_count));
    patch.extend_from_slice(&encode_7bit_varint(cover_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(0)); // compress_cover_buf_size (0 = nocomp)
    patch.extend_from_slice(&encode_7bit_varint(rle_ctrl_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(0)); // compress_rle_ctrl_buf_size
    patch.extend_from_slice(&encode_7bit_varint(rle_code_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(0)); // compress_rle_code_buf_size
    patch.extend_from_slice(&encode_7bit_varint(new_data_diff.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(0)); // compress_new_data_diff_size

    // Data sections (consecutive)
    patch.extend_from_slice(&cover_buf);
    patch.extend_from_slice(&rle_ctrl_buf);
    patch.extend_from_slice(&rle_code_buf);
    patch.extend_from_slice(&new_data_diff);

    patch
}

/// Generate an HDIFF13 patch whose covers exercise additive or literal RLE runs.
fn gen_hdiff_v13_rle_patch(old: &[u8], run_kind: u8) -> (Vec<u8>, Vec<u8>) {
    let run_len = old.len();
    let new = old
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if run_kind == 2 {
                byte.wrapping_add(0x37)
            } else {
                byte.wrapping_add((index as u8).wrapping_mul(13).wrapping_add(1))
            }
        })
        .collect::<Vec<_>>();

    let cover_buf = [
        encode_tagged_1bit(0, 0),
        encode_7bit_varint(0),
        encode_7bit_varint(run_len as i64),
    ]
    .concat();
    let rle_ctrl_buf = encode_tagged_2bit(run_kind, run_len as i64 - 1);
    let rle_code_buf = if run_kind == 2 {
        vec![0x37]
    } else {
        old.iter()
            .zip(&new)
            .map(|(&old_byte, &new_byte)| new_byte.wrapping_sub(old_byte))
            .collect()
    };
    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF13&nocomp\0");
    for value in [
        new.len() as i64,
        old.len() as i64,
        1,
        cover_buf.len() as i64,
        0,
        rle_ctrl_buf.len() as i64,
        0,
        rle_code_buf.len() as i64,
        0,
        0,
        0,
    ] {
        patch.extend_from_slice(&encode_7bit_varint(value));
    }
    patch.extend_from_slice(&cover_buf);
    patch.extend_from_slice(&rle_ctrl_buf);
    patch.extend_from_slice(&rle_code_buf);
    (patch, new)
}

fn bench_v13_large_uncovered_prefix(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let (prefix_size, suffix_size) =
        irmin_rs::bench_support::prepare_hdiff_uncovered_prefix_fixture(dir.path(), 64).unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("patch.hdiff");
    let out_path = dir.path().join("out.bin");
    let expected_path = dir.path().join("new.bin");
    let expected_size = fs::metadata(&expected_path).unwrap().len();
    assert_eq!(expected_size, (prefix_size + suffix_size) as u64);
    let expected_hash = hash_file_xxh64(&expected_path);

    let mut patcher = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        patch_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(patcher.apply(None), "large leading-gap patch failed");
    assert_eq!(fs::metadata(&out_path).unwrap().len(), expected_size);
    assert_eq!(hash_file_xxh64(&out_path), expected_hash);

    let mut group = c.benchmark_group("hdiffpatch_v13_uncovered_prefix");
    group.sample_size(10);
    group.throughput(Throughput::Bytes(expected_size));
    let fixture_label = format!(
        "random_prefix_{}m_cover_{}m",
        prefix_size / (1024 * 1024),
        suffix_size / (1024 * 1024)
    );
    group.bench_function(fixture_label, |bencher| {
        bencher.iter(|| {
            let mut patcher = HDiff::new(
                old_path.to_string_lossy().into_owned(),
                patch_path.to_string_lossy().into_owned(),
                out_path.to_string_lossy().into_owned(),
            );
            assert!(patcher.apply(None));
        });
    });
    group.finish();
}

/// Generate a valid HDIFF13&zstd patch (each stream section individually zstd-compressed).
fn gen_hdiff_v13_patch_zstd(old: &[u8], new: &[u8]) -> Vec<u8> {
    // Reuse the same cover/rle logic as nocomp variant
    let block_size = 4096usize;
    let num_blocks = old.len() / block_size;

    let mut covers: Vec<(u64, u64, u64)> = Vec::new();
    let mut run_start: Option<usize> = None;
    for i in 0..num_blocks {
        let off = i * block_size;
        let same = &old[off..off + block_size] == &new[off..off + block_size];
        if same {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start {
            covers.push((
                (start * block_size) as u64,
                (start * block_size) as u64,
                ((i - start) * block_size) as u64,
            ));
            run_start = None;
        }
    }
    if let Some(start) = run_start {
        covers.push((
            (start * block_size) as u64,
            (start * block_size) as u64,
            ((num_blocks - start) * block_size) as u64,
        ));
    }

    let cover_count = covers.len() as i64;
    let new_data_size = new.len() as i64;
    let old_data_size = old.len() as i64;

    // Build cover buffer
    let mut cover_buf = Vec::new();
    let mut last_old_pos_back = 0i64;
    let mut last_new_pos_back = 0i64;
    for &(old_pos, new_pos, length) in &covers {
        let (old_pos, new_pos, length) = (old_pos as i64, new_pos as i64, length as i64);
        let inc = (old_pos - last_old_pos_back).unsigned_abs() as i64;
        let sign: u8 = if old_pos >= last_old_pos_back { 0 } else { 1 };
        cover_buf.extend_from_slice(&encode_tagged_1bit(sign, inc));
        cover_buf.extend_from_slice(&encode_7bit_varint(new_pos - last_new_pos_back));
        cover_buf.extend_from_slice(&encode_7bit_varint(length));
        last_old_pos_back = old_pos + length;
        last_new_pos_back = new_pos + length;
    }

    // Build rle_ctrl
    let mut rle_ctrl_buf = Vec::new();
    let mut prev_new_end = 0i64;
    for &(_, new_pos, length) in &covers {
        let (new_pos, length) = (new_pos as i64, length as i64);
        let gap = new_pos - prev_new_end;
        if gap > 0 {
            rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, gap - 1));
        }
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, length - 1));
        prev_new_end = new_pos + length;
    }
    let tail_gap = new_data_size - prev_new_end;
    if tail_gap > 0 {
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, tail_gap - 1));
    }

    let rle_code_buf: Vec<u8> = Vec::new();

    // new_data_diff
    let mut new_data_diff = Vec::new();
    let mut prev_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_end {
            new_data_diff.extend_from_slice(&new[prev_end as usize..new_pos as usize]);
        }
        prev_end = new_pos + length;
    }
    if prev_end < new.len() as u64 {
        new_data_diff.extend_from_slice(&new[prev_end as usize..]);
    }

    // Compress each section
    let cover_compressed = zstd::encode_all(cover_buf.as_slice(), 3).unwrap();
    let rle_ctrl_compressed = zstd::encode_all(rle_ctrl_buf.as_slice(), 3).unwrap();
    // rle_code is empty — keep as empty (compress_size = 0)
    let new_data_compressed = zstd::encode_all(new_data_diff.as_slice(), 3).unwrap();

    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF13&zstd\0");
    patch.extend_from_slice(&encode_7bit_varint(new_data_size));
    patch.extend_from_slice(&encode_7bit_varint(old_data_size));
    patch.extend_from_slice(&encode_7bit_varint(cover_count));
    patch.extend_from_slice(&encode_7bit_varint(cover_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(cover_compressed.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(rle_ctrl_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(rle_ctrl_compressed.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(rle_code_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(0)); // rle_code not compressed (empty)
    patch.extend_from_slice(&encode_7bit_varint(new_data_diff.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(new_data_compressed.len() as i64));

    patch.extend_from_slice(&cover_compressed);
    patch.extend_from_slice(&rle_ctrl_compressed);
    // rle_code: 0 bytes (empty, compress_size=0 so reads rle_code_buf_size=0 raw)
    patch.extend_from_slice(&new_data_compressed);

    patch
}

const HDIFF_MIB: usize = 4;

/// End-to-end HDiffPatch throughput: full patch application.
fn bench_hdiffpatch_e2e(c: &mut Criterion) {
    let old = gen_prng_data(0xDEAD, HDIFF_MIB * 1024 * 1024);
    let new = mutate_blocks(&old, 10);
    let patch_v20 = gen_hdiff_v20_patch(&old, &new);
    let patch_v20_zstd = gen_hdiff_v20_patch_zstd(&old, &new);
    let patch_v13 = gen_hdiff_v13_patch(&old, &new);
    let patch_v13_zstd = gen_hdiff_v13_patch_zstd(&old, &new);
    let (patch_v13_rle_add, new_rle_add) = gen_hdiff_v13_rle_patch(&old, 2);
    let (patch_v13_rle_copy, new_rle_copy) = gen_hdiff_v13_rle_patch(&old, 3);

    let dir = tempfile::tempdir().expect("tempdir");
    let old_path = dir.path().join("old.bin");
    let patch_v20_path = dir.path().join("patch_v20.hdiff");
    let patch_v20z_path = dir.path().join("patch_v20_zstd.hdiff");
    let patch_v13_path = dir.path().join("patch_v13.hdiff");
    let patch_v13z_path = dir.path().join("patch_v13_zstd.hdiff");
    let patch_v13_add_path = dir.path().join("patch_v13_rle_add.hdiff");
    let patch_v13_copy_path = dir.path().join("patch_v13_rle_copy.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_v20_path, &patch_v20).unwrap();
    fs::write(&patch_v20z_path, &patch_v20_zstd).unwrap();
    fs::write(&patch_v13_path, &patch_v13).unwrap();
    fs::write(&patch_v13z_path, &patch_v13_zstd).unwrap();
    fs::write(&patch_v13_add_path, &patch_v13_rle_add).unwrap();
    fs::write(&patch_v13_copy_path, &patch_v13_rle_copy).unwrap();

    // Verify all variants produce correct output
    for (name, path) in [
        ("v20_nocomp", &patch_v20_path),
        ("v20_zstd", &patch_v20z_path),
        ("v13_nocomp", &patch_v13_path),
        ("v13_zstd", &patch_v13z_path),
    ] {
        let mut hdiff = HDiff::new(
            old_path.to_string_lossy().to_string(),
            path.to_string_lossy().to_string(),
            out_path.to_string_lossy().to_string(),
        );
        assert!(hdiff.apply(None), "{name} patch verification failed");
        let result = fs::read(&out_path).unwrap();
        assert_eq!(result, new, "{name} patch output mismatch");
    }
    for (name, path, expected) in [
        ("v13_rle_add", &patch_v13_add_path, &new_rle_add),
        ("v13_rle_copy", &patch_v13_copy_path, &new_rle_copy),
    ] {
        let mut hdiff = HDiff::new(
            old_path.to_string_lossy().to_string(),
            path.to_string_lossy().to_string(),
            out_path.to_string_lossy().to_string(),
        );
        assert!(hdiff.apply(None), "{name} patch verification failed");
        assert_eq!(
            fs::read(&out_path).unwrap(),
            *expected,
            "{name} output mismatch"
        );
    }

    let mut group = c.benchmark_group("hdiffpatch_e2e");
    group.throughput(Throughput::Bytes(new.len() as u64));

    for (label, path) in [
        ("v20_nocomp", &patch_v20_path),
        ("v20_zstd", &patch_v20z_path),
        ("v13_nocomp", &patch_v13_path),
        ("v13_zstd", &patch_v13z_path),
    ] {
        group.bench_function(BenchmarkId::new(label, HDIFF_MIB), |b| {
            b.iter(|| {
                let mut hdiff = HDiff::new(
                    old_path.to_string_lossy().to_string(),
                    path.to_string_lossy().to_string(),
                    out_path.to_string_lossy().to_string(),
                );
                assert!(hdiff.apply(None));
            });
        });
    }
    for (label, path) in [
        ("v13_rle_add", &patch_v13_add_path),
        ("v13_rle_copy", &patch_v13_copy_path),
    ] {
        group.bench_function(BenchmarkId::new(label, HDIFF_MIB), |b| {
            b.iter(|| {
                let mut hdiff = HDiff::new(
                    old_path.to_string_lossy().to_string(),
                    path.to_string_lossy().to_string(),
                    out_path.to_string_lossy().to_string(),
                );
                assert!(hdiff.apply(None));
            });
        });
    }

    group.finish();
}

/// Generate a v20 (PatchSF) patch with a full-file RLE XOR run (lenv > 0),
/// exercising the `add_from_source` SIMD path. `run_kind` 2 adds a constant,
/// 3 adds a per-byte delta — mirroring the v13 RLE generators.
fn gen_hdiff_v20_rle_patch(old: &[u8], run_kind: u8) -> (Vec<u8>, Vec<u8>) {
    let run_len = old.len();
    let new = old
        .iter()
        .enumerate()
        .map(|(index, &byte)| {
            if run_kind == 2 {
                byte.wrapping_add(0x37)
            } else {
                byte.wrapping_add((index as u8).wrapping_mul(13).wrapping_add(1))
            }
        })
        .collect::<Vec<_>>();

    let mut cover_data = Vec::new();
    cover_data.extend_from_slice(&encode_tagged_1bit(0, 0));
    cover_data.extend_from_slice(&encode_7bit_varint(0));
    cover_data.extend_from_slice(&encode_7bit_varint(run_len as i64));

    // The v20 RLE0 format stores actual XOR bytes in the RLE buffer, not a
    // single repeated value. lenv = run_len means run_len bytes of XOR data.
    let mut rle_data = Vec::new();
    rle_data.extend_from_slice(&encode_7bit_varint(0));
    rle_data.extend_from_slice(&encode_7bit_varint(run_len as i64));
    if run_kind == 2 {
        // Constant add: every byte is 0x37.
        rle_data.resize(rle_data.len() + run_len, 0x37);
    } else {
        // Per-byte delta: XOR value for each position.
        for i in 0..run_len {
            rle_data.push((new[i]).wrapping_sub(old[i]));
        }
    }

    let step_mem_size = (cover_data.len() + rle_data.len()) as i64;
    let mut diff_stream = Vec::new();
    diff_stream.extend_from_slice(&encode_7bit_varint(cover_data.len() as i64));
    diff_stream.extend_from_slice(&encode_7bit_varint(rle_data.len() as i64));
    diff_stream.extend_from_slice(&cover_data);
    diff_stream.extend_from_slice(&rle_data);

    let mut patch = b"HDIFF20&nocomp\0".to_vec();
    for value in [
        new.len() as i64,
        old.len() as i64,
        1i64,
        step_mem_size,
        diff_stream.len() as i64,
        0,
    ] {
        patch.extend_from_slice(&encode_7bit_varint(value));
    }
    patch.extend_from_slice(&diff_stream);
    (patch, new)
}

/// Focused RLE-only throughput at larger sizes to isolate the SIMD-accelerated
/// wrapping-add loops from I/O and decompression overhead.
fn bench_hdiffpatch_rle_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("hdiffpatch_rle");
    for &size_mib in &[16usize, 64usize] {
        let old = gen_prng_data(0xBEEF + size_mib as u64, size_mib * 1024 * 1024);
        let (patch_rle_add, _) = gen_hdiff_v13_rle_patch(&old, 2);
        let (patch_rle_copy, _) = gen_hdiff_v13_rle_patch(&old, 3);

        let dir = tempfile::tempdir().expect("tempdir");
        let old_path = dir.path().join("old.bin");
        let add_path = dir.path().join("rle_add.hdiff");
        let copy_path = dir.path().join("rle_copy.hdiff");
        let out_path = dir.path().join("out.bin");
        fs::write(&old_path, &old).unwrap();
        fs::write(&add_path, &patch_rle_add).unwrap();
        fs::write(&copy_path, &patch_rle_copy).unwrap();

        for (label, path) in [("rle_add", &add_path), ("rle_copy", &copy_path)] {
            group.throughput(Throughput::Bytes(old.len() as u64));
            group.bench_function(BenchmarkId::new(label, size_mib), |b| {
                b.iter(|| {
                    let mut hdiff = HDiff::new(
                        old_path.to_string_lossy().to_string(),
                        path.to_string_lossy().to_string(),
                        out_path.to_string_lossy().to_string(),
                    );
                    assert!(hdiff.apply(None));
                });
            });
        }
    }
    // v20 RLE0 format stores actual XOR bytes (no set-constant mode), so the
    // patch size equals the data size. Use 8 MiB to stay under the 16 MiB
    // single-step limit (MAX_HDIFF20_STEP_SIZE).
    for &size_mib in &[8usize] {
        let old = gen_prng_data(0xF00D + size_mib as u64, size_mib * 1024 * 1024);
        let (patch_v20_rle_add, _) = gen_hdiff_v20_rle_patch(&old, 2);
        let (patch_v20_rle_copy, _) = gen_hdiff_v20_rle_patch(&old, 3);

        let dir = tempfile::tempdir().expect("tempdir");
        let old_path = dir.path().join("old.bin");
        let v20_add_path = dir.path().join("v20_rle_add.hdiff");
        let v20_copy_path = dir.path().join("v20_rle_copy.hdiff");
        let out_path = dir.path().join("out.bin");
        fs::write(&old_path, &old).unwrap();
        fs::write(&v20_add_path, &patch_v20_rle_add).unwrap();
        fs::write(&v20_copy_path, &patch_v20_rle_copy).unwrap();

        for (label, path) in [
            ("v20_rle_add", &v20_add_path),
            ("v20_rle_copy", &v20_copy_path),
        ] {
            group.throughput(Throughput::Bytes(old.len() as u64));
            group.bench_function(BenchmarkId::new(label, size_mib), |b| {
                b.iter(|| {
                    let mut hdiff = HDiff::new(
                        old_path.to_string_lossy().to_string(),
                        path.to_string_lossy().to_string(),
                        out_path.to_string_lossy().to_string(),
                    );
                    assert!(hdiff.apply(None));
                });
            });
        }
    }
    group.finish();
}

fn bench_file_md5_buffer_capacity_optional(c: &mut Criterion) {
    #[cfg(feature = "bench-internals")]
    bench_file_md5_buffer_capacity(c);
    #[cfg(not(feature = "bench-internals"))]
    let _ = c;
}

fn bench_md5_multibuffer_optional(c: &mut Criterion) {
    #[cfg(feature = "bench-internals")]
    bench_md5_multibuffer(c);
    #[cfg(not(feature = "bench-internals"))]
    let _ = c;
}

fn bench_phase2a_chunk_verify_optional(c: &mut Criterion) {
    #[cfg(feature = "bench-internals")]
    bench_phase2a_chunk_verify(c);
    #[cfg(not(feature = "bench-internals"))]
    let _ = c;
}

fn bench_assembly_hash_gate_optional(c: &mut Criterion) {
    #[cfg(feature = "bench-internals")]
    bench_assembly_hash_gate(c);
    #[cfg(not(feature = "bench-internals"))]
    let _ = c;
}

fn bench_file_metadata_cost_optional(c: &mut Criterion) {
    #[cfg(feature = "bench-internals")]
    bench_file_metadata_cost(c);
    #[cfg(not(feature = "bench-internals"))]
    let _ = c;
}

/// Planning fixtures: 8 installers x 25k assets with realistic (~95 byte)
/// asset names, so the attribution cost mirrors a real manifest.
fn planner_fixture_installers() -> Vec<SophonInstaller> {
    use irmin_rs::api_scrape::{Compression, DownloadInfo};
    use irmin_rs::proto_parse::{SophonManifestAssetProperty, SophonManifestProto};

    let asset = |i: usize, j: usize| {
        SophonManifestAssetProperty::from_parts(
            format!(
                "StarRail_Data/StreamingAssets/AssetBundles/characters/{i:02}/{j:06}/character_{j:06}_bundle_v2_win.pck"
            )
            .into(),
            vec![],
            0,
            0,
            bytes::Bytes::new(),
        )
    };
    (0..8)
        .map(|i| SophonInstaller {
            manifest: SophonManifestProto {
                assets: (0..25_000).map(|j| asset(i, j)).collect(),
            },
            chunk_download: DownloadInfo {
                encryption: 0,
                password: String::new(),
                compression: Compression::None,
                url_prefix: format!("https://cdn{i}.example.com/"),
                url_suffix: "chunks".into(),
            },
            label: format!("inst{i}"),
            matching_field: format!("field{i}"),
            manifest_hash: format!("hash{i}"),
        })
        .collect()
}

/// Paired measurement of the two planning strategies on identical inputs:
/// the legacy name-keyed attribution map vs the positional per-installer
/// accounting now used in production. The legacy replica lives in the lib
/// so both run in the same binary against the same fixture state.
fn bench_planner_attribution(c: &mut Criterion) {
    use irmin_rs::game_installer::installer::bench_internals;

    const ASSETS: u64 = 8 * 25_000;
    let game_dir = tempfile::tempdir().expect("tempdir");
    let hkrpg_blacklist_dir = tempfile::tempdir().expect("tempdir");
    {
        let persistent = hkrpg_blacklist_dir.path().join("StarRail_Data/Persistent");
        std::fs::create_dir_all(&persistent).expect("create persistent dir");
        let blacklist: String = (0..300)
            .map(|i| {
                format!(
                    "{{\"fileName\":\"StarRail_Data/StreamingAssets/AudioLaucher/voice_file_{i:03}.pck\"}}\n"
                )
            })
            .collect();
        std::fs::write(persistent.join("DownloadBlacklist.json"), blacklist)
            .expect("write blacklist");
    }
    let mut group = c.benchmark_group("planner_attribution");
    group.throughput(Throughput::Elements(ASSETS));

    let plan_args = |installers: Vec<SophonInstaller>, game_code: &str, dir: &Path| {
        (
            installers,
            game_code.to_string(),
            dir.to_path_buf(),
            Vec::<String>::new(),
        )
    };
    for (label, game_code, dir) in [
        ("unknown_game", "unknown", game_dir.path().to_path_buf()),
        ("hkrpg_blacklist", "hkrpg", hkrpg_blacklist_dir.path().to_path_buf()),
    ] {
        group.bench_function(BenchmarkId::new("legacy_name_map", label), |b| {
            b.iter_batched(
                || plan_args(planner_fixture_installers(), game_code, &dir),
                |(installers, game_code, dir, langs)| {
                    let counts = bench_internals::legacy_plan_counts(
                        installers, &game_code, &dir, &langs,
                    );
                    std::hint::black_box(counts);
                },
                BatchSize::PerIteration,
            )
        });
        group.bench_function(BenchmarkId::new("positional_segments", label), |b| {
            b.iter_batched(
                || plan_args(planner_fixture_installers(), game_code, &dir),
                |(installers, game_code, dir, langs)| {
                    let counts = bench_internals::positional_plan_counts(
                        installers, &game_code, &dir, &langs,
                    );
                    std::hint::black_box(counts);
                },
                BatchSize::PerIteration,
            )
        });
    }

    group.finish();
}

/// Paired measurement of preinstall chunk-completion tracking: the frozen
/// DashMap shape (String-keyed insert, full-map name clone on every
/// periodic save) vs the dense ChunkSizes shape (atomic mark, borrowed-name
/// snapshot). Map fullness grows over a download, so the save is measured
/// at the terminal size where the legacy clone is most expensive. Both
/// saves are timed including the drop of their output, which is the true
/// per-save cost each shape pays in production (the legacy snapshot's
/// 200k-String drop is real legacy cost, not bench noise).
#[cfg(feature = "benchmark")]
fn bench_assembly_small_old_chunk_hash(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly::{
        benchmark_write_from_old_file_hashed, legacy_small_chunk_hash_whole_file_mmap,
    };
    use std::io::Write;

    // The small-chunk hashing path (< 1 MiB) used to fstat + map the whole
    // output file per chunk just to hash the region it wrote. The map cost
    // scales with the output file's length, so the fixture uses a large
    // sparse output (set_len) with real source data and rotates offsets
    // so every iteration copies and hashes fresh bytes. Fixture caveats:
    // on btrfs (this repo's bench TMPDIR, compress=zstd:1) copy_file_range
    // may reflink instead of splicing, so the readback can fault in from
    // the shared extent; both arms see this equally, so the comparison
    // stays fair. The 64 MiB source is page-cache-warm after the first
    // iteration for both arms.
    const OUT_LEN: u64 = 1 << 30; // 1 GiB sparse output
    const SRC_LEN: usize = 64 * 1024 * 1024; // 64 MiB real source
    const CHUNKS_PER_SAMPLE: usize = 256;

    let dir = tempfile::tempdir().expect("tempdir");
    let old_path = dir.path().join("old.bin");
    let out_path = dir.path().join("out.bin");
    {
        let mut old = fs::File::create(&old_path).expect("create old");
        let block_len = 1024 * 1024;
        // Deterministic LCG filler; each block is seeded from its index so
        // the source has no cross-block repetition for hashing to window.
        for block_idx in 0..(SRC_LEN / block_len) {
            let mut seeded = vec![0u8; block_len];
            let mut state: u64 = 42 + block_idx as u64;
            for b in &mut seeded {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                *b = (state >> 33) as u8;
            }
            old.write_all(&seeded).expect("write old");
        }
        old.sync_all().expect("sync old");
    }
    let old_file = fs::File::open(&old_path).expect("open old");
    let out_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&out_path)
        .expect("open out");
    out_file.set_len(OUT_LEN).expect("size out");

    let mut buffer = vec![0u8; irmin_rs::game_installer::FILE_WRITE_BUFFER_SIZE];

    let mut group = c.benchmark_group("assembly_small_old_chunk_hash");

    for &chunk_kib in &[64usize, 512] {
        let chunk_len = chunk_kib * 1024;
        group.throughput(Throughput::Bytes((chunk_len * CHUNKS_PER_SAMPLE) as u64));
        // Rotating destinations so each iteration covers a different region
        // of the sparse output; sources rotate inside the real old file.
        let dests: Vec<u64> = (0..CHUNKS_PER_SAMPLE)
            .map(|i| {
                ((i as u64) * chunk_len as u64 * 7 + 17) % (OUT_LEN - chunk_len as u64 * 8)
            })
            .collect();
        let srcs: Vec<u64> = (0..CHUNKS_PER_SAMPLE)
            .map(|i| ((i as u64) * chunk_len as u64 * 3) % (SRC_LEN as u64 - chunk_len as u64))
            .collect();

        // The chunk md5 must match the actual source bytes for the hash
        // check to pass: recompute per source region.
        let region: Vec<u8> = vec![0u8; chunk_len];
        let mut md5s: Vec<Vec<u8>> = Vec::with_capacity(CHUNKS_PER_SAMPLE);
        for &src in &srcs {
            let mut r = region.clone();
            old_file.read_exact_at(&mut r, src).expect("read region");
            md5s.push(fast_md5::digest(&r).to_vec());
        }

        group.bench_function(BenchmarkId::new("legacy_whole_file_mmap", chunk_kib), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = std::time::Instant::now();
                    for i in 0..CHUNKS_PER_SAMPLE {
                        legacy_small_chunk_hash_whole_file_mmap(
                            &old_file,
                            &old_path,
                            &out_file,
                            dests[i],
                            srcs[i],
                            chunk_len as u64,
                            &md5s[i],
                        )
                        .expect("legacy copy+hash");
                    }
                    total += start.elapsed();
                }
                total
            })
        });

        group.bench_function(BenchmarkId::new("readback_buffer", chunk_kib), |b| {
            b.iter_custom(|iters| {
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let start = std::time::Instant::now();
                    for i in 0..CHUNKS_PER_SAMPLE {
                        benchmark_write_from_old_file_hashed(
                            &old_file,
                            &old_path,
                            &out_file,
                            dests[i],
                            srcs[i],
                            chunk_len as u64,
                            &mut buffer,
                            &md5s[i],
                        )
                        .expect("readback copy+hash");
                    }
                    total += start.elapsed();
                }
                total
            })
        });
    }

    group.finish();
}

#[cfg(feature = "benchmark")]
fn bench_chunk_bitmap_sync(c: &mut Criterion) {
    use irmin_rs::game_installer::chunk_bitmap::{legacy_truncate_sync_replica, ChunkBitmap};

    // 100k chunks = 12.5 KiB body. The periodic saver syncs every 500
    // chunk completions, so each round marks 500 fresh bits then syncs
    // once. Marks accumulate across rounds on the shared instance; both
    // arms read the same live words, so body content evolves identically
    // (and as in production). Body size is deliberately small: the
    // legacy cost is dominated by the open+truncate+close cycle and the
    // extent churn truncate causes on copy-on-write filesystems, not the
    // data bytes.
    const TOTAL_CHUNKS: usize = 100_000;
    const MARKS_PER_ITER: usize = 500;
    const SYNC_ROUNDS: usize = 8;

    let dir = tempfile::tempdir().expect("tempdir");

    let mut group = c.benchmark_group("chunk_bitmap_sync");
    group.throughput(Throughput::Elements(SYNC_ROUNDS as u64));

    group.bench_function("legacy_truncate_reopen", |b| {
        b.iter_batched(
            || {
                let path = dir.path().join(format!(
                    "legacy-{}.bitmap",
                    std::process::id()
                ));
                let bm = ChunkBitmap::create(&path, TOTAL_CHUNKS).unwrap();
                (path, bm)
            },
            |(path, bm)| {
                for round in 0..SYNC_ROUNDS {
                    let base = round * MARKS_PER_ITER;
                    for i in 0..MARKS_PER_ITER {
                        bm.mark_complete(base + i);
                    }
                    // The replica reads the live words directly, matching
                    // what the in-place arm syncs.
                    legacy_truncate_sync_replica(&path, bm.bits_for_benchmark(), TOTAL_CHUNKS)
                        .unwrap();
                }
                std::hint::black_box(&bm);
                let _ = std::fs::remove_file(&path);
            },
            BatchSize::PerIteration,
        )
    });

    group.bench_function("in_place", |b| {
        b.iter_batched(
            || {
                let path = dir.path().join(format!(
                    "inplace-{}.bitmap",
                    std::process::id()
                ));
                let bm = ChunkBitmap::create(&path, TOTAL_CHUNKS).unwrap();
                (path, bm)
            },
            |(path, bm)| {
                for round in 0..SYNC_ROUNDS {
                    let base = round * MARKS_PER_ITER;
                    for i in 0..MARKS_PER_ITER {
                        bm.mark_complete(base + i);
                    }
                    bm.sync().unwrap();
                }
                std::hint::black_box(&bm);
                let _ = std::fs::remove_file(&path);
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

#[cfg(feature = "benchmark")]
fn bench_eager_stream_input_chain(c: &mut Criterion) {
    use irmin_rs::game_installer::eager_decompress::{
        legacy_stream_decompress_double_buffered, stream_decompress_to_file,
    };

    // The streaming eager path used to feed zstd through TWO stacked
    // 256 KiB BufReaders (installer-side outer + entry-point inner), at
    // ~3 memcpys per compressed byte and two transient >mmap-threshold
    // allocations per chunk. The paired arms below differ ONLY in that
    // copy sandwich around identical zstd work. The source yields
    // frame-sized slices (like the mpsc frames the production reader
    // hands out) so zstd's internal input staging behaves realistically;
    // a whole-buffer Cursor would hide the staging cost and overstate
    // the difference.
    const DATA_MIB: usize = 64;
    const FRAME_KIB: usize = 64;

    let mut data = vec![0u8; DATA_MIB * 1024 * 1024];
    let mut seed: u64 = 0xF00D;
    for b in data.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (seed >> 33) as u8;
    }
    let compressed = zstd::encode_all(std::io::Cursor::new(&data), 3).unwrap();
    drop(data);
    let decompressed_hash = fast_md5::digest(
        &zstd::decode_all(std::io::Cursor::new(&compressed)).unwrap(),
    )
    .to_vec();

    /// Yields the compressed stream in FRAME_KIB slices, like the
    /// channel frames production feeds the chunk reader.
    struct FrameReader<'a> {
        bytes: &'a [u8],
        offset: usize,
    }

    impl std::io::BufRead for FrameReader<'_> {
        fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
            let end = (self.offset + FRAME_KIB * 1024).min(self.bytes.len());
            Ok(&self.bytes[self.offset..end])
        }

        fn consume(&mut self, amt: usize) {
            self.offset += amt.min(self.bytes.len() - self.offset);
        }
    }

    impl std::io::Read for FrameReader<'_> {
        fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
            if output.is_empty() {
                return Ok(0);
            }
            let filled = std::io::BufRead::fill_buf(self)?;
            let count = output.len().min(filled.len());
            output[..count].copy_from_slice(&filled[..count]);
            std::io::BufRead::consume(self, count);
            Ok(count)
        }
    }


    let mut group = c.benchmark_group("eager_stream_input_chain");
    group.throughput(Throughput::Bytes(DATA_MIB as u64 * 1024 * 1024));

    let out_dir = tempfile::tempdir().expect("tempdir");
    let out_path = out_dir.path().join("out.bin");
    let open_output = || {
        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .expect("open output");
        out_file
            .set_len(DATA_MIB as u64 * 1024 * 1024)
            .expect("size output");
        out_file
    };

    group.bench_function("legacy_double_buffered", |b| {
        b.iter(|| {
            // The old production shape minus the channel: outer 256 KiB
            // BufReader + the replica's internal 256 KiB BufReader.
            let reader = std::io::BufReader::with_capacity(
                256 * 1024,
                FrameReader {
                    bytes: &compressed,
                    offset: 0,
                },
            );
            legacy_stream_decompress_double_buffered(
                reader,
                &open_output(),
                0,
                DATA_MIB as u64 * 1024 * 1024,
                &decompressed_hash,
            )
            .expect("legacy decode")
        });
    });

    group.bench_function("native_bufread", |b| {
        b.iter(|| {
            stream_decompress_to_file(
                FrameReader {
                    bytes: &compressed,
                    offset: 0,
                },
                &open_output(),
                0,
                DATA_MIB as u64 * 1024 * 1024,
                &decompressed_hash,
            )
            .expect("native decode")
        });
    });

    group.finish();
}

#[cfg(feature = "bench-internals")]
fn bench_preinstall_chunk_tracking(c: &mut Criterion) {
    use dashmap::DashMap;
    use irmin_rs::game_installer::preinstall::bench_internals::{
        ChunkSizes, legacy_dashmap_snapshot, save_state_snapshot,
    };
    use irmin_rs::game_installer::installer::StateSaver;
    use irmin_rs::game_installer::preinstall::PatchChunkInfo;

    const COUNT: usize = 200_000;
    let names: Vec<String> = (0..COUNT)
        .map(|i| format!("patch-chunk-{i:08}.bin"))
        .collect();
    let saver: StateSaver = Arc::new(|_entries: &[(&str, u64)]| {});

    let mut group = c.benchmark_group("preinstall_chunk_tracking");
    group.throughput(Throughput::Elements(COUNT as u64));

    group.bench_function("legacy_save", |b| {
        b.iter_batched(
            || {
                let map: DashMap<String, u64> = DashMap::with_capacity(COUNT);
                for (i, name) in names.iter().enumerate() {
                    map.insert(name.clone(), 256 * 1024);
                    let _ = i;
                }
                map
            },
            |map| {
                let snapshot = legacy_dashmap_snapshot(&map);
                std::hint::black_box(&snapshot);
            },
            BatchSize::PerIteration,
        )
    });

    group.bench_function("dense_save", |b| {
        b.iter_batched(
            || {
                let sizes = ChunkSizes::new(COUNT);
                for i in 0..COUNT {
                    sizes.mark(i, 256 * 1024);
                }
                sizes
            },
            |sizes| {
                save_state_snapshot(
                    &sizes,
                    &(0..COUNT)
                        .map(|i| PatchChunkInfo {
                            patch_name: names[i].clone(),
                            patch_size: 256 * 1024,
                            patch_md5: String::new(),
                            matching_field: String::new(),
                        })
                        .collect::<Vec<_>>(),
                    &saver,
                );
            },
            BatchSize::PerIteration,
        )
    });

    group.bench_function("legacy_record", |b| {
        b.iter_batched(
            || DashMap::<String, u64>::with_capacity(COUNT),
            |map| {
                for name in &names {
                    map.insert(name.clone(), 256 * 1024);
                }
                std::hint::black_box(&map);
            },
            BatchSize::PerIteration,
        )
    });

    group.bench_function("dense_record", |b| {
        b.iter_batched(
            || ChunkSizes::new(COUNT),
            |sizes| {
                for i in 0..COUNT {
                    sizes.mark(i, 256 * 1024);
                }
                std::hint::black_box(&sizes);
            },
            BatchSize::PerIteration,
        )
    });

    group.finish();
}

#[cfg(not(feature = "bench-internals"))]
fn bench_preinstall_chunk_tracking(c: &mut Criterion) {
    let _ = c;
}

#[cfg(feature = "bench-internals")]
fn bench_preinstall_save_offload(c: &mut Criterion) {
    use irmin_rs::game_installer::installer::{SaveSlot, StateSaver};
    use irmin_rs::game_installer::preinstall::bench_internals::{ChunkSizes, save_state_snapshot};
    use irmin_rs::game_installer::preinstall::PatchChunkInfo;
    use irmin_rs::sophon::bench_write_state_file;

    // 50k entries keeps blocking-pool background work bounded while the
    // inline-vs-handoff gap stays orders of magnitude wide.
    const COUNT: usize = 50_000;
    let unique_chunks: std::sync::Arc<[PatchChunkInfo]> = (0..COUNT)
        .map(|i| PatchChunkInfo {
            patch_name: format!("patch-chunk-{i:08}.bin"),
            patch_size: 256 * 1024,
            patch_md5: String::new(),
            matching_field: String::new(),
        })
        .collect();
    let sizes = ChunkSizes::new(COUNT);
    for i in 0..COUNT {
        sizes.mark(i, 256 * 1024);
    }
    let state_dir = tempfile::tempdir().expect("tempdir");
    let state_file = state_dir.path().join("state.json");
    let saver: StateSaver = {
        let state_file = state_file.clone();
        Arc::new(move |entries: &[(&str, u64)]| {
            bench_write_state_file(&state_file, entries);
        })
    };

    let mut group = c.benchmark_group("preinstall_save_offload");
    group.throughput(Throughput::Elements(COUNT as u64));

    // Legacy shape: the async caller runs the full serialize-and-rename
    // itself, so the worker thread is blocked for the entire save.
    group.bench_function("legacy_inline", |b| {
        b.iter(|| {
            save_state_snapshot(&sizes, &unique_chunks, &saver);
        })
    });

    // New shape: the caller only hands the save to the blocking pool.
    // The handle is drained outside the timed region so the pool is
    // quiescent for every iteration and the measurement is the pure
    // async-side handoff (lock + spawn), not the save itself. This is a
    // stall-removal metric (async-side cost per save), not total save
    // throughput; the runtime is built once per sample, outside the
    // timed region.
    group.bench_function("offloaded_handoff", |b| {
        b.iter_custom(|iters| {
            // trigger() calls spawn_blocking, which needs a runtime
            // context; criterion's thread has none. A dedicated
            // current-thread runtime drives the handoff and the drain.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bench runtime");
            runtime.block_on(async {
                let slot = SaveSlot::new();
                // The save is read-only over the sizes, so one populated
                // payload serves every iteration; triggers hand off a clone.
                let payload = std::sync::Arc::new(ChunkSizes::new(COUNT));
                for i in 0..COUNT {
                    payload.mark(i, 256 * 1024);
                }
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let sizes = std::sync::Arc::clone(&payload);
                    let chunks = std::sync::Arc::clone(&unique_chunks);
                    let saver = Arc::clone(&saver);
                    let start = std::time::Instant::now();
                    slot.trigger(move || {
                        save_state_snapshot(&sizes, &chunks, &saver);
                    });
                    total += start.elapsed();
                    // Untimed: quiesce the pool so the next timed handoff
                    // measures a real spawn, not a coalescing skip.
                    slot.drain().await;
                }
                total
            })
        })
    });

    group.finish();
}

#[cfg(not(feature = "bench-internals"))]
fn bench_preinstall_save_offload(c: &mut Criterion) {
    let _ = c;
}

#[cfg(feature = "bench-internals")]
fn bench_installer_save_offload(c: &mut Criterion) {
    use std::sync::OnceLock;

    use irmin_rs::game_installer::compact_manifest::StringArena;
    use irmin_rs::game_installer::installer::bench_internals::{
        legacy_await_then_spawn_save, snapshot_downloaded_chunks, ChunkNameLookup, SaveSlot,
    };
    use irmin_rs::game_installer::installer::StateSaver;
    use irmin_rs::sophon::bench_write_state_file;
    use std::sync::atomic::{AtomicU32, AtomicUsize};

    // 50k entries keeps blocking-pool background work bounded while the
    // caller-cost gap stays orders of magnitude wide.
    const COUNT: usize = 50_000;
    const WAVE_WORKERS: usize = 8;

    let names: Vec<String> = (0..COUNT).map(|i| format!("chunk-{i:08}.zstd")).collect();
    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let arena = StringArena::from(name_refs.as_slice());
    let lookup = Arc::new(ChunkNameLookup::from_arena(arena));
    let downloaded: Arc<OnceLock<Vec<AtomicU32>>> = Arc::new(OnceLock::new());
    assert!(
        downloaded
            .set((0..COUNT).map(|_| AtomicU32::new(256 * 1024)).collect())
            .is_ok()
    );
    let save_chunk_names: Arc<OnceLock<Arc<ChunkNameLookup>>> = Arc::new(OnceLock::new());
    // is_ok() avoids the Debug bound that ChunkNameLookup does not carry.
    assert!(save_chunk_names.set(Arc::clone(&lookup)).is_ok());

    let state_dir = tempfile::tempdir().expect("tempdir");
    let state_file = state_dir.path().join("state.json");
    let saves = Arc::new(AtomicUsize::new(0));
    let saver: StateSaver = {
        let state_file = state_file.clone();
        let saves = Arc::clone(&saves);
        Arc::new(move |entries: &[(&str, u64)]| {
            saves.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            bench_write_state_file(&state_file, entries);
        })
    };

    let mut group = c.benchmark_group("installer_save_offload");
    group.throughput(Throughput::Elements(COUNT as u64));

    // Sequential boundary hits: the legacy caller awaits the previous save
    // (a full serialize-and-rename) before spawning its own, so its
    // per-boundary cost is one whole save; the coalesced caller only hands
    // off. Drains are untimed and keep the blocking pool quiescent.
    group.bench_function("legacy_await_prev", |b| {
        b.iter_custom(|iters| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bench runtime");
            runtime.block_on(async {
                let last_save: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>> =
                    std::sync::Mutex::new(None);
                let mut total = std::time::Duration::ZERO;
                let saves_at_start = saves.load(std::sync::atomic::Ordering::Relaxed);
                for _ in 0..iters {
                    let start = std::time::Instant::now();
                    legacy_await_then_spawn_save(&last_save, &downloaded, &save_chunk_names, &saver)
                        .await;
                    total += start.elapsed();
                }
                // Untimed final drain. Timed region = await of the
                // previous iteration's real save + spawn of this one;
                // this is the production caller stall, not the cost of
                // a save.
                if let Some(handle) = last_save.lock().unwrap().take() {
                    let _ = handle.await;
                }
                // Each iteration awaits the previous save and the final
                // drain completes the last one, so exactly one real save
                // must have run per boundary.
                assert_eq!(
                    saves.load(std::sync::atomic::Ordering::Relaxed) - saves_at_start,
                    iters as usize,
                    "legacy arm must run one real save per boundary"
                );
                total
            })
        })
    });

    group.bench_function("coalesced_trigger", |b| {
        b.iter_custom(|iters| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bench runtime");
            runtime.block_on(async {
                let slot = SaveSlot::new();
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let saves_before = saves.load(std::sync::atomic::Ordering::Relaxed);
                    let dc = Arc::clone(&downloaded);
                    let names = Arc::clone(&save_chunk_names);
                    let saver = Arc::clone(&saver);
                    let start = std::time::Instant::now();
                    slot.trigger(move || {
                        let entries = if let (Some(dc), Some(names)) = (dc.get(), names.get()) {
                            snapshot_downloaded_chunks(dc, names)
                        } else {
                            Vec::new()
                        };
                        saver(&entries);
                    });
                    total += start.elapsed();
                    // Untimed: quiesce the pool so every timed trigger
                    // measures a real spawn, not a coalescing skip.
                    slot.drain().await;
                    // The quiesced save must have run for every sample.
                    assert!(
                        saves.load(std::sync::atomic::Ordering::Relaxed) > saves_before,
                        "coalesced arm must run a real save per trigger"
                    );
                }
                total
            })
        })
    });

    // A wave of concurrent boundary hits — the overlap scenario the slot
    // removes. Each iteration times one full wave (spawn + await of all
    // wave tasks) and then drains untimed; the save counter is consumed so
    // criterion cannot optimize it away. The wall time shows the save work
    // is moved off the caller, not hidden. The legacy arm drains only the
    // last handle (matching production legacy semantics), so detached
    // earlier saves may still run into the next wave — an overlap the
    // legacy shape tolerated, at a ~500x gap that cannot mask it.
    group.bench_function("legacy_concurrent_8", |b| {
        b.iter_custom(|iters| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bench runtime");
            runtime.block_on(async {
                let last_save: std::sync::Arc<std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>> =
                    Arc::new(std::sync::Mutex::new(None));
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let saves_before = saves.load(std::sync::atomic::Ordering::Relaxed);
                    let start = std::time::Instant::now();
                    let mut wave = tokio::task::JoinSet::new();
                    for _ in 0..WAVE_WORKERS {
                        let last_save = Arc::clone(&last_save);
                        let dc = Arc::clone(&downloaded);
                        let names = Arc::clone(&save_chunk_names);
                        let saver = Arc::clone(&saver);
                        wave.spawn(async move {
                            legacy_await_then_spawn_save(&last_save, &dc, &names, &saver).await;
                        });
                    }
                    while wave.join_next().await.is_some() {}
                    total += start.elapsed();
                    if let Some(handle) = last_save.lock().unwrap().take() {
                        let _ = handle.await;
                    }
                    // The wave must have produced real saves.
                    assert!(
                        saves.load(std::sync::atomic::Ordering::Relaxed) > saves_before,
                        "legacy concurrent arm must run real saves"
                    );
                }
                total
            })
        })
    });

    group.bench_function("coalesced_concurrent_8", |b| {
        b.iter_custom(|iters| {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("bench runtime");
            runtime.block_on(async {
                let slot = Arc::new(SaveSlot::new());
                let mut total = std::time::Duration::ZERO;
                for _ in 0..iters {
                    let saves_before = saves.load(std::sync::atomic::Ordering::Relaxed);
                    let start = std::time::Instant::now();
                    let mut wave = tokio::task::JoinSet::new();
                    for _ in 0..WAVE_WORKERS {
                        let slot = Arc::clone(&slot);
                        let dc = Arc::clone(&downloaded);
                        let names = Arc::clone(&save_chunk_names);
                        let saver = Arc::clone(&saver);
                        wave.spawn(async move {
                            slot.trigger(move || {
                                let entries =
                                    if let (Some(dc), Some(names)) = (dc.get(), names.get()) {
                                        snapshot_downloaded_chunks(dc, names)
                                    } else {
                                        Vec::new()
                                    };
                                saver(&entries);
                            });
                        });
                    }
                    while wave.join_next().await.is_some() {}
                    total += start.elapsed();
                    slot.drain().await;
                    // The wave's coalesced save must have run.
                    assert!(
                        saves.load(std::sync::atomic::Ordering::Relaxed) > saves_before,
                        "coalesced concurrent arm must run a real save per wave"
                    );
                }
                total
            })
        })
    });

    group.finish();
}

#[cfg(not(feature = "bench-internals"))]
fn bench_installer_save_offload(c: &mut Criterion) {
    let _ = c;
}

#[cfg(feature = "bench-internals")]
fn bench_update_diff_sizes(c: &mut Criterion) {
    use bytes::Bytes;
    use irmin_rs::game_installer::update::benchmark;
    use irmin_rs::proto_parse::{
        SophonManifestAssetChunk, SophonManifestAssetProperty, SophonManifestProto,
    };

    let files = 5000usize;
    let chunks_per = 50usize;

    let make_file = |idx: usize, is_old: bool| {
        let chunks: Vec<SophonManifestAssetChunk> = (0..chunks_per)
            .map(|c| {
                let hash = if is_old {
                    format!("hash-{:04}", c % 32)
                } else {
                    format!("hash-{idx:06}-{c:04}")
                };
                SophonManifestAssetChunk::from_parts(
                    Bytes::from(format!("chunk-{idx:06}-{c:04}")),
                    Bytes::from(hash),
                    (c * 1024 * 1024) as u64,
                    256 * 1024,
                    1024 * 1024,
                    0,
                    Bytes::new(),
                    -1,
                )
            })
            .collect();
        SophonManifestAssetProperty::from_parts(
            Bytes::from(format!("asset-{idx:06}.pak")),
            chunks,
            0,
            (chunks_per * 1024 * 1024) as u64,
            Bytes::from(format!("file-{idx:06}")),
        )
    };

    let old_manifest = SophonManifestProto {
        assets: (0..files).map(|i| make_file(i, true)).collect(),
    };
    let new_manifest = SophonManifestProto {
        assets: (0..files).map(|i| make_file(i, false)).collect(),
    };

    let old_files_md5: rustc_hash::FxHashMap<&str, &str> = old_manifest
        .assets
        .iter()
        .filter(|f| !f.is_directory())
        .map(|f| (f.asset_name_str(), f.asset_hash_md5_str()))
        .collect();
    let old_chunks: rustc_hash::FxHashSet<&str> = old_manifest
        .assets
        .iter()
        .filter(|f| !f.is_directory())
        .flat_map(|f| f.asset_chunks.iter())
        .map(|c| c.chunk_decompressed_hash_md5_str())
        .collect();

    let mut group = c.benchmark_group("update_diff_sizes");
    group.throughput(Throughput::Elements(files as u64));
    group.bench_function("5000x50", |b| {
        b.iter(|| {
            let result = benchmark::run_diff_sizes(&old_files_md5, &old_chunks, &new_manifest);
            std::hint::black_box((result.compressed, result.decompressed));
        });
    });
    group.finish();
}

#[cfg(not(feature = "bench-internals"))]
fn bench_update_diff_sizes(c: &mut Criterion) {
    let _ = c;
}

criterion_group!(
    benches,
    bench_io_copy,
    bench_md5,
    bench_md5_resident,
    bench_md5_multibuffer_optional,
    bench_phase2a_chunk_verify_optional,
    bench_assembly_hash_gate_optional,
    bench_zstd_decompress,
    bench_lz4_deflate_decompress,
    bench_assembly_e2e,
    bench_assembly_many_chunks,
    bench_assembly_epoch_durability,
    bench_assembly_old_chunk_reuse,
    bench_assembly_small_old_chunk_hash,
    bench_chunk_bitmap_sync,
    bench_verify_file_md5,
    bench_xxh64,
    bench_zstd_oneshot,
    bench_chunk_decompress_production,
    bench_compact_manifest_build,
    bench_chunk_accessors,
    bench_chunk_lookup,
    bench_varint_decode,
    bench_cover_stream_decode,
    bench_protobuf_decode,
    bench_manifest_content_hash,
    bench_buffer_pool,
    bench_hdiffpatch_e2e,
    bench_hdiffpatch_rle_throughput,
    bench_v20_cover_shapes,
    bench_v20_step_buffer_stress,
    bench_v13_large_uncovered_prefix,
    bench_eager_decompress,
    bench_eager_stream_input_chain,
    bench_file_md5_buffer_capacity_optional,
    bench_file_metadata_cost_optional,
    bench_update_diff_sizes,
    bench_download_throughput,
    bench_verify_file_hash,
    bench_state_save,
    bench_hex_to_bytes,
    bench_md5_hex_eq,
    bench_xxh64_hex_eq,
    bench_load_download_state,
    bench_planner_attribution,
    bench_preinstall_chunk_tracking,
    bench_preinstall_save_offload,
    bench_installer_save_offload,
);
criterion_main!(benches);

#[cfg(feature = "bench-internals")]
fn bench_download_throughput(c: &mut Criterion) {
    use bytes::Bytes;
    use futures_util::{StreamExt, stream};
    use http_body_util::StreamBody;
    use hyper::body::{Frame, Incoming};
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper::{Request, Response};
    use hyper_util::rt::TokioIo;
    use irmin_rs::game_installer::assembly_opt;
    use irmin_rs::game_installer::download_benchmark::download_and_verify_md5;
    use std::convert::Infallible;
    use std::net::TcpListener as StdTcpListener;
    use std::sync::mpsc;
    use std::thread;

    let chunk_mib = 16;
    let total_bytes = chunk_mib * 1024 * 1024;
    let pattern = Bytes::from(
        (0..64 * 1024)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
            .collect::<Vec<_>>(),
    );

    let (address_tx, address_rx) = mpsc::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server_pattern = pattern.clone();
    let server = thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build server runtime");
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
                        let p = server_pattern.clone();
                        let service = service_fn(move |req: Request<Incoming>| {
                            let p = p.clone();
                            async move {
                                let size = req.headers()
                                    .get("x-size")
                                    .and_then(|v| v.to_str().ok())
                                    .and_then(|v| v.parse::<usize>().ok())
                                    .unwrap_or(total_bytes);
                                let full = size / p.len();
                                let rem = size % p.len();
                                let rp = p.clone();
                                let body = stream::repeat_with(move || rp.clone())
                                    .take(full)
                                    .chain(stream::iter((rem > 0).then(|| p.slice(..rem))))
                                    .map(|b| Ok::<_, Infallible>(Frame::data(b)));
                                let mut resp = Response::new(StreamBody::new(body));
                                resp.headers_mut().insert(
                                    hyper::header::CONTENT_LENGTH,
                                    size.to_string().parse().unwrap(),
                                );
                                Ok::<_, Infallible>(resp)
                            }
                        });
                        let _ = http1::Builder::new()
                            .serve_connection(TokioIo::new(socket), service)
                            .await;
                    }
                }
            }
        });
    });
    let address = address_rx.recv().unwrap();

    let mut expected_md5 = fast_md5::Md5::new();
    for offset in (0..total_bytes).step_by(pattern.len()) {
        let count = pattern.len().min(total_bytes - offset);
        expected_md5.update(&pattern[..count]);
    }
    let expected_md5_hex = assembly_opt::md5_to_hex(&expected_md5.finalize());

    let client = reqwest::Client::new();
    let dir = tempfile::tempdir().unwrap();
    let dest = dir.path().join("payload");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let mut group = c.benchmark_group("download_throughput");
    group.throughput(Throughput::Bytes(total_bytes as u64));

    group.bench_function("16m_md5", |b| {
        b.iter_custom(|iters| {
            runtime.block_on(async {
                let start = std::time::Instant::now();
                for _ in 0..iters {
                    let url = format!("http://{address}/payload");
                    download_and_verify_md5(
                        &client,
                        &url,
                        &dest,
                        total_bytes as u64,
                        &expected_md5_hex,
                    )
                    .await
                    .unwrap();
                }
                start.elapsed()
            })
        });
    });

    group.finish();
    let _ = shutdown_tx.send(());
    server.join().unwrap();
}

#[cfg(not(feature = "bench-internals"))]
fn bench_download_throughput(c: &mut Criterion) {
    let _ = c;
}

fn bench_eager_decompress(c: &mut Criterion) {
    let dir = tempfile::tempdir().unwrap();
    let out_path = dir.path().join("eager_out.bin");

    // Generate 8 MiB of compressible data
    let data_size: usize = 8 * 1024 * 1024;
    let mut data = vec![0u8; data_size];
    let mut seed: u64 = 99999;
    for b in data.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        *b = (seed >> 33) as u8;
    }

    let compressed = zstd::encode_all(std::io::Cursor::new(&data), 3).unwrap();
    let comp_hash = irmin_rs::game_installer::assembly_opt::hex_to_bytes(&hex::encode(
        fast_md5::digest(&compressed),
    ))
    .unwrap();
    let decomp_hash =
        irmin_rs::game_installer::assembly_opt::hex_to_bytes(&hex::encode(fast_md5::digest(&data)))
            .unwrap();

    let mut group = c.benchmark_group("eager_decompress");
    group.throughput(Throughput::Bytes(data_size as u64));

    group.bench_function("8m", |b| {
        b.iter(|| {
            let out_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&out_path)
                .unwrap();
            out_file.set_len(data_size as u64).unwrap();
            irmin_rs::game_installer::eager_decompress::eager_decompress_chunk(
                &compressed,
                &out_file,
                0,
                data_size as u64,
                comp_hash.as_slice(),
                decomp_hash.as_slice(),
            )
            .unwrap();
        });
    });

    group.bench_function("8m_streaming_full_verify", |b| {
        b.iter(|| {
            let out_file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(&out_path)
                .unwrap();
            out_file.set_len(data_size as u64).unwrap();
            irmin_rs::game_installer::eager_decompress::stream_decompress_to_file(
                HashingRead::new(&compressed, &hex::encode(&comp_hash)),
                &out_file,
                0,
                data_size as u64,
                decomp_hash.as_slice(),
            )
            .unwrap();
        });
    });

    group.finish();
}

struct HashingRead<'a> {
    bytes: &'a [u8],
    offset: usize,
    hasher: Option<fast_md5::Md5>,
    expected: &'a str,
    verified: bool,
}

impl<'a> HashingRead<'a> {
    fn new(bytes: &'a [u8], expected: &'a str) -> Self {
        Self {
            bytes,
            offset: 0,
            hasher: Some(fast_md5::Md5::new()),
            expected,
            verified: false,
        }
    }
}

impl std::io::BufRead for HashingRead<'_> {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        if self.offset >= self.bytes.len() && !self.verified {
            self.verified = true;
            let digest = self
                .hasher
                .take()
                .expect("hash context is available at EOF")
                .finalize();
            if hex::encode(digest) != self.expected {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "compressed MD5 mismatch",
                ));
            }
        }
        Ok(&self.bytes[self.offset..])
    }

    fn consume(&mut self, amt: usize) {
        let remaining = self.bytes.len() - self.offset;
        debug_assert!(amt <= remaining, "consumer over-consumed the slice");
        self.hasher
            .as_mut()
            .expect("hash context is available before EOF")
            .update(&self.bytes[self.offset..self.offset + amt.min(remaining)]);
        self.offset += amt.min(remaining);
    }
}

impl std::io::Read for HashingRead<'_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        // Standard BufRead blanket shape: one fill_buf, one copy out.
        if output.is_empty() {
            return Ok(0);
        }
        let filled = std::io::BufRead::fill_buf(self)?;
        let count = output.len().min(filled.len());
        output[..count].copy_from_slice(&filled[..count]);
        std::io::BufRead::consume(self, count);
        Ok(count)
    }
}

fn bench_state_save(c: &mut Criterion) {
    use irmin_rs::game_installer::compact_manifest::StringArena;
    use irmin_rs::game_installer::installer::bench_internals::{
        ChunkNameLookup, snapshot_downloaded_chunks,
    };
    use std::sync::atomic::AtomicU32;

    let chunk_counts = [10_000usize, 100_000, 200_000];

    let mut group = c.benchmark_group("state_save");

    for &count in &chunk_counts {
        let names: Vec<String> = (0..count)
            .map(|i| format!("patch-chunk-{i:08}.bin"))
            .collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let arena = StringArena::from(name_refs.as_slice());
        let lookup = ChunkNameLookup::from_arena(arena);
        let downloaded: Vec<AtomicU32> = (0..count)
            .map(|i| AtomicU32::new(if i % 2 == 0 { 256 * 1024 } else { 0 }))
            .collect();

        group.throughput(Throughput::Elements(count as u64));
        group.bench_with_input(BenchmarkId::from_parameter(count), &downloaded, |b, dc| {
            b.iter(|| {
                let entries = snapshot_downloaded_chunks(dc, &lookup);
                criterion::black_box(entries);
            });
        });

        // Full save: snapshot plus serialize + write through the production
        // state-file path. Compare against the /200000 snapshot bench from
        // the same criterion run; cross-run subtraction is unreliable on
        // this machine.
        if count == *chunk_counts.last().expect("non-empty") {
            let state_dir = tempfile::tempdir().expect("tempdir");
            let state_file = state_dir.path().join("state.json");
            group.bench_function(
                BenchmarkId::new("end_to_end", count),
                |b| {
                    b.iter(|| {
                        let entries = snapshot_downloaded_chunks(&downloaded, &lookup);
                        irmin_rs::sophon::bench_write_state_file(&state_file, &entries);
                    });
                },
            );
            group.bench_function(BenchmarkId::new("serialize_only", count), |b| {
                b.iter(|| {
                    let entries = snapshot_downloaded_chunks(&downloaded, &lookup);
                    let len = irmin_rs::sophon::bench_serialize_state(&entries);
                    criterion::black_box(len);
                });
            });
        }
    }

    group.finish();
}

/// Assembly-pipeline gate: measures whether per-chunk decompressed MD5
/// verification (VerifyMode::Full) is CPU-bound at the pipeline level. Six
/// workers (one per core) decompress 8 chunks each through the production
/// `decompress_chunk_optimized`; the only difference between variants is
/// whether the chunk hash is computed. If `hash_on` ≈ `hash_off` in wall
/// time the pipeline is disk-bound and hash batching cannot help; if it is
/// materially slower, hashing is the CPU bottleneck and multi-buffer
/// batching (see `md5_multibuffer`) is the lever.
#[cfg(feature = "bench-internals")]
fn bench_assembly_hash_gate(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly_opt::bench_internals::decompress_chunk_optimized;
    use std::sync::Arc;

    const CHUNK_MIB: usize = 8;
    const CHUNKS_PER_WORKER: usize = 8;
    const WORKERS: usize = 6;
    const TOTAL_BYTES: u64 =
        (CHUNK_MIB * CHUNKS_PER_WORKER * WORKERS) as u64 * 1024 * 1024;

    let dir = tempfile::tempdir().expect("tempdir");
    let chunks_dir = dir.path().join("chunks");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Unique per-worker chunk files so no worker's reads race another's
    // eviction; equal sizes keep SIMD lanes comparable if promoted later.
    let mut chunk_paths: Vec<Arc<Vec<std::path::PathBuf>>> = Vec::with_capacity(WORKERS);
    let mut out_files: Vec<std::fs::File> = Vec::with_capacity(WORKERS);
    let mut expected_digests: Vec<Vec<[u8; 16]>> = Vec::with_capacity(WORKERS);
    for w in 0..WORKERS {
        let mut paths = Vec::with_capacity(CHUNKS_PER_WORKER);
        let mut digests = Vec::with_capacity(CHUNKS_PER_WORKER);
        for i in 0..CHUNKS_PER_WORKER {
            let raw = gen_prng_data(
                0x6A6A + (w * CHUNKS_PER_WORKER + i) as u64,
                CHUNK_MIB * 1024 * 1024,
            );
            let comp = zstd::encode_all(&raw[..], 3).unwrap();
            let path = chunks_dir.join(format!("w{w}_c{i:02}.zstd"));
            fs::write(&path, &comp).unwrap();
            paths.push(path);
            digests.push(fast_md5::digest(&raw));
        }
        chunk_paths.push(Arc::new(paths));
        expected_digests.push(digests);

        let out_path = dir.path().join(format!("out_{w}.bin"));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        file.set_len((CHUNK_MIB * 1024 * 1024 * CHUNKS_PER_WORKER) as u64)
            .unwrap();
        out_files.push(file);
    }

    let mut group = c.benchmark_group("assembly_hash_gate");
    group.throughput(Throughput::Bytes(TOTAL_BYTES));

    // `hash_on`: workers hash each chunk (VerifyMode::Full shape). `hash_off`:
    // an empty digest slice disables the chunk hash (chunk_hash_required).
    let run_pipeline = |chunk_hashes: Option<&Vec<Vec<[u8; 16]>>>| {
        std::thread::scope(|s| {
            for w in 0..WORKERS {
                let paths = Arc::clone(&chunk_paths[w]);
                let out = &out_files[w];
                let hashes = chunk_hashes.map(|h| &h[w]);
                s.spawn(move || {
                    for (i, path) in paths.iter().enumerate() {
                        let hash: &[u8] = match hashes {
                            Some(per_worker) => &per_worker[i],
                            None => b"",
                        };
                        let written = decompress_chunk_optimized(
                            path,
                            out,
                            (i * CHUNK_MIB * 1024 * 1024) as u64,
                            (CHUNK_MIB * 1024 * 1024) as u64,
                            None,
                            hash,
                            false,
                        )
                        .expect("decompress");
                        debug_assert_eq!(written, (CHUNK_MIB * 1024 * 1024) as u64);
                    }
                });
            }
        });
    };

    group.bench_function("hash_off", |b| {
        b.iter(|| run_pipeline(None));
    });

    group.bench_function("hash_on", |b| {
        b.iter(|| run_pipeline(Some(&expected_digests)));
    });

    group.finish();
}

fn bench_load_download_state(c: &mut Criterion) {
    use irmin_rs::DownloadType;
    use irmin_rs::sophon::load_download_state;
    use irmin_rs::sophon::state_file_path;
    use irmin_rs::types::DownloadState;

    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path();

    for &entry_count in &[10_000usize, 100_000, 200_000] {
        let state = DownloadState {
            game_id: "hk4e".into(),
            vo_lang: "en-us".into(),
            output_path: "/games/test".into(),
            download_type: DownloadType::Fresh,
            current_tag: None,
            manifest_hash: "abc123".into(),
            downloaded_chunks: (0..entry_count)
                .map(|i| (format!("patch-chunk-{i:08}.bin"), 256 * 1024))
                .collect(),
            completed_files: Default::default(),
        };
        let path = state_file_path(state_dir, "hk4e");
        let json = serde_json::to_vec(&state).unwrap();
        std::fs::write(&path, &json).unwrap();
        let file_size = json.len() as u64;

        let mut group = c.benchmark_group("load_download_state");
        group.throughput(Throughput::Bytes(file_size));
        group.bench_with_input(
            BenchmarkId::from_parameter(entry_count),
            &path,
            |b, path| {
                b.iter(|| {
                    let state = load_download_state(path.parent().unwrap(), "hk4e");
                    criterion::black_box(state);
                });
            },
        );
        group.finish();
    }
}

/// Repair-sweep chunk verification: unique chunks re-verified before
/// re-download. Baseline is the old serial scalar loop (cache miss per
/// chunk, one fast_md5 stream); candidate is the SIMD multi-buffer batch
/// (8 mmap lanes per dispatch through md5-simd's AVX2 kernel).
/// Benchmark-only (not compiled into production paths).
#[cfg(feature = "bench-internals")]
fn bench_phase2a_chunk_verify(c: &mut Criterion) {
    const CHUNK_MIB: usize = 8;
    const NUM_CHUNKS: usize = 64;
    let dir = tempfile::tempdir().expect("tempdir");
    let mut items = Vec::with_capacity(NUM_CHUNKS);
    let mut files = Vec::with_capacity(NUM_CHUNKS);
    for i in 0..NUM_CHUNKS {
        let raw = gen_prng_data(0xA11CE + i as u64, CHUNK_MIB * 1024 * 1024);
        let comp = zstd::encode_all(&raw[..], 3).unwrap();
        let path = dir.path().join(format!("chunk_{i:04}.zstd"));
        fs::write(&path, &comp).unwrap();
        items.push((path, fast_md5::digest(&comp)));
        files.push(comp);
    }
    let total: u64 = files.iter().map(|c| c.len() as u64).sum();

    let mut group = c.benchmark_group("phase2a_chunk_verify");
    group.throughput(Throughput::Bytes(total));

    group.bench_function("scalar_serial", |b| {
        b.iter(|| {
            // Old production path: per-chunk buffered scalar hash. Page-cache
            // advice disabled to isolate CPU hashing cost from eviction
            // policy (the SIMD path does not evict either).
            #[cfg(feature = "bench-internals")]
            {
                for (path, expected) in &items {
                    let actual = irmin_rs::game_installer::cache::benchmark::file_md5_digest_with_options(path, 256 * 1024, false).unwrap();
                    criterion::black_box(actual == *expected);
                }
            }
            #[cfg(not(feature = "bench-internals"))]
            {
                for (path, expected) in &items {
                    let actual = irmin_rs::game_installer::cache::file_md5_digest(path).unwrap();
                    criterion::black_box(actual == *expected);
                }
            }
        });
    });

    group.bench_function("simd_batched", |b| {
        b.iter(|| {
            let results =
                irmin_rs::game_installer::md5_multibuffer::verify_md5_batch(&items).unwrap();
            criterion::black_box(results);
        });
    });

    group.finish();
}

fn bench_verify_file_hash(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly_opt::hex_to_bytes;
    use irmin_rs::game_installer::preinstall::bench_internals::verify_file_hash;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.bin");
    let data = b"hello world, this is test data for hashing";
    fs::write(&path, data).unwrap();
    let md5_hex = hex::encode(fast_md5::digest(data));
    let md5_bytes = hex_to_bytes(&md5_hex).unwrap();

    let mut group = c.benchmark_group("verify_file_hash");
    group.throughput(Throughput::Bytes(data.len() as u64));
    group.bench_function("binary", |b| {
        b.iter(|| {
            let ok = verify_file_hash(&path, &md5_bytes);
            criterion::black_box(ok);
        });
    });
    group.finish();
}

fn bench_hex_to_bytes(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly_opt::hex_to_bytes;

    let hex = "d41d8cd98f00b204e9800998ecf8427e".to_string();

    let mut group = c.benchmark_group("hex_to_bytes");
    group.throughput(Throughput::Bytes(16));
    group.bench_function("lut", |b| {
        b.iter(|| {
            let bytes = hex_to_bytes(&hex);
            criterion::black_box(bytes);
        });
    });
    group.finish();
}

fn bench_md5_hex_eq(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly_opt::md5_hex_eq;

    let digest: [u8; 16] = fast_md5::digest(b"hello world");
    let hex = hex::encode(digest);

    let mut group = c.benchmark_group("md5_hex_eq");
    group.throughput(Throughput::Bytes(16));
    group.bench_function("lut", |b| {
        b.iter(|| {
            let ok = md5_hex_eq(&digest, &hex);
            criterion::black_box(ok);
        });
    });
    group.finish();
}

fn bench_xxh64_hex_eq(c: &mut Criterion) {
    use irmin_rs::game_installer::assembly_opt::xxh64_hex_eq;

    let value: u64 = 0x1234567890abcdef;
    let hex = format!("{value:016x}");

    let mut group = c.benchmark_group("xxh64_hex_eq");
    group.throughput(Throughput::Bytes(8));
    group.bench_function("lut", |b| {
        b.iter(|| {
            let ok = xxh64_hex_eq(value, &hex);
            criterion::black_box(ok);
        });
    });
    group.finish();
}

fn bench_chunk_accessors(c: &mut Criterion) {
    let hash_pool: Vec<String> = (0..MANIFEST_FILES).map(|i| format!("{i:032x}")).collect();
    let assets: Vec<SophonManifestAssetProperty> = (0..MANIFEST_FILES)
        .map(|f| {
            let chunks: Vec<SophonManifestAssetChunk> = (0..MANIFEST_CHUNKS_PER_FILE)
                .map(|c| {
                    let h = &hash_pool[(f + c) % MANIFEST_FILES];
                    SophonManifestAssetChunk::from_parts(
                        format!("f{f}c{c}").into(),
                        h.clone().into(),
                        (c * 1024 * 1024) as u64,
                        0,
                        0,
                        0,
                        bytes::Bytes::new(),
                        -1,
                    )
                })
                .collect();
            let size = chunks.iter().map(|c| c.chunk_size_decompressed).sum();
            SophonManifestAssetProperty::from_parts(
                format!("asset_{f}.pak").into(),
                chunks,
                0,
                size,
                format!("{f:032x}").into(),
            )
        })
        .collect();
    let manifest = CompactManifest::from(assets);
    let total_chunks = manifest.num_chunks();

    let mut group = c.benchmark_group("chunk_accessors");
    group.throughput(Throughput::Elements(total_chunks as u64));

    group.bench_function("chunkref_scan", |b| {
        b.iter(|| {
            let mut seen: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
            let total: u64 = (0..total_chunks)
                .map(|i| manifest.chunk(i))
                .filter(|c| c.chunk_old_offset < 0)
                .filter(|c| seen.insert(c.chunk_name))
                .map(|c| c.chunk_size)
                .fold(0u64, |acc, x| acc.saturating_add(x));
            criterion::black_box(total);
        });
    });

    group.bench_function("direct_accessor_scan", |b| {
        b.iter(|| {
            let mut seen: rustc_hash::FxHashSet<&str> = rustc_hash::FxHashSet::default();
            let total: u64 = (0..total_chunks)
                .filter(|&i| manifest.chunk_old_offset_at(i) < 0)
                .filter(|&i| seen.insert(manifest.chunk_name_at(i)))
                .map(|i| manifest.chunk_size_at(i))
                .fold(0u64, |acc, x| acc.saturating_add(x));
            criterion::black_box(total);
        });
    });

    group.finish();
}
