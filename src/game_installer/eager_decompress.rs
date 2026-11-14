//! Inline download + decompression for eager install path.
//!
//! Feeds HTTP bytes to a zstd streaming decoder and writes decompressed output
//! directly to the target file at the chunk's offset via pwrite. Computes both
//! compressed and decompressed hashes inline.
#![allow(dead_code, unused_imports)]

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;
#[cfg(feature = "bench-diagnostics")]
use std::sync::Arc;

#[cfg(feature = "bench-diagnostics")]
use std::sync::atomic::Ordering;

use super::assembly_opt::{
    self, Md5, bytes_to_hex, chunk_hash_required, md5_bytes_eq, md5_hex_eq, md5_to_hex, return_md5,
    take_md5, xxh64_bytes_eq, xxh64_hex_eq,
};
use super::error::{SophonError, SophonResult};

/// Buffer size for reading from the zstd decoder and writing to the output file.
/// Buffer size used by eager streaming decompression.
pub const EAGER_BUFFER_SIZE: usize = 1024 * 1024;

#[cfg(feature = "bench-diagnostics")]
#[derive(Default)]
pub(crate) struct EagerStreamPhaseTimings {
    pub(crate) producer_wait_ns: std::sync::atomic::AtomicU64,
    pub(crate) compressed_hash_ns: std::sync::atomic::AtomicU64,
    pub(crate) channel_send_ns: std::sync::atomic::AtomicU64,
    pub(crate) consumer_wait_ns: std::sync::atomic::AtomicU64,
    /// Frames pulled from the channel into the reader (distinct
    /// receives; fill_buf re-offers of a partially consumed frame do not
    /// count).
    pub(crate) input_read_calls: std::sync::atomic::AtomicU64,
    /// Bytes the decoder actually ingested via `consume` (never counts a
    /// byte twice, unlike a fill_buf-supplied tally).
    pub(crate) input_read_requested_bytes: std::sync::atomic::AtomicU64,
    pub(crate) decoder_read_ns: std::sync::atomic::AtomicU64,
    pub(crate) decoder_read_count: std::sync::atomic::AtomicU64,
    pub(crate) decoder_read_bytes: std::sync::atomic::AtomicU64,
    pub(crate) output_write_ns: std::sync::atomic::AtomicU64,
    pub(crate) output_hash_ns: std::sync::atomic::AtomicU64,
    pub(crate) decoder_thread_cpu_ns: std::sync::atomic::AtomicU64,
}

