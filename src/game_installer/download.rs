use std::io::Read;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use futures_util::StreamExt;
use libc;
use reqwest::Client;
use tokio::io::{AsyncWriteExt, BufWriter};

use super::CHUNK_WRITE_BUFFER_SIZE;
use super::assembly_opt::{
    Md5, md5_hex_eq, md5_to_hex, posix_advise, return_md5, take_md5, xxh64_hex_eq,
};
use super::compact_manifest::ChunkRef;
use super::error::{SophonError, SophonResult};
use super::handle::DownloadHandle;
use crate::api_scrape::DownloadInfo;

struct PooledMd5 {
    hasher: Option<Md5>,
}

pub(crate) trait ChunkHasher {
    fn update_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()>;
}

#[cfg(feature = "bench-diagnostics")]
#[derive(Clone, Copy, Debug, Default)]
pub struct DownloadPhaseTimings {
    /// Time spent inside the streaming loop, including waiting for response data.
    pub total: std::time::Duration,
    /// Time awaiting each next response body chunk.
    pub stream_wait: std::time::Duration,
    /// Time writing response chunks through the buffered file writer.
    pub write: std::time::Duration,
    /// Time updating the selected chunk hash.
    pub hash: std::time::Duration,
    /// Time spent flushing and advising the kernel after integrity checks.
    pub final_flush_and_evict: std::time::Duration,
    /// Sum of per-chunk time measurements, separate from accumulated phase totals.
    pub per_chunk: DownloadPhaseSamples,
    /// Fixed buckets for response body chunk lengths: <=4, <=16, <=64, <=256 KiB, >256 KiB.
    pub chunk_size_buckets: [u64; 5],
    /// Hash algorithm selected for this stream.
    pub hash_algorithm: DownloadHashAlgorithm,
    /// Total response body bytes consumed by the stream.
    pub bytes: u64,
    /// Number of response body chunks consumed by the stream.
    pub stream_chunks: u64,
}

#[cfg(feature = "bench-diagnostics")]
#[derive(Clone, Copy, Debug, Default)]
pub struct DownloadPhaseSamples {
    /// Count of stream polls, including the final poll that observes EOF.
    pub stream_wait_count: u64,
    pub write_count: u64,
    pub hash_count: u64,
    pub stream_wait_min: std::time::Duration,
    pub stream_wait_max: std::time::Duration,
    pub write_min: std::time::Duration,
    pub write_max: std::time::Duration,
    pub hash_min: std::time::Duration,
    pub hash_max: std::time::Duration,
    pub stream_wait_sum: std::time::Duration,
    pub write_sum: std::time::Duration,
    pub hash_sum: std::time::Duration,
}

#[cfg(feature = "bench-diagnostics")]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DownloadHashAlgorithm {
    #[default]
    None,
    Md5,
    Xxh64,
}

#[cfg(feature = "bench-diagnostics")]
fn download_chunk_size_bucket(len: usize) -> usize {
    if len <= 4 * 1024 {
        0
    } else if len <= 16 * 1024 {
        1
    } else if len <= 64 * 1024 {
        2
    } else if len <= 256 * 1024 {
        3
    } else {
        4
    }
}

impl ChunkHasher for PooledMd5 {
    fn update_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.get_mut().update(bytes)
    }
}

impl ChunkHasher for fast_md5::Md5 {
    fn update_chunk(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.update(bytes);
        Ok(())
    }
}

fn update_download_hashes<H: ChunkHasher>(
    bytes: &[u8],
    hasher: Option<&mut H>,
    xxh64_hasher: Option<&mut xxhash_rust::xxh64::Xxh64>,
) -> std::io::Result<()> {
    if let Some(hasher) = hasher {
        hasher.update_chunk(bytes)?;
    }
    if let Some(hasher) = xxh64_hasher {
        hasher.update(bytes);
    }
    Ok(())
}

fn check_download_chunk_size(
    bytes: &[u8],
    expected_total: u64,
    total_len: u64,
) -> Result<u64, StreamConsumeError> {
    if bytes.is_empty() && total_len < expected_total {
        return Err(StreamConsumeError::EmptyChunk);
    }
    let total_len = total_len
        .checked_add(bytes.len() as u64)
        .ok_or(StreamConsumeError::TooLarge(u64::MAX))?;
    if total_len > expected_total {
        return Err(StreamConsumeError::TooLarge(total_len));
    }
    Ok(total_len)
}

#[derive(Debug)]
enum StreamConsumeError {
    EmptyChunk,
    TooLarge(u64),
}

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod benchmark {
    #[allow(dead_code)]
    pub fn check_chunk_size(bytes: &[u8], expected: u64, total: u64) -> Result<u64, bool> {
        super::check_download_chunk_size(bytes, expected, total).map_err(|error| match error {
            super::StreamConsumeError::EmptyChunk => false,
            super::StreamConsumeError::TooLarge(_) => true,
        })
    }

    #[allow(dead_code)]
    pub fn update_md5(hasher: &mut fast_md5::Md5, bytes: &[u8]) -> std::io::Result<()> {
        super::update_download_hashes::<fast_md5::Md5>(bytes, Some(hasher), None)
    }

    #[allow(dead_code)]
    pub fn update_xxh64(
        hasher: &mut xxhash_rust::xxh64::Xxh64,
        bytes: &[u8],
    ) -> std::io::Result<()> {
        let mut md5 = None::<fast_md5::Md5>;
        super::update_download_hashes::<fast_md5::Md5>(bytes, md5.as_mut(), Some(hasher))
    }

    use std::path::Path;

    #[allow(clippy::too_many_arguments)]
    pub async fn download_and_verify_md5(
        client: &reqwest::Client,
        url: &str,
        dest: &Path,
        expected_size: u64,
        expected_md5: &str,
    ) -> Result<(), super::SophonError> {
        use std::path::Path;
        let resp = client.get(url).send().await?.error_for_status()?;
        let file = super::create_preallocated_file(dest, expected_size).await?;
        let mut writer = super::EvictingWriter::new(file);
        let mut hasher = Some(super::PooledMd5::new(
            super::super::assembly_opt::take_md5()?
        ));
        let mut xxh = None;
        super::stream_and_verify(
            &mut writer,
            resp,
            &mut hasher,
            &mut xxh,
            expected_size,
            0,
            "bench",
            expected_md5,
            dest,
            None,
        )
        .await?;
        writer.flush_and_evict_all().await.ok();
        Ok(())
    }
}

impl PooledMd5 {
    fn new(hasher: Md5) -> Self {
        Self {
            hasher: Some(hasher),
        }
    }

    fn get_mut(&mut self) -> &mut Md5 {
        self.hasher
            .as_mut()
            .expect("pooled MD5 hasher is available")
    }

    fn finish(&mut self) -> std::io::Result<[u8; 16]> {
        self.get_mut().finish()
    }
}

impl Drop for PooledMd5 {
    fn drop(&mut self) {
        if let Some(hasher) = self.hasher.take() {
            return_md5(hasher);
        }
    }
}

