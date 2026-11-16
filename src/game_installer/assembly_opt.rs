//! Optimized assembly helpers with direct offset writes and kernel-side copies.

use std::cell::RefCell;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::os::unix::fs::FileExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use super::FILE_WRITE_BUFFER_SIZE;
use super::error::{SophonError, SophonResult};
use super::sysio;

const ASSEMBLY_BUFFER_SIZE: usize = 1024 * 1024;

const ONESHOT_MAX_SIZE: usize = 64 * 1024 * 1024;

const EMPTY_MD5_BYTES: [u8; 16] = [0; 16];

pub(crate) const MAX_WINDOW_LOG: u32 = 26;

/// Compute the minimum `WindowLogMax` that fits a zstd frame of
/// `decompressed_size`. The zstd window must be a power of 2 and at least
/// as large as the decompressed size, so we round up to the next power of
/// two and use its log2. Clamped to libzstd's `[10, 26]` valid range.
#[inline]
pub(crate) fn window_log_for_size(decompressed_size: u64) -> u32 {
    const MIN_WINDOW_LOG: u32 = 10;
    let pow2 = decompressed_size.max(1).next_power_of_two();
    let log = pow2.trailing_zeros();
    log.clamp(MIN_WINDOW_LOG, MAX_WINDOW_LOG)
}

/// MD5 hasher backed by hand-written x86_64 assembly (fast-md5 crate).
/// No FFI overhead — the compression function inlines into the update loop.
pub struct Md5 {
    inner: fast_md5::Md5,
}

impl Md5 {
    /// Initialize a reusable MD5 context, returning an error if backend setup fails.
    pub fn new() -> SophonResult<Self> {
        Ok(Self {
            inner: fast_md5::Md5::new(),
        })
    }

    #[inline]
    pub fn update(&mut self, data: &[u8]) -> io::Result<()> {
        self.inner.update(data);
        Ok(())
    }

    #[inline]
    pub fn finish(&mut self) -> io::Result<[u8; 16]> {
        let state = std::mem::replace(&mut self.inner, fast_md5::Md5::new());
        Ok(state.finalize())
    }

    #[inline]
    pub(crate) fn reset(&mut self) {
        self.inner = fast_md5::Md5::new();
    }
}

thread_local! {
    static MD5_POOL: RefCell<Option<Md5>> = const { RefCell::new(None) };
}

#[inline]
pub(crate) fn take_md5() -> SophonResult<Md5> {
    MD5_POOL.with(|cell| cell.borrow_mut().take().map(Ok).unwrap_or_else(Md5::new))
}

#[inline]
pub(crate) fn return_md5(md5: Md5) {
    let mut md5 = md5;
    md5.reset();
    let _ = MD5_POOL.try_with(|cell| cell.borrow_mut().replace(md5));
}

#[inline]
pub(crate) fn take_xxh64() -> xxhash_rust::xxh64::Xxh64 {
    xxhash_rust::xxh64::Xxh64::new(0)
}

#[inline]
pub(crate) fn return_xxh64(_hasher: xxhash_rust::xxh64::Xxh64) {}

/// Upper bound on the per-thread read buffer retained for hashing.
pub(crate) const HASH_READ_BUFFER_SIZE: usize = 512 * 1024;

thread_local! {
    static HASH_READ_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Thread-local read buffer reused across file-hash calls.
///
/// Retains at most [`HASH_READ_BUFFER_SIZE`] bytes per thread; larger requested
/// capacities use a local allocation so they cannot evict the retained buffer.
pub(crate) struct HashReadBuffer {
    buffer: Vec<u8>,
    /// Length handed to each `read`, bounded by the caller's request.
    read_len: usize,
    /// Whether `buffer` came from the thread-local pool and should return to it.
    pooled: bool,
}

impl HashReadBuffer {
    #[inline(always)]
    pub(crate) fn take(capacity: usize) -> Self {
        if tls_reclaim_stale() {
            HASH_READ_BUF.with(|cell| {
                *cell.borrow_mut() = Vec::new();
            });
        }
        if capacity > HASH_READ_BUFFER_SIZE {
            return Self {
                buffer: vec![0; capacity],
                read_len: capacity,
                pooled: false,
            };
        }
        debug_assert!(capacity > 0, "hash read buffer capacity must be nonzero");
        let mut buffer = HASH_READ_BUF
            .try_with(|cell| std::mem::take(&mut *cell.borrow_mut()))
            .unwrap_or_default();
        if buffer.len() < capacity {
            // `resize` grows amortized and can overshoot the retention cap.
            buffer.reserve_exact(capacity - buffer.len());
            buffer.resize(capacity, 0);
        }
        Self {
            buffer,
            read_len: capacity,
            pooled: true,
        }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        &mut self.buffer[..self.read_len]
    }

    pub(crate) fn filled(&self, len: usize) -> &[u8] {
        &self.buffer[..len]
    }
}

impl Drop for HashReadBuffer {
    #[inline]
    fn drop(&mut self) {
        if self.pooled && self.buffer.capacity() <= HASH_READ_BUFFER_SIZE {
            let buffer = std::mem::take(&mut self.buffer);
            let _ = HASH_READ_BUF.try_with(|cell| *cell.borrow_mut() = buffer);
        }
    }
}

/// Compare an MD5 digest against an expected lowercase hex string without
/// allocating. Returns false on length mismatch.
#[inline(always)]
#[cfg_attr(feature = "benchmark", allow(unreachable_pub))]
pub fn md5_hex_eq(digest: &[u8; 16], expected: &str) -> bool {
    if expected.len() != 32 {
        return false;
    }
    let bytes = expected.as_bytes();
    let mut i = 0;
    while i < 16 {
        let hi = HEX_DECODE_LUT[bytes[i * 2] as usize];
        let lo = HEX_DECODE_LUT[bytes[i * 2 + 1] as usize];
        if hi == 0xFF || lo == 0xFF || (hi << 4) | lo != digest[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Compare an MD5 digest against an expected binary hash. Returns false on
/// length mismatch.
#[inline(always)]
pub(crate) fn md5_bytes_eq(digest: &[u8; 16], expected: &[u8]) -> bool {
    expected.len() == 16 && expected == digest
}

/// Compare a u64 value against a 16-char lowercase hex string without
/// allocating. Used for XXH64 hash verification.
#[inline]
#[cfg_attr(feature = "benchmark", allow(unreachable_pub))]
pub fn xxh64_hex_eq(value: u64, expected: &str) -> bool {
    if expected.len() != 16 {
        return false;
    }
    let bytes = expected.as_bytes();
    let mut acc: u64 = 0;
    let mut i = 0;
    while i < 16 {
        let nibble = HEX_DECODE_LUT[bytes[i] as usize];
        if nibble == 0xFF {
            return false;
        }
        acc = (acc << 4) | nibble as u64;
        i += 1;
    }
    acc == value
}

/// Compare a u64 value against an 8-byte binary hash.
#[inline]
pub(crate) fn xxh64_bytes_eq(value: u64, expected: &[u8]) -> bool {
    if expected.len() != 8 {
        return false;
    }
    let bytes: [u8; 8] = expected.try_into().unwrap_or([0; 8]);
    u64::from_be_bytes(bytes) == value
}

/// Encode an MD5 digest to a lowercase hex String. Only for error paths.
#[inline(always)]
pub fn md5_to_hex(digest: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in digest {
        s.push(hex_digit(b >> 4));
        s.push(hex_digit(b & 0xf));
    }
    s
}

/// Encode binary bytes to a lowercase hex String. Only for error paths.
pub fn bytes_to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(hex_digit(b >> 4));
        s.push(hex_digit(b & 0xf));
    }
    s
}

#[inline]
fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + n - 10) as char,
    }
}

