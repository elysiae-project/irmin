//! Persistent completion bitmap for eager decompression resume.
//!
//! Tracks which chunks have been decompressed and written to their output file.
//! One bit per chunk index, stored as a file with an 8-byte header.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: u32 = 0x49524D42; // "IRMB"

pub struct ChunkBitmap {
    bits: Vec<AtomicU64>,
    path: PathBuf,
    total_chunks: usize,
}

impl ChunkBitmap {
    /// Creates a new empty bitmap file at `path` for `total_chunks` entries.
    pub fn create(path: &Path, total_chunks: usize) -> io::Result<Self> {
        let word_count = total_chunks.div_ceil(64);
        let byte_count = word_count * 8;

        let mut f = File::create(path)?;
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..8].copy_from_slice(&(total_chunks as u32).to_le_bytes());
        f.write_all(&header)?;
        f.write_all(&vec![0u8; byte_count])?;
        f.sync_all()?;

        let bits = (0..word_count).map(|_| AtomicU64::new(0)).collect();
        Ok(Self {
            bits,
            path: path.to_path_buf(),
            total_chunks,
        })
    }

    /// Loads an existing bitmap from disk.
    pub fn load(path: &Path) -> io::Result<Self> {
        let data = fs::read(path)?;
        if data.len() < 8 {
            return Err(io::Error::other("bitmap file too short"));
        }

        let magic = u32::from_le_bytes(data[..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(io::Error::other("invalid bitmap magic"));
        }

        let total_chunks = u32::from_le_bytes(data[4..8].try_into().unwrap()) as usize;
        let word_count = total_chunks.div_ceil(64);
        let expected_len = 8 + word_count * 8;

        if data.len() < expected_len {
            return Err(io::Error::other("bitmap file truncated"));
        }

        let bits: Vec<AtomicU64> = (0..word_count)
            .map(|i| {
                let offset = 8 + i * 8;
                let word = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
                AtomicU64::new(word)
            })
            .collect();

        Ok(Self {
            bits,
            path: path.to_path_buf(),
            total_chunks,
        })
    }

    /// Marks a chunk as complete. Thread-safe (atomic OR).
    pub fn mark_complete(&self, chunk_idx: usize) {
        debug_assert!(chunk_idx < self.total_chunks);
        let word_idx = chunk_idx / 64;
        let bit_idx = chunk_idx % 64;
        self.bits[word_idx].fetch_or(1u64 << bit_idx, Ordering::Release);
    }

    /// Checks if a chunk is marked complete.
    pub fn is_complete(&self, chunk_idx: usize) -> bool {
        if chunk_idx >= self.total_chunks {
            return false;
        }
        let word_idx = chunk_idx / 64;
        let bit_idx = chunk_idx % 64;
        (self.bits[word_idx].load(Ordering::Acquire) >> bit_idx) & 1 == 1
    }

    /// Checks if all chunks in [start..end) are complete.
    pub fn all_complete_for_range(&self, start: usize, end: usize) -> bool {
        (start..end).all(|i| self.is_complete(i))
    }

    /// Clears all bits for chunks in [start..end).
    pub fn clear_range(&self, start: usize, end: usize) {
        for i in start..end {
            let word_idx = i / 64;
            let bit_idx = i % 64;
            self.bits[word_idx].fetch_and(!(1u64 << bit_idx), Ordering::Release);
        }
    }

    /// Flushes the bitmap to disk.
    pub fn sync(&self) -> io::Result<()> {
        let word_count = self.bits.len();
        let mut buf = Vec::with_capacity(8 + word_count * 8);

        buf.extend_from_slice(&MAGIC.to_le_bytes());
        buf.extend_from_slice(&(self.total_chunks as u32).to_le_bytes());
        for word in &self.bits {
            buf.extend_from_slice(&word.load(Ordering::Acquire).to_le_bytes());
        }

        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        f.write_all(&buf)?;
        f.sync_data()?;
        Ok(())
    }

    pub fn total_chunks(&self) -> usize {
        self.total_chunks
    }

    /// Counts the number of completed chunks.
    pub fn count_complete(&self) -> usize {
        self.bits
            .iter()
            .map(|w| w.load(Ordering::Relaxed).count_ones() as usize)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        let bm = ChunkBitmap::create(&path, 200).unwrap();
        assert!(!bm.is_complete(0));
        assert!(!bm.is_complete(199));

        bm.mark_complete(0);
        bm.mark_complete(63);
        bm.mark_complete(64);
        bm.mark_complete(199);

        assert!(bm.is_complete(0));
        assert!(bm.is_complete(63));
        assert!(bm.is_complete(64));
        assert!(bm.is_complete(199));
        assert!(!bm.is_complete(1));
        assert!(!bm.is_complete(100));
    }

    #[test]
    fn sync_and_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        let bm = ChunkBitmap::create(&path, 128).unwrap();
        bm.mark_complete(5);
        bm.mark_complete(77);
        bm.mark_complete(127);
        bm.sync().unwrap();

        let loaded = ChunkBitmap::load(&path).unwrap();
        assert!(loaded.is_complete(5));
        assert!(loaded.is_complete(77));
        assert!(loaded.is_complete(127));
        assert!(!loaded.is_complete(6));
        assert!(!loaded.is_complete(0));
        assert_eq!(loaded.total_chunks(), 128);
    }

    #[test]
    fn all_complete_for_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        let bm = ChunkBitmap::create(&path, 10).unwrap();
        for i in 3..7 {
            bm.mark_complete(i);
        }

        assert!(bm.all_complete_for_range(3, 7));
        assert!(!bm.all_complete_for_range(2, 7));
        assert!(!bm.all_complete_for_range(3, 8));
    }

    #[test]
    fn clear_range() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        let bm = ChunkBitmap::create(&path, 100).unwrap();
        for i in 0..100 {
            bm.mark_complete(i);
        }
        assert_eq!(bm.count_complete(), 100);

        bm.clear_range(10, 20);
        assert_eq!(bm.count_complete(), 90);
        assert!(!bm.is_complete(10));
        assert!(!bm.is_complete(19));
        assert!(bm.is_complete(9));
        assert!(bm.is_complete(20));
    }

    #[test]
    fn concurrent_mark() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        let bm = std::sync::Arc::new(ChunkBitmap::create(&path, 1000).unwrap());
        let handles: Vec<_> = (0..8)
            .map(|t| {
                let bm = std::sync::Arc::clone(&bm);
                std::thread::spawn(move || {
                    for i in (t..1000).step_by(8) {
                        bm.mark_complete(i);
                    }
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(bm.count_complete(), 1000);
    }
}