pub(crate) struct EvictingWriter {
    inner: BufWriter<tokio::fs::File>,
    written: u64,
    last_evicted: u64,
}

const EVICT_INTERVAL: u64 = 4 * 1024 * 1024;

impl EvictingWriter {
    pub(crate) fn new(file: tokio::fs::File) -> Self {
        Self {
            inner: BufWriter::with_capacity(CHUNK_WRITE_BUFFER_SIZE, file),
            written: 0,
            last_evicted: 0,
        }
    }

    pub(crate) fn with_offset(file: tokio::fs::File, offset: u64) -> Self {
        Self {
            inner: BufWriter::with_capacity(CHUNK_WRITE_BUFFER_SIZE, file),
            written: offset,
            last_evicted: offset,
        }
    }

    pub(crate) async fn write_all(&mut self, buf: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(buf).await?;
        self.written += buf.len() as u64;
        // Periodically evict written pages to reduce page cache pressure.
        if self.written - self.last_evicted >= EVICT_INTERVAL {
            self.inner.flush().await?;
            let fd = self.inner.get_ref().as_raw_fd();
            // Sync the written range to disk so pages become clean and
            // can be immediately reclaimed by posix_fadvise(DONTNEED).
            // Use WAIT_BEFORE|WRITE without WAIT_AFTER to avoid blocking
            // the async runtime thread — the kernel will write pages
            // asynchronously and DONTNEED will evict clean ones immediately.
            // Dirty pages remain until writeback completes, then get evicted.
            let evict_start = self.last_evicted;
            let evict_len = self.written - self.last_evicted;
            let _ = unsafe {
                libc::sync_file_range(
                    fd,
                    evict_start as libc::off64_t,
                    evict_len as libc::off64_t,
                    libc::SYNC_FILE_RANGE_WAIT_BEFORE | libc::SYNC_FILE_RANGE_WRITE,
                )
            };
            posix_advise(fd, evict_start, evict_len, libc::POSIX_FADV_DONTNEED);
            self.last_evicted = self.written;
        }
        Ok(())
    }

    pub(crate) async fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush().await
    }

    pub(crate) async fn flush_and_evict_all(&mut self) -> std::io::Result<()> {
        self.flush().await?;
        let fd = self.inner.get_ref().as_raw_fd();
        // Sync all written data to disk, then evict from page cache.
        let _ = unsafe { libc::fdatasync(fd) };
        posix_advise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn written(&self) -> u64 {
        self.written
    }
}

fn get_available_space(path: &Path) -> Option<u64> {
    use std::cell::RefCell;
    use std::os::unix::ffi::OsStrExt;
    use std::time::Instant;

    thread_local! {
        static SPACE_CACHE: RefCell<Option<(Instant, u64, std::ffi::CString)>> = const { RefCell::new(None) };
    }

    const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(1);

    let path_bytes = path.as_os_str().as_bytes();
    let hit = SPACE_CACHE.with(|cell| {
        let b = cell.borrow();
        match b.as_ref() {
            Some((ts, space, cs)) if ts.elapsed() < CACHE_TTL && cs.as_bytes() == path_bytes => {
                Some(*space)
            }
            _ => None,
        }
    });
    if let Some(space) = hit {
        return Some(space);
    }

    let cpath = std::ffi::CString::new(path_bytes).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let ret = unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) };
    if ret != 0 {
        return None;
    }
    // statvfs field widths differ across Unix targets.
    #[allow(clippy::unnecessary_cast)]
    let space = (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64);
    SPACE_CACHE.with(|cell| *cell.borrow_mut() = Some((Instant::now(), space, cpath)));
    Some(space)
}

/// Ensure the filesystem containing `dest` has at least `needed` available
/// bytes. Returns `NoSpaceAvailable` with the measured free space when short.
pub fn check_available_space(dest: &Path, needed: u64) -> Result<(), SophonError> {
    let Some(parent) = dest.parent() else {
        return Ok(());
    };
    if let Some(available) = get_available_space(parent)
        && available < needed
    {
        return Err(SophonError::NoSpaceAvailable {
            path: parent.display().to_string(),
            needed,
            available,
        });
    }
    Ok(())
}

/// Format a Range header value into a stack buffer to avoid heap allocation.
#[inline]
fn format_range_header(existing_size: u64, buf: &mut [u8; 32]) -> &str {
    let mut offset = 0;
    buf[offset..offset + 6].copy_from_slice(b"bytes=");
    offset += 6;

    let mut tmp = [0u8; 20];
    let mut n = existing_size;
    let mut i = 0;
    while n > 0 {
        tmp[i] = b'0' + (n % 10) as u8;
        n /= 10;
        i += 1;
    }
    if i == 0 {
        tmp[0] = b'0';
        i = 1;
    }
    for j in 0..i {
        buf[offset + j] = tmp[i - 1 - j];
    }
    offset += i;
    buf[offset] = b'-';
    offset += 1;

    std::str::from_utf8(&buf[..offset]).unwrap()
}

/// Parse Content-Range header to extract start position.
fn parse_content_range_start(range_str: &str) -> Option<u64> {
    let prefix = "bytes ";
    if !range_str.starts_with(prefix) {
        return None;
    }
    let after_prefix = &range_str[prefix.len()..];
    let dash_pos = after_prefix.find('-')?;
    let start_str = &after_prefix[..dash_pos];
    start_str.parse().ok()
}

fn pread_hash_slice(
    path: &Path,
    max_bytes: u64,
    f: &mut dyn FnMut(&[u8]) -> std::io::Result<()>,
) -> SophonResult<()> {
    if max_bytes == 0 {
        return Ok(());
    }
    let mut file = std::fs::File::open(path)?;
    let fd = file.as_raw_fd();
    posix_advise(fd, 0, max_bytes, libc::POSIX_FADV_SEQUENTIAL);
    let cap = (max_bytes as usize).min(super::assembly_opt::HASH_READ_BUFFER_SIZE);
    let mut buffer = super::assembly_opt::HashReadBuffer::take(cap);
    let mut remaining = max_bytes;
    while remaining > 0 {
        let n = file.read(buffer.as_mut_slice())?;
        if n == 0 {
            break;
        }
        let take = n.min(remaining as usize);
        f(buffer.filled(take))?;
        remaining -= take as u64;
    }
    posix_advise(fd, 0, max_bytes, libc::POSIX_FADV_DONTNEED);
    Ok(())
}

fn pread_hash_md5_digest(path: &Path, known_size: Option<u64>) -> SophonResult<[u8; 16]> {
    let mut file = std::fs::File::open(path)?;
    let len = match known_size {
        Some(s) => s,
        None => file.metadata()?.len(),
    };
    let fd = file.as_raw_fd();
    let mut hasher = take_md5()?;
    let result = if len == 0 {
        hasher.update(b"")?;
        hasher.finish().map_err(SophonError::Io)
    } else {
        posix_advise(fd, 0, len, libc::POSIX_FADV_SEQUENTIAL);
        let cap = (len as usize).min(super::assembly_opt::HASH_READ_BUFFER_SIZE);
        let mut buffer = super::assembly_opt::HashReadBuffer::take(cap);
        loop {
            let n = file.read(buffer.as_mut_slice())?;
            if n == 0 {
                break;
            }
            hasher.update(buffer.filled(n))?;
        }
        hasher.finish().map_err(SophonError::Io)
    };
    posix_advise(fd, 0, len, libc::POSIX_FADV_DONTNEED);
    return_md5(hasher);
    result
}

