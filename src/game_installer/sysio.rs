//! Linux syscall wrappers for low-level assembly I/O.

use std::fs::File;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::Path;

use super::error::{SophonError, SophonResult};

/// Kernel-side copy between two open file descriptors. Mirrors
/// `copy_file_range(2)`: copies up to `len` bytes from `src_fd` at
/// `src_off` to `dst_fd` at `dst_off`. Returns the number of bytes copied.
/// Loops if the kernel copies fewer than requested per call.
/// Copy a byte range between file descriptors using the platform copy-range facility.
#[inline(always)]
pub fn copy_file_range(
    src_fd: RawFd,
    src_off: u64,
    dst_fd: RawFd,
    dst_off: u64,
    len: u64,
) -> SophonResult<u64> {
    // copy_file_range takes off_t* (in/out); pass mutable stack slots so the
    // kernel updates the running offsets across iterations.
    let mut src_off_val: libc::off_t = src_off as libc::off_t;
    let mut dst_off_val: libc::off_t = dst_off as libc::off_t;
    let mut copied: u64 = 0;
    while copied < len {
        let remaining = len - copied;
        let this_iter = remaining.min(u64::try_from(isize::MAX).unwrap_or(usize::MAX as u64));
        let n = unsafe {
            libc::copy_file_range(
                src_fd,
                &mut src_off_val,
                dst_fd,
                &mut dst_off_val,
                this_iter as usize,
                0,
            )
        };
        if n < 0 {
            return Err(SophonError::Io(std::io::Error::last_os_error()));
        }
        if n == 0 {
            break;
        }
        copied += n as u64;
    }
    Ok(copied)
}

/// Copy a region from an open source file into `dst_file` at `dst_off` using
/// `copy_file_range`. Applies `POSIX_FADV_DONTNEED` on the source region
/// once copied so the source pages do not stay resident.
/// Copy a region from `src` to `dst` using `copy_file_range`. Evicts source
/// pages after the copy. Callers handle destination eviction.
/// Copy file contents from a source path into an existing destination handle.
#[inline]
pub fn copy_file_from(
    src: &File,
    src_off: u64,
    dst: &File,
    dst_off: u64,
    len: u64,
) -> SophonResult<u64> {
    let src_fd = src.as_raw_fd();
    let dst_fd = dst.as_raw_fd();
    let copied = copy_file_range(src_fd, src_off, dst_fd, dst_off, len)?;
    let _ = unsafe {
        libc::posix_fadvise(
            src_fd,
            src_off as libc::off_t,
            len as libc::off_t,
            libc::POSIX_FADV_DONTNEED,
        )
    };
    Ok(copied)
}

/// Copy a region of `src_path` into `dst_file` at `dst_off` using
/// `copy_file_range`. Applies `POSIX_FADV_DONTNEED` on the source region
/// once copied so the source pages do not stay resident.
/// Copy a specified source-file region into a destination file at an offset.
pub fn copy_file_region_to(
    src_path: &Path,
    src_off: u64,
    dst_file: &File,
    dst_off: u64,
    len: u64,
) -> SophonResult<u64> {
    let src = File::open(src_path).map_err(SophonError::Io)?;
    copy_file_from(&src, src_off, dst_file, dst_off, len)
}

