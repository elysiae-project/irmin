use std::collections::HashMap;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::UNIX_EPOCH;

use dashmap::DashMap;

pub use dashmap::DashMap as VerificationCache;

pub(crate) const VERIFICATION_CACHE_MAX_ENTRIES: usize = 5_000;
const MD5_READ_BUFFER_SIZE: usize = 256 * 1024;

#[cfg(test)]
use fast_md5;
use serde::{Deserialize, Serialize};

use super::VERIFICATION_CACHE_FILE;
use super::assembly_opt::{bytes_to_hex, hex_to_bytes, md5_bytes_eq};

#[derive(Debug, Clone)]
/// Cached proof that a file matched a manifest size and MD5 at a particular
/// whole-second modification time. A changed size or timestamp forces hashing.
pub struct VerificationEntry {
    pub size: u64,
    pub md5: Vec<u8>,
    pub mtime_secs: u64,
}

impl Serialize for VerificationEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("VerificationEntry", 3)?;
        state.serialize_field("size", &self.size)?;
        state.serialize_field("md5", &bytes_to_hex(&self.md5))?;
        state.serialize_field("mtime_secs", &self.mtime_secs)?;
        state.end()
    }
}

impl<'de> Deserialize<'de> for VerificationEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Raw {
            size: u64,
            md5: String,
            #[serde(alias = "mtime_secs")]
            mtime_secs: u64,
        }
        let raw = Raw::deserialize(deserializer)?;
        let md5 = hex_to_bytes(&raw.md5).unwrap_or_else(|| raw.md5.into_bytes());
        Ok(VerificationEntry {
            size: raw.size,
            md5,
            mtime_secs: raw.mtime_secs,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerificationCacheSerializable {
    files: HashMap<String, VerificationEntry>,
}

/// Load the per-install verification cache, treating missing or malformed
/// cache files as empty and trimming oversized caches to the configured limit.
pub fn load_verification_cache(game_dir: &Path) -> DashMap<String, VerificationEntry> {
    let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);

    // Clean up the temp file a crashed save may have left behind. The save path
    // uses one fixed name, so remove it directly instead of scanning the whole
    // game directory.
    let _ = fs::remove_file(cache_path.with_extension("tmp"));

    let serializable: VerificationCacheSerializable = match fs::read(&cache_path) {
        Ok(bytes) => {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| VerificationCacheSerializable {
                files: HashMap::new(),
            })
        }
        Err(_) => VerificationCacheSerializable {
            files: HashMap::new(),
        },
    };
    // Trim cache to half the max if it exceeds the limit.
    const MAX_CACHE_ENTRIES: usize = VERIFICATION_CACHE_MAX_ENTRIES;
    let trim_keys: Vec<String> = if serializable.files.len() > MAX_CACHE_ENTRIES {
        let count = serializable.files.len();
        let to_trim: Vec<_> = serializable
            .files
            .keys()
            .take(count - MAX_CACHE_ENTRIES / 2)
            .cloned()
            .collect();
        let trim_len = to_trim.len();
        log::warn!(
            "Verification cache has {count} entries (max {MAX_CACHE_ENTRIES}), trimming {trim_len} entries",
        );
        to_trim
    } else {
        Vec::new()
    };

    let cache: DashMap<String, VerificationEntry> = DashMap::new();
    for (k, v) in serializable.files {
        cache.insert(k, v);
    }

    for key in &trim_keys {
        cache.remove(key);
    }
    if !trim_keys.is_empty() {
        let cache_len = cache.len();
        log::debug!("Trimmed verification cache to {cache_len} entries");
    }

    // Stale entries are pruned lazily during the next check_file_md5_cached call.

    cache
}

