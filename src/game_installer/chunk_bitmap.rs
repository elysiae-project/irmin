//! Persistent completion bitmap for eager decompression resume.
//!
//! Tracks which chunks have been decompressed and written to their output file.
//! One bit per chunk index, stored as a file with an 8-byte header.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
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
    sync_lock: Mutex<Vec<u8>>,
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
            sync_lock: Mutex::new(Vec::new()),
        })
    }

    /// Loads an existing bitmap from disk.
    pub fn load(path: &Path) -> io::Result<Self> {
        let mut file = File::open(path)?;
        let file_len = file.metadata()?.len();
        if file_len < 8 {
            return Err(io::Error::other("bitmap file too short"));
        }

        let mut header = [0u8; 8];
        file.read_exact(&mut header).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::other("bitmap file too short")
            } else {
                error
            }
        })?;
        let magic = u32::from_le_bytes(header[..4].try_into().unwrap());
        if magic != MAGIC {
            return Err(io::Error::other("invalid bitmap magic"));
        }

        let total_chunks = u32::from_le_bytes(header[4..8].try_into().unwrap());
        let word_count_u64 = u64::from(total_chunks).div_ceil(64);
        let expected_len = word_count_u64
            .checked_mul(8)
            .and_then(|body_len| body_len.checked_add(8))
            .ok_or_else(|| io::Error::other("bitmap size overflow"))?;

        if file_len < expected_len {
            return Err(io::Error::other("bitmap file truncated"));
        }

        let total_chunks = usize::try_from(total_chunks)
            .map_err(|_| io::Error::other("bitmap chunk count does not fit memory"))?;
        let word_count = usize::try_from(word_count_u64)
            .map_err(|_| io::Error::other("bitmap word count does not fit memory"))?;
        let mut bits = Vec::with_capacity(word_count);
        let body_bytes = usize::try_from(word_count_u64 * 8)
            .map_err(|_| io::Error::other("bitmap size does not fit memory"))?;
        let mut buffer = vec![0u8; body_bytes.min(64 * 1024)];
        let mut remaining_words = word_count;
        while remaining_words > 0 {
            let words = remaining_words.min(buffer.len() / 8);
            let bytes = &mut buffer[..words * 8];
            file.read_exact(bytes).map_err(|error| {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    io::Error::other("bitmap file truncated")
                } else {
                    error
                }
            })?;
            bits.extend(
                bytes
                    .chunks_exact(8)
                    .map(|word| AtomicU64::new(u64::from_le_bytes(word.try_into().unwrap()))),
            );
            remaining_words -= words;
        }

        Ok(Self {
            bits,
            path: path.to_path_buf(),
            total_chunks,
            sync_lock: Mutex::new(Vec::new()),
        })
    }

    /// Marks a chunk as complete. Thread-safe (atomic OR).
    /// Callers are responsible for calling `sync()` with correct write ordering
    /// (output data must be durable before the bitmap is persisted).
    #[inline]
    pub fn mark_complete(&self, chunk_idx: usize) {
        if chunk_idx >= self.total_chunks {
            return;
        }
        let word_idx = chunk_idx / 64;
        let bit_idx = chunk_idx % 64;
        self.bits[word_idx].fetch_or(1u64 << bit_idx, Ordering::Release);
    }

    /// Checks if a chunk is marked complete.
    #[inline]
    pub fn is_complete(&self, chunk_idx: usize) -> bool {
        if chunk_idx >= self.total_chunks {
            return false;
        }
        let word_idx = chunk_idx / 64;
        let bit_idx = chunk_idx % 64;
        (self.bits[word_idx].load(Ordering::Acquire) >> bit_idx) & 1 == 1
    }

    /// Checks if all chunks in [start..end) are complete.
    #[inline]
    pub fn all_complete_for_range(&self, start: usize, end: usize) -> bool {
        if start >= end {
            return true;
        }
        if end > self.total_chunks {
            return false;
        }

        let first_word = start / 64;
        let last_word = (end - 1) / 64;
        if first_word == last_word {
            let first_bit = start % 64;
            let bit_count = end - start;
            let mask = (u64::MAX >> (64 - bit_count)) << first_bit;
            return self.bits[first_word].load(Ordering::Acquire) & mask == mask;
        }

        let first_bit = start % 64;
        let first_mask = u64::MAX << first_bit;
        if self.bits[first_word].load(Ordering::Acquire) & first_mask != first_mask {
            return false;
        }

        for word_idx in first_word + 1..last_word {
            if self.bits[word_idx].load(Ordering::Acquire) != u64::MAX {
                return false;
            }
        }

        let last_bit_count = end % 64;
        if last_bit_count == 0 {
            self.bits[last_word].load(Ordering::Acquire) == u64::MAX
        } else {
            let last_mask = u64::MAX >> (64 - last_bit_count);
            self.bits[last_word].load(Ordering::Acquire) & last_mask == last_mask
        }
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
        let mut buffer = self.sync_lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut f = OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(&MAGIC.to_le_bytes());
        header[4..].copy_from_slice(&(self.total_chunks as u32).to_le_bytes());
        f.write_all(&header)?;

        let words_per_block = (64 * 1024) / 8;
        buffer.clear();
        for words in self.bits.chunks(words_per_block) {
            for word in words {
                buffer.extend_from_slice(&word.load(Ordering::Acquire).to_le_bytes());
            }
            f.write_all(&buffer)?;
            buffer.clear();
        }
        f.sync_data()?;
        Ok(())
    }

    /// Number of chunk indices represented by this bitmap.
    #[inline]
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
    #[cfg(feature = "bench-diagnostics")]
    use std::fs;

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
    fn all_complete_for_range_matches_bitwise_reference() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = 0xD1B5_4A32_D192_ED03u64;
        for (case_idx, total) in [0usize, 1, 63, 64, 65, 127, 128, 129, 1025]
            .into_iter()
            .enumerate()
        {
            let bitmap =
                ChunkBitmap::create(&dir.path().join(format!("range-{case_idx}.bitmap")), total)
                    .unwrap();
            for index in 0..total {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                if state & 1 != 0 {
                    bitmap.mark_complete(index);
                }
            }

            for _ in 0..500 {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let start = state as usize % (total + 3);
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                let end = state as usize % (total + 3);
                let expected = (start..end).all(|index| bitmap.is_complete(index));
                assert_eq!(bitmap.all_complete_for_range(start, end), expected);
            }
        }
    }

    #[test]
    fn all_complete_for_range_preserves_empty_reversed_and_out_of_bounds_behavior() {
        let dir = tempfile::tempdir().unwrap();
        let bitmap = ChunkBitmap::create(&dir.path().join("range-boundaries.bitmap"), 65).unwrap();
        assert!(bitmap.all_complete_for_range(0, 0));
        assert!(bitmap.all_complete_for_range(64, 63));
        assert!(!bitmap.all_complete_for_range(64, 66));
        assert!(!bitmap.all_complete_for_range(65, 66));
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

        let mut huge_header = [0u8; 8];
        huge_header[..4].copy_from_slice(&MAGIC.to_le_bytes());
        huge_header[4..].copy_from_slice(&u32::MAX.to_le_bytes());
        std::fs::write(&path, huge_header).unwrap();
        assert!(ChunkBitmap::load(&path).is_err());
    }

    #[test]
    fn load_accepts_trailing_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trailing.bitmap");
        let bitmap = ChunkBitmap::create(&path, 65).unwrap();
        bitmap.mark_complete(64);
        bitmap.sync().unwrap();
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&[1, 2, 3, 4]).unwrap();

        let loaded = ChunkBitmap::load(&path).unwrap();
        assert_eq!(loaded.total_chunks(), 65);
        assert!(loaded.is_complete(64));
    }

    #[test]
    fn load_empty_bitmap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.bitmap");
        ChunkBitmap::create(&path, 0).unwrap();

        let loaded = ChunkBitmap::load(&path).unwrap();
        assert_eq!(loaded.total_chunks(), 0);
        assert_eq!(loaded.count_complete(), 0);
    }

    #[test]
    fn load_bitmap_across_multiple_read_blocks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("multi-block.bitmap");
        let bitmap = ChunkBitmap::create(&path, 64 * 10_000).unwrap();
        for index in [0, 64 * 8191, 64 * 8192, 64 * 9999] {
            bitmap.mark_complete(index);
        }
        bitmap.sync().unwrap();

        let loaded = ChunkBitmap::load(&path).unwrap();
        assert_eq!(loaded.total_chunks(), 64 * 10_000);
        for index in [0, 64 * 8191, 64 * 8192, 64 * 9999] {
            assert!(loaded.is_complete(index));
        }
        assert!(!loaded.is_complete(1));
    }

    #[test]
    fn load_rejects_body_truncated_after_a_read_block() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("truncated-multi-block.bitmap");
        let total_chunks = 64 * 10_000;
        let mut file = File::create(&path).unwrap();
        file.write_all(&MAGIC.to_le_bytes()).unwrap();
        file.write_all(&(total_chunks as u32).to_le_bytes())
            .unwrap();
        file.write_all(&vec![0; 64 * 1024 + 8]).unwrap();

        assert!(
            ChunkBitmap::load(&path)
                .is_err_and(|error| { error.to_string() == "bitmap file truncated" })
        );
    }

    #[test]
    fn sync_streams_multiple_blocks_and_preserves_atomic_words() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sync-multi-block.bitmap");
        let bitmap = ChunkBitmap::create(&path, 64 * 10_000).unwrap();
        for index in [0, 64 * 8191, 64 * 8192, 64 * 9999] {
            bitmap.mark_complete(index);
        }
        bitmap.sync().unwrap();

        let loaded = ChunkBitmap::load(&path).unwrap();
        assert_eq!(loaded.total_chunks(), 64 * 10_000);
        for index in [0, 64 * 8191, 64 * 8192, 64 * 9999] {
            assert!(loaded.is_complete(index));
        }
        assert!(!loaded.is_complete(1));
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "measures bitmap load/sync time and process VmHWM"]
    fn benchmark_bitmap_io_fixture() {
        use std::time::Instant;

        fn vm_hwm_kib() -> Option<u64> {
            let status = fs::read_to_string("/proc/self/status").ok()?;
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
        }

        let path = std::env::var_os("IRMIN_BITMAP_BENCH_PATH")
            .expect("IRMIN_BITMAP_BENCH_PATH must point to a prepared bitmap file");
        let path = PathBuf::from(path);
        let operation = std::env::var("IRMIN_BITMAP_BENCH_OP").unwrap_or_else(|_| "load".into());
        assert!(matches!(operation.as_str(), "load" | "sync"));
        let bitmap = if operation == "sync" {
            ChunkBitmap::load(&path).unwrap()
        } else {
            drop(ChunkBitmap::load(&path).unwrap());
            let before_hwm = vm_hwm_kib();
            let started = Instant::now();
            let bitmap = ChunkBitmap::load(&path).unwrap();
            let elapsed = started.elapsed();
            let after_hwm = vm_hwm_kib();
            println!(
                "bitmap_load chunks={} bytes={} elapsed_ms={} hwm_before_kib={before_hwm:?} hwm_after_kib={after_hwm:?}",
                bitmap.total_chunks(),
                fs::metadata(&path).unwrap().len(),
                elapsed.as_millis(),
            );
            std::hint::black_box(bitmap.count_complete());
            return;
        };
        let before_hwm = vm_hwm_kib();
        let started = Instant::now();
        bitmap.sync().unwrap();
        let elapsed = started.elapsed();
        let after_hwm = vm_hwm_kib();
        println!(
            "bitmap_sync chunks={} bytes={} elapsed_ms={} hwm_before_kib={before_hwm:?} hwm_after_kib={after_hwm:?}",
            bitmap.total_chunks(),
            fs::metadata(&path).unwrap().len(),
            elapsed.as_millis(),
        );
        std::hint::black_box(bitmap.count_complete());
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

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "measures bitmap range-completion checks"]
    fn benchmark_all_complete_for_range() {
        use std::time::Instant;

        fn reference(bitmap: &ChunkBitmap, start: usize, end: usize) -> bool {
            (start..end).all(|index| bitmap.is_complete(index))
        }

        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (1usize, 0usize, 1usize),
            (63, 0, 63),
            (64, 0, 64),
            (65, 1, 65),
            (1_024, 0, 1_024),
            (1_024, 1, 1_023),
            (1_000_000, 0, 1_000_000),
            (1_000_000, 1, 1_000_000),
        ];
        for (case_idx, (total, start, end)) in cases.into_iter().enumerate() {
            for missing in [None, Some(start), Some(end.saturating_sub(1))] {
                let bitmap = ChunkBitmap::create(
                    &dir.path().join(format!("{case_idx}-{missing:?}.bitmap")),
                    total,
                )
                .unwrap();
                for word in &bitmap.bits {
                    word.store(u64::MAX, Ordering::Relaxed);
                }
                if let Some(index) = missing.filter(|index| *index < total) {
                    bitmap.bits[index / 64].fetch_and(!(1 << (index % 64)), Ordering::Relaxed);
                }
                let expected = reference(&bitmap, start, end);
                assert_eq!(bitmap.all_complete_for_range(start, end), expected);
                let repetitions = if total >= 1_000_000 { 5_000 } else { 50_000 };
                let mut baseline_ns = 0u128;
                let mut candidate_ns = 0u128;
                for round in 0..repetitions {
                    if round % 2 == 0 {
                        let started = Instant::now();
                        std::hint::black_box(reference(&bitmap, start, end));
                        baseline_ns += started.elapsed().as_nanos();
                        let started = Instant::now();
                        std::hint::black_box(bitmap.all_complete_for_range(start, end));
                        candidate_ns += started.elapsed().as_nanos();
                    } else {
                        let started = Instant::now();
                        std::hint::black_box(bitmap.all_complete_for_range(start, end));
                        candidate_ns += started.elapsed().as_nanos();
                        let started = Instant::now();
                        std::hint::black_box(reference(&bitmap, start, end));
                        baseline_ns += started.elapsed().as_nanos();
                    }
                }
                println!(
                    "bitmap_range_complete total={total} range={start}..{end} missing={missing:?} repetitions={repetitions} baseline_ns={} candidate_ns={}",
                    baseline_ns / repetitions as u128,
                    candidate_ns / repetitions as u128,
                );
            }
        }
    }
}