/// Lookup table mapping each byte to its hex nibble value (0-15) or 0xFF for
/// invalid hex. Built at compile time.
static HEX_DECODE_LUT: [u8; 256] = {
    let mut lut = [0xFFu8; 256];
    let mut i = 0;
    while i < 256 {
        lut[i] = match i as u8 {
            b'0'..=b'9' => i as u8 - b'0',
            b'a'..=b'f' => i as u8 - b'a' + 10,
            b'A'..=b'F' => i as u8 - b'A' + 10,
            _ => 0xFF,
        };
        i += 1;
    }
    lut
};

/// Decode a hex string to binary. Returns `None` on invalid hex or
/// odd length. Uses a 256-entry lookup table for single-access nibble decode.
/// Decode hex into a caller-provided buffer. Returns the number of bytes
/// written, or `None` if `hex` is odd-length, contains non-hex chars, or
/// doesn't fit in `out`.
pub(crate) fn hex_decode_into(hex: &str, out: &mut [u8]) -> Option<usize> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let len = bytes.len() / 2;
    if len > out.len() {
        return None;
    }
    for i in 0..len {
        let hi = HEX_DECODE_LUT[bytes[i * 2] as usize];
        let lo = HEX_DECODE_LUT[bytes[i * 2 + 1] as usize];
        if hi == 0xFF || lo == 0xFF {
            return None;
        }
        out[i] = (hi << 4) | lo;
    }
    Some(len)
}

#[cfg_attr(not(feature = "benchmark"), allow(dead_code))]
pub fn hex_to_bytes(hex: &str) -> Option<Vec<u8>> {
    let bytes = hex.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = HEX_DECODE_LUT[bytes[i] as usize];
        let lo = HEX_DECODE_LUT[bytes[i + 1] as usize];
        if hi == 0xFF || lo == 0xFF {
            return None;
        }
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

use std::sync::atomic::{AtomicU64, Ordering};

/// Generation counter for lazy thread-local pool cleanup. `reclaim_memory()`
/// increments it; `take_*` functions clear their buffer when the stored
/// generation is stale. Reclaims pools on all threads without cross-thread
/// TLS access.
static RECLAIM_GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static OPT_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    static ZSTD_DCTX: RefCell<Option<zstd::zstd_safe::DCtx<'static>>> = const { RefCell::new(None) };
    static TLS_RECLAIM_GEN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

fn tls_reclaim_stale() -> bool {
    let generation = RECLAIM_GENERATION.load(Ordering::Relaxed);
    let stale = TLS_RECLAIM_GEN.get() != generation;
    if stale {
        TLS_RECLAIM_GEN.set(generation);
    }
    stale
}

/// Clears thread-local pools on the calling thread and bumps the global
/// reclaim generation so other threads lazily clear their pools on next use.
pub(crate) fn trim_thread_local_buffers() {
    RECLAIM_GENERATION.fetch_add(1, Ordering::Relaxed);
    TLS_RECLAIM_GEN.set(RECLAIM_GENERATION.load(Ordering::Relaxed));
    OPT_BUFFER.with(|cell| {
        cell.borrow_mut().clear();
        cell.borrow_mut().shrink_to_fit();
    });
    ZSTD_DCTX.with(|cell| {
        *cell.borrow_mut() = None;
    });
    COMPRESSED_BUF.with(|cell| {
        cell.borrow_mut().clear();
        cell.borrow_mut().shrink_to_fit();
    });
    HASH_READ_BUF.with(|cell| {
        let mut buf = cell.borrow_mut();
        buf.clear();
        buf.shrink_to_fit();
    });
}

/// Takes a zstd decompression context, creating a fresh one if the pool is empty.
#[inline]
pub(crate) fn take_dctx() -> zstd::zstd_safe::DCtx<'static> {
    ZSTD_DCTX.with(|cell| {
        if tls_reclaim_stale() {
            *cell.borrow_mut() = None;
        }
        cell.borrow_mut()
            .take()
            .unwrap_or_else(zstd::zstd_safe::DCtx::create)
    })
}

#[inline]
pub(crate) fn return_dctx(ctx: zstd::zstd_safe::DCtx<'static>) {
    // Pools the DCtx for reuse on the next decompress, avoiding a
    // malloc+free cycle per chunk. Memory cost: up to 2^window_log per
    // thread; bounded by MAX_WINDOW_LOG worst case. Reclaim via
    // trim_thread_local_buffers drops all pooled DCtxs across threads.
    ZSTD_DCTX.with(|cell| cell.borrow_mut().replace(ctx));
}

pub(crate) struct MmapGuard {
    pub(crate) ptr: *mut libc::c_void,
    pub(crate) len: usize,
    pub(crate) map_base: *mut libc::c_void,
    pub(crate) map_len: usize,
    pub(crate) file_offset: u64,
    pub(crate) fd: std::os::unix::io::RawFd,
}