#[cfg(feature = "bench-diagnostics")]
fn thread_cpu_time_ns() -> u64 {
    let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
    let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, time.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    let time = unsafe { time.assume_init() };
    (time.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(time.tv_nsec as u64)
}

/// Performs eager decompression: decompresses compressed data and writes
/// decompressed output directly to `out_file` at `chunk_offset`.
///
/// Returns the number of decompressed bytes written on success.
/// Decompress a chunk directly into output storage without retaining a `.zstd` intermediate.
pub fn eager_decompress_chunk(
    compressed_data: &[u8],
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    compressed_hash: &[u8],
    decompressed_hash: &[u8],
) -> SophonResult<u64> {
    eager_decompress_chunk_inner::<true>(
        compressed_data,
        out_file,
        chunk_offset,
        expected_decompressed_size,
        compressed_hash,
        decompressed_hash,
    )
}

#[cfg(feature = "bench-diagnostics")]
pub(crate) fn benchmark_eager_decompress_chunk_with_prefault(
    compressed_data: &[u8],
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    compressed_hash: &[u8],
    decompressed_hash: &[u8],
) -> SophonResult<u64> {
    eager_decompress_chunk_inner::<true>(
        compressed_data,
        out_file,
        chunk_offset,
        expected_decompressed_size,
        compressed_hash,
        decompressed_hash,
    )
}

#[cfg(feature = "bench-diagnostics")]
pub(crate) fn benchmark_eager_decompress_chunk_without_prefault(
    compressed_data: &[u8],
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    compressed_hash: &[u8],
    decompressed_hash: &[u8],
) -> SophonResult<u64> {
    eager_decompress_chunk_inner::<false>(
        compressed_data,
        out_file,
        chunk_offset,
        expected_decompressed_size,
        compressed_hash,
        decompressed_hash,
    )
}

/// True when the compressed hash must be verified in the eager path.
///
/// A verified decompressed digest subsumes the compressed one: transport
/// corruption either breaks zstd framing or changes the decompressed
/// bytes, and the decompressed MD5 catches both. Hashing the compressed
/// stream on top of that is pure CPU for zero extra detection, so it is
/// skipped whenever the decompressed hash will actually be checked
/// (non-empty, real length). When the decompressed hash is absent — the
/// zero placeholder, an unsupported length, or an empty manifest field —
/// the compressed digest is the only integrity check and stays.
#[inline]
pub(crate) fn compressed_hash_needed(compressed_hash: &[u8], decompressed_hash: &[u8]) -> bool {
    !compressed_hash.is_empty()
        && !chunk_hash_required(decompressed_hash)
        && matches!(compressed_hash.len(), 16 | 8)
}

fn eager_decompress_chunk_inner<const PREFAULT: bool>(
    compressed_data: &[u8],
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    compressed_hash: &[u8],
    decompressed_hash: &[u8],
) -> SophonResult<u64> {
    // Verify the compressed hash only when it is the sole integrity check
    // for this chunk (`compressed_hash_needed` documents the rationale).
    if compressed_hash_needed(compressed_hash, decompressed_hash) {
        match compressed_hash.len() {
            16 => {
                let mut hasher = take_md5()?;
                hasher.update(compressed_data)?;
                let digest = hasher.finish()?;
                return_md5(hasher);
                if !md5_bytes_eq(&digest, compressed_hash) {
                    return Err(SophonError::Md5Mismatch {
                        item: format!("eager chunk at offset {chunk_offset}"),
                        expected: bytes_to_hex(compressed_hash),
                        actual: md5_to_hex(&digest),
                    });
                }
            }
            8 => {
                let mut xxh = assembly_opt::take_xxh64();
                xxh.update(compressed_data);
                let digest = xxh.digest();
                assembly_opt::return_xxh64(xxh);
                if !xxh64_bytes_eq(digest, compressed_hash) {
                    return Err(SophonError::Md5Mismatch {
                        item: format!("eager chunk at offset {chunk_offset}"),
                        expected: bytes_to_hex(compressed_hash),
                        actual: format!("{:016x}", digest),
                    });
                }
            }
            // `compressed_hash_needed` admits only 16- and 8-byte digests;
            // other lengths fail before the attempt loop.
            _ => {}
        }
    }

    // Decompress directly into mmap'd output file region (one-shot).
    // Single DCtx::decompress call — no per-read streaming overhead.
    let need_decompressed_hash = chunk_hash_required(decompressed_hash);
    let expected_usize = expected_decompressed_size as usize;

    let mut out_mmap = assembly_opt::mmap_read_write(out_file, chunk_offset, expected_usize)
        .map_err(SophonError::Io)?;

    if PREFAULT {
        unsafe {
            libc::madvise(
                out_mmap.map_base,
                out_mmap.map_len,
                libc::MADV_POPULATE_WRITE,
            );
        }
    }

    let output = out_mmap.as_mut_slice();

    let mut ctx = assembly_opt::take_dctx();
    ctx.reset(zstd::zstd_safe::ResetDirective::SessionAndParameters)
        .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx reset")))?;
    let window_log = assembly_opt::window_log_for_size(expected_decompressed_size);
    ctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(window_log))
        .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx WindowLogMax")))?;
    if need_decompressed_hash {
        let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(true));
    }

    let written = ctx
        .decompress(output, compressed_data)
        .map_err(|e| SophonError::Io(io::Error::other(e.to_string())))?;
    assembly_opt::return_dctx(ctx, window_log);

    if written != expected_usize {
        out_mmap.flush_and_evict();
        return Err(SophonError::SizeMismatch {
            item: format!("eager chunk at offset {chunk_offset}"),
            expected: expected_decompressed_size,
            actual: written as u64,
        });
    }

    // Verify decompressed hash (Full mode only).
    if need_decompressed_hash {
        let mut hasher = take_md5()?;
        hasher.update(&output[..written])?;
        let digest = hasher.finish()?;
        return_md5(hasher);
        if !md5_bytes_eq(&digest, decompressed_hash) {
            out_mmap.flush_and_evict();
            return Err(SophonError::Md5Mismatch {
                item: format!("eager chunk decompressed at offset {chunk_offset}"),
                expected: bytes_to_hex(decompressed_hash),
                actual: md5_to_hex(&digest),
            });
        }
    }

    out_mmap.flush_and_evict();
    Ok(written as u64)
}

