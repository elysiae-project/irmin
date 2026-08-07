use std::cell::RefCell;
use std::fs::{self, File, OpenOptions};

use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd as _;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread_local;
use std::time::{Duration, Instant};

use dashmap::DashMap;

thread_local! {
    static TRANSFER_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

use super::assembly_opt::{Md5, md5_hex_eq, md5_to_hex, return_md5, take_md5};
use super::cache::VerificationEntry;
use super::error::{SophonError, SophonResult};
use super::installer::ChunkNameLookup;
use super::{FILE_WRITE_BUFFER_SIZE, PROGRESS_UPDATE_INTERVAL_MS};
use crate::SophonProgress;
use crate::game_installer::compact_manifest::CompactManifest;

#[inline]
pub fn chunk_filename(chunk_name: &str) -> String {
    let mut s = String::with_capacity(chunk_name.len() + 5);
    s.push_str(chunk_name);
    s.push_str(".zstd");
    s
}

#[inline]
pub fn decrement_chunk_refcount(
    chunk_name: &str,
    chunk_lookup: &ChunkNameLookup,
    chunk_refcounts: &[AtomicU32],
    chunks_dir: &Path,
) {
    if !validate_chunk_name(chunk_name) {
        return;
    }
    let Some(idx) = chunk_lookup.lookup(chunk_name) else {
        return;
    };
    let prev = chunk_refcounts[idx].fetch_sub(1, Ordering::Release);
    if prev == 1 {
        std::sync::atomic::fence(Ordering::Acquire);
        let mut p = chunks_dir.join(chunk_lookup.get(idx));
        p.set_extension("zstd");
        let _ = fs::remove_file(&p);
    }
}

pub fn cleanup_tmp_files(dir: &Path) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            cleanup_tmp_files(&path)?;
        } else if path.extension().map(|ext| ext == "tmp").unwrap_or(false) {
            let _ = fs::remove_file(&path);
        }
    }
    Ok(())
}