impl MmapGuard {
    pub(crate) fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr as *const u8, self.len) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr as *mut u8, self.len) }
    }

    pub(crate) fn flush_and_evict(&mut self) {
        // Queues writeback of dirty pages without waiting for completion
        // (no WAIT_AFTER). MADV_DONTNEED evicts clean pages immediately;
        // dirty pages stay until writeback completes, then the kernel
        // reclaims them.
        // Only valid for guards from `mmap_read_write` (fd != -1).
        debug_assert_ne!(self.fd, -1, "flush_and_evict requires a write mmap");
        unsafe {
            libc::sync_file_range(
                self.fd,
                self.file_offset as libc::off64_t,
                self.len as libc::off64_t,
                libc::SYNC_FILE_RANGE_WAIT_BEFORE | libc::SYNC_FILE_RANGE_WRITE,
            );
        }
        unsafe {
            libc::madvise(self.map_base, self.map_len, libc::MADV_DONTNEED);
        }
    }

    pub(crate) fn evict_prefix(&self, start: usize, end: usize) -> usize {
        let page = page_size();
        let aligned_start = start.min(self.len).div_ceil(page) * page;
        let aligned_end = end.min(self.len) / page * page;
        if aligned_end <= aligned_start {
            return start;
        }
        let _ = unsafe {
            libc::madvise(
                (self.map_base as *mut u8).add(aligned_start).cast(),
                aligned_end - aligned_start,
                libc::MADV_DONTNEED,
            )
        };
        aligned_end
    }
}

impl Drop for MmapGuard {
    #[inline]
    fn drop(&mut self) {
        if self.map_base != libc::MAP_FAILED && !self.map_base.is_null() {
            unsafe { libc::munmap(self.map_base, self.map_len) };
        }
    }
}

/// Map the prefix of a file for read-only access with kernel-default paging behavior.
///
/// # Safety
/// The caller must ensure `len` does not exceed the file size and no process modifies or truncates
/// the file while the mapping is used. Truncation can cause `SIGBUS` when mapped pages are read.
pub(crate) unsafe fn mmap_read_only_unchecked(file: &File, len: usize) -> io::Result<MmapGuard> {
    if len == 0 {
        return Err(io::Error::other("empty file"));
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    Ok(MmapGuard {
        ptr,
        len,
        map_base: ptr,
        map_len: len,
        file_offset: 0,
        fd: -1,
    })
}

/// MAP_SHARED read-only mapping: sees concurrent pwrite updates to the file's
/// page cache. Used by the parallel hasher thread to hash assembling output
/// without per-chunk read_at syscalls. Advice is best-effort.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SharedMmapAdvice {
    PopulateRead,
    DemandSequential,
}

pub(crate) fn mmap_shared_read_only(
    file: &File,
    len: usize,
    advice: SharedMmapAdvice,
) -> io::Result<MmapGuard> {
    if len == 0 {
        return Err(io::Error::other("empty file"));
    }
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if ptr == libc::MAP_FAILED {
        return Err(io::Error::last_os_error());
    }
    unsafe {
        match advice {
            SharedMmapAdvice::PopulateRead if len <= 256 * 1024 * 1024 => {
                libc::madvise(ptr, len, libc::MADV_POPULATE_READ);
            }
            SharedMmapAdvice::PopulateRead => {
                libc::madvise(ptr, len, libc::MADV_SEQUENTIAL);
            }
            SharedMmapAdvice::DemandSequential => {
                libc::madvise(ptr, len, libc::MADV_SEQUENTIAL);
            }
        }
        libc::madvise(ptr, len, libc::MADV_HUGEPAGE);
    }
    Ok(MmapGuard {
        ptr,
        len,
        map_base: ptr,
        map_len: len,
        file_offset: 0,
        fd: -1,
    })
}

fn page_size() -> usize {
    static CACHED: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| unsafe { libc::sysconf(libc::_SC_PAGE_SIZE) } as usize)
}

pub(crate) fn mmap_read_write(file: &File, offset: u64, len: usize) -> io::Result<MmapGuard> {
    if len == 0 {
        return Err(io::Error::other("zero-length mmap"));
    }
    let ps = page_size();
    let page_mask = !(ps - 1);
    let map_offset = offset & page_mask as u64;
    let ptr_offset = (offset - map_offset) as usize;
    let map_len = len + ptr_offset;
    unsafe {
        let base = libc::mmap(
            std::ptr::null_mut(),
            map_len,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            map_offset as libc::off_t,
        );
        if base == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Hint transparent hugepages for regions >= 2 MiB to reduce TLB pressure.
        if map_len >= 2 * 1024 * 1024 {
            libc::madvise(base, map_len, libc::MADV_HUGEPAGE);
        }
        Ok(MmapGuard {
            ptr: base.add(ptr_offset),
            len,
            map_base: base,
            map_len,
            file_offset: offset,
            fd: file.as_raw_fd(),
        })
    }
}

