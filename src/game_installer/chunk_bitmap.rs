//! Persistent completion bitmap for eager decompression resume.
//!
//! Tracks which chunks have been decompressed and written to their output file.
//! One bit per chunk index, stored as a file with an 8-byte header.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

const MAGIC: u32 = 0x49524D42; // "IRMB"

/// Thread-safe, disk-backed completion bits for chunk indices.
///
/// `sync` persists the current bits, but callers must persist output bytes
/// before syncing a corresponding completion bit to avoid false resume claims.
pub struct ChunkBitmap {
    bits: Vec<AtomicU64>,
    path: PathBuf,
    total_chunks: usize,
    sync_lock: Mutex<()>,
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
            sync_lock: Mutex::new(()),
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
            sync_lock: Mutex::new(()),
        })
    }

    /// Marks a chunk as complete. Thread-safe (atomic OR).
    /// Callers are responsible for calling `sync()` with correct write ordering
    /// (output data must be durable before the bitmap is persisted).
    pub fn mark_complete(&self, chunk_idx: usize) {
        if chunk_idx >= self.total_chunks {
            return;
        }
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

    /// Clears all bits for chunks in `[start..end)`. Each affected word is
    /// updated atomically, but the range as a whole is not cleared atomically.
    pub fn clear_range(&self, start: usize, end: usize) {
        let mut cursor = start;
        while cursor < end {
            let word_idx = cursor / 64;
            let bit_offset = cursor % 64;
            let bit_count = (64 - bit_offset).min(end - cursor);
            let mask = (u64::MAX >> (64 - bit_count)) << bit_offset;
            self.bits[word_idx].fetch_and(!mask, Ordering::Release);
            cursor += bit_count;
        }
    }

    /// Flushes the bitmap to disk. Serialized: concurrent callers block.
    pub fn sync(&self) -> io::Result<()> {
        let _guard = self.sync_lock.lock().unwrap_or_else(|e| e.into_inner());
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

    /// Number of chunk indices represented by this bitmap.
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

    #[test]
    fn non_64_aligned_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        // 65 chunks = 1 full word + 1 bit in second word
        let bm = ChunkBitmap::create(&path, 65).unwrap();
        bm.mark_complete(64);
        assert!(bm.is_complete(64));
        assert!(!bm.is_complete(63));

        bm.sync().unwrap();
        let loaded = ChunkBitmap::load(&path).unwrap();
        assert!(loaded.is_complete(64));
        assert_eq!(loaded.total_chunks(), 65);
    }

    #[test]
    fn corrupted_file_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.bitmap");

        // Too short
        std::fs::write(&path, [0u8; 4]).unwrap();
        assert!(ChunkBitmap::load(&path).is_err());

        // Wrong magic
        let mut data = vec![0u8; 16];
        data[..4].copy_from_slice(&0xDEADBEEFu32.to_le_bytes());
        data[4..8].copy_from_slice(&10u32.to_le_bytes());
        std::fs::write(&path, &data).unwrap();
        assert!(ChunkBitmap::load(&path).is_err());

        // Truncated bitmap body
        let mut data = vec![0u8; 8]; // header only, no bitmap bytes
        data[..4].copy_from_slice(&0x49524D42u32.to_le_bytes()); // correct magic
        data[4..8].copy_from_slice(&100u32.to_le_bytes()); // claims 100 chunks
        std::fs::write(&path, &data).unwrap();
        assert!(ChunkBitmap::load(&path).is_err());
    }

    #[test]
    fn clear_range_crosses_word_boundary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.bitmap");

        let bm = ChunkBitmap::create(&path, 128).unwrap();
        // Mark chunks 60-70 (crosses the 64-bit word boundary at index 64)
        for i in 60..70 {
            bm.mark_complete(i);
        }
        assert!(bm.all_complete_for_range(60, 70));

        // Clear 62-67 (crosses boundary)
        bm.clear_range(62, 67);
        assert!(!bm.is_complete(62));
        assert!(!bm.is_complete(66));
        assert!(bm.is_complete(60));
        assert!(bm.is_complete(61));
        assert!(bm.is_complete(67));
        assert!(bm.is_complete(68));
    }

    #[test]
    fn clear_range_handles_empty_reversed_full_and_partial_tail_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("edge-ranges.bitmap");
        let bitmap = ChunkBitmap::create(&path, 130).unwrap();

        for index in 0..130 {
            bitmap.mark_complete(index);
        }
        bitmap.clear_range(64, 64);
        bitmap.clear_range(80, 70);
        assert_eq!(bitmap.count_complete(), 130);

        bitmap.clear_range(64, 128);
        assert_eq!(bitmap.count_complete(), 66);
        assert!(bitmap.is_complete(63));
        assert!(!bitmap.is_complete(64));
        assert!(!bitmap.is_complete(127));
        assert!(bitmap.is_complete(128));

        bitmap.clear_range(0, 130);
        assert_eq!(bitmap.count_complete(), 0);
    }

    #[test]
    fn clear_range_handles_word_boundaries_and_single_bits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("word-boundaries.bitmap");
        let bitmap = ChunkBitmap::create(&path, 130).unwrap();

        for index in 0..130 {
            bitmap.mark_complete(index);
        }
        bitmap.clear_range(0, 64);
        bitmap.clear_range(63, 64);
        bitmap.clear_range(64, 65);
        bitmap.clear_range(64, 128);

        assert!(!bitmap.is_complete(0));
        assert!(!bitmap.is_complete(63));
        assert!(!bitmap.is_complete(64));
        assert!(!bitmap.is_complete(127));
        assert!(bitmap.is_complete(128));
        assert!(bitmap.is_complete(129));
        assert_eq!(bitmap.count_complete(), 2);
    }

    #[test]
    fn clear_range_out_of_bounds_panics_after_clearing_valid_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out-of-bounds.bitmap");
        let bitmap = ChunkBitmap::create(&path, 65).unwrap();
        bitmap.mark_complete(0);
        bitmap.mark_complete(64);

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            bitmap.clear_range(0, 129);
        }));
        assert!(result.is_err());
        assert!(!bitmap.is_complete(0));
        assert!(!bitmap.is_complete(64));
    }

    #[test]
    fn clear_range_matches_bitwise_reference_for_many_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("reference-ranges.bitmap");
        let bitmap = ChunkBitmap::create(&path, 257).unwrap();
        let mut state = 0x9e37_79b9u32;

        for _ in 0..2_000 {
            let mut expected = [false; 257];
            for (word_idx, word) in bitmap.bits.iter().enumerate() {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let value = (u64::from(state) << 32) | u64::from(state.rotate_left(13));
                word.store(value, Ordering::Relaxed);
                for bit_idx in 0..64 {
                    let index = word_idx * 64 + bit_idx;
                    if index < expected.len() {
                        expected[index] = (value >> bit_idx) & 1 != 0;
                    }
                }
            }
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let start = state as usize % 258;
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let end = state as usize % 258;
            let (start, end) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };

            for bit in expected.iter_mut().take(end).skip(start) {
                *bit = false;
            }
            bitmap.clear_range(start, end);
            for (index, expected_bit) in expected.iter().enumerate() {
                assert_eq!(bitmap.is_complete(index), *expected_bit);
            }
        }
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "measures atomic bitmap range clear throughput"]
    fn benchmark_clear_range() {
        use std::time::Instant;

        let dir = tempfile::tempdir().unwrap();
        let mut rows = Vec::new();
        let cases = [
            (1usize, 0usize, 1usize),
            (64, 0, 63),
            (64, 1, 64),
            (128, 0, 64),
            (128, 1, 66),
            (8_192, 0, 4_096),
            (8_192, 1, 4_097),
            (1_000_000, 0, 1_000_000),
            (1_000_000, 1, 1_000_000),
        ];
        for (case_idx, (total, start, end)) in cases.into_iter().enumerate() {
            let bitmap =
                ChunkBitmap::create(&dir.path().join(format!("{case_idx}.bitmap")), total).unwrap();
            let repetitions = if total >= 1_000_000 { 20 } else { 2_000 };
            let mut elapsed_ns = 0u128;
            for _ in 0..repetitions {
                for word in &bitmap.bits {
                    word.store(u64::MAX, Ordering::Relaxed);
                }
                let started = Instant::now();
                std::hint::black_box(&bitmap)
                    .clear_range(std::hint::black_box(start), std::hint::black_box(end));
                elapsed_ns += started.elapsed().as_nanos();
            }
            rows.push((
                total,
                start,
                end,
                repetitions,
                elapsed_ns / repetitions as u128,
            ));
        }
        println!("bitmap_clear_range={rows:?}");
    }
}
