use std::collections::HashMap;
use std::fs::{self, File};
use std::io::{self, BufRead, Write};
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::time::UNIX_EPOCH;

use dashmap::DashMap;

pub use dashmap::DashMap as VerificationCache;

pub(crate) const VERIFICATION_CACHE_MAX_ENTRIES: usize = 5_000;

#[cfg(test)]
use fast_md5;
use serde::{Deserialize, Serialize};

use super::VERIFICATION_CACHE_FILE;
use super::assembly_opt::md5_hex_eq;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationEntry {
    pub size: u64,
    pub md5: String,
    pub mtime_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VerificationCacheSerializable {
    files: HashMap<String, VerificationEntry>,
}

pub fn load_verification_cache(game_dir: &Path) -> DashMap<String, VerificationEntry> {
    let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);

    // Clean up leftover .tmp files from a previous crash.
    if let Some(parent) = cache_path.parent()
        && let Ok(entries) = parent.read_dir()
    {
        let cache_stem = cache_path.file_stem();
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "tmp")
                && cache_stem == entry.path().file_stem()
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    let serializable: VerificationCacheSerializable = match File::open(&cache_path) {
        Ok(f) => serde_json::from_reader(std::io::BufReader::new(f)).unwrap_or_else(|_| {
            VerificationCacheSerializable {
                files: HashMap::new(),
            }
        }),
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

pub fn save_verification_cache(
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<()> {
    let cache_path = game_dir.join(VERIFICATION_CACHE_FILE);
    let tmp_path = cache_path.with_extension("tmp");
    let serializable = VerificationCacheSerializable {
        files: cache
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect::<std::collections::HashMap<_, _>>(),
    };
    {
        let f = File::create(&tmp_path)?;
        let mut writer = std::io::BufWriter::with_capacity(super::FILE_WRITE_BUFFER_SIZE, f);
        serde_json::to_writer(&mut writer, &serializable)?;
        writer.flush()?;
        writer.get_ref().sync_data()?;
    }
    fs::rename(&tmp_path, &cache_path).inspect_err(|_| {
        let _ = fs::remove_file(&tmp_path);
    })?;
    Ok(())
}

pub fn check_file_md5_cached(
    path: &Path,
    expected_size: u64,
    expected_md5: &str,
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<bool> {
    let relative = path.strip_prefix(game_dir).unwrap_or(path);
    let cache_key = relative.to_string_lossy();
    check_file_md5_with_cache_key(path, expected_size, expected_md5, &cache_key, cache)
}

pub fn check_file_md5_cached_with_size(
    path: &Path,
    expected_size: u64,
    expected_md5: &str,
    game_dir: &Path,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<(bool, Option<u64>)> {
    let relative = path.strip_prefix(game_dir).unwrap_or(path);
    let cache_key = relative.to_string_lossy();
    check_file_md5_with_cache_key_and_size(path, expected_size, expected_md5, &cache_key, cache)
}

pub fn check_file_md5_with_cache_key(
    path: &Path,
    expected_size: u64,
    expected_md5: &str,
    cache_key: &str,
    cache: &DashMap<String, VerificationEntry>,
) -> io::Result<bool> {
    check_file_md5_with_cache_key_and_size(path, expected_size, expected_md5, cache_key, cache)
        .map(|(ok, _)| ok)
}

pub fn check_file_md5_with_cache_key_and_size(
    path: &Path,
    expected_size: u64,
    expected_md5: &str,
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
    {
        if entry.mtime_secs == mtime {
            return Ok((true, Some(actual_size)));
        }
        // Size matches and cache entry matches — trust it.
        return Ok((true, Some(actual_size)));
    }

    let actual = file_md5_digest(path)?;
    let matches = md5_hex_eq(&actual, expected_md5);

    if matches {
        if cache.len() >= VERIFICATION_CACHE_MAX_ENTRIES {
            return Ok((true, Some(actual_size)));
        }
        cache.insert(
            cache_key.to_string(),
            VerificationEntry {
                size: expected_size,
                md5: expected_md5.to_string(),
                mtime_secs: mtime,
            },
        );
    } else {
        cache.remove(cache_key);
    }

    Ok((matches, Some(actual_size)))
}

pub fn file_md5_digest(path: &Path) -> io::Result<[u8; 16]> {
    let file = File::open(path)?;
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok([
            0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8,
            0x42, 0x7e,
        ]);
    }

    let fd = file.as_raw_fd();
    super::assembly_opt::posix_advise(fd, 0, len, libc::POSIX_FADV_SEQUENTIAL);

    let mut hasher =
        super::assembly_opt::take_md5().map_err(|e| io::Error::other(e.to_string()))?;
    let mut reader = io::BufReader::with_capacity(256 * 1024, file);
    loop {
        let buf = reader.fill_buf()?;
        if buf.is_empty() {
            break;
        }
        hasher
            .update(buf)
            .map_err(|e| io::Error::other(e.to_string()))?;
        let n = buf.len();
        reader.consume(n);
    }
    super::assembly_opt::posix_advise(fd, 0, len, libc::POSIX_FADV_DONTNEED);

    let digest = hasher
        .finish()
        .map_err(|e| io::Error::other(e.to_string()))?;
    super::assembly_opt::return_md5(hasher);
    Ok(digest)
}


#[cfg(test)]
#[path = "cache_tests.rs"]
mod tests;