// Thread-local buffer for reading compressed chunk data via pread.
thread_local! {
    static COMPRESSED_BUF: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Takes the compressed-chunk buffer, growing it if needed. Caller must
/// return it via [`return_compressed_buf`] when done.
#[inline]
pub(super) fn take_compressed_buf(len: usize) -> Vec<u8> {
    COMPRESSED_BUF.with(|cell| {
        if tls_reclaim_stale() {
            *cell.borrow_mut() = Vec::new();
        }
        let mut buf = std::mem::take(&mut *cell.borrow_mut());
        buf.resize(len, 0);
        buf
    })
}

/// Returns the compressed buffer to the thread-local pool. Shrinks oversized
/// buffers to cap retained memory.
#[inline]
pub(super) fn return_compressed_buf(mut buf: Vec<u8>) {
    const MAX_RETAINED_COMPRESSED_BUF: usize = 8 * 1024 * 1024;
    if buf.capacity() > MAX_RETAINED_COMPRESSED_BUF {
        buf = Vec::with_capacity(MAX_RETAINED_COMPRESSED_BUF);
    }
    COMPRESSED_BUF.with(|cell| {
        *cell.borrow_mut() = buf;
    });
}

#[cfg(all(feature = "bench-diagnostics", test))]
pub(crate) fn compressed_buffer_capacity() -> usize {
    COMPRESSED_BUF.with(|cell| cell.borrow().capacity())
}

pub(crate) use super::decompress_oneshot::decompress_chunk_oneshot;

/// Hint the kernel about access to `fd`: SEQUENTIAL enables read-ahead for
/// streaming reads; DONTNEED evicts pages from the page cache.
#[inline]
pub(crate) fn posix_advise(
    fd: std::os::unix::io::RawFd,
    offset: u64,
    len: u64,
    advice: libc::c_int,
) {
    let _ = unsafe { libc::posix_fadvise(fd, offset as libc::off_t, len as libc::off_t, advice) };
}

/// Flushes a range of the output file to disk, then evicts its pages from
/// the page cache. Reduces peak resident memory during assembly of large
/// multi-chunk files. Omits WAIT_AFTER to avoid blocking on writeback;
/// dirty pages remain until the kernel's periodic writeback completes.
#[inline]
pub(crate) fn sync_and_evict_range(fd: std::os::unix::io::RawFd, offset: u64, len: u64) {
    let _ = unsafe {
        libc::sync_file_range(
            fd,
            offset as libc::off64_t,
            len as libc::off64_t,
            libc::SYNC_FILE_RANGE_WAIT_BEFORE | libc::SYNC_FILE_RANGE_WRITE,
        )
    };
    posix_advise(fd, offset, len, libc::POSIX_FADV_DONTNEED);
}

/// Returns true when the chunk hash must be verified against the decompressed
/// bytes. Accepts binary hash (16 bytes = MD5, 8 bytes = xxh64, 0 = none).
pub(crate) fn chunk_hash_required(chunk_decompressed_hash_md5: &[u8]) -> bool {
    !chunk_decompressed_hash_md5.is_empty()
        && chunk_decompressed_hash_md5 != &EMPTY_MD5_BYTES[..]
        && matches!(chunk_decompressed_hash_md5.len(), 16 | 8)
}

/// Copy a chunk from an old file into `out_file` at `new_offset`. Uses the
/// kernel `copy_file_range` syscall when no per-chunk or per-file hashing is
/// required, avoiding any in-user-space copy of the bytes. Otherwise streams
/// the region through a thread-local buffer to compute MD5 while writing.
#[allow(clippy::too_many_arguments)]
pub fn write_chunk_from_mmap(
    old_file: &File,
    old_file_path: &Path,
    out_file: &File,
    new_offset: u64,
    old_offset: u64,
    expected_size: u64,
    mut file_hasher: Option<&mut Md5>,
    chunk_decompressed_hash_md5: &[u8],
    old_file_len: Option<u64>,
    out_file_len: u64,
) -> SophonResult<u64> {
    let needs_chunk_hash = chunk_hash_required(chunk_decompressed_hash_md5);
    if file_hasher.is_none() && !needs_chunk_hash {
        let copied =
            sysio::copy_file_from(old_file, old_offset, out_file, new_offset, expected_size)?;
        if copied != expected_size {
            return Err(SophonError::SizeMismatch {
                item: old_file_path.display().to_string(),
                expected: expected_size,
                actual: copied,
            });
        }
        return Ok(copied);
    }

    let file_len = match old_file_len {
        Some(len) => len,
        None => old_file.metadata().map_err(SophonError::Io)?.len(),
    };
    if old_offset + expected_size > file_len {
        return Err(SophonError::SizeMismatch {
            item: old_file_path.display().to_string(),
            expected: expected_size,
            actual: file_len.saturating_sub(old_offset),
        });
    }

    posix_advise(
        old_file.as_raw_fd(),
        old_offset,
        expected_size,
        libc::POSIX_FADV_SEQUENTIAL,
    );

    let expected_size_u = expected_size as usize;

    let mut chunk_hasher: Option<Md5> = if needs_chunk_hash {
        Some(take_md5()?)
    } else {
        None
    };
    let mut buf = OPT_BUFFER.with(|cell| {
        let mut buf = cell.take();
        buf.resize(ASSEMBLY_BUFFER_SIZE, 0);
        buf
    });

    // mmap requires the output region to be pre-sized; fall back to pwrite otherwise.
    let mmap_result = if out_file_len >= new_offset + expected_size {
        mmap_read_write(out_file, new_offset, expected_size_u)
    } else {
        Err(io::Error::other("output file too small for mmap"))
    };
    let mut bytes_written: usize = 0;
    match mmap_result {
        Ok(mut out_mmap) => {
            // Prefault pages before borrowing the slice.
            unsafe {
                libc::madvise(
                    out_mmap.map_base,
                    out_mmap.map_len,
                    libc::MADV_POPULATE_WRITE,
                );
            }
            let out_slice = out_mmap.as_mut_slice();
            let mut remaining = expected_size_u;
            while remaining > 0 {
                let to_read = remaining.min(buf.len());
                let read_at = old_offset + (expected_size_u - remaining) as u64;
                let n = old_file
                    .read_at(&mut buf[..to_read], read_at)
                    .map_err(SophonError::Io)?;
                if n == 0 {
                    break;
                }
                if let Some(ref mut ch) = chunk_hasher {
                    ch.update(&buf[..n])?;
                }
                if let Some(hasher) = file_hasher.as_deref_mut() {
                    hasher.update(&buf[..n])?;
                }
                out_slice[bytes_written..bytes_written + n].copy_from_slice(&buf[..n]);
                bytes_written += n;
                remaining -= n;
            }
        }
        Err(_) => {
            // Fall back to pwrite if mmap fails.
            let mut remaining = expected_size_u;
            let mut write_offset = new_offset;
            while remaining > 0 {
                let to_read = remaining.min(buf.len());
                let read_at = old_offset + (expected_size_u - remaining) as u64;
                let n = old_file
                    .read_at(&mut buf[..to_read], read_at)
                    .map_err(SophonError::Io)?;
                if n == 0 {
                    break;
                }
                if let Some(ref mut ch) = chunk_hasher {
                    ch.update(&buf[..n])?;
                }
                if let Some(hasher) = file_hasher.as_deref_mut() {
                    hasher.update(&buf[..n])?;
                }
                out_file
                    .write_all_at(&buf[..n], write_offset)
                    .map_err(SophonError::Io)?;
                write_offset += n as u64;
                bytes_written += n;
                remaining -= n;
            }
        }
    }

    posix_advise(
        old_file.as_raw_fd(),
        old_offset,
        expected_size,
        libc::POSIX_FADV_DONTNEED,
    );

    let bytes_written = bytes_written as u64;

    OPT_BUFFER.with(|cell| cell.replace(buf));

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
        if !md5_bytes_eq(&digest, chunk_decompressed_hash_md5) {
            return Err(SophonError::Md5Mismatch {
                item: old_file_path.display().to_string(),
                expected: bytes_to_hex(chunk_decompressed_hash_md5),
                actual: md5_to_hex(&digest),
            });
        }
    }

    Ok(bytes_written)
}

