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

use super::assembly_opt::{
    self, Md5, chunk_hash_required, md5_hex_eq, md5_to_hex, return_md5, take_md5, xxh64_hex_eq,
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
    pub(crate) input_read_calls: std::sync::atomic::AtomicU64,
    pub(crate) input_read_requested_bytes: std::sync::atomic::AtomicU64,
    pub(crate) input_read_bypass_calls: std::sync::atomic::AtomicU64,
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
    compressed_hash: &str,
    decompressed_hash: &str,
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
    compressed_hash: &str,
    decompressed_hash: &str,
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
    compressed_hash: &str,
    decompressed_hash: &str,
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

fn eager_decompress_chunk_inner<const PREFAULT: bool>(
    compressed_data: &[u8],
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    compressed_hash: &str,
    decompressed_hash: &str,
) -> SophonResult<u64> {
    // Verify compressed hash first.
    if !compressed_hash.is_empty() {
        match compressed_hash.len() {
            32 => {
                let mut hasher = take_md5()?;
                hasher.update(compressed_data)?;
                let digest = hasher.finish()?;
                return_md5(hasher);
                if !md5_hex_eq(&digest, compressed_hash) {
                    return Err(SophonError::Md5Mismatch {
                        item: format!("eager chunk at offset {chunk_offset}"),
                        expected: compressed_hash.to_string(),
                        actual: md5_to_hex(&digest),
                    });
                }
            }
            16 => {
                let mut xxh = assembly_opt::take_xxh64();
                xxh.update(compressed_data);
                let digest = xxh.digest();
                assembly_opt::return_xxh64(xxh);
                if !xxh64_hex_eq(digest, compressed_hash) {
                    return Err(SophonError::Md5Mismatch {
                        item: format!("eager chunk at offset {chunk_offset}"),
                        expected: compressed_hash.to_string(),
                        actual: format!("{:016x}", digest),
                    });
                }
            }
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
        if !md5_hex_eq(&digest, decompressed_hash) {
            out_mmap.flush_and_evict();
            return Err(SophonError::Md5Mismatch {
                item: format!("eager chunk decompressed at offset {chunk_offset}"),
                expected: decompressed_hash.to_string(),
                actual: md5_to_hex(&digest),
            });
        }
    }

    out_mmap.flush_and_evict();
    Ok(written as u64)
}

/// Stream a compressed chunk into a file range while checking optional hashes.
pub fn stream_decompress_to_file(
    reader: impl io::Read,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &str,
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
    reader: impl io::Read,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &str,
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
    reader: impl io::Read,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
    decompressed_hash: &str,
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

    let buf = assembly_opt::take_eager_buf();
    let decompressed_hasher = if need_decompressed_hash {
        Some(take_md5()?)
    } else {
        None
    };
    let mut ctx = DctxGuard(Some(ctx));
    let mut buf = EagerBuffer(Some(buf));
    let decompressed_hasher = PooledMd5Guard(decompressed_hasher);
    let mut output = StreamOutput {
        file: out_file,
        chunk_offset,
        expected_size: expected_decompressed_size,
        write_offset: chunk_offset,
        bytes_written: 0,
        hasher: decompressed_hasher,
        #[cfg(feature = "bench-diagnostics")]
        timings,
    };
    {
        #[cfg(feature = "bench-diagnostics")]
        let decoder_thread_started = thread_cpu_time_ns();
        let mut decoder = zstd::Decoder::with_context(
            io::BufReader::with_capacity(256 * 1024, &mut reader),
            ctx.0.as_mut().expect("decoder context is present"),
        )
        .single_frame();
        read_decompressed(
            &mut decoder,
            buf.0.as_mut().expect("eager buffer is present"),
            &mut output,
        )?;
        decoder
            .finish_frame()
            .map_err(|error| SophonError::Io(io::Error::other(error.to_string())))?;
        #[cfg(feature = "bench-diagnostics")]
        let mut reader = decoder.finish();
        #[cfg(not(feature = "bench-diagnostics"))]
        let _ = decoder.finish();
        #[cfg(feature = "bench-diagnostics")]
        if drain_reader_to_eof {
            io::copy(&mut reader, &mut io::sink()).map_err(SophonError::Io)?;
        }
        #[cfg(feature = "bench-diagnostics")]
        if let Some(timings) = output.timings.as_ref() {
            timings.decoder_thread_cpu_ns.fetch_add(
                thread_cpu_time_ns().saturating_sub(decoder_thread_started),
                std::sync::atomic::Ordering::Relaxed,
            );
        }
    }

    if output.bytes_written != expected_decompressed_size {
        return Err(SophonError::SizeMismatch {
            item: format!("stream chunk at offset {chunk_offset}"),
            expected: expected_decompressed_size,
            actual: output.bytes_written,
        });
    }

    if let Some(mut hasher) = output.hasher.0.take() {
        let digest = hasher.finish()?;
        return_md5(hasher);
        if !md5_hex_eq(&digest, decompressed_hash) {
            return Err(SophonError::Md5Mismatch {
                item: format!("stream chunk decompressed at offset {chunk_offset}"),
                expected: decompressed_hash.to_string(),
                actual: md5_to_hex(&digest),
            });
        }
    }

    Ok(output.bytes_written)
}

struct DctxGuard(Option<zstd::zstd_safe::DCtx<'static>>);

impl Drop for DctxGuard {
    fn drop(&mut self) {
        if let Some(ctx) = self.0.take() {
            assembly_opt::return_dctx(ctx, assembly_opt::MAX_WINDOW_LOG);
        }
    }
}

struct EagerBuffer(Option<Vec<u8>>);

impl Drop for EagerBuffer {
    fn drop(&mut self) {
        if let Some(buffer) = self.0.take() {
            assembly_opt::return_eager_buf(buffer);
        }
    }
}

struct PooledMd5Guard(Option<Md5>);

impl Drop for PooledMd5Guard {
    fn drop(&mut self) {
        if let Some(hasher) = self.0.take() {
            return_md5(hasher);
        }
    }
}

struct StreamOutput<'a> {
    file: &'a File,
    chunk_offset: u64,
    expected_size: u64,
    write_offset: u64,
    bytes_written: u64,
    hasher: PooledMd5Guard,
    #[cfg(feature = "bench-diagnostics")]
    timings: Option<Arc<EagerStreamPhaseTimings>>,
}

fn read_decompressed(
    decoder: &mut impl io::Read,
    buf: &mut [u8],
    output: &mut StreamOutput<'_>,
) -> SophonResult<()> {
    loop {
        let remaining = output.expected_size.saturating_sub(output.bytes_written);
        let read_limit = remaining.saturating_add(1).min(buf.len() as u64) as usize;
        #[cfg(feature = "bench-diagnostics")]
        let read_started = std::time::Instant::now();
        let n = decoder
            .read(&mut buf[..read_limit])
            .map_err(|e| SophonError::Io(io::Error::other(e.to_string())))?;
        #[cfg(feature = "bench-diagnostics")]
        if let Some(timings) = output.timings.as_ref() {
            let elapsed = read_started.elapsed().as_nanos() as u64;
            timings
                .decoder_read_ns
                .fetch_add(elapsed, std::sync::atomic::Ordering::Relaxed);
            if n > 0 {
                timings
                    .decoder_read_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                timings
                    .decoder_read_bytes
                    .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
            }
        }
        if n == 0 {
            return Ok(());
        }
        let next_bytes_written = output
            .bytes_written
            .checked_add(n as u64)
            .ok_or_else(|| SophonError::Io(io::Error::other("decompressed size overflow")))?;
        if next_bytes_written > output.expected_size {
            return Err(SophonError::SizeMismatch {
                item: format!("stream chunk at offset {}", output.chunk_offset),
                expected: output.expected_size,
                actual: next_bytes_written,
            });
        }
        #[cfg(feature = "bench-diagnostics")]
        let write_started = std::time::Instant::now();
        output
            .file
            .write_all_at(&buf[..n], output.write_offset)
            .map_err(SophonError::Io)?;
        #[cfg(feature = "bench-diagnostics")]
        if let Some(timings) = output.timings.as_ref() {
            timings.output_write_ns.fetch_add(
                write_started.elapsed().as_nanos() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        #[cfg(feature = "bench-diagnostics")]
        let hash_started = std::time::Instant::now();
        if let Some(hasher) = output.hasher.0.as_mut() {
            hasher.update(&buf[..n])?;
        }
        #[cfg(feature = "bench-diagnostics")]
        if let Some(timings) = output.timings.as_ref()
            && output.hasher.0.is_some()
        {
            timings.output_hash_ns.fetch_add(
                hash_started.elapsed().as_nanos() as u64,
                std::sync::atomic::Ordering::Relaxed,
            );
        }
        output.write_offset += n as u64;
        output.bytes_written = next_bytes_written;
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
        let comp_hash = md5_hex(&compressed);
        let decomp_hash = md5_hex(&original);

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(4096).unwrap();

        let written =
            eager_decompress_chunk(&compressed, &out_file, 0, 4096, &comp_hash, &decomp_hash)
                .unwrap();

        assert_eq!(written, 4096);

        let result = std::fs::read(&out_path).unwrap();
        assert_eq!(result, original);
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

        let result = eager_decompress_chunk(
            &compressed,
            &out_file,
            0,
            1024,
            "00000000000000000000000000000000", // wrong hash
            &md5_hex(&original),
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            SophonError::Md5Mismatch { .. } => {}
            other => panic!("expected Md5Mismatch, got: {other:?}"),
        }
    }

    #[test]
    fn eager_decompress_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let chunk_data = vec![0x42u8; 2048];
        let compressed = compress_data(&chunk_data);
        let comp_hash = md5_hex(&compressed);
        let decomp_hash = md5_hex(&chunk_data);

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        // File is 8192 bytes, chunk goes at offset 4096
        out_file.set_len(8192).unwrap();

        let written =
            eager_decompress_chunk(&compressed, &out_file, 4096, 2048, &comp_hash, &decomp_hash)
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
        let xxh = format!("{:016x}", xxhash_rust::xxh64::xxh64(&compressed, 0));
        let decomp_hash = md5_hex(&original);

        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(2048).unwrap();

        let written =
            eager_decompress_chunk(&compressed, &out_file, 0, 2048, &xxh, &decomp_hash).unwrap();

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
        let result = eager_decompress_chunk(&garbage, &out_file, 0, 1024, "", "");
        assert!(result.is_err());
    }

    #[test]
    fn eager_decompress_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let out_path = dir.path().join("output.bin");

        let original = vec![0x55u8; 1024];
        let compressed = compress_data(&original);
        let comp_hash = md5_hex(&compressed);

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
            "",
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
        let comp_hash = md5_hex(&compressed);

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
            "ffffffffffffffffffffffffffffffff", // wrong decompressed hash
        );

        assert!(result.is_err());
        match result.unwrap_err() {
            super::super::error::SophonError::Md5Mismatch { .. } => {}
            other => panic!("expected Md5Mismatch, got: {other:?}"),
        }
    }
}