fn pread_hash_xxh64_digest(path: &Path, known_size: Option<u64>) -> SophonResult<u64> {
    let mut file = std::fs::File::open(path)?;
    let len = match known_size {
        Some(s) => s,
        None => file.metadata()?.len(),
    };
    let fd = file.as_raw_fd();
    let result = if len == 0 {
        let mut hasher = super::assembly_opt::take_xxh64();
        hasher.update(b"");
        let digest = hasher.digest();
        super::assembly_opt::return_xxh64(hasher);
        Ok(digest)
    } else {
        posix_advise(fd, 0, len, libc::POSIX_FADV_SEQUENTIAL);
        let cap = (len as usize).min(super::assembly_opt::HASH_READ_BUFFER_SIZE);
        let mut buffer = super::assembly_opt::HashReadBuffer::take(cap);
        let mut hasher = super::assembly_opt::take_xxh64();
        loop {
            let n = file.read(buffer.as_mut_slice())?;
            if n == 0 {
                break;
            }
            hasher.update(buffer.filled(n));
        }
        let digest = hasher.digest();
        super::assembly_opt::return_xxh64(hasher);
        Ok(digest)
    };
    posix_advise(fd, 0, len, libc::POSIX_FADV_DONTNEED);
    result
}

async fn create_preallocated_file(path: &Path, len: u64) -> std::io::Result<tokio::fs::File> {
    let path = path.to_owned();
    let file = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::create(path)?;
        let _ = super::sysio::preallocate(&file, len);
        Ok::<_, std::io::Error>(file)
    })
    .await
    .map_err(|err| std::io::Error::other(err.to_string()))??;
    Ok(tokio::fs::File::from_std(file))
}

async fn verify_existing_file_hash(
    path: &Path,
    expected_hash: &str,
    known_size: Option<u64>,
) -> SophonResult<bool> {
    if expected_hash.is_empty() {
        return Ok(true);
    }
    let path = path.to_path_buf();
    let expected_len = expected_hash.len();
    if expected_len > 32 {
        log::warn!("Unknown hash format (length={expected_len}) for verification");
        return Ok(false);
    }
    let mut hash_buf = [0u8; 32];
    hash_buf[..expected_len].copy_from_slice(expected_hash.as_bytes());
    tokio::task::spawn_blocking(move || match expected_len {
        32 => {
            let digest = pread_hash_md5_digest(&path, known_size)?;
            let expected = std::str::from_utf8(&hash_buf[..32]).unwrap_or("");
            Ok(md5_hex_eq(&digest, expected))
        }
        16 => {
            let digest = pread_hash_xxh64_digest(&path, known_size)?;
            let expected = std::str::from_utf8(&hash_buf[..16]).unwrap_or("");
            Ok(xxh64_hex_eq(digest, expected))
        }
        _ => {
            log::warn!(
                "Unknown hash format (length={len}) for verification",
                len = expected_len
            );
            Ok(false)
        }
    })
    .await?
}

/// Download a single chunk.
pub async fn download_chunk(
    client: &Client,
    chunk_download: &DownloadInfo,
    chunk: ChunkRef<'_>,
    dest: &Path,
    existing_size: Option<u64>,
    handle: Option<&super::handle::DownloadHandle>,
    skip_hash: bool,
) -> SophonResult<()> {
    if !super::assembly::validate_chunk_name(chunk.chunk_name) {
        return Err(SophonError::PathTraversal(chunk.chunk_name.into()));
    }

    let mut url = String::with_capacity(256);
    chunk_download.url_for_into(chunk.chunk_name, &mut url);
    do_download_chunk(client, &url, chunk, dest, existing_size, handle, skip_hash).await
}

async fn do_download_chunk(
    client: &Client,
    url: &str,
    chunk: ChunkRef<'_>,
    dest: &Path,
    existing_size: Option<u64>,
    handle: Option<&super::handle::DownloadHandle>,
    skip_hash: bool,
) -> SophonResult<()> {
    let mut existing_size = match existing_size {
        Some(s) => s,
        None => match tokio::fs::metadata(dest).await {
            Ok(meta) => meta.len(),
            Err(_) => 0,
        },
    };

    if existing_size >= chunk.chunk_size {
        // Truncate if oversized, then verify
        if existing_size > chunk.chunk_size {
            match tokio::fs::OpenOptions::new().write(true).open(dest).await {
                Ok(f) => {
                    if let Err(err) = f.set_len(chunk.chunk_size).await {
                        log::warn!(
                            "Failed to truncate {name} to {size}: {err}; deleting and re-downloading",
                            name = chunk.chunk_name,
                            size = chunk.chunk_size
                        );
                        let _ = tokio::fs::remove_file(dest).await;
                        existing_size = 0;
                    } else {
                        existing_size = chunk.chunk_size;
                    }
                }
                Err(err) => {
                    log::warn!(
                        "Failed to open {name} for truncation: {err}; deleting and re-downloading",
                        name = chunk.chunk_name
                    );
                    let _ = tokio::fs::remove_file(dest).await;
                    existing_size = 0;
                }
            }
        }

        if existing_size >= chunk.chunk_size {
            if !chunk.chunk_compressed_hash_md5.is_empty() {
                if verify_existing_file_hash(
                    dest,
                    chunk.chunk_compressed_hash_md5,
                    Some(existing_size),
                )
                .await?
                {
                    return Ok(());
                }
                let _ = tokio::fs::remove_file(dest).await;
            } else if chunk.chunk_decompressed_hash_md5.is_empty() {
                log::warn!(
                    "Chunk {name} has no compressed or decompressed MD5; trusting size match",
                    name = chunk.chunk_name
                );
                return Ok(());
            } else {
                log::warn!(
                    "Chunk {name} has no compressed MD5; re-downloading for integrity",
                    name = chunk.chunk_name
                );
                let _ = tokio::fs::remove_file(dest).await;
            }
        }
    }

    if existing_size > 0 && existing_size < chunk.chunk_size {
        // Try to resume with Range request
        let mut range_buf = [0u8; 32];
        let range_header = format_range_header(existing_size, &mut range_buf);
        let resp = client
            .get(url)
            .header(reqwest::header::RANGE, range_header)
            .send()
            .await?;

        if resp.status() == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            let _ = tokio::fs::remove_file(dest).await;
        } else if resp.status() == reqwest::StatusCode::PARTIAL_CONTENT {
            let resp = resp.error_for_status()?;
            let range_header_valid = resp
                .headers()
                .get("content-range")
                .and_then(|v| v.to_str().ok())
                .map(|range_str| {
                    if range_str.contains("*/") {
                        return false;
                    }
                    parse_content_range_start(range_str)
                        .map(|start| start == existing_size)
                        .unwrap_or(false)
                })
                .unwrap_or(false);
            if range_header_valid {
                return download_with_resume(resp, chunk, dest, existing_size, handle, skip_hash)
                    .await;
            }
            let _ = tokio::fs::remove_file(dest).await;
        } else {
            let _ = tokio::fs::remove_file(dest).await;
        }
    }

    // Fresh download
    let resp = client.get(url).send().await?;
    let resp = resp.error_for_status()?;
    download_full_file_with_response(
        resp,
        chunk,
        dest,
        handle,
        skip_hash,
        #[cfg(feature = "bench-diagnostics")]
        None,
    )
    .await
}