/// Decompress a chunk file and write the bytes to `out_file` at `offset`,
/// computing the chunk and file MD5 values in the same pass.
///
/// Tries a tight `WindowLogMax` first to shrink the DStream's pre-allocated
/// window buffer. If the frame's actual window exceeds the guess (the error
/// surfaces on the first read, before any hashing), falls back to the full
/// capacity cap.
#[allow(clippy::too_many_arguments)]
pub fn decompress_chunk_optimized(
    chunk_path: &Path,
    out_file: &File,
    offset: u64,
    expected_size: u64,
    file_hasher: Option<&mut Md5>,
    chunk_decompressed_hash_md5: &[u8],
    deferred_to_batch_hasher: bool,
    compressed_size: Option<u64>,
) -> SophonResult<u64> {
    let mut file_hasher = file_hasher;

    if expected_size > 0 && expected_size <= ONESHOT_MAX_SIZE as u64 {
        let result = decompress_chunk_oneshot(
            chunk_path,
            out_file,
            offset,
            expected_size,
            &mut file_hasher,
            chunk_decompressed_hash_md5,
            deferred_to_batch_hasher,
            compressed_size,
        );
        match &result {
            Ok(_) => return result,
            // Hash/size mismatches are data-integrity failures; streaming won't fix them.
            Err(SophonError::Md5Mismatch { .. }) => return result,
            Err(SophonError::SizeMismatch { .. }) => return result,
            // Decompression/IO errors may recover via streaming with a different window.
            Err(_) => {}
        }
    }

    let dynamic_log = window_log_for_size(expected_size);
    let result = decompress_chunk_with_window(
        chunk_path,
        out_file,
        offset,
        expected_size,
        &mut file_hasher,
        chunk_decompressed_hash_md5,
        deferred_to_batch_hasher,
        dynamic_log,
    );
    if is_window_too_small(&result) {
        return decompress_chunk_with_window(
            chunk_path,
            out_file,
            offset,
            expected_size,
            &mut file_hasher,
            chunk_decompressed_hash_md5,
            deferred_to_batch_hasher,
            MAX_WINDOW_LOG,
        );
    }
    result
}