/// Atomically replace the on-disk verification cache via a synced temporary file.
pub fn save_verification_cache(
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<()> {
    let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);
    let tmp_path = cache_path.with_extension("tmp");
    {
        let f = File::create(&tmp_path)?;
        let mut writer = std::io::BufWriter::with_capacity(super::FILE_WRITE_BUFFER_SIZE, f);
        // Serialize directly from the DashMap iterator, avoiding the
        // intermediate HashMap clone of every entry.
        let wrapper = SaveWrapper { files: cache };
        serde_json::to_writer(&mut writer, &wrapper)?;
        writer.flush()?;
        writer.get_ref().sync_data()?;
    }
    fs::rename(&tmp_path, &cache_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })?;
    Ok(())
}

/// Serialization-only wrapper that borrows a `DashMap` as the `files` field,
/// producing the same JSON structure as `VerificationCacheSerializable` without
/// cloning every entry into an intermediate `HashMap`. The field name `files`
/// must stay in sync with `VerificationCacheSerializable`.
struct SaveWrapper<'a> {
    files: &'a DashMap<String, VerificationEntry>,
}

impl Serialize for SaveWrapper<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut state = serializer.serialize_struct("VerificationCacheSerializable", 1)?;
        state.serialize_field("files", self.files)?;
        state.end()
    }
}

/// Verify a file using a path relative to `game_dir` as its cache key.
pub fn check_file_md5_cached(
    path: &Path,
    expected_size: u64,
    expected_md5: &[u8],
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<bool> {
    let cache_key = cache_key_for_path(path, game_dir);
    check_file_md5_with_cache_key(path, expected_size, expected_md5, &cache_key, cache)
}

/// As [`check_file_md5_cached`], also returning the observed size when the file exists.
pub fn check_file_md5_cached_with_size(
    path: &Path,
    expected_size: u64,
    expected_md5: &[u8],
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<(bool, Option<u64>)> {
    let cache_key = cache_key_for_path(path, game_dir);
    check_file_md5_with_cache_key_and_size(path, expected_size, expected_md5, &cache_key, cache)
}

/// Verify a file using an explicit cache key; returns false for missing or mismatched files.
pub fn check_file_md5_with_cache_key(
    path: &Path,
    expected_size: u64,
    expected_md5: &[u8],
    cache_key: &str,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<bool> {
    check_file_md5_with_cache_key_and_size(path, expected_size, expected_md5, cache_key, cache)
        .map(|(ok, _)| ok)
}

/// As [`check_file_md5_with_cache_key`], returning `Some(actual_size)` when present.
#[inline]
pub fn check_file_md5_with_cache_key_and_size(
    path: &Path,
    expected_size: u64,
    expected_md5: &[u8],
    cache_key: &str,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<(bool, Option<u64>)> {
    let metadata = match path.metadata() {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            cache.remove(cache_key);
            return Ok((false, None));
        }
        Err(err) => return Err(err),
    };
    check_file_md5_with_metadata(
        path,
        expected_size,
        expected_md5,
        cache_key,
        cache,
        metadata,
    )
}

pub(crate) fn check_file_md5_cached_with_metadata(
    path: &Path,
    expected_size: u64,
    expected_md5: &[u8],
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
    metadata: Metadata,
) -> io::Result<bool> {
    let cache_key = cache_key_for_path(path, game_dir);
    check_file_md5_with_metadata(
        path,
        expected_size,
        expected_md5,
        &cache_key,
        cache,
        metadata,
    )
    .map(|(ok, _)| ok)
}

fn cache_key_for_path<'a>(path: &'a Path, game_dir: &Path) -> std::borrow::Cow<'a, str> {
    path.strip_prefix(game_dir)
        .unwrap_or(path)
        .to_string_lossy()
}

#[inline]
fn check_file_md5_with_metadata(
    path: &Path,
    expected_size: u64,
    expected_md5: &[u8],
    cache_key: &str,
    cache: &DashMap<String, VerificationEntry>,
    metadata: Metadata,
) -> io::Result<(bool, Option<u64>)> {
    let actual_size = metadata.len();

    // Fast rejection: skip DashMap lookup if size doesn't match.
    if actual_size != expected_size {
        cache.remove(cache_key);
        return Ok((false, Some(actual_size)));
    }

    let mtime = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    if let Some(entry) = cache.get(cache_key)
        && entry.size == expected_size
        && entry.md5 == expected_md5
        && entry.mtime_secs == mtime
    {
        return Ok((true, Some(actual_size)));
    }

    let actual = file_md5_digest(path)?;
    let matches = md5_bytes_eq(&actual, expected_md5);

    if matches {
        if cache.len() >= VERIFICATION_CACHE_MAX_ENTRIES {
            return Ok((true, Some(actual_size)));
        }
        cache.insert(
            cache_key.to_string(),
            VerificationEntry {
                size: expected_size,
                md5: expected_md5.to_vec(),
                mtime_secs: mtime,
            },
        );
    } else {
        cache.remove(cache_key);
    }

    Ok((matches, Some(actual_size)))
}

/// Compute the raw 16-byte MD5 digest of a file using buffered sequential reads.
/// Returns an I/O error if opening or reading the file fails.
#[inline]
pub fn file_md5_digest(path: &Path) -> io::Result<[u8; 16]> {
    file_md5_digest_with_options(path, MD5_READ_BUFFER_SIZE, true)
}

#[inline]
fn file_md5_digest_with_options(
    path: &Path,
    buffer_capacity: usize,
    advise_page_cache: bool,
) -> io::Result<[u8; 16]> {
    // Preserve invalid-capacity precedence over file-open errors.
    if buffer_capacity == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MD5 read buffer capacity must be nonzero",
        ));
    }
    let file = File::open(path)?;
    file_md5_digest_with_file(file, buffer_capacity, advise_page_cache)
}

