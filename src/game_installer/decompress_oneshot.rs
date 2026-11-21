//! One-shot zstd chunk decompression extracted to a separate compilation
//! unit so codegen changes here do not shift [`super::assembly_opt`]'s
//! instruction layout (which is sensitive to the state_save canary).

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::assembly_opt::{
    Md5, bytes_to_hex, chunk_hash_required, md5_bytes_eq, md5_to_hex, mmap_read_only_unchecked,
    mmap_read_write, posix_advise, return_dctx, return_md5, take_dctx, take_md5,
};
use super::error::{SophonError, SophonResult};

/// Map the compressed chunk read-only instead of copying it into a per-worker
/// buffer.
///
/// The previous `pread` path staged each chunk in a pooled `Vec` owned by the
/// worker thread, so peak RSS scaled with `workers * chunk_size` instead of
/// with the output size. A private file mapping is backed by page cache
/// instead: those pages are clean and reclaimable (they count as `RssFile`,
/// not anonymous memory), so the kernel may drop them under pressure rather
/// than forcing them to swap or triggering the OOM killer.
#[inline]
pub(crate) fn map_compressed_chunk(
    file: &File,
    len: usize,
) -> SophonResult<super::assembly_opt::MmapGuard> {
    // SAFETY: `len` was validated against the file's on-disk length by
    // `resolve_map_len`, and chunk files are written once during download then
    // only read during assembly, so they are not truncated while mapped. This
    // process never writes them.
    let map = unsafe { mmap_read_only_unchecked(file, len) }.map_err(SophonError::Io)?;
    // The chunk is decompressed start-to-end exactly once, so ask for
    // sequential readahead. Best-effort: advice failures are not fatal.
    unsafe {
        libc::madvise(map.map_base, map.map_len, libc::MADV_SEQUENTIAL);
    }
    Ok(map)
}

/// Resolve the number of bytes to map for a compressed chunk.
///
/// The manifest's declared compressed size is untrusted input. Mapping past
/// EOF would make a later page touch raise SIGBUS, so the declared size must
/// fit inside the file. A declared size that EXCEEDS the on-disk length means
/// the chunk is truncated or corrupt: that is a hard error rather than a
/// silent clamp, because a shorter mapping would make the decompressor read a
/// partial frame and report a confusing failure.
#[inline]
pub(crate) fn resolve_map_len(
    declared: Option<u64>,
    on_disk_len: u64,
    chunk_path: &Path,
) -> SophonResult<usize> {
    let len = match declared {
        Some(s) => {
            if s > on_disk_len {
                return Err(SophonError::SizeMismatch {
                    item: chunk_path.display().to_string(),
                    expected: s,
                    actual: on_disk_len,
                });
            }
            s
        }
        None => on_disk_len,
    };
    usize::try_from(len)
        .map_err(|_| SophonError::Io(std::io::Error::other("compressed chunk too large to map")))
}

#[expect(
    clippy::too_many_arguments,
    reason = "matches the original assembly_opt signature"
)]
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
    let on_disk_len = f.metadata()?.len();
    let compressed_size = resolve_map_len(compressed_size, on_disk_len, chunk_path)?;
    if compressed_size == 0 {
        return Err(SophonError::Io(std::io::Error::other("empty chunk")));
    }

    let compressed_map = map_compressed_chunk(&f, compressed_size)?;
    let compressed = compressed_map.as_slice();

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
        // pooled DCtx only needs parameters toggled, not a full
        // session+parameter reset after every completed frame. Set the
        // window large enough for any chunk: a prior hdiffpatch clip on
        // the same thread may have left a smaller WindowLogMax sticky.
        let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(
            super::assembly_opt::MAX_WINDOW_LOG,
        ));
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

    // Unmap the compressed chunk BEFORE advising the page cache to drop it:
    // POSIX_FADV_DONTNEED only drops pages with no remaining mapping, so the
    // order here is what makes the eviction actually happen.
    drop(compressed_map);
    posix_advise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
    drop(f);
    out_mmap.flush_and_evict();
    drop(out_mmap);
    result
}