#[cfg(feature = "bench-diagnostics")]
pub async fn benchmark_download_chunk_with_timings(
    client: &Client,
    url: &str,
    chunk: ChunkRef<'_>,
    dest: &Path,
) -> SophonResult<DownloadPhaseTimings> {
    if !super::assembly::validate_chunk_name(chunk.chunk_name) {
        return Err(SophonError::PathTraversal(chunk.chunk_name.into()));
    }
    let resp = client.get(url).send().await?.error_for_status()?;
    let mut timings = DownloadPhaseTimings::default();
    download_full_file_with_response(resp, chunk, dest, None, false, Some(&mut timings)).await?;
    Ok(timings)
}

/// Shared streaming loop: reads bytes from a response stream, writes to an
/// EvictingWriter, updates hashers, checks size, handles cancellation.
/// Returns Ok(()) after verifying the final hash matches.
#[allow(clippy::too_many_arguments)]
async fn stream_and_verify(
    file: &mut EvictingWriter,
    resp: reqwest::Response,
    hasher: &mut Option<PooledMd5>,
    xxh64_hasher: &mut Option<xxhash_rust::xxh64::Xxh64>,
    expected_total: u64,
    starting_len: u64,
    chunk_name: &str,
    compressed_hash: &str,
    dest: &Path,
    handle: Option<&DownloadHandle>,
    #[cfg(feature = "bench-diagnostics")] mut timings: Option<&mut DownloadPhaseTimings>,
) -> SophonResult<()> {
    #[cfg(feature = "bench-diagnostics")]
    let total_started = std::time::Instant::now();
    let mut stream = resp.bytes_stream();
    let mut total_len = starting_len;

    loop {
        #[cfg(feature = "bench-diagnostics")]
        let wait_started = std::time::Instant::now();
        let next_chunk = stream.next();
        let result = if let Some(handle) = handle {
            tokio::select! {
                biased;
                _ = handle.cancelled_future() => {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(SophonError::Cancelled);
                }
                result = next_chunk => result,
            }
        } else {
            next_chunk.await
        };
        #[cfg(feature = "bench-diagnostics")]
        if let Some(timings) = timings.as_deref_mut() {
            let elapsed = wait_started.elapsed();
            timings.stream_wait += elapsed;
            if timings.per_chunk.stream_wait_count == 0 {
                timings.per_chunk.stream_wait_min = elapsed;
            } else {
                timings.per_chunk.stream_wait_min = timings.per_chunk.stream_wait_min.min(elapsed);
            }
            timings.per_chunk.stream_wait_max = timings.per_chunk.stream_wait_max.max(elapsed);
            timings.per_chunk.stream_wait_sum += elapsed;
            timings.per_chunk.stream_wait_count += 1;
        }

        match result {
            Some(Ok(bytes)) => {
                #[cfg(feature = "bench-diagnostics")]
                if let Some(timings) = timings.as_deref_mut() {
                    timings.stream_chunks += 1;
                    timings.bytes += bytes.len() as u64;
                    timings.chunk_size_buckets[download_chunk_size_bucket(bytes.len())] += 1;
                }
                let next_total_len =
                    match check_download_chunk_size(&bytes, expected_total, total_len) {
                        Ok(total_len) => total_len,
                        Err(StreamConsumeError::EmptyChunk) => {
                            let _ = tokio::fs::remove_file(dest).await;
                            return Err(SophonError::Io(std::io::Error::new(
                                std::io::ErrorKind::InvalidData,
                                "corrupted compressed data: empty chunk while data remaining",
                            )));
                        }
                        Err(StreamConsumeError::TooLarge(actual)) => {
                            let _ = tokio::fs::remove_file(dest).await;
                            return Err(SophonError::SizeMismatch {
                                item: chunk_name.to_string(),
                                expected: expected_total,
                                actual,
                            });
                        }
                    };
                #[cfg(feature = "bench-diagnostics")]
                let write_started = std::time::Instant::now();
                let write_result = file.write_all(&bytes).await;
                #[cfg(feature = "bench-diagnostics")]
                if let Some(timings) = timings.as_deref_mut() {
                    let elapsed = write_started.elapsed();
                    timings.write += elapsed;
                    if timings.per_chunk.write_count == 0 {
                        timings.per_chunk.write_min = elapsed;
                    } else {
                        timings.per_chunk.write_min = timings.per_chunk.write_min.min(elapsed);
                    }
                    timings.per_chunk.write_max = timings.per_chunk.write_max.max(elapsed);
                    timings.per_chunk.write_sum += elapsed;
                    timings.per_chunk.write_count += 1;
                }
                if let Err(error) = write_result {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(SophonError::Io(error));
                }
                #[cfg(feature = "bench-diagnostics")]
                let hash_started = std::time::Instant::now();
                let hash_result =
                    update_download_hashes(&bytes, hasher.as_mut(), xxh64_hasher.as_mut());
                #[cfg(feature = "bench-diagnostics")]
                if let Some(timings) = timings.as_deref_mut() {
                    let elapsed = hash_started.elapsed();
                    timings.hash += elapsed;
                    if timings.per_chunk.hash_count == 0 {
                        timings.per_chunk.hash_min = elapsed;
                    } else {
                        timings.per_chunk.hash_min = timings.per_chunk.hash_min.min(elapsed);
                    }
                    timings.per_chunk.hash_max = timings.per_chunk.hash_max.max(elapsed);
                    timings.per_chunk.hash_sum += elapsed;
                    timings.per_chunk.hash_count += 1;
                }
                if let Err(error) = hash_result {
                    return Err(SophonError::Io(error));
                }
                total_len = next_total_len;
            }
            Some(Err(error)) => {
                let _ = tokio::fs::remove_file(dest).await;
                return Err(error.into());
            }
            None => break,
        }
    }

    if let Err(err) = file.flush().await {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(SophonError::Io(err));
    }

    if total_len != expected_total {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(SophonError::SizeMismatch {
            item: chunk_name.to_string(),
            expected: expected_total,
            actual: total_len,
        });
    }

    if !compressed_hash.is_empty() {
        match compressed_hash.len() {
            32 => {
                let digest: [u8; 16] = hasher
                    .as_mut()
                    .expect("hasher present when compressed hash is non-empty")
                    .finish()
                    .map_err(SophonError::Io)?;
                if !md5_hex_eq(&digest, compressed_hash) {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(SophonError::Md5Mismatch {
                        item: chunk_name.to_string(),
                        expected: compressed_hash.to_string(),
                        actual: md5_to_hex(&digest),
                    });
                }
            }
            16 => {
                let digest = xxh64_hasher
                    .as_ref()
                    .expect("xxh64_hasher present when hash len == 16")
                    .digest();
                if !xxh64_hex_eq(digest, compressed_hash) {
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(SophonError::Md5Mismatch {
                        item: chunk_name.to_string(),
                        expected: compressed_hash.to_string(),
                        actual: format!("{:016x}", digest),
                    });
                }
            }
            _ => {
                log::warn!(
                    "Unknown compressed hash format (length={len}) for chunk {name}",
                    len = compressed_hash.len(),
                    name = chunk_name
                );
            }
        }
    } else {
        log::warn!(
            "Chunk {name} downloaded without compressed hash verification",
            name = chunk_name
        );
    }

    #[cfg(feature = "bench-diagnostics")]
    let final_flush_started = std::time::Instant::now();
    file.flush_and_evict_all().await.ok();
    #[cfg(feature = "bench-diagnostics")]
    if let Some(timings) = timings.as_deref_mut() {
        timings.final_flush_and_evict += final_flush_started.elapsed();
    }
    hasher.take();
    if let Some(h) = xxh64_hasher.take() {
        super::assembly_opt::return_xxh64(h);
    }
    #[cfg(feature = "bench-diagnostics")]
    if let Some(timings) = timings {
        timings.total = total_started.elapsed();
    }
    Ok(())
}