fn file_md5_digest_with_file(
    mut file: File,
    buffer_capacity: usize,
    advise_page_cache: bool,
) -> io::Result<[u8; 16]> {
    // Validate independently because benchmark callers can pass a File directly.
    if buffer_capacity == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "MD5 read buffer capacity must be nonzero",
        ));
    }
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok([
            0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
            0x42, 0x7e,
        ]);
    }

    let fd = file.as_raw_fd();
    if advise_page_cache {
        super::assembly_opt::posix_advise(fd, 0, len, libc::POSIX_FADV_SEQUENTIAL);
    }

    // Start small: don't grow the reusable buffer past what this file needs.
    let read_capacity = buffer_capacity.min(usize::try_from(len).unwrap_or(usize::MAX));

    let mut hasher =
        super::assembly_opt::take_md5().map_err(|e| io::Error::other(e.to_string()))?;
    let result = (|| {
        let mut buffer = super::assembly_opt::HashReadBuffer::take(read_capacity);
        loop {
            let n = {
                let slice = buffer.as_mut_slice();
                file.read(slice)?
            };
            if n == 0 {
                break;
            }
            hasher
                .update(buffer.filled(n))
                .map_err(|e| io::Error::other(e.to_string()))?;
        }
        hasher.finish().map_err(|e| io::Error::other(e.to_string()))
    })();
    if advise_page_cache {
        super::assembly_opt::posix_advise(fd, 0, len, libc::POSIX_FADV_DONTNEED);
    }
    super::assembly_opt::return_md5(hasher);
    result
}

#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub mod benchmark {
    use std::fs::File;
    use std::io;
    use std::path::Path;

    pub fn file_md5_digest_with_options(
        path: &Path,
        capacity: usize,
        advise_page_cache: bool,
    ) -> io::Result<[u8; 16]> {
        super::file_md5_digest_with_options(path, capacity, advise_page_cache)
    }

    pub fn file_md5_digest_with_file(
        file: File,
        capacity: usize,
        advise_page_cache: bool,
    ) -> io::Result<[u8; 16]> {
        // The file-consuming benchmark helper hashes from the current cursor.
        super::file_md5_digest_with_file(file, capacity, advise_page_cache)
    }
}

#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
