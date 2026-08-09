//! Inline download + decompression for eager install path.
//!
//! Feeds HTTP bytes to a zstd streaming decoder and writes decompressed output
//! directly to the target file at the chunk's offset via pwrite. Computes both
//! compressed and decompressed hashes inline.
#![allow(dead_code, unused_imports)]

use std::fs::File;
use std::io::{self, Read};
use std::os::unix::fs::FileExt;

use super::assembly_opt::{
    self, chunk_hash_required, md5_hex_eq, md5_to_hex, return_md5, take_md5, xxh64_hex_eq, Md5,
};
use super::error::{SophonError, SophonResult};

/// Buffer size for reading from the zstd decoder and writing to the output file.
pub const EAGER_BUFFER_SIZE: usize = 1024 * 1024;

/// Performs eager decompression: decompresses compressed data and writes
/// decompressed output directly to `out_file` at `chunk_offset`.
///
/// Returns the number of decompressed bytes written on success.
pub fn eager_decompress_chunk(
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

    // Decompress and write to output file using pooled DCtx.
    let need_decompressed_hash = chunk_hash_required(decompressed_hash);
    let mut ctx = assembly_opt::take_dctx();
    ctx.reset(zstd::zstd_safe::ResetDirective::SessionAndParameters)
        .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx reset")))?;
    // Use MAX_WINDOW_LOG since we don't know the frame's actual window from metadata.
    let window_log = assembly_opt::MAX_WINDOW_LOG;
    ctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(window_log))
        .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx WindowLogMax")))?;
    if need_decompressed_hash {
        let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(true));
    }

    let mut decoder = zstd::Decoder::with_context(std::io::Cursor::new(compressed_data), &mut ctx);

    let mut decompressed_hasher: Option<Md5> = if need_decompressed_hash {
        Some(take_md5()?)
    } else {
        None
    };

    // Reuse thread-local buffer to avoid 1 MiB alloc per call.
    let mut buf = assembly_opt::take_eager_buf();
    let mut bytes_written: u64 = 0;
    let mut write_offset = chunk_offset;

    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| SophonError::Io(io::Error::other(e.to_string())))?;
        if n == 0 {
            break;
        }

        out_file
            .write_all_at(&buf[..n], write_offset)
            .map_err(SophonError::Io)?;

        if let Some(ref mut h) = decompressed_hasher {
            h.update(&buf[..n])?;
        }

        write_offset += n as u64;
        bytes_written += n as u64;
    }

    // Return pooled resources.
    // ctx is not pooled: window_log=26 exceeds the pool's 22-bit threshold.
    drop(decoder);
    drop(ctx);
    assembly_opt::return_eager_buf(buf);

    // Verify decompressed size.
    if bytes_written != expected_decompressed_size {
        return Err(SophonError::SizeMismatch {
            item: format!("eager chunk at offset {chunk_offset}"),
            expected: expected_decompressed_size,
            actual: bytes_written,
        });
    }

    // Verify decompressed hash.
    if let Some(mut h) = decompressed_hasher {
        let digest = h.finish()?;
        return_md5(h);
        if !md5_hex_eq(&digest, decompressed_hash) {
            return Err(SophonError::Md5Mismatch {
                item: format!("eager chunk decompressed at offset {chunk_offset}"),
                expected: decompressed_hash.to_string(),
                actual: md5_to_hex(&digest),
            });
        }
    }

    Ok(bytes_written)
}

/// Streaming variant: decompresses from a sync `Read` source directly to the
/// output file. Avoids buffering the entire compressed chunk in memory.
/// Used in non-Full verify modes where no compressed hash is needed.
pub fn stream_decompress_to_file(
    reader: impl io::Read,
    out_file: &File,
    chunk_offset: u64,
    expected_decompressed_size: u64,
) -> SophonResult<u64> {
    let mut ctx = assembly_opt::take_dctx();
    ctx.reset(zstd::zstd_safe::ResetDirective::SessionAndParameters)
        .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx reset")))?;
    ctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
        assembly_opt::MAX_WINDOW_LOG,
    ))
    .map_err(|_| SophonError::Io(io::Error::other("zstd DCtx WindowLogMax")))?;

    let mut decoder =
        zstd::Decoder::with_context(io::BufReader::with_capacity(65536, reader), &mut ctx);

    let mut buf = assembly_opt::take_eager_buf();
    let mut bytes_written: u64 = 0;
    let mut write_offset = chunk_offset;

    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| SophonError::Io(io::Error::other(e.to_string())))?;
        if n == 0 {
            break;
        }

        out_file
            .write_all_at(&buf[..n], write_offset)
            .map_err(SophonError::Io)?;

        write_offset += n as u64;
        bytes_written += n as u64;
    }

    drop(decoder);
    drop(ctx);
    assembly_opt::return_eager_buf(buf);

    if bytes_written != expected_decompressed_size {
        return Err(SophonError::SizeMismatch {
            item: format!("stream chunk at offset {chunk_offset}"),
            expected: expected_decompressed_size,
            actual: bytes_written,
        });
    }

    Ok(bytes_written)
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
