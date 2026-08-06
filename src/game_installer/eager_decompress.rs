//! Inline download + decompression for eager install path.
//!
//! Feeds HTTP bytes to a zstd streaming decoder and writes decompressed output
//! directly to the target file at the chunk's offset via pwrite. Computes both
//! compressed and decompressed hashes inline.

use std::fs::File;
use std::io::{self, BufReader, Read, Write};
use std::os::unix::fs::FileExt;
use std::sync::Arc;

use super::assembly_opt::{
    self, chunk_hash_required, md5_hex_eq, md5_to_hex, return_md5, take_md5, xxh64_hex_eq, Md5,
};
use super::error::{SophonError, SophonResult};
use super::handle::DownloadHandle;

/// Buffer size for reading from the zstd decoder and writing to the output file.
const EAGER_BUFFER_SIZE: usize = 1024 * 1024;

/// Performs eager decompression: downloads a chunk, decompresses inline, and writes
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

    // Decompress and write to output file.
    let reader = BufReader::with_capacity(256 * 1024, compressed_data);
    let mut decoder =
        zstd::Decoder::new(reader).map_err(|e| SophonError::Io(io::Error::other(e.to_string())))?;

    // Skip zstd frame checksum when we verify via MD5.
    let need_decompressed_hash = chunk_hash_required(decompressed_hash);
    if need_decompressed_hash {
        let _ = decoder.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(true));
    }

    let mut decompressed_hasher: Option<Md5> = if need_decompressed_hash {
        Some(take_md5()?)
    } else {
        None
    };

    let mut buf = vec![0u8; EAGER_BUFFER_SIZE];
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
}