/// Download a full file, streaming body chunks to disk with MD5/XXH64 hashing.
async fn download_full_file_with_response(
    resp: reqwest::Response,
    chunk: ChunkRef<'_>,
    dest: &Path,
    handle: Option<&DownloadHandle>,
    skip_hash: bool,
    #[cfg(feature = "bench-diagnostics")] mut timings: Option<&mut DownloadPhaseTimings>,
) -> SophonResult<()> {
    let content_length = resp.content_length();
    if let Some(len) = content_length
        && len != chunk.chunk_size
    {
        log::warn!(
            "Content-Length ({len}) != expected chunk_size ({expected}) for {name}, proceeding anyway",
            expected = chunk.chunk_size,
            name = chunk.chunk_name
        );
    }

    check_available_space(dest, chunk.chunk_size)?;

    let file = create_preallocated_file(dest, chunk.chunk_size).await?;
    let mut file = EvictingWriter::new(file);
    let mut hasher = if !skip_hash && chunk.chunk_compressed_hash_md5.len() == 32 {
        Some(PooledMd5::new(take_md5()?))
    } else {
        None
    };
    let mut xxh64_hasher: Option<xxhash_rust::xxh64::Xxh64> =
        if !skip_hash && chunk.chunk_compressed_hash_md5.len() == 16 {
            Some(super::assembly_opt::take_xxh64())
        } else {
            None
        };
    #[cfg(feature = "bench-diagnostics")]
    if let Some(timings) = timings.as_deref_mut() {
        timings.hash_algorithm = if hasher.is_some() {
            DownloadHashAlgorithm::Md5
        } else if xxh64_hasher.is_some() {
            DownloadHashAlgorithm::Xxh64
        } else {
            DownloadHashAlgorithm::None
        };
    }

    stream_and_verify(
        &mut file,
        resp,
        &mut hasher,
        &mut xxh64_hasher,
        chunk.chunk_size,
        0,
        chunk.chunk_name,
        if skip_hash {
            ""
        } else {
            chunk.chunk_compressed_hash_md5
        },
        dest,
        handle,
        #[cfg(feature = "bench-diagnostics")]
        timings,
    )
    .await
}