/// Frozen replica of the pre-BufRead input chain (commit 5323491): the
/// streaming entry point used to accept any `Read` and wrap it in an
/// internal 256 KiB `BufReader`, which stacked with the installer-side
/// outer 256 KiB `BufReader` around the channel reader — two extra copies
/// of the compressed stream plus two transient allocations per chunk.
/// Kept for paired criterion comparison against
/// [`stream_decompress_to_file`]; safe to delete once its measurements
/// are no longer needed.
#[cfg(feature = "benchmark")]
pub fn legacy_stream_decompress_double_buffered(
    reader: impl io::Read,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &[u8],
) -> SophonResult<u64> {
    stream_decompress_to_file_inner(
        io::BufReader::with_capacity(256 * 1024, reader),
        out_file,
        chunk_offset,
        expected_decompressed_size,
        decompressed_hash,
        #[cfg(feature = "bench-diagnostics")]
        None,
        #[cfg(feature = "bench-diagnostics")]
        false,
    )
}

/// Stream a compressed chunk into a file range while checking optional hashes.
///
/// The reader is consumed as a [`std::io::BufRead`] directly by the zstd
/// decoder: pass a reader whose `fill_buf` yields the source's natural
/// chunk slices (channel frames, buffer slices) and do NOT wrap it in
/// another `BufReader` — that would only reintroduce a copy of the
/// compressed stream.
pub fn stream_decompress_to_file(
    reader: impl io::BufRead,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &[u8],
) -> SophonResult<u64> {
    stream_decompress_to_file_inner(
        reader,
        out_file,
        chunk_offset,
        expected_decompressed_size,
        decompressed_hash,
        #[cfg(feature = "bench-diagnostics")]
        None,
        #[cfg(feature = "bench-diagnostics")]
        false,
    )
}

#[cfg(feature = "bench-diagnostics")]
pub(crate) fn stream_decompress_to_file_with_timings(
    reader: impl io::BufRead,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &[u8],
    timings: Arc<EagerStreamPhaseTimings>,
) -> SophonResult<u64> {
    stream_decompress_to_file_inner(
        reader,
        out_file,
        chunk_offset,
        expected_decompressed_size,
        decompressed_hash,
        Some(timings),
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn stream_decompress_to_file_inner(
    reader: impl io::BufRead,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &[u8],
    #[cfg(feature = "bench-diagnostics")] timings: Option<Arc<EagerStreamPhaseTimings>>,
    #[cfg(feature = "bench-diagnostics")] drain_reader_to_eof: bool,
) -> SophonResult<u64> {
    let mut reader = reader;

    let mut ctx = assembly_opt::take_dctx();
    ctx.reset(zstd::zstd_safe::ResetDirective::SessionAndParameters)
        .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx reset")))?;
    ctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
        assembly_opt::MAX_WINDOW_LOG,
    ))
    .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx WindowLogMax")))?;
    let need_decompressed_hash = chunk_hash_required(decompressed_hash);
    if need_decompressed_hash {
        let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(true));
    }

    let decompressed_hasher = if need_decompressed_hash {
        Some(take_md5()?)
    } else {
        None
    };
    let mut ctx = DctxGuard(Some(ctx));
    let mut decompressed_hasher = PooledMd5Guard(decompressed_hasher);

    // Mmap the output region and decompress directly into it, eliminating
    // both the intermediate buffer and pwrite syscalls.
    let expected_usize = expected_decompressed_size as usize;
    let mut out_mmap = assembly_opt::mmap_read_write(out_file, chunk_offset, expected_usize)?;
    let output_slice = out_mmap.as_mut_slice();
    let mut bytes_written: usize = 0;

    {
        #[cfg(feature = "bench-diagnostics")]
        let decoder_thread_started = thread_cpu_time_ns();
        let mut decoder = zstd::Decoder::with_context(
            &mut reader,
            ctx.0.as_mut().expect("decoder context is present"),
        )
        .single_frame();
        loop {
            let remaining = expected_usize.saturating_sub(bytes_written);
            if remaining == 0 {
                break;
            }
            #[cfg(feature = "bench-diagnostics")]
            let read_started = std::time::Instant::now();
            let n = decoder
                .read(&mut output_slice[bytes_written..bytes_written + remaining])
                .map_err(|e| SophonError::Io(io::Error::other(e.to_string())))?;
            #[cfg(feature = "bench-diagnostics")]
            if let Some(timings) = timings.as_ref() {
                // A terminal read of zero bytes still costs a decoder call,
                // so the count naturally includes the EOF read.
                timings
                    .decoder_read_ns
                    .fetch_add(read_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                timings.decoder_read_count.fetch_add(1, Ordering::Relaxed);
                timings
                    .decoder_read_bytes
                    .fetch_add(n as u64, Ordering::Relaxed);
            }
            if n == 0 {
                break;
            }
            bytes_written += n;
        }
        if let Some(hasher) = decompressed_hasher.0.as_mut() {
            #[cfg(feature = "bench-diagnostics")]
            let hash_started = std::time::Instant::now();
            hasher.update(&output_slice[..bytes_written])?;
            // Timings are recorded only on the success path: an `?` abort
            // invalidates the whole stream, so a partially recorded batch
            // would be misleading.
            #[cfg(feature = "bench-diagnostics")]
            if let Some(timings) = timings.as_ref() {
                timings
                    .output_hash_ns
                    .fetch_add(hash_started.elapsed().as_nanos() as u64, Ordering::Relaxed);
            }
        }
        decoder
            .finish_frame()
            .map_err(|error| SophonError::Io(io::Error::other(error.to_string())))?;
        #[cfg(feature = "bench-diagnostics")]
        if let Some(timings) = timings.as_ref() {
            timings
                .decoder_thread_cpu_ns
                .fetch_add(thread_cpu_time_ns().saturating_sub(decoder_thread_started), Ordering::Relaxed);
        }
        #[cfg(not(feature = "bench-diagnostics"))]
        let _ = decoder.finish();
        #[cfg(feature = "bench-diagnostics")]
        if drain_reader_to_eof {
            io::copy(&mut reader, &mut io::sink()).map_err(SophonError::Io)?;
        }
    }

    if bytes_written as u64 != expected_decompressed_size {
        return Err(SophonError::SizeMismatch {
            item: format!("stream chunk at offset {chunk_offset}"),
            expected: expected_decompressed_size,
            actual: bytes_written as u64,
        });
    }

    if let Some(mut hasher) = decompressed_hasher.0.take() {
        let digest = hasher.finish()?;
        return_md5(hasher);
        if !md5_bytes_eq(&digest, decompressed_hash) {
            return Err(SophonError::Md5Mismatch {
                item: format!("stream chunk decompressed at offset {chunk_offset}"),
                expected: bytes_to_hex(decompressed_hash),
                actual: md5_to_hex(&digest),
            });
        }
    }

    Ok(bytes_written as u64)
}

