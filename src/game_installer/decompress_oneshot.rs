//! One-shot zstd chunk decompression extracted to a separate compilation
//! unit so codegen changes here do not shift [`super::assembly_opt`]'s
//! instruction layout (which is sensitive to the state_save canary).

use std::fs::File;
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::assembly_opt::{
    Md5, bytes_to_hex, chunk_hash_required, md5_bytes_eq, md5_to_hex, mmap_read_write,
    posix_advise, return_compressed_buf, return_dctx, return_md5, take_compressed_buf,
    take_dctx, take_md5,
};
use super::error::{SophonError, SophonResult};

#[expect(clippy::too_many_arguments, reason = "matches the original assembly_opt signature")]
pub(crate) fn decompress_chunk_oneshot(
    chunk_path: &Path,
    out_file: &File,
    offset: u64,
    expected_size: u64,
    file_hasher: &mut Option<&mut Md5>,
    chunk_decompressed_hash_md5: &[u8],
    deferred_to_batch_hasher: bool,
    compressed_size: Option<u64>,
) -> SophonResult<u64> {
    if expected_size == 0 {
        return Ok(0);
    }
    let expected_usize = expected_size as usize;

    let f = File::open(chunk_path)?;
    let compressed_size = match compressed_size {
        Some(s) => s as usize,
        None => f.metadata()?.len() as usize,
    };
    if compressed_size == 0 {
        return Err(SophonError::Io(std::io::Error::other("empty chunk")));
    }

    // pread into thread-local buffer instead of mmap; saves ~5 syscalls/chunk.
    let mut compressed_buf = take_compressed_buf(compressed_size);
    let n = f.read_at(&mut compressed_buf[..compressed_size], 0)?;
    if n != compressed_size {
        return_compressed_buf(compressed_buf);
        return Err(SophonError::Io(std::io::Error::other(
            "short read on compressed chunk",
        )));
    }
    let compressed = &compressed_buf[..compressed_size];

    let mut out_mmap = mmap_read_write(out_file, offset, expected_usize)?;

    // Prefault output pages in a single kernel batch. Avoids per-page faults
    // during decompression. EINVAL means kernel < 5.14; silently degrade.
    // Skip for small regions: the zstd decompress touches every page anyway,
    // and the kernel batches small contiguous write faults efficiently.
    if out_mmap.map_len >= 2 * 1024 * 1024 {
        unsafe {
            let ret = libc::madvise(
                out_mmap.map_base,
                out_mmap.map_len,
                libc::MADV_POPULATE_WRITE,
            );
            if ret != 0 {
                let e = std::io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EINVAL) {
                    return Err(SophonError::Io(e));
                }
            }
        }
    }

    let output = out_mmap.as_mut_slice();

    let result = (|| {
        let mut ctx = take_dctx();
        // zstd DCtx parameters are sticky across decompress calls, so a
        // pooled DCtx only needs the checksum parameter toggled, not a
        // full session+parameter reset after every completed frame.
        // Only skip frame checksum when MD5 verification covers integrity:
        // in-worker, file-level, or deferred to the per-file batch hasher.
        let skip_checksum = chunk_hash_required(chunk_decompressed_hash_md5)
            || file_hasher.is_some()
            || deferred_to_batch_hasher;
        let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(
            skip_checksum,
        ));

        let written = ctx
            .decompress(output, compressed)
            .map_err(|e| SophonError::Io(std::io::Error::other(e.to_string())))?;
        return_dctx(ctx);

        if written != expected_usize {
            return Err(SophonError::SizeMismatch {
                item: chunk_path.display().to_string(),
                expected: expected_size,
                actual: written as u64,
            });
        }

        let need_chunk_hash = chunk_hash_required(chunk_decompressed_hash_md5);
        if need_chunk_hash {
            let mut chunk_hasher = take_md5()?;
            chunk_hasher.update(&output[..written])?;
            let digest = chunk_hasher.finish()?;
            return_md5(chunk_hasher);
            if !md5_bytes_eq(&digest, chunk_decompressed_hash_md5) {
                return Err(SophonError::Md5Mismatch {
                    item: chunk_path.display().to_string(),
                    expected: bytes_to_hex(chunk_decompressed_hash_md5),
                    actual: md5_to_hex(&digest),
                });
            }
        }

        if let Some(hasher) = file_hasher.as_deref_mut() {
            hasher.update(&output[..written])?;
        }

        Ok(expected_size)
    })();

    return_compressed_buf(compressed_buf);
    // Evict compressed chunk pages from page cache.
    posix_advise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    drop(f);
    out_mmap.flush_and_evict();
    drop(out_mmap);
    result
}