/// Pre-allocate disk blocks for `file` up to `len` bytes using
/// `fallocate(2)` with `FALLOC_FL_KEEP_SIZE`. Allocates extents without
/// zeroing or changing file size. Silently ignores unsupported filesystems.
/// Reserve space for `len` bytes in `file`, returning the operating-system error.
pub fn preallocate(file: &File, len: u64) -> std::io::Result<()> {
    if len == 0 {
        return Ok(());
    }
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let ret = {
        let len = libc::off64_t::try_from(len).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preallocation length exceeds off64_t",
            )
        })?;
        unsafe { libc::fallocate64(file.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 0, len) }
    };
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let ret = {
        let len = libc::off_t::try_from(len).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "preallocation length exceeds off_t",
            )
        })?;
        unsafe { libc::fallocate(file.as_raw_fd(), libc::FALLOC_FL_KEEP_SIZE, 0, len) }
    };
    if ret != 0 {
        let e = std::io::Error::last_os_error();
        match e.raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::ENOSYS) => {}
            _ => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Seek;
    use std::io::Write;
    use std::os::unix::fs::FileExt;

    #[test]
    fn preallocate_rejects_lengths_outside_native_off_t() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preallocated.bin");
        let file = File::create(&path).unwrap();
        let too_large = (libc::off_t::MAX as u64).saturating_add(1);

        assert_eq!(
            preallocate(&file, too_large).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(
            preallocate(&file, u64::MAX).unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn preallocate_gnu_large_file_range_exceeds_32_bit_off_t() {
        let len = u64::from(u32::MAX) + 1;
        let large_file_len = libc::off64_t::try_from(len).unwrap();
        assert_eq!(large_file_len as u64, len);

        #[cfg(target_pointer_width = "32")]
        assert!(libc::off_t::try_from(len).is_err());
    }

    #[test]
    fn preallocate_zero_length_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preallocated.bin");
        let file = File::create(&path).unwrap();

        preallocate(&file, 0).unwrap();
        assert_eq!(file.metadata().unwrap().len(), 0);
    }

    #[test]
    fn copy_file_range_basic() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.bin");
        let dst_path = dir.path().join("dst.bin");
        let data = b"hello copy_file_range world";
        std::fs::write(&src_path, data).unwrap();

        let dst = File::create(&dst_path).unwrap();
        // Pre-size so the offset write is valid.
        dst.set_len(data.len() as u64).unwrap();
        let n = copy_file_region_to(&src_path, 0, &dst, 0, data.len() as u64).unwrap();
        assert_eq!(n, data.len() as u64);
        dst.sync_all().unwrap();
        drop(dst);
        assert_eq!(std::fs::read(&dst_path).unwrap(), data);
    }

    #[test]
    fn copy_file_range_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.bin");
        let dst_path = dir.path().join("dst.bin");
        std::fs::write(&src_path, b"AAAABBBBCCCCDDDD").unwrap();

        let dst = File::create(&dst_path).unwrap();
        dst.set_len(16).unwrap();
        // Copy bytes 4..8 ("BBBB") to destination offset 0.
        let n = copy_file_region_to(&src_path, 4, &dst, 0, 4).unwrap();
        assert_eq!(n, 4);
        dst.sync_all().unwrap();
        drop(dst);
        assert_eq!(&std::fs::read(&dst_path).unwrap()[..4], b"BBBB");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copy_file_range_permuted_large_ranges_preserves_explicit_offsets() {
        const CHUNK_SIZE: usize = 1024 * 1024;
        const CHUNK_COUNT: usize = 4;
        let dir = tempfile::tempdir().unwrap();
        let src_path = dir.path().join("src.bin");
        let dst_path = dir.path().join("dst.bin");

        let mut source_file = File::create(&src_path).unwrap();
        let mut source_chunk = vec![0; CHUNK_SIZE];
        let mut source_hashes = Vec::with_capacity(CHUNK_COUNT);
        for index in 0..CHUNK_COUNT {
            source_chunk.fill((index + 1) as u8);
            source_hashes.push(fast_md5::digest(&source_chunk));
            source_file.write_all(&source_chunk).unwrap();
        }
        source_file.sync_all().unwrap();
        drop(source_file);
        let mut src = File::open(&src_path).unwrap();
        let mut dst = File::create(&dst_path).unwrap();
        let sentinel = 0xA5;
        let sentinel_chunk = vec![sentinel; CHUNK_SIZE];
        for _ in 0..CHUNK_COUNT {
            dst.write_all(&sentinel_chunk).unwrap();
        }
        dst.write_all(&sentinel_chunk).unwrap();
        dst.sync_all().unwrap();
        dst.seek(std::io::SeekFrom::Start(0)).unwrap();

        for output_index in 0..CHUNK_COUNT {
            let source_index = (output_index * 3) % CHUNK_COUNT;
            let source_offset = (source_index * CHUNK_SIZE) as u64;
            let destination_offset = (output_index * CHUNK_SIZE) as u64;
            let copied = match copy_file_from(
                &src,
                source_offset,
                &dst,
                destination_offset,
                CHUNK_SIZE as u64,
            ) {
                Ok(copied) => copied,
                Err(error) => panic!("copy_file_from failed: {error}"),
            };
            assert_eq!(copied, CHUNK_SIZE as u64);
            assert_eq!(src.stream_position().unwrap(), 0);
            assert_eq!(dst.stream_position().unwrap(), 0);
        }

        dst.sync_all().unwrap();
        let output_file = File::open(&dst_path).unwrap();
        let mut output_chunk = vec![0; CHUNK_SIZE];
        for output_index in 0..CHUNK_COUNT {
            let source_index = (output_index * 3) % CHUNK_COUNT;
            let output_start = output_index * CHUNK_SIZE;
            output_file
                .read_exact_at(&mut output_chunk, output_start as u64)
                .unwrap();
            assert_eq!(fast_md5::digest(&output_chunk), source_hashes[source_index]);
            assert!(
                output_chunk
                    .iter()
                    .all(|&byte| byte == (source_index + 1) as u8)
            );
        }
        assert_eq!(
            dst.metadata().unwrap().len(),
            (CHUNK_SIZE * (CHUNK_COUNT + 1)) as u64
        );
        output_file
            .read_exact_at(&mut output_chunk, (CHUNK_SIZE * CHUNK_COUNT) as u64)
            .unwrap();
        assert!(output_chunk.iter().all(|&byte| byte == sentinel));
    }

    #[test]
    fn copy_file_range_missing_source_fails() {
        let dir = tempfile::tempdir().unwrap();
        let dst_path = dir.path().join("dst.bin");
        let dst = File::create(&dst_path).unwrap();
        let nope = dir.path().join("nope.bin");
        let res = copy_file_region_to(&nope, 0, &dst, 0, 4);
        assert!(res.is_err());
    }
}