async fn download_with_resume(
    resp: reqwest::Response,
    chunk: ChunkRef<'_>,
    dest: &Path,
    existing_size: u64,
    handle: Option<&DownloadHandle>,
    skip_hash: bool,
) -> SophonResult<()> {
    if resp.status() == reqwest::StatusCode::OK {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(SophonError::ResumeFailed {
            message: "Server returned 200 OK instead of 206 Partial Content".to_string(),
        });
    }

    let expected_total = chunk.chunk_size;
    let remaining = expected_total.saturating_sub(existing_size);

    if let Some(len) = resp.content_length()
        && len != remaining
    {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(SophonError::SizeMismatch {
            item: chunk.chunk_name.to_string(),
            expected: remaining,
            actual: len,
        });
    }

    check_available_space(dest, remaining)?;

    let mut hasher = if !skip_hash && chunk.chunk_compressed_hash_md5.len() == 32 {
        Some(PooledMd5::new(take_md5()?))
    } else {
        None
    };
    let mut xxh64_hasher: Option<xxhash_rust::xxh64::Xxh64> =
        if !skip_hash && chunk.chunk_compressed_hash_md5.len() == 16 {
            Some(super::assembly_opt::take_xxh64())
        } else {
            None
        };
    if hasher.is_some() || xxh64_hasher.is_some() {
        let dest_owned = dest.to_path_buf();
        let mut h = hasher.take();
        let mut xh = xxh64_hasher.take();
        let result = tokio::task::spawn_blocking(move || {
            pread_hash_slice(&dest_owned, existing_size, &mut |chunk| {
                if let Some(hasher) = h.as_mut() {
                    hasher.get_mut().update(chunk)?;
                }
                if let Some(ref mut xxh) = xh {
                    xxh.update(chunk);
                }
                Ok(())
            })?;
            Ok::<_, SophonError>((h, xh))
        })
        .await
        .map_err(|e| SophonError::Io(std::io::Error::other(e.to_string())))??;
        hasher = result.0;
        xxh64_hasher = result.1;
    }

    let file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(dest)
        .await?;
    let mut file = EvictingWriter::with_offset(file, existing_size);

    stream_and_verify(
        &mut file,
        resp,
        &mut hasher,
        &mut xxh64_hasher,
        expected_total,
        existing_size,
        chunk.chunk_name,
        if skip_hash {
            ""
        } else {
            chunk.chunk_compressed_hash_md5
        },
        dest,
        handle,
        #[cfg(feature = "bench-diagnostics")]
        None,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn download_hash_helpers_match_one_shot_digests_across_chunks() {
        let chunks = [b"alpha".as_slice(), b" beta", b" gamma"];
        let expected = b"alpha beta gamma";
        let mut md5 = fast_md5::Md5::new();
        let mut xxh64 = Some(xxhash_rust::xxh64::Xxh64::new(0));
        let mut total = 0;

        for bytes in chunks {
            total = check_download_chunk_size(bytes, expected.len() as u64, total).unwrap();
            update_download_hashes(bytes, Some(&mut md5), xxh64.as_mut()).unwrap();
        }

        let mut expected_md5 = fast_md5::Md5::new();
        expected_md5.update(expected);
        assert_eq!(md5.finalize(), expected_md5.finalize());
        let mut expected_xxh64 = xxhash_rust::xxh64::Xxh64::new(0);
        expected_xxh64.update(expected);
        assert_eq!(xxh64.unwrap().digest(), expected_xxh64.digest());
        assert_eq!(total, expected.len() as u64);
    }

    #[test]
    fn download_hash_helpers_support_md5_and_xxh64_together() {
        let bytes = b"combined hash input";
        let mut md5 = fast_md5::Md5::new();
        let mut xxh64 = Some(xxhash_rust::xxh64::Xxh64::new(0));

        update_download_hashes(bytes, Some(&mut md5), xxh64.as_mut()).unwrap();

        let mut expected_md5 = fast_md5::Md5::new();
        expected_md5.update(bytes);
        assert_eq!(md5.finalize(), expected_md5.finalize());
        let mut expected_xxh64 = xxhash_rust::xxh64::Xxh64::new(0);
        expected_xxh64.update(bytes);
        assert_eq!(xxh64.unwrap().digest(), expected_xxh64.digest());
    }

    #[test]
    fn download_hash_helpers_accept_empty_and_reject_bad_stream_sizes() {
        assert_eq!(check_download_chunk_size(b"", 0, 0).unwrap(), 0);
        assert!(matches!(
            check_download_chunk_size(b"", 1, 0),
            Err(StreamConsumeError::EmptyChunk)
        ));
        assert!(matches!(
            check_download_chunk_size(b"ab", 1, 0),
            Err(StreamConsumeError::TooLarge(2))
        ));
        assert!(matches!(
            check_download_chunk_size(b"x", u64::MAX, u64::MAX),
            Err(StreamConsumeError::TooLarge(u64::MAX))
        ));

        let mut md5 = None::<fast_md5::Md5>;
        update_download_hashes::<fast_md5::Md5>(b"ignored", md5.as_mut(), None).unwrap();
    }

    #[tokio::test]
    async fn create_preallocated_file_truncates_and_keeps_logical_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preallocated.bin");
        std::fs::write(&path, b"previous contents").unwrap();

        let file = create_preallocated_file(&path, 4096).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 0);
        drop(file);
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    }

    #[tokio::test]
    async fn create_preallocated_file_ignores_invalid_preallocation_length() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("preallocated.bin");

        let file = create_preallocated_file(&path, u64::MAX).await.unwrap();
        assert_eq!(file.metadata().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn failed_stream_verification_returns_pooled_md5() {
        use crate::api_scrape::Compression;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = b"downloaded bytes";
        Mock::given(method("GET"))
            .and(path("/chunks/payload"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body.to_vec()))
            .mount(&server)
            .await;

        let chunk = ChunkRef {
            chunk_name: "payload",
            chunk_decompressed_hash_md5: "",
            chunk_size: body.len() as u64,
            chunk_size_decompressed: body.len() as u64,
            chunk_compressed_hash_md5: "00000000000000000000000000000000",
            chunk_old_offset: -1,
        };
        let download = DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: Compression::None,
            url_prefix: server.uri(),
            url_suffix: "chunks".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("payload");

        assert!(matches!(
            download_chunk(&Client::new(), &download, chunk, &dest, None, None, false).await,
            Err(SophonError::Md5Mismatch { .. })
        ));
        assert!(!dest.exists());

        let mut reused = take_md5().unwrap();
        reused.update(b"hello world").unwrap();
        assert_eq!(
            md5_to_hex(&reused.finish().unwrap()),
            "5eb63bbbe01eeed093cb22bb8f5acdc3"
        );
        return_md5(reused);
    }

    #[cfg(feature = "bench-diagnostics")]
    #[tokio::test]
    #[ignore = "diagnostic loopback download timing; run explicitly with --ignored"]
    async fn diagnose_loopback_download_phase_distributions() {
        use bytes::Bytes;
        use futures_util::{StreamExt, stream};
        use http_body_util::StreamBody;
        use hyper::body::{Frame, Incoming};
        use hyper::server::conn::http1;
        use hyper::service::service_fn;
        use hyper::{Request, Response};
        use hyper_util::rt::TokioIo;
        use std::convert::Infallible;
        use std::net::TcpListener as StdTcpListener;
        use std::sync::mpsc;
        use std::thread;

        let sizes_mib = std::env::var("IRMIN_DOWNLOAD_DIAGNOSTIC_SIZES_MIB")
            .unwrap_or_else(|_| "16".to_owned());
        let pattern = Bytes::from(
            (0..64 * 1024)
                .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
                .collect::<Vec<_>>(),
        );
        let (address_tx, address_rx) = mpsc::channel();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
        let server_pattern = pattern.clone();
        let server = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build diagnostic server runtime");
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
                            let pattern = server_pattern.clone();
                            let service = service_fn(move |request: Request<Incoming>| {
                                let pattern = pattern.clone();
                                async move {
                                    let size = request
                                        .headers()
                                        .get("x-diagnostic-size")
                                        .and_then(|value| value.to_str().ok())
                                        .and_then(|value| value.parse::<usize>().ok())
                                        .expect("diagnostic request includes a valid size");
                                    let full_chunks = size / pattern.len();
                                    let remainder = size % pattern.len();
                                    let repeat_pattern = pattern.clone();
                                    let full = stream::repeat_with(move || repeat_pattern.clone())
                                        .take(full_chunks);
                                    let tail = (remainder > 0).then(|| pattern.slice(..remainder));
                                    let body = full
                                        .chain(stream::iter(tail))
                                        .map(|bytes| Ok::<_, Infallible>(Frame::data(bytes)));
                                    let mut response = Response::new(StreamBody::new(body));
                                    response.headers_mut().insert(
                                        hyper::header::CONTENT_LENGTH,
                                        size.to_string().parse().unwrap(),
                                    );
                                    Ok::<_, Infallible>(response)
                                }
                            });
                            let connection = http1::Builder::new()
                                .serve_connection(TokioIo::new(socket), service);
                            let _ = connection.await;
                        }
                    }
                }
            });
        });
        let address = address_rx.recv().unwrap();

        for value in sizes_mib.split(',') {
            let size_mib = value
                .parse::<usize>()
                .expect("diagnostic sizes must be comma-separated MiB integers");
            assert!(
                (16..=256).contains(&size_mib),
                "diagnostic sizes must be between 16 and 256 MiB"
            );
            let size = size_mib * 1024 * 1024;
            let mut expected_md5 = fast_md5::Md5::new();
            let mut expected_xxh64 = xxhash_rust::xxh64::Xxh64::new(0);
            for offset in (0..size).step_by(pattern.len()) {
                let count = pattern.len().min(size - offset);
                expected_md5.update(&pattern[..count]);
                expected_xxh64.update(&pattern[..count]);
            }
            let md5 = md5_to_hex(&expected_md5.finalize());
            let xxh64 = format!("{:016x}", expected_xxh64.digest());
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("payload");

            for (algorithm, expected_hash) in [
                (DownloadHashAlgorithm::Md5, md5.as_str()),
                (DownloadHashAlgorithm::Xxh64, xxh64.as_str()),
            ] {
                let url = format!("http://{address}/payload");
                let response = Client::new()
                    .get(&url)
                    .header("x-diagnostic-size", size.to_string())
                    .send()
                    .await
                    .unwrap()
                    .error_for_status()
                    .unwrap();
                let mut file = EvictingWriter::new(
                    create_preallocated_file(&dest, size as u64).await.unwrap(),
                );
                let mut md5_hasher = (algorithm == DownloadHashAlgorithm::Md5)
                    .then(|| PooledMd5::new(take_md5().unwrap()));
                let mut xxh64_hasher = (algorithm == DownloadHashAlgorithm::Xxh64)
                    .then(super::super::assembly_opt::take_xxh64);
                let mut timings = DownloadPhaseTimings {
                    hash_algorithm: algorithm,
                    ..DownloadPhaseTimings::default()
                };
                stream_and_verify(
                    &mut file,
                    response,
                    &mut md5_hasher,
                    &mut xxh64_hasher,
                    size as u64,
                    0,
                    "payload",
                    expected_hash,
                    &dest,
                    None,
                    Some(&mut timings),
                )
                .await
                .unwrap();

                let mut output = std::fs::File::open(&dest).unwrap();
                let mut output_md5 = fast_md5::Md5::new();
                let mut output_xxh64 = xxhash_rust::xxh64::Xxh64::new(0);
                let mut output_block = [0u8; 64 * 1024];
                loop {
                    let count = std::io::Read::read(&mut output, &mut output_block).unwrap();
                    if count == 0 {
                        break;
                    }
                    output_md5.update(&output_block[..count]);
                    output_xxh64.update(&output_block[..count]);
                }
                assert_eq!(md5_to_hex(&output_md5.finalize()), md5);
                assert_eq!(format!("{:016x}", output_xxh64.digest()), xxh64);
                assert_eq!(timings.bytes, size as u64);
                assert!(timings.stream_chunks > 0);
                assert_eq!(timings.hash_algorithm, algorithm);
                assert_eq!(
                    timings.chunk_size_buckets.iter().sum::<u64>(),
                    timings.stream_chunks
                );
                assert_eq!(
                    timings.per_chunk.stream_wait_count,
                    timings.stream_chunks + 1
                );
                assert_eq!(timings.per_chunk.write_count, timings.stream_chunks);
                assert_eq!(timings.per_chunk.hash_count, timings.stream_chunks);
                assert_eq!(timings.per_chunk.stream_wait_sum, timings.stream_wait);
                assert_eq!(timings.per_chunk.write_sum, timings.write);
                assert_eq!(timings.per_chunk.hash_sum, timings.hash);
                assert!(timings.per_chunk.stream_wait_min <= timings.per_chunk.stream_wait_max);
                assert!(timings.per_chunk.write_min <= timings.per_chunk.write_max);
                assert!(timings.per_chunk.hash_min <= timings.per_chunk.hash_max);
                println!("size_mib={size_mib} hash={algorithm:?} timings={timings:#?}");
            }
        }
        shutdown_tx.send(()).unwrap();
        server.join().unwrap();
    }

    #[cfg(feature = "bench-diagnostics")]
    #[tokio::test]
    async fn diagnostic_download_rejects_invalid_chunk_name() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("payload");
        let chunk = ChunkRef {
            chunk_name: "../payload",
            chunk_decompressed_hash_md5: "",
            chunk_size: 0,
            chunk_size_decompressed: 0,
            chunk_compressed_hash_md5: "",
            chunk_old_offset: -1,
        };

        assert!(matches!(
            benchmark_download_chunk_with_timings(
                &Client::new(),
                "http://127.0.0.1:1",
                chunk,
                &dest
            )
            .await,
            Err(SophonError::PathTraversal(_))
        ));
        assert!(!dest.exists());
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    fn download_chunk_size_buckets_cover_boundaries() {
        for (len, expected) in [
            (0, 0),
            (4 * 1024, 0),
            (4 * 1024 + 1, 1),
            (16 * 1024, 1),
            (16 * 1024 + 1, 2),
            (64 * 1024, 2),
            (64 * 1024 + 1, 3),
            (256 * 1024, 3),
            (256 * 1024 + 1, 4),
        ] {
            assert_eq!(download_chunk_size_bucket(len), expected, "len={len}");
        }
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    fn fast_md5_matches_rfc_1321_vectors() {
        assert_eq!(
            hex::encode(fast_md5::digest(b"")),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(
            hex::encode(fast_md5::digest(b"abc")),
            "900150983cd24fb0d6963f7d28e17f72"
        );
        let million_a = vec![b'a'; 1_000_000];
        assert_eq!(
            hex::encode(fast_md5::digest(&million_a)),
            "7707d6ae4e027c70eea2a935c2296f21"
        );
    }

    #[cfg(feature = "bench-diagnostics")]
    #[tokio::test]
    #[ignore = "diagnostic MD5 update granularity; run explicitly with --ignored"]
    async fn diagnose_md5_update_granularity() {
        use std::hint::black_box;
        use std::time::Instant;

        const LARGE_SIZE: usize = 64 * 1024 * 1024;
        const SMALL_SIZE: usize = 1024 * 1024;
        const REPEATS: usize = 10;
        let make_input = |size: usize| {
            (0..size)
                .map(|index| (index as u8).wrapping_mul(31).wrapping_add(7))
                .collect::<Vec<_>>()
        };
        let small_input = make_input(SMALL_SIZE);
        let small_expected = fast_md5::digest(&small_input);
        let input = make_input(LARGE_SIZE);
        let expected = fast_md5::digest(&input);
        let one_shot_warmup = fast_md5::digest(black_box(&input));
        assert_eq!(one_shot_warmup, expected);
        println!(
            "md5_update_granularity one_shot bytes={} repeats={REPEATS}",
            input.len()
        );
        let one_shot_samples = (0..REPEATS)
            .map(|_| {
                let start = Instant::now();
                let digest = fast_md5::digest(black_box(&input));
                let elapsed = start.elapsed();
                assert_eq!(digest, expected);
                (
                    elapsed,
                    input.len() as f64 / elapsed.as_secs_f64() / 1_000_000.0,
                )
            })
            .collect::<Vec<_>>();
        println!("md5_update_granularity one_shot_samples=(duration, MB/s) {one_shot_samples:?}");

        for chunk_size in [
            1usize,
            63,
            64,
            65,
            4 * 1024 - 1,
            4 * 1024 + 1,
            16 * 1024,
            64 * 1024,
            256 * 1024,
        ] {
            let (input, expected, repeats) = if chunk_size <= 65 {
                (&small_input, small_expected, 3)
            } else {
                (&input, expected, REPEATS)
            };
            let mut warm = fast_md5::Md5::new();
            for bytes in input.chunks(chunk_size) {
                warm.update(bytes);
            }
            assert_eq!(warm.finalize(), expected);
            let samples = (0..repeats)
                .map(|_| {
                    let start = Instant::now();
                    let mut hasher = fast_md5::Md5::new();
                    for bytes in input.chunks(chunk_size) {
                        hasher.update(black_box(bytes));
                    }
                    let digest = hasher.finalize();
                    let elapsed = start.elapsed();
                    assert_eq!(digest, expected);
                    (
                        elapsed,
                        input.len() as f64 / elapsed.as_secs_f64() / 1_000_000.0,
                    )
                })
                .collect::<Vec<_>>();
            println!(
                "md5_update_granularity bytes={} chunk_size={chunk_size} repeats={repeats} samples=(duration, MB/s) {samples:?}",
                input.len()
            );
        }
    }

    #[test]
    fn pooled_md5_returns_context_on_drop() {
        {
            let mut hasher = PooledMd5::new(take_md5().unwrap());
            hasher.get_mut().update(b"dirty context").unwrap();
        }

        let mut hasher = take_md5().unwrap();
        hasher.update(b"hello world").unwrap();
        let digest = hasher.finish().unwrap();
        return_md5(hasher);
        assert_eq!(md5_to_hex(&digest), "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn pread_hash_slice_streams_full_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let data = vec![0xABu8; 300_000];
        std::fs::write(&path, &data).unwrap();

        let mut collected = Vec::new();
        pread_hash_slice(&path, data.len() as u64, &mut |chunk| {
            collected.extend_from_slice(chunk);
            Ok(())
        })
        .unwrap();
        assert_eq!(collected, data);
    }

    #[test]
    fn pread_hash_slice_respects_max_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        let data = vec![0xCDu8; 100_000];
        std::fs::write(&path, &data).unwrap();

        let mut total = 0u64;
        pread_hash_slice(&path, 50_000, &mut |chunk| {
            total += chunk.len() as u64;
            Ok(())
        })
        .unwrap();
        assert_eq!(total, 50_000);
    }

    #[test]
    fn pread_hash_slice_zero_bytes_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"data").unwrap();

        let mut called = false;
        pread_hash_slice(&path, 0, &mut |_| {
            called = true;
            Ok(())
        })
        .unwrap();
        assert!(!called);
    }

    #[test]
    fn pread_hash_md5_digest_matches_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let digest = pread_hash_md5_digest(&path, None).unwrap();
        let hex = md5_to_hex(&digest);
        assert_eq!(hex, "5eb63bbbe01eeed093cb22bb8f5acdc3");
    }

    #[test]
    fn pread_hash_md5_digest_empty_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bin");
        std::fs::write(&path, b"").unwrap();

        let digest = pread_hash_md5_digest(&path, None).unwrap();
        let hex = md5_to_hex(&digest);
        assert_eq!(hex, "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn pread_hash_xxh64_digest_matches_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"hello world").unwrap();

        let value = pread_hash_xxh64_digest(&path, None).unwrap();
        let mut hasher = super::super::assembly_opt::take_xxh64();
        hasher.update(b"hello world");
        assert_eq!(value, hasher.digest());
        super::super::assembly_opt::return_xxh64(hasher);
    }

    #[tokio::test]
    async fn verify_existing_file_hash_empty_returns_true() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bin");
        std::fs::write(&path, b"data").unwrap();
        assert!(verify_existing_file_hash(&path, "", None).await.unwrap());
    }

    #[tokio::test]
    async fn evicting_writer_multiple_writes_produces_correct_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut w = EvictingWriter::new(file);

        w.write_all(b"hello ").await.unwrap();
        w.write_all(b"world").await.unwrap();
        w.write_all(b"!").await.unwrap();
        w.flush().await.unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"hello world!");
    }

    #[tokio::test]
    async fn evicting_writer_written_tracks_total_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut w = EvictingWriter::new(file);

        assert_eq!(w.written(), 0);
        w.write_all(&[0u8; 1000]).await.unwrap();
        assert_eq!(w.written(), 1000);
        w.write_all(&[0u8; 500]).await.unwrap();
        assert_eq!(w.written(), 1500);
    }

    #[tokio::test]
    async fn evicting_writer_with_offset_starts_at_offset() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");

        // Pre-create a file with existing content
        std::fs::write(&path, b"EXISTING_HEADER_").unwrap();

        let file = tokio::fs::OpenOptions::new()
            .write(true)
            .append(true)
            .open(&path)
            .await
            .unwrap();
        let offset = 16u64;
        let mut w = EvictingWriter::with_offset(file, offset);

        assert_eq!(w.written(), offset);
        w.write_all(b"appended_data").await.unwrap();
        assert_eq!(w.written(), offset + 13);
        w.flush().await.unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents, b"EXISTING_HEADER_appended_data");
    }

    #[tokio::test]
    async fn evicting_writer_flush_and_evict_all_completes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let file = tokio::fs::File::create(&path).await.unwrap();
        let mut w = EvictingWriter::new(file);

        w.write_all(&[0xAB; 4096]).await.unwrap();
        // flush_and_evict_all must not error
        w.flush_and_evict_all().await.unwrap();

        let contents = std::fs::read(&path).unwrap();
        assert_eq!(contents.len(), 4096);
        assert!(contents.iter().all(|&b| b == 0xAB));
    }

    /// Pins xxh64 digest to canonical reference values. Guards against seed
    /// or endianness drift that would silently break chunk hash verification.
    #[test]
    fn xxh64_canonical_reference_value() {
        let mut h = xxhash_rust::xxh64::Xxh64::new(0);
        h.update(b"");
        assert_eq!(h.digest(), 0xef46db3751d8e999, "XXH64 empty string");

        let mut h2 = xxhash_rust::xxh64::Xxh64::new(0);
        h2.update(b"hello world");
        assert_eq!(format!("{:016x}", h2.digest()), "45ab6734b21e6968");
    }

    /// Verifies pread_hash_md5_digest produces correct output for files
    /// larger than the 256KB BufReader buffer (exercises multi-iteration loop).
    #[test]
    fn pread_hash_md5_digest_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        let data: Vec<u8> = (0..524288u32).map(|i| (i.wrapping_mul(7)) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let digest = pread_hash_md5_digest(&path, None).unwrap();
        // Compute expected via single-shot
        let expected = {
            let mut h = super::super::assembly_opt::take_md5().unwrap();
            h.update(&data).unwrap();
            h.finish().unwrap()
        };
        assert_eq!(digest, expected);
    }

    /// Verifies pread_hash_xxh64_digest produces correct output for files
    /// larger than the 256KB BufReader buffer.
    #[test]
    fn pread_hash_xxh64_digest_large_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large.bin");
        let data: Vec<u8> = (0..524288u32).map(|i| (i.wrapping_mul(13)) as u8).collect();
        std::fs::write(&path, &data).unwrap();

        let value = pread_hash_xxh64_digest(&path, None).unwrap();
        let mut h = xxhash_rust::xxh64::Xxh64::new(0);
        h.update(&data);
        assert_eq!(value, h.digest());
    }
}