/// Single-pass byte scan for path-safety. Rejects empty names, null bytes,
/// absolute paths, drive letters, and `..` path components.
pub fn validate_chunk_name(chunk_name: &str) -> bool {
    let b = chunk_name.as_bytes();
    if b.is_empty() {
        return false;
    }
    // Drive letter (e.g. "C:")
    if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
        return false;
    }
    // Absolute path
    if b[0] == b'/' || b[0] == b'\\' {
        return false;
    }
    // Single pass: detect null bytes and ".." components
    let mut seg_start: usize = 0;
    let mut i: usize = 0;
    while i < b.len() {
        match b[i] {
            0 => return false,
            b'/' | b'\\' => {
                if i - seg_start == 2 && b[seg_start] == b'.' && b[seg_start + 1] == b'.' {
                    return false;
                }
                seg_start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    // Check trailing segment
    if i - seg_start == 2 && b[seg_start] == b'.' && b[seg_start + 1] == b'.' {
        return false;
    }
    true
}

pub fn validate_asset_name(name: &str) -> SophonResult<()> {
    if name.is_empty() {
        return Err(SophonError::InvalidAssetName(
            "asset_name cannot be empty".into(),
        ));
    }
    if name.starts_with('/') || name.starts_with('\\') {
        return Err(SophonError::PathTraversal(name.into()));
    }
    if name.starts_with("./") || name.starts_with(".\\") {
        return Err(SophonError::PathTraversal(name.into()));
    }
    if name.split(&['/', '\\']).any(|component| component == "..") {
        return Err(SophonError::PathTraversal(name.into()));
    }
    if name.contains('\0') {
        return Err(SophonError::InvalidAssetName(
            "asset_name cannot contain null bytes".into(),
        ));
    }
    let mut chars = name.chars();
    if let (Some(first), Some(':')) = (chars.next(), chars.next())
        && first.is_ascii_alphabetic()
    {
        return Err(SophonError::PathTraversal(name.into()));
    }
    Ok(())
}

struct DecrementGuard<'a> {
    chunks: Vec<&'a str>,
    chunk_lookup: &'a ChunkNameLookup,
    chunk_refcounts: &'a [AtomicU32],
    chunks_dir: &'a Path,
}

impl Drop for DecrementGuard<'_> {
    fn drop(&mut self) {
        for chunk_name in self.chunks.drain(..) {
            decrement_chunk_refcount(
                chunk_name,
                self.chunk_lookup,
                self.chunk_refcounts,
                self.chunks_dir,
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn assemble_file(
    all_files: &CompactManifest,
    file_idx: usize,
    game_dir: &Path,
    chunks_dir: &Path,
    temp_dir: &Path,
    chunk_lookup: &ChunkNameLookup,
    chunk_refcounts: &[AtomicU32],
    verify_cache: &DashMap<String, VerificationEntry>,
    skip_md5_check: bool,
) -> SophonResult<()> {
    let file_name = all_files.file_name(file_idx);
    let file_size = all_files.file_size(file_idx);
    let file_hash_md5 = all_files.file_hash_md5(file_idx);
    let is_dir = all_files.is_directory(file_idx);
    let chunk_range = all_files.file_chunk_range(file_idx);

    validate_asset_name(file_name)?;
    if is_dir {
        return Ok(());
    }
    let target_path = game_dir.join(file_name);
    #[allow(clippy::needless_range_loop)]
    let mut name_buf = [0u8; 20];
    #[allow(clippy::needless_range_loop)]
    for i in 0..16 {
        let nibble = ((file_idx as u64) >> (60 - i * 4)) & 0xf;
        name_buf[i] = match nibble {
            0..=9 => b'0' + nibble as u8,
            _ => b'a' + nibble as u8 - 10,
        };
    }
    name_buf[16..20].copy_from_slice(b".tmp");
    let tmp_name = std::str::from_utf8(&name_buf).unwrap();
    let tmp_path = temp_dir.join(tmp_name);

    if !skip_md5_check && target_path.exists() {
        let already_valid = super::cache::check_file_md5_cached(
            &target_path,
            file_size,
            file_hash_md5,
            game_dir,
            verify_cache,
        )?;

        if already_valid {
            log::debug!(
                "assemble_file: skipping already-valid file '{name}' ({size} bytes, md5={md5})",
                name = file_name,
                size = file_size,
                md5 = file_hash_md5
            );
            for ci in chunk_range.start..chunk_range.end {
                decrement_chunk_refcount(
                    all_files.chunk(ci as usize).chunk_name,
                    chunk_lookup,
                    chunk_refcounts,
                    chunks_dir,
                );
            }
            return Ok(());
        }
        log::warn!(
            "assemble_file: file '{name}' exists but MD5 mismatch, re-assembling",
            name = file_name
        );
    }

    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let out_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)?;

    out_file.set_len(file_size)?;
    let _ = super::sysio::preallocate(&out_file, file_size);

    let mut total_written: u64 = 0;
    let has_file_hash = !file_hash_md5.is_empty();
    let total_chunks = (chunk_range.end - chunk_range.start) as usize;
    const MAX_PARALLEL_WORKERS: usize = 4;
    let num_workers = std::thread::available_parallelism()
        .map(|n| n.get().min(MAX_PARALLEL_WORKERS))
        .unwrap_or(1);

    let mut transfer_buffer = TRANSFER_BUF.with(|cell| {
        let mut buf = cell.take();
        if buf.capacity() < FILE_WRITE_BUFFER_SIZE {
            buf = Vec::with_capacity(FILE_WRITE_BUFFER_SIZE);
        }
        // Safety: buffer is fully overwritten by read() before write_all_at().
        unsafe { buf.set_len(FILE_WRITE_BUFFER_SIZE) };
        buf
    });
    let mut guard = DecrementGuard {
        chunks: Vec::with_capacity((chunk_range.end - chunk_range.start) as usize),
        chunk_lookup,
        chunk_refcounts,
        chunks_dir,
    };

    let chunk_offsets: Vec<u64> = all_files.file_chunk_offsets(file_idx);
    // Verify total decompressed size, check chunk hashes, and detect old chunks in one pass.
    let mut total_decompressed: u64 = 0;
    let mut all_chunks_have_hashes = true;
    let mut has_old_chunks = false;
    for ci in chunk_range.start..chunk_range.end {
        let chunk = all_files.chunk(ci as usize);
        total_decompressed = total_decompressed
            .checked_add(chunk.chunk_size_decompressed)
            .ok_or_else(|| SophonError::SizeMismatch {
                item: file_name.to_string(),
                expected: file_size,
                actual: total_decompressed,
            })?;
        if all_chunks_have_hashes
            && !super::assembly_opt::chunk_hash_required(chunk.chunk_decompressed_hash_md5)
        {
            all_chunks_have_hashes = false;
        }
        if chunk.chunk_old_offset >= 0 {
            has_old_chunks = true;
        }
    }
    if total_decompressed != file_size {
        return Err(SophonError::SizeMismatch {
            item: file_name.to_string(),
            expected: file_size,
            actual: total_decompressed,
        });
    }

    let parallelize =
        num_workers > 1 && total_chunks >= 4 && (all_chunks_have_hashes || has_file_hash);
    let mut file_hasher = if file_hash_md5.is_empty() {
        if !is_dir {
            log::warn!(
                "File '{name}' has no asset_hash_md5; assembled without file-level verification",
                name = file_name
            );
        }
        None
    } else if all_chunks_have_hashes || parallelize {
        None
    } else {
        Some(take_md5()?)
    };

    let parallel_hashed_file = parallelize && has_file_hash && !all_chunks_have_hashes;
    let hasher_result: Mutex<Option<SophonResult<[u8; 16]>>> = Mutex::new(None);

    let old_file: Option<File> = if has_old_chunks {
        Some(File::open(&target_path).map_err(SophonError::Io)?)
    } else {
        None
    };

    if parallelize {
        let total_written_atomic = AtomicU64::new(0);
        let first_error = Mutex::new(None);
        let has_error = AtomicBool::new(false);
        let downloaded_chunks: Vec<AtomicBool> =
            (0..total_chunks).map(|_| AtomicBool::new(false)).collect();
        let raw_fd = out_file.as_raw_fd();

        let chunk_done: Vec<AtomicBool> =
            (0..total_chunks).map(|_| AtomicBool::new(false)).collect();
        let chunk_infos: Vec<(u64, u64)> = (chunk_range.start..chunk_range.end)
            .map(|ci| {
                let i = (ci - chunk_range.start) as usize;
                let chunk = all_files.chunk(ci as usize);
                (chunk_offsets[i], chunk.chunk_size_decompressed)
            })
            .collect();

        let hasher_thread_handle: std::sync::OnceLock<std::thread::Thread> =
            std::sync::OnceLock::new();

        std::thread::scope(|s| {
            if parallel_hashed_file {
                let tmp_path = &tmp_path;
                let chunk_done = &chunk_done;
                let chunk_infos = &chunk_infos;
                let err_flag = &has_error;
                let hasher_result = &hasher_result;
                let ht = &hasher_thread_handle;
                let map_len = file_size as usize;
                s.spawn(move || {
                    ht.set(std::thread::current()).ok();
                    let file = match File::open(tmp_path) {
                        Ok(f) => f,
                        Err(e) => {
                            *hasher_result.lock().unwrap() = Some(Err(SophonError::Io(e)));
                            return;
                        }
                    };
                    let mut hasher = match super::assembly_opt::take_md5() {
                        Ok(h) => h,
                        Err(e) => {
                            *hasher_result.lock().unwrap() = Some(Err(e));
                            return;
                        }
                    };
                    // MAP_SHARED read-only mmap sees concurrent pwrite_at updates
                    // to the page cache, eliminating per-chunk read_at syscalls.
                    let map = match super::assembly_opt::mmap_shared_read_only(&file, map_len) {
                        Ok(m) => m,
                        Err(e) => {
                            *hasher_result.lock().unwrap() = Some(Err(SophonError::Io(e)));
                            return;
                        }
                    };
                    for (i, &(offset, size)) in chunk_infos.iter().enumerate() {
                        loop {
                            if err_flag.load(Ordering::Relaxed) {
                                return;
                            }
                            if chunk_done[i].load(Ordering::Acquire) {
                                break;
                            }
                            std::thread::park_timeout(std::time::Duration::from_millis(1));
                        }
                        let off = offset as usize;
                        let end = off + size as usize;
                        if let Err(e) = hasher.update(&map.as_slice()[off..end]) {
                            *hasher_result.lock().unwrap() = Some(Err(SophonError::Io(e)));
                            return;
                        }
                    }
                    match hasher.finish() {
                        Ok(digest) => {
                            *hasher_result.lock().unwrap() = Some(Ok(digest));
                            super::assembly_opt::return_md5(hasher);
                        }
                        Err(e) => {
                            *hasher_result.lock().unwrap() = Some(Err(SophonError::Io(e)));
                        }
                    }
                    drop(map);
                });
            }

            for worker in 0..num_workers {
                let indices: Vec<u32> = (chunk_range.start..chunk_range.end)
                    .enumerate()
                    .filter(|(i, _)| i % num_workers == worker)
                    .map(|(_, ci)| ci)
                    .collect();
                if indices.is_empty() {
                    continue;
                }
                let tw = &total_written_atomic;
                let err = &first_error;
                let err_flag = &has_error;
                let dl_chunks = &downloaded_chunks;
                let target = &target_path;
                let old = old_file.as_ref();
                let out = &out_file;
                let files = all_files;
                let cdir = chunks_dir;
                let skip_evict = parallel_hashed_file;
                let chunk_done = &chunk_done;
                let chunk_offsets = &chunk_offsets[..];
                let ht = &hasher_thread_handle;
                s.spawn(move || {
                    let mut transfer_buf = TRANSFER_BUF.with(|cell| {
                        let mut buf = cell.take();
                        if buf.capacity() < FILE_WRITE_BUFFER_SIZE {
                            buf = Vec::with_capacity(FILE_WRITE_BUFFER_SIZE);
                        }
                        unsafe { buf.set_len(FILE_WRITE_BUFFER_SIZE) };
                        buf
                    });
                    let mut chunk_path = cdir.to_path_buf();
                    for ci in indices {
                        if err_flag.load(Ordering::Relaxed) {
                            break;
                        }
                        let chunk = files.chunk(ci as usize);
                        let oi = (ci - chunk_range.start) as usize;
                        let offset = chunk_offsets[oi];
                        let result = if chunk.chunk_old_offset >= 0 {
                            let old_file = old.expect("has_old_chunks guarantees old file");
                            write_from_old_file(
                                old_file,
                                target,
                                out,
                                offset,
                                chunk.chunk_old_offset as u64,
                                chunk.chunk_size_decompressed,
                                None,
                                &mut transfer_buf,
                                chunk.chunk_decompressed_hash_md5,
                            )
                        } else {
                            if !validate_chunk_name(chunk.chunk_name) {
                                let mut guard = err.lock().unwrap();
                                if guard.is_none() {
                                    *guard = Some(SophonError::PathTraversal(
                                        chunk.chunk_name.into(),
                                    ));
                                }
                                err_flag.store(true, Ordering::Relaxed);
                                break;
                            }
                            chunk_path.push(chunk.chunk_name);
                            chunk_path.set_extension("zstd");
                            let result = write_decompressed_chunk_at(
                                &chunk_path,
                                out,
                                offset,
                                chunk.chunk_size_decompressed,
                                None,
                                chunk.chunk_decompressed_hash_md5,
                            );
                            chunk_path.pop();
                            result
                        };
                        match result {
                            Ok(bytes) => {
                                tw.fetch_add(bytes, Ordering::Relaxed);
                                if chunk.chunk_old_offset < 0 {
                                    let idx = (ci - chunk_range.start) as usize;
                                    dl_chunks[idx].store(true, Ordering::Relaxed);
                                }
                                if !skip_evict {
                                    super::assembly_opt::sync_and_evict_range(
                                        raw_fd,
                                        offset,
                                        chunk.chunk_size_decompressed,
                                    );
                                }
                                let idx = (ci - chunk_range.start) as usize;
                                chunk_done[idx].store(true, Ordering::Release);
                                if let Some(t) = ht.get() {
                                    t.unpark();
                                }
                            }
                            Err(e) => {
                                let mut guard = err.lock().unwrap();
                                if guard.is_none() {
                                    *guard = Some(e);
                                }
                                err_flag.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                    transfer_buf.clear();
                    TRANSFER_BUF.with(|cell| cell.replace(transfer_buf));
                });
            }
        });

        for (i, flag) in downloaded_chunks.iter().enumerate() {
            if flag.load(Ordering::Relaxed) {
                let ci = chunk_range.start + i as u32;
                guard.chunks.push(all_files.chunk(ci as usize).chunk_name);
            }
        }
        total_written = total_written_atomic.into_inner();

        if let Some(e) = first_error.into_inner().unwrap() {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    } else {
        let mut chunk_path = chunks_dir.to_path_buf();
        for ci in chunk_range.start..chunk_range.end {
            let chunk = all_files.chunk(ci as usize);
            let oi = (ci - chunk_range.start) as usize;
            let offset = chunk_offsets[oi];
            if chunk.chunk_old_offset >= 0 {
                let old_file = old_file
                    .as_ref()
                    .expect("has_old_chunks guarantees old file");
                let bytes_written = write_from_old_file(
                    old_file,
                    &target_path,
                    &out_file,
                    offset,
                    chunk.chunk_old_offset as u64,
                    chunk.chunk_size_decompressed,
                    file_hasher.as_mut(),
                    &mut transfer_buffer,
                    chunk.chunk_decompressed_hash_md5,
                )
                .inspect_err(|_| {
                    let _ = fs::remove_file(&tmp_path);
                })?;
                total_written += bytes_written;
            // Old-source chunks were never downloaded.
            } else {
                if !validate_chunk_name(chunk.chunk_name) {
                    let _ = fs::remove_file(&tmp_path);
                    return Err(SophonError::PathTraversal(chunk.chunk_name.into()));
                }
                chunk_path.push(chunk.chunk_name);
                chunk_path.set_extension("zstd");

                let bytes_written = write_decompressed_chunk_at(
                    &chunk_path,
                    &out_file,
                    offset,
                    chunk.chunk_size_decompressed,
                    file_hasher.as_mut(),
                    chunk.chunk_decompressed_hash_md5,
                )
                .inspect_err(|_| {
                    let _ = fs::remove_file(&tmp_path);
                })?;
                chunk_path.pop();

                total_written += bytes_written;
                guard.chunks.push(chunk.chunk_name);
            }
            // Flush and evict each chunk's output range to keep peak resident
            // memory low during assembly of large multi-chunk files.
            super::assembly_opt::sync_and_evict_range(
                out_file.as_raw_fd(),
                offset,
                chunk.chunk_size_decompressed,
            );
        }
    }

    // Set permissions on the tmp file before syncing; avoids a stat after rename.
    let _ = unsafe { libc::fchmod(out_file.as_raw_fd(), 0o644) };

    out_file.sync_data().map_err(|err| {
        let _ = fs::remove_file(&tmp_path);
        SophonError::Io(err)
    })?;

    if parallel_hashed_file {
        let result = hasher_result.into_inner().unwrap();
        match result {
            Some(Ok(ref digest)) if !md5_hex_eq(digest, file_hash_md5) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(SophonError::Md5Mismatch {
                    item: file_name.to_string(),
                    expected: file_hash_md5.to_string(),
                    actual: md5_to_hex(digest),
                });
            }
            Some(Ok(_)) => {}
            Some(Err(e)) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(e);
            }
            None => {}
        }
    }

    // Only evict the full file when per-chunk eviction was skipped (hasher needs pages resident).
    if parallel_hashed_file {
        super::assembly_opt::posix_advise(
            out_file.as_raw_fd(),
            0,
            file_size,
            libc::POSIX_FADV_DONTNEED,
        );
    }
    drop(out_file);

    if total_written != file_size {
        let _ = fs::remove_file(&tmp_path);
        return Err(SophonError::SizeMismatch {
            item: file_name.to_string(),
            expected: file_size,
            actual: total_written,
        });
    }

    if let Some(mut hasher) = file_hasher {
        let digest = hasher.finish()?;
        return_md5(hasher);
        if !md5_hex_eq(&digest, file_hash_md5) {
            let _ = fs::remove_file(&tmp_path);
            return Err(SophonError::Md5Mismatch {
                item: file_name.to_string(),
                expected: file_hash_md5.to_string(),
                actual: md5_to_hex(&digest),
            });
        }
    }

    if let Err(err) = fs::rename(&tmp_path, &target_path) {
        if err.raw_os_error() == Some(libc::EXDEV)
            || err.kind() == std::io::ErrorKind::CrossesDevices
        {
            log::warn!("rename EXDEV; falling back to copy + unlink: {err}");
            fs::copy(&tmp_path, &target_path)?;
            let _ = fs::remove_file(&tmp_path);
        } else {
            let _ = fs::remove_file(&tmp_path);
            return Err(SophonError::Io(err));
        }
    }

    transfer_buffer.clear();
    TRANSFER_BUF.with(|cell| cell.replace(transfer_buffer));

    Ok(())
}

fn write_decompressed_chunk_at(
    chunk_path: &Path,
    out_file: &File,
    offset: u64,
    expected_size: u64,
    file_hasher: Option<&mut Md5>,
    chunk_decompressed_hash_md5: &str,
) -> SophonResult<u64> {
    super::assembly_opt::decompress_chunk_optimized(
        chunk_path,
        out_file,
        offset,
        expected_size,
        file_hasher,
        chunk_decompressed_hash_md5,
    )
}

/// Copy decompressed bytes from an existing file to `out_file`. Large chunks
/// (>= 1 MiB) use the optimized path in `assembly_opt`; small chunks stream
/// through a thread-local buffer. Writes use `write_all_at` so no seek is
/// needed on the destination file.
#[allow(clippy::too_many_arguments)]
fn write_from_old_file(
    old_file: &std::fs::File,
    old_file_path: &std::path::Path,
    out_file: &File,
    new_offset: u64,
    old_offset: u64,
    expected_size: u64,
    mut file_hasher: Option<&mut Md5>,
    buffer: &mut [u8],
    chunk_decompressed_hash_md5: &str,
) -> SophonResult<u64> {
    const MMA_THRESHOLD: u64 = 1024 * 1024;

    if expected_size >= MMA_THRESHOLD {
        super::assembly_opt::write_chunk_from_mmap(
            old_file,
            old_file_path,
            out_file,
            new_offset,
            old_offset,
            expected_size,
            file_hasher,
            chunk_decompressed_hash_md5,
        )
    } else {
        let fd = old_file.as_raw_fd();
        super::assembly_opt::posix_advise(
            fd,
            old_offset,
            expected_size,
            libc::POSIX_FADV_SEQUENTIAL,
        );

        let mut write_offset = new_offset;
        let mut bytes_written: u64 = 0;
        let need_chunk_hash = super::assembly_opt::chunk_hash_required(chunk_decompressed_hash_md5);
        let mut chunk_hasher: Option<Md5> = if need_chunk_hash {
            Some(take_md5()?)
        } else {
            None
        };
        let mut remaining = expected_size;
        let mut read_offset = old_offset;

        while remaining > 0 {
            let to_read = remaining.min(buffer.len() as u64) as usize;
            let n = old_file
                .read_at(&mut buffer[..to_read], read_offset)
                .map_err(SophonError::Io)?;
            if n == 0 {
                break;
            }
            if let Some(ref mut ch) = chunk_hasher {
                ch.update(&buffer[..n])?;
            }
            if let Some(hasher) = file_hasher.as_deref_mut() {
                hasher.update(&buffer[..n])?;
            }
            out_file.write_all_at(&buffer[..n], write_offset)?;
            write_offset += n as u64;
            read_offset += n as u64;
            bytes_written += n as u64;
            remaining = remaining.saturating_sub(n as u64);
        }

        super::assembly_opt::posix_advise(fd, old_offset, expected_size, libc::POSIX_FADV_DONTNEED);

        if bytes_written != expected_size {
            return Err(SophonError::SizeMismatch {
                item: old_file_path.display().to_string(),
                expected: expected_size,
                actual: bytes_written,
            });
        }

        if let Some(ref mut ch) = chunk_hasher {
            let digest = ch.finish()?;
            return_md5(chunk_hasher.take().unwrap());
            if !md5_hex_eq(&digest, chunk_decompressed_hash_md5) {
                return Err(SophonError::Md5Mismatch {
                    item: old_file_path.display().to_string(),
                    expected: chunk_decompressed_hash_md5.to_string(),
                    actual: md5_to_hex(&digest),
                });
            }
        }

        Ok(bytes_written)
    }
}

pub struct AssemblyTaskParams {
    pub file_idx: usize,
    pub tmp_dir_idx: usize,
    pub all_files: Arc<CompactManifest>,
    pub all_tmp_dirs: Arc<Vec<std::path::PathBuf>>,
    pub game_dir: Arc<std::path::PathBuf>,
    pub chunks_dir: Arc<std::path::PathBuf>,
    pub chunk_refcounts: Arc<Vec<AtomicU32>>,
    pub chunk_names: Arc<ChunkNameLookup>,
    pub verify_cache: Arc<DashMap<String, VerificationEntry>>,
    pub assembled_files: Arc<AtomicU64>,
    pub checked_files: Arc<AtomicU64>,
    pub last_assembly_update: Arc<Mutex<Instant>>,
    pub total_files: u64,
    pub profiler: Arc<super::profiling::PipelineProfiler>,
    /// Live file completion bitset indexed by `file_idx`. Replaces the prior
    /// `Arc<Mutex<HashSet<String>>>` with a constant-memory atomic array: no
    /// per-file String alloc and zero lock contention.
    pub completion_flags: Arc<[AtomicBool]>,
}

pub fn run_assembly_task(
    params: AssemblyTaskParams,
    updater: impl Fn(SophonProgress) + Send + Sync + 'static,
) -> SophonResult<()> {
    let AssemblyTaskParams {
        file_idx,
        tmp_dir_idx,
        all_files,
        all_tmp_dirs,
        game_dir,
        chunks_dir,
        chunk_refcounts,
        chunk_names,
        verify_cache,
        assembled_files,
        checked_files,
        last_assembly_update,
        total_files,
        profiler,
        completion_flags,
    } = params;

    let _assembly_timer = super::profiling::AssemblyTimer::new(&profiler);

    if file_idx >= all_files.num_files() {
        return Err(SophonError::IndexOutOfBounds {
            kind: "file",
            index: file_idx,
        });
    }
    if tmp_dir_idx >= all_tmp_dirs.len() {
        return Err(SophonError::IndexOutOfBounds {
            kind: "temp dir",
            index: tmp_dir_idx,
        });
    }

    let tmp_dir = &all_tmp_dirs[tmp_dir_idx];
    let file_name = all_files.file_name(file_idx);
    let chunk_range = all_files.file_chunk_range(file_idx);

    // Claim the slot for this file once: the swap returns the previous value,
    // so true here means another task already finished it. Either way, after
    // this call the bit is true; the work branches below decide whether the
    // current task skips or proceeds.
    if completion_flags[file_idx].swap(true, Ordering::AcqRel) {
        for ci in chunk_range.start..chunk_range.end {
            decrement_chunk_refcount(
                all_files.chunk(ci as usize).chunk_name,
                &chunk_names,
                &chunk_refcounts,
                &chunks_dir,
            );
        }
        let checked = checked_files.fetch_add(1, Ordering::Relaxed) + 1;
        let count = assembled_files.fetch_add(1, Ordering::Relaxed) + 1;
        let force = checked == total_files || count == total_files;
        if force {
            updater(SophonProgress::CheckingFiles {
                checked_files: checked,
                total_files,
            });
            updater(SophonProgress::Assembling {
                assembled_files: count,
                total_files,
            });
        } else if let Ok(mut lu) = last_assembly_update.try_lock()
            && lu.elapsed() >= Duration::from_millis(PROGRESS_UPDATE_INTERVAL_MS)
        {
            updater(SophonProgress::CheckingFiles {
                checked_files: checked,
                total_files,
            });
            updater(SophonProgress::Assembling {
                assembled_files: count,
                total_files,
            });
            *lu = Instant::now();
        }
        _assembly_timer.finish();
        return Ok(());
    }

    let file_size = all_files.file_size(file_idx);
    let file_hash_md5 = all_files.file_hash_md5(file_idx);
    let target_path = game_dir.join(file_name);
    let needs_assembly = match target_path.metadata() {
        Ok(metadata) if metadata.len() == file_size => {
            if let Some(entry) = verify_cache.get(target_path.to_string_lossy().as_ref())
                && entry.size == file_size
                && entry.md5 == file_hash_md5
            {
                for ci in chunk_range.start..chunk_range.end {
                    decrement_chunk_refcount(
                        all_files.chunk(ci as usize).chunk_name,
                        &chunk_names,
                        &chunk_refcounts,
                        &chunks_dir,
                    );
                }
                false
            } else {
                true
            }
        }
        _ => true,
    };

    let checked = checked_files.fetch_add(1, Ordering::Relaxed) + 1;
    let force = checked == total_files;
    if force {
        updater(SophonProgress::CheckingFiles {
            checked_files: checked,
            total_files,
        });
    } else if let Ok(mut lu) = last_assembly_update.try_lock()
        && lu.elapsed() >= Duration::from_millis(PROGRESS_UPDATE_INTERVAL_MS)
    {
        updater(SophonProgress::CheckingFiles {
            checked_files: checked,
            total_files,
        });
        *lu = Instant::now();
    }

    if !needs_assembly {
        let count = assembled_files.fetch_add(1, Ordering::Relaxed) + 1;
        let force = count == total_files;
        if force {
            updater(SophonProgress::Assembling {
                assembled_files: count,
                total_files,
            });
        } else if let Ok(mut lu) = last_assembly_update.try_lock()
            && lu.elapsed() >= Duration::from_millis(PROGRESS_UPDATE_INTERVAL_MS)
        {
            updater(SophonProgress::Assembling {
                assembled_files: count,
                total_files,
            });
            *lu = Instant::now();
        }
        _assembly_timer.finish();
        return Ok(());
    }

    let result = assemble_file(
        &all_files,
        file_idx,
        &game_dir,
        &chunks_dir,
        tmp_dir,
        &chunk_names,
        &chunk_refcounts,
        &verify_cache,
        true,
    );
    if let Err(err) = result {
        // Release the claim so a later retry can reprocess this file_idx.
        completion_flags[file_idx].store(false, Ordering::Release);
        return Err(SophonError::AssemblyFailed {
            file: file_name.to_string(),
            error: err.to_string(),
        });
    }

    let count = assembled_files.fetch_add(1, Ordering::Relaxed) + 1;

    let force = count == total_files;
    if force {
        updater(SophonProgress::Assembling {
            assembled_files: count,
            total_files,
        });
    } else if let Ok(mut lu) = last_assembly_update.try_lock()
        && lu.elapsed() >= Duration::from_millis(PROGRESS_UPDATE_INTERVAL_MS)
    {
        updater(SophonProgress::Assembling {
            assembled_files: count,
            total_files,
        });
        *lu = Instant::now();
    }

    _assembly_timer.finish();

    Ok(())
}

pub async fn spawn_assembly_task(
    params: AssemblyTaskParams,
    updater: impl Fn(SophonProgress) + Send + Sync + 'static,
) -> SophonResult<()> {
    tokio::task::spawn_blocking(move || run_assembly_task(params, updater))
        .await
        .map_err(|_| SophonError::Io(std::io::Error::other("assembly task cancelled")))
        .and_then(|r| r)
}


#[cfg(test)]
#[path = "assembly_tests.rs"]
mod tests;