struct DctxGuard(Option<zstd::zstd_safe::DCtx<'static>>);

impl Drop for DctxGuard {
    #[inline]
    fn drop(&mut self) {
        if let Some(ctx) = self.0.take() {
            assembly_opt::return_dctx(ctx, assembly_opt::MAX_WINDOW_LOG);
        }
    }
}

struct PooledMd5Guard(Option<Md5>);

impl Drop for PooledMd5Guard {
    #[inline]
    fn drop(&mut self) {
        if let Some(hasher) = self.0.take() {
            return_md5(hasher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn compress_data(data: &[u8]) -> Vec<u8> {
        zstd::encode_all(std::io::Cursor::new(data), 3).unwrap()
    }

    fn md5_hex(data: &[u8]) -> String {
        hex::encode(fast_md5::digest(data))
    }

    #[test]
    fn eager_decompress_success() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let original = vec![0xABu8; 4096];
        let compressed = compress_data(&original);
        let comp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&compressed)).unwrap();
        let decomp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&original)).unwrap();

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(4096).unwrap();

        let written = eager_decompress_chunk(
            &compressed,
            &out_file,
            0,
            4096,
            comp_hash.as_slice(),
            decomp_hash.as_slice(),
        )
        .unwrap();

        assert_eq!(written, 4096);

        let result = std::fs::read(&out_path).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn compressed_hash_needed_only_when_decompressed_check_absent() {
        let real = [0x11u8; 16];
        // A decompressed digest that will be verified subsumes the
        // compressed check for both digest widths.
        assert!(!compressed_hash_needed(&[0x22; 16], &real));
        assert!(!compressed_hash_needed(&[0x22; 8], &real));
        // Without a real decompressed digest the compressed hash is the
        // sole integrity check…
        assert!(compressed_hash_needed(&[0x22; 16], b""));
        assert!(compressed_hash_needed(&[0x22; 8], b""));
        assert!(compressed_hash_needed(&[0x22; 16], &[0u8; 16]));
        // …unless it is itself absent or of an unsupported length. Note an
        // 8-byte digest counts as "will be verified" because
        // `chunk_hash_required` is shared with the xxh64-capable paths.
        assert!(!compressed_hash_needed(b"", &real));
        assert!(!compressed_hash_needed(&[0x22; 4], b""));
        assert!(!compressed_hash_needed(&[0x22; 8], &[0u8; 8]));
    }

    #[test]
    fn eager_decompress_compressed_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let original = vec![0xCDu8; 1024];
        let compressed = compress_data(&original);

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(1024).unwrap();

        // A verified decompressed digest subsumes the compressed check, so a
        // wrong compressed hash is ignored when the decompressed hash is real.
        let result = eager_decompress_chunk(
            &compressed,
            &out_file,
            0,
            1024,
            &[0; 16], // wrong compressed hash
            &super::assembly_opt::hex_to_bytes(&md5_hex(&original)).unwrap()[..],
        );
        assert_eq!(result.unwrap(), 1024);
        assert_eq!(std::fs::read(&out_path).unwrap(), original);

        // Without a decompressed digest the compressed hash is the only
        // integrity check and a mismatch must fail the chunk.
        let out_file2 = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(dir.path().join("output2.bin"))
            .unwrap();
        out_file2.set_len(1024).unwrap();
        let result = eager_decompress_chunk(
            &compressed,
            &out_file2,
            0,
            1024,
            &[0; 16], // wrong compressed hash
            b"",      // no decompressed digest to fall back on
        );
        assert!(result.is_err());
        match result.unwrap_err() {
            SophonError::Md5Mismatch { expected, .. } => {
                // The report keeps the hex digest shape callers log.
                assert_eq!(expected, "00000000000000000000000000000000");
            }
            other => panic!("expected Md5Mismatch, got: {other:?}"),
        }
    }

    #[test]
    fn eager_decompress_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let chunk_data = vec![0x42u8; 2048];
        let compressed = compress_data(&chunk_data);
        let comp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&compressed)).unwrap();
        let decomp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&chunk_data)).unwrap();

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        // File is 8192 bytes, chunk goes at offset 4096
        out_file.set_len(8192).unwrap();

        let written = eager_decompress_chunk(
            &compressed,
            &out_file,
            4096,
            2048,
            comp_hash.as_slice(),
            decomp_hash.as_slice(),
        )
        .unwrap();

        assert_eq!(written, 2048);

        let full = std::fs::read(&out_path).unwrap();
        assert_eq!(&full[4096..6144], &chunk_data[..]);
        // Bytes before and after the chunk should be zero (from set_len)
        assert!(full[..4096].iter().all(|&b| b == 0));
        assert!(full[6144..].iter().all(|&b| b == 0));
    }

    #[test]
    fn eager_decompress_xxh64_hash_path() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let original = vec![0x77u8; 2048];
        let compressed = compress_data(&original);
        // XXH64 hash is 16 hex chars
        let xxh = super::assembly_opt::hex_to_bytes(&format!(
            "{:016x}",
            xxhash_rust::xxh64::xxh64(&compressed, 0)
        ))
        .unwrap();
        let decomp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&original)).unwrap();

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(2048).unwrap();

        let written = eager_decompress_chunk(
            &compressed,
            &out_file,
            0,
            2048,
            xxh.as_slice(),
            decomp_hash.as_slice(),
        )
        .unwrap();

        assert_eq!(written, 2048);
        let result = std::fs::read(&out_path).unwrap();
        assert_eq!(result, original);
    }

    #[test]
    fn eager_decompress_corrupt_zstd_data() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        // Garbage bytes that aren't valid zstd
        let garbage = vec![0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x01, 0x02, 0x03];

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(1024).unwrap();

        // Empty hash = skip verification, let decoder fail
        let result = eager_decompress_chunk(&garbage, &out_file, 0, 1024, b"", b"");
        assert!(result.is_err());
    }

    #[test]
    fn eager_decompress_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let original = vec![0x55u8; 1024];
        let compressed = compress_data(&original);
        let comp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&compressed)).unwrap();

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(2048).unwrap();

        // Expect 2048 but data decompresses to 1024
        let result = eager_decompress_chunk(
            &compressed,
            &out_file,
            0,
            2048, // wrong expected size
            &comp_hash,
            b"",
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            super::super::error::SophonError::SizeMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, 2048);
                assert_eq!(actual, 1024);
            }
            other => panic!("expected SizeMismatch, got: {other:?}"),
        }
    }

    #[test]
    fn eager_decompress_decompressed_hash_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let original = vec![0x99u8; 512];
        let compressed = compress_data(&original);
        let comp_hash = super::assembly_opt::hex_to_bytes(&md5_hex(&compressed)).unwrap();

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(512).unwrap();

        // Correct compressed hash but wrong decompressed hash
        let result = eager_decompress_chunk(
            &compressed,
            &out_file,
            0,
            512,
            &comp_hash,
            &[0xff; 16], // wrong decompressed hash
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            super::super::error::SophonError::Md5Mismatch { .. } => {}
            other => panic!("expected Md5Mismatch, got: {other:?}"),
        }
    }
}