pub(crate) fn is_window_too_small(r: &SophonResult<u64>) -> bool {
    match r {
        Err(SophonError::Io(e)) => {
            let msg = e.to_string();
            msg.contains("out of bound") || msg.contains("too much memory for decoding")
        }
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
fn decompress_chunk_with_window(
    chunk_path: &Path,
    out_file: &File,
    offset: u64,
    expected_size: u64,
    file_hasher: &mut Option<&mut Md5>,
    chunk_decompressed_hash_md5: &[u8],
    deferred_to_batch_hasher: bool,
    window_log: u32,
) -> SophonResult<u64> {
    let f = File::open(chunk_path)?;
    let compressed_fd = f.as_raw_fd();
    posix_advise(compressed_fd, 0, 0, libc::POSIX_FADV_SEQUENTIAL);

    let expected_usize = expected_size as usize;

    let (bytes_written, mut chunk_hasher) = {
        let mut ctx = take_dctx();
        ctx.reset(zstd::zstd_safe::ResetDirective::SessionAndParameters)
            .map_err(|_| SophonError::Io(std::io::Error::other("zstd DCtx reset")))?;
        ctx.set_parameter(zstd::zstd_safe::DParameter::WindowLogMax(window_log))
            .map_err(|_| SophonError::Io(std::io::Error::other("zstd DCtx WindowLogMax")))?;
        if chunk_hash_required(chunk_decompressed_hash_md5)
            || file_hasher.is_some()
            || deferred_to_batch_hasher
        {
            let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(true));
        }

        let (bytes, chunk_hasher) = {
            let buf_reader = BufReader::with_capacity(FILE_WRITE_BUFFER_SIZE, f);
            let mut decoder = zstd::Decoder::with_context(buf_reader, &mut ctx);

            let need_chunk_hash = chunk_hash_required(chunk_decompressed_hash_md5);
            let mut chunk_hasher = if need_chunk_hash {
                Some(take_md5()?)
            } else {
                None
            };

            // Use mmap for output to eliminate pwrite syscalls — the
            // decoder writes directly to the mapped output file region.
            let mut out_mmap = mmap_read_write(out_file, offset, expected_usize)?;
            let output = out_mmap.as_mut_slice();

            let mut bytes: usize = 0;
            match file_hasher.as_deref_mut() {
                Some(hasher) => loop {
                    let n = decoder.read(&mut output[bytes..])?;
                    if n == 0 {
                        break;
                    }
                    if let Some(ref mut ch) = chunk_hasher {
                        ch.update(&output[bytes..bytes + n])?;
                    }
                    hasher.update(&output[bytes..bytes + n])?;
                    bytes += n;
                },
                None => loop {
                    let n = decoder.read(&mut output[bytes..])?;
                    if n == 0 {
                        break;
                    }
                    if let Some(ref mut ch) = chunk_hasher {
                        ch.update(&output[bytes..bytes + n])?;
                    }
                    bytes += n;
                },
            }

            (bytes as u64, chunk_hasher)
        }; // decoder dropped, ctx borrow released

        return_dctx(ctx);
        (bytes, chunk_hasher)
    };

    posix_advise(compressed_fd, 0, 0, libc::POSIX_FADV_DONTNEED);

    if bytes_written != expected_size {
        return Err(SophonError::SizeMismatch {
            item: chunk_path.display().to_string(),
            expected: expected_size,
            actual: bytes_written,
        });
    }

    if let Some(ref mut ch) = chunk_hasher {
        let digest = ch.finish()?;
        return_md5(chunk_hasher.take().unwrap());
        if !md5_bytes_eq(&digest, chunk_decompressed_hash_md5) {
            return Err(SophonError::Md5Mismatch {
                item: chunk_path.display().to_string(),
                expected: bytes_to_hex(chunk_decompressed_hash_md5),
                actual: md5_to_hex(&digest),
            });
        }
    }

    Ok(bytes_written)
}

/// Production oneshot decompress path for benchmarks: pread compressed chunk,
/// mmap output at offset, one-shot decode, flush and evict.
#[cfg(feature = "benchmark")]
#[doc(hidden)]
pub mod bench_internals {
    pub use super::decompress_chunk_optimized;

    /// Per-phase timing breakdown for one chunk's decompress+verify cycle.
    /// All fields are nanoseconds spent in that phase. Returned by
    /// [`decompress_chunk_optimized_timed`].
    #[cfg(feature = "bench-diagnostics")]
    #[derive(Clone, Copy, Debug, Default)]
    pub struct ChunkPhaseTimings {
        /// File open + metadata + pread of compressed data.
        pub read_ns: u64,
        /// mmap of the output region.
        pub mmap_ns: u64,
        /// zstd DCtx take + reset + decompress.
        pub decompress_ns: u64,
        /// Chunk MD5 hash (when not deferred).
        pub hash_ns: u64,
        /// mmap flush + evict + munmap + compressed fadvise.
        pub evict_ns: u64,
        /// Total wall time of the call.
        pub total_ns: u64,
    }

    /// Timed variant of [`decompress_chunk_optimized`] that reports per-phase
    /// nanoseconds. Only the oneshot path is instrumented; the streaming
    /// fallback returns zero timings (it is rarely hit in benchmarks).
    #[cfg(feature = "bench-diagnostics")]
    pub fn decompress_chunk_optimized_timed(
        chunk_path: &std::path::Path,
        out_file: &std::fs::File,
        offset: u64,
        expected_size: u64,
        chunk_decompressed_hash_md5: &[u8],
        compressed_size: Option<u64>,
    ) -> super::SophonResult<(u64, ChunkPhaseTimings)> {
        use std::time::Instant;

        use std::os::unix::fs::FileExt;
        use std::os::unix::io::AsRawFd;

        let wall = Instant::now();
        let mut timings = ChunkPhaseTimings::default();

        if expected_size == 0 || expected_size > super::ONESHOT_MAX_SIZE as u64 {
            let r = super::decompress_chunk_optimized(
                chunk_path, out_file, offset, expected_size, None,
                chunk_decompressed_hash_md5, false, compressed_size,
            );
            timings.total_ns = wall.elapsed().as_nanos() as u64;
            return r.map(|b| (b, timings));
        }

        let expected_usize = expected_size as usize;

        let t = Instant::now();
        let f = std::fs::File::open(chunk_path)?;
        let cs = match compressed_size {
            Some(s) => s as usize,
            None => f.metadata()?.len() as usize,
        };
        if cs == 0 {
            return Err(super::SophonError::Io(std::io::Error::other("empty chunk")));
        }
        let mut compressed_buf = super::take_compressed_buf(cs);
        let n = f.read_at(&mut compressed_buf[..cs], 0)?;
        if n != cs {
            super::return_compressed_buf(compressed_buf);
            return Err(super::SophonError::Io(std::io::Error::other("short read")));
        }
        let compressed = &compressed_buf[..cs];
        timings.read_ns = t.elapsed().as_nanos() as u64;

        let t = Instant::now();
        let mut out_mmap = super::mmap_read_write(out_file, offset, expected_usize)?;
        if out_mmap.map_len >= 2 * 1024 * 1024 {
            unsafe {
                let ret = libc::madvise(
                    out_mmap.map_base, out_mmap.map_len, libc::MADV_POPULATE_WRITE,
                );
                if ret != 0 {
                    let e = std::io::Error::last_os_error();
                    if e.raw_os_error() != Some(libc::EINVAL) {
                        super::return_compressed_buf(compressed_buf);
                        return Err(super::SophonError::Io(e));
                    }
                }
            }
        }
        let output = out_mmap.as_mut_slice();
        timings.mmap_ns = t.elapsed().as_nanos() as u64;

        let t = Instant::now();
        let result = (|| {
            let mut ctx = super::take_dctx();
            ctx.reset(zstd::zstd_safe::ResetDirective::SessionAndParameters)
                .map_err(|_| super::SophonError::Io(std::io::Error::other("zstd DCtx reset")))?;
            if super::chunk_hash_required(chunk_decompressed_hash_md5) {
                let _ = ctx.set_parameter(zstd::zstd_safe::DParameter::ForceIgnoreChecksum(true));
            }
            let written = ctx
                .decompress(output, compressed)
                .map_err(|e| super::SophonError::Io(std::io::Error::other(e.to_string())))?;
            super::return_dctx(ctx);
            if written != expected_usize {
                return Err(super::SophonError::SizeMismatch {
                    item: chunk_path.display().to_string(),
                    expected: expected_size,
                    actual: written as u64,
                });
            }
            Ok(written)
        })();
        timings.decompress_ns = t.elapsed().as_nanos() as u64;

        let t = Instant::now();
        if let Ok(written) = &result
            && super::chunk_hash_required(chunk_decompressed_hash_md5)
        {
            let mut chunk_hasher = super::take_md5()?;
            chunk_hasher.update(&output[..*written])?;
            let digest = chunk_hasher.finish()?;
            super::return_md5(chunk_hasher);
            if !super::md5_bytes_eq(&digest, chunk_decompressed_hash_md5) {
                super::return_compressed_buf(compressed_buf);
                super::posix_advise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
                drop(f);
                out_mmap.flush_and_evict();
                return Err(super::SophonError::Md5Mismatch {
                    item: chunk_path.display().to_string(),
                    expected: super::bytes_to_hex(chunk_decompressed_hash_md5),
                    actual: super::md5_to_hex(&digest),
                });
            }
        }
        timings.hash_ns = t.elapsed().as_nanos() as u64;

        let t = Instant::now();
        super::return_compressed_buf(compressed_buf);
        super::posix_advise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        drop(f);
        out_mmap.flush_and_evict();
        drop(out_mmap);
        timings.evict_ns = t.elapsed().as_nanos() as u64;

        timings.total_ns = wall.elapsed().as_nanos() as u64;
        result.map(|w| (w as u64, timings))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_only_mmap_maps_file_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        std::fs::write(&path, b"mapped source").unwrap();
        let file = File::open(&path).unwrap();

        let mapping = unsafe { mmap_read_only_unchecked(&file, 6) }.unwrap();
        assert_eq!(mapping.as_slice(), b"mapped");
    }

    fn write_at_new(path: &Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .unwrap()
    }

    #[test]
    fn write_chunk_from_mmap_basic() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        std::fs::write(&src, b"hello world").unwrap();

        let dst_path = dir.path().join("out.bin");
        let out = write_at_new(&dst_path);
        let src_file = File::open(&src).unwrap();
        let bytes =
            write_chunk_from_mmap(&src_file, &src, &out, 0, 0, 11, None, b"", None, 11).unwrap();
        drop(out);
        assert_eq!(bytes, 11);
        assert_eq!(std::fs::read(&dst_path).unwrap(), b"hello world");
    }

    #[test]
    fn write_chunk_from_mmap_reads_multiple_buffers() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("large.bin");
        let data = vec![0xABu8; ASSEMBLY_BUFFER_SIZE * 3];
        std::fs::write(&src, &data).unwrap();

        let dst_path = dir.path().join("out.bin");
        let out = write_at_new(&dst_path);
        let src_file = File::open(&src).unwrap();
        let bytes = write_chunk_from_mmap(
            &src_file,
            &src,
            &out,
            0,
            0,
            data.len() as u64,
            None,
            b"",
            None,
            0,
        )
        .unwrap();
        drop(out);
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(std::fs::read(&dst_path).unwrap().len(), data.len());
    }

    #[test]
    fn write_chunk_from_mmap_with_chunk_hash() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"verify my md5 please";
        let src = dir.path().join("src.bin");
        std::fs::write(&src, data).unwrap();
        let chunk_md5 = {
            let mut h = Md5::new().unwrap();
            h.update(data).unwrap();
            hex::encode(h.finish().unwrap())
        };

        let dst_path = dir.path().join("out.bin");
        let out = write_at_new(&dst_path);
        let src_file = File::open(&src).unwrap();
        let bytes = write_chunk_from_mmap(
            &src_file,
            &src,
            &out,
            0,
            0,
            data.len() as u64,
            None,
            chunk_md5.as_bytes(),
            None,
            0,
        )
        .unwrap();
        drop(out);
        assert_eq!(bytes, data.len() as u64);
        assert_eq!(std::fs::read(&dst_path).unwrap(), data);
    }

    #[test]
    fn write_chunk_from_mmap_hash_mismatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let data = b"real data";
        let src = dir.path().join("src.bin");
        std::fs::write(&src, data).unwrap();
        let wrong = "ffffffffffffffffffffffffffffffff";

        let dst_path = dir.path().join("out.bin");
        let out = write_at_new(&dst_path);
        let src_file = File::open(&src).unwrap();
        let res = write_chunk_from_mmap(
            &src_file,
            &src,
            &out,
            0,
            0,
            data.len() as u64,
            None,
            &super::super::assembly_opt::hex_to_bytes(wrong).unwrap()[..],
            None,
            0,
        );
        assert!(res.is_err());
    }

    #[test]
    fn decompress_chunk_optimized_basic() {
        let dir = tempfile::tempdir().unwrap();
        let raw = b"decompress this payload";
        let chunk_path = dir.path().join("chunk.zstd");
        let f = File::create(&chunk_path).unwrap();
        let mut enc = zstd::Encoder::new(f, 3).unwrap();
        std::io::Write::write_all(&mut enc, raw).unwrap();
        enc.finish().unwrap();

        let compressed_size = std::fs::metadata(&chunk_path).unwrap().len();

        let dst_path = dir.path().join("out.bin");
        let out = write_at_new(&dst_path);
        out.set_len(raw.len() as u64).unwrap();
        let bytes = decompress_chunk_optimized(
            &chunk_path,
            &out,
            0,
            raw.len() as u64,
            None,
            b"",
            false,
            None,
        )
        .unwrap();
        drop(out);
        assert_eq!(bytes, raw.len() as u64);
        assert_eq!(std::fs::read(&dst_path).unwrap(), raw);

        // The Some(compressed_size) path skips the fstat and produces identical output.
        let dst2 = dir.path().join("out2.bin");
        let out2 = write_at_new(&dst2);
        out2.set_len(raw.len() as u64).unwrap();
        let bytes2 = decompress_chunk_optimized(
            &chunk_path,
            &out2,
            0,
            raw.len() as u64,
            None,
            b"",
            false,
            Some(compressed_size),
        )
        .unwrap();
        drop(out2);
        assert_eq!(bytes2, raw.len() as u64);
        assert_eq!(std::fs::read(&dst2).unwrap(), raw);
    }

    #[test]
    fn window_log_for_size_returns_minimal_fit() {
        assert_eq!(window_log_for_size(1), 10);
        assert_eq!(window_log_for_size(2), 10);
        assert_eq!(window_log_for_size(4), 10);
        assert_eq!(window_log_for_size(1024), 10);
        assert_eq!(window_log_for_size(1 << 20), 20);
        assert_eq!(window_log_for_size((1 << 20) + 1), 21);
        assert_eq!(window_log_for_size(1 << 23), 23);
        assert_eq!(window_log_for_size(1u64 << 32), MAX_WINDOW_LOG);
        assert_eq!(window_log_for_size(0), 10);
    }

    #[test]
    fn md5_hex_eq_matches_string_comparison() {
        let data = b"the quick brown fox";
        let mut h = Md5::new().unwrap();
        h.update(data).unwrap();
        let digest = h.finish().unwrap();
        let hex = md5_to_hex(&digest);
        assert!(md5_hex_eq(&digest, &hex));
        assert!(!md5_hex_eq(&digest, "00000000000000000000000000000000"));
        assert!(!md5_hex_eq(&digest, "deadbeef"));
        assert!(!md5_hex_eq(&digest, &hex[..31]));
    }

    #[test]
    fn md5_to_hex_roundtrips_against_hex_encode() {
        let data = b"-stack md5 test payload-";
        let mut h = Md5::new().unwrap();
        h.update(data).unwrap();
        let digest = h.finish().unwrap();
        let stack = md5_to_hex(&digest);
        let heap = hex::encode(digest);
        assert_eq!(stack, heap);
    }

    #[test]
    fn xxh64_hex_eq_matches_known_value() {
        let value: u64 = 0x0123456789abcdef;
        let hex = format!("{:016x}", value);
        assert!(xxh64_hex_eq(value, &hex));
        assert!(xxh64_hex_eq(value, &hex.to_uppercase()));
    }

    #[test]
    fn xxh64_hex_eq_rejects_mismatch() {
        let value: u64 = 0x0123456789abcdef;
        assert!(!xxh64_hex_eq(value, "fedcba9876543210"));
        assert!(!xxh64_hex_eq(value, "0123456789abcde"));
        assert!(!xxh64_hex_eq(value, "0123456789abcdeeff"));
    }

    #[test]
    fn xxh64_hex_eq_zero() {
        assert!(xxh64_hex_eq(0, "0000000000000000"));
        assert!(!xxh64_hex_eq(1, "0000000000000000"));
    }

    #[test]
    fn chunk_hash_required_detects_empty_and_placeholder() {
        assert!(!chunk_hash_required(b""));
        assert!(!chunk_hash_required(&[0; 16]));
        assert!(chunk_hash_required(&[
            0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
            0x42, 0x7e
        ]));
        assert!(!chunk_hash_required(b"short"));
    }

    #[test]
    fn is_window_too_small_detects_window_errors() {
        assert!(is_window_too_small(&Err(SophonError::Io(
            std::io::Error::other("window size too small: out of bound")
        ))));
        assert!(is_window_too_small(&Err(SophonError::Io(
            std::io::Error::other("too much memory for decoding")
        ))));
        assert!(!is_window_too_small(&Err(SophonError::Io(
            std::io::Error::other("some other error")
        ))));
        assert!(!is_window_too_small(&Ok(42u64)));
    }

    #[test]
    fn md5_pool_reset_after_finish() {
        let mut h1 = take_md5().unwrap();
        h1.update(b"hello").unwrap();
        let d1 = h1.finish().unwrap();
        return_md5(h1);

        let mut h2 = take_md5().unwrap();
        h2.update(b"hello").unwrap();
        let d2 = h2.finish().unwrap();
        return_md5(h2);

        assert_eq!(d1, d2, "pooled MD5 must reset after finish");
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "diagnostic assembly phase breakdown; run with --ignored"]
    fn diagnose_assembly_phase_breakdown() {
        use std::os::unix::fs::FileExt;

        let chunk_mib = 1;
        let num_chunks = 100;
        let chunk_size = chunk_mib * 1024 * 1024;

        let dir = tempfile::tempdir().unwrap();
        let chunks_dir = dir.path().join("chunks");
        std::fs::create_dir_all(&chunks_dir).unwrap();
        let out_path = dir.path().join("out.bin");

        // Create compressed chunk fixtures.
        let mut chunk_paths = Vec::with_capacity(num_chunks);
        let mut hashes = Vec::with_capacity(num_chunks);
        let mut total_decompressed = 0u64;
        for i in 0..num_chunks {
            let raw: Vec<u8> = (0..chunk_size)
                .map(|j| ((j as u8).wrapping_mul(31)).wrapping_add(i as u8))
                .collect();
            let comp = zstd::encode_all(&raw[..], 3).unwrap();
            let p = chunks_dir.join(format!("c{i}.zstd"));
            let comp_size = comp.len() as u64;
            std::fs::write(&p, &comp).unwrap();
            let digest = fast_md5::digest(&raw);
            chunk_paths.push((p, comp_size));
            hashes.push(digest);
            total_decompressed += chunk_size as u64;
        }

        // Create the output file preallocated to full size.
        let out_file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&out_path)
            .unwrap();
        out_file.set_len(total_decompressed).unwrap();

        // Warmup: run once to fill TLS pools and warm caches.
        for (i, (p, cs)) in chunk_paths.iter().enumerate() {
            let offset = (i * chunk_size) as u64;
            decompress_chunk_optimized(
                p, &out_file, offset, chunk_size as u64, None,
                &hashes[i], false, Some(*cs),
            )
            .unwrap();
        }

        // Measure: run all chunks and accumulate phase timings.
        use bench_internals::decompress_chunk_optimized_timed;
        let mut total_read = 0u64;
        let mut total_mmap = 0u64;
        let mut total_decompress = 0u64;
        let mut total_hash = 0u64;
        let mut total_evict = 0u64;
        let mut total_wall = 0u64;

        for (i, (p, cs)) in chunk_paths.iter().enumerate() {
            let offset = (i * chunk_size) as u64;
            let (_, t) = decompress_chunk_optimized_timed(
                p, &out_file, offset, chunk_size as u64,
                &hashes[i], Some(*cs),
            )
            .unwrap();
            total_read += t.read_ns;
            total_mmap += t.mmap_ns;
            total_decompress += t.decompress_ns;
            total_hash += t.hash_ns;
            total_evict += t.evict_ns;
            total_wall += t.total_ns;
        }

        let total_ms = total_wall as f64 / 1_000_000.0;
        let pct = |ns: u64| ns as f64 / total_wall as f64 * 100.0;
        let ms = |ns: u64| ns as f64 / 1_000_000.0;
        println!("=== ASSEMBLY PHASE BREAKDOWN ({num_chunks}×{chunk_mib}MiB) ===");
        println!("total:        {total_ms:.2} ms");
        println!("read:         {:.2} ms ({:.0}%)", ms(total_read), pct(total_read));
        println!("mmap:         {:.2} ms ({:.0}%)", ms(total_mmap), pct(total_mmap));
        println!("decompress:   {:.2} ms ({:.0}%)", ms(total_decompress), pct(total_decompress));
        println!("hash:         {:.2} ms ({:.0}%)", ms(total_hash), pct(total_hash));
        println!("evict:        {:.2} ms ({:.0}%)", ms(total_evict), pct(total_evict));
    }
}
