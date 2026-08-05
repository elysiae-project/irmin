//! Columnar manifest storage.
//!
//! Replaces the protobuf `Vec<SophonManifestAssetProperty>` representation
//! with per-field `Vec` columns and a shared string arena. This eliminates
//! per-string and per-Vec heap allocation overhead and improves jemalloc
//! fragmentation by co-locating fields in contiguous allocations.

use crate::proto_parse::{SophonManifestAssetChunk, SophonManifestAssetProperty};
use rustc_hash::FxHashMap;
use std::hash::{Hash, Hasher};

impl<'a> From<&'a SophonManifestAssetChunk> for ChunkRef<'a> {
    fn from(chunk: &'a SophonManifestAssetChunk) -> Self {
        ChunkRef {
            chunk_name: &chunk.chunk_name,
            chunk_decompressed_hash_md5: &chunk.chunk_decompressed_hash_md5,
            chunk_size: chunk.chunk_size,
            chunk_size_decompressed: chunk.chunk_size_decompressed,
            chunk_compressed_hash_md5: &chunk.chunk_compressed_hash_md5,
            chunk_old_offset: chunk.chunk_old_offset,
        }
    }
}

/// Shared string arena. All manifest strings are concatenated into a single
/// `String` and referenced by offset. Length is derived from consecutive
/// offsets, so each string costs 4 bytes (u32 offset) instead of 8.
#[derive(Default)]
#[allow(clippy::len_without_is_empty)]
pub struct StringArena {
    data: String,
    offsets: Vec<u32>,
    dedup: FxHashMap<u64, u32>,
}

impl StringArena {
    pub fn with_capacity(spans: usize, total_bytes: usize) -> Self {
        Self {
            data: String::with_capacity(total_bytes),
            offsets: Vec::with_capacity(spans + 1),
            dedup: FxHashMap::default(),
        }
    }

    pub fn intern(&mut self, s: &str) -> u32 {
        let idx = self.offsets.len() as u32;
        self.offsets.push(self.data.len() as u32);
        self.data.push_str(s);
        idx
    }

    /// Intern a string, returning an existing index if an identical string
    /// was already interned. Uses a hash key to avoid per-entry String allocs.
    pub fn intern_dedup(&mut self, s: &str) -> u32 {
        let mut h = rustc_hash::FxHasher::default();
        s.hash(&mut h);
        let key = h.finish();
        if let Some(&idx) = self.dedup.get(&key) {
            // Verify match to handle (rare) hash collisions.
            if self.get(idx) == s {
                return idx;
            }
        }
        let idx = self.intern(s);
        self.dedup.insert(key, idx);
        idx
    }

    #[inline]
    pub fn get(&self, idx: u32) -> &str {
        let start = self.offsets[idx as usize] as usize;
        let end = self
            .offsets
            .get(idx as usize + 1)
            .copied()
            .unwrap_or(self.data.len() as u32) as usize;
        &self.data[start..end]
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    #[inline]
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// Drop the deduplication map to free memory after all interning is done.
    pub fn clear_dedup(&mut self) {
        self.dedup.clear();
        self.dedup.shrink_to_fit();
    }
}

impl From<&[&str]> for StringArena {
    fn from(names: &[&str]) -> Self {
        let total_bytes: usize = names.iter().map(|n| n.len()).sum();
        let mut arena = StringArena::with_capacity(names.len(), total_bytes);
        for name in names {
            arena.intern(name);
        }
        arena
    }
}

/// Zero-copy view into a single chunk row in `CompactManifest`.
#[derive(Clone, Copy)]
pub struct ChunkRef<'a> {
    pub chunk_name: &'a str,
    pub chunk_decompressed_hash_md5: &'a str,
    pub chunk_size: u64,
    pub chunk_size_decompressed: u64,
    pub chunk_compressed_hash_md5: &'a str,
    pub chunk_old_offset: i64,
}

/// Columnar manifest storage.
///
/// Chunks are stored in a single flat array indexed globally. Each file
/// owns a contiguous slice `[file_chunk_start[i]..file_chunk_start[i+1])`.
pub struct CompactManifest {
    arena: StringArena,
    file_name_idx: Vec<u32>,
    file_hash_idx: Vec<u32>,
    file_type: Vec<u8>,
    file_size: Vec<u64>,
    file_chunk_start: Vec<u32>,
    chunk_name_idx: Vec<u32>,
    chunk_decomp_hash_idx: Vec<u32>,
    chunk_comp_hash_idx: Vec<u32>,
    chunk_size: Vec<u32>,
    chunk_size_decompressed: Vec<u32>,
    chunk_old_offset: Vec<i64>,
}

impl CompactManifest {
    pub fn num_files(&self) -> usize {
        self.file_name_idx.len()
    }

    pub fn num_chunks(&self) -> usize {
        self.chunk_name_idx.len()
    }

    #[inline]
    pub fn file_name(&self, file_idx: usize) -> &str {
        self.arena.get(self.file_name_idx[file_idx])
    }

    #[inline]
    pub fn file_hash_md5(&self, file_idx: usize) -> &str {
        self.arena.get(self.file_hash_idx[file_idx])
    }

    #[cfg(test)]
    #[inline]
    pub fn file_type(&self, file_idx: usize) -> u32 {
        self.file_type[file_idx] as u32
    }

    #[inline]
    pub fn file_size(&self, file_idx: usize) -> u64 {
        self.file_size[file_idx]
    }

    #[inline]
    pub fn is_directory(&self, file_idx: usize) -> bool {
        self.file_type[file_idx] != 0 || self.file_hash_md5(file_idx).is_empty()
    }

    #[inline]
    pub fn file_chunk_range(&self, file_idx: usize) -> std::ops::Range<u32> {
        let start = self.file_chunk_start[file_idx];
        let end = self
            .file_chunk_start
            .get(file_idx + 1)
            .copied()
            .unwrap_or(self.chunk_name_idx.len() as u32);
        start..end
    }

    /// Find the file index that owns a given global chunk index via binary
    /// search.
    #[inline]
    pub fn file_idx_for_chunk(&self, chunk_idx: usize) -> usize {
        match self.file_chunk_start.binary_search(&(chunk_idx as u32)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    #[inline]
    pub fn chunk(&self, chunk_idx: usize) -> ChunkRef<'_> {
        ChunkRef {
            chunk_name: self.arena.get(self.chunk_name_idx[chunk_idx]),
            chunk_decompressed_hash_md5: self.arena.get(self.chunk_decomp_hash_idx[chunk_idx]),
            chunk_size: self.chunk_size[chunk_idx] as u64,
            chunk_size_decompressed: self.chunk_size_decompressed[chunk_idx] as u64,
            chunk_compressed_hash_md5: self.arena.get(self.chunk_comp_hash_idx[chunk_idx]),
            chunk_old_offset: self.chunk_old_offset[chunk_idx],
        }
    }

    /// Access a chunk by its `(file_idx, chunk_idx_in_file)` pair.
    #[inline]
    pub fn file_chunk(&self, file_idx: usize, chunk_idx_in_file: usize) -> ChunkRef<'_> {
        let range = self.file_chunk_range(file_idx);
        self.chunk(range.start as usize + chunk_idx_in_file)
    }

    #[inline]
    pub fn chunk_name_arena_idx(&self, chunk_idx: usize) -> u32 {
        self.chunk_name_idx[chunk_idx]
    }

    /// Compute a chunk's offset within its file by summing decompressed sizes
    /// of all preceding chunks in the same file. O(chunks_before) per call.
    #[inline]
    pub fn chunk_file_offset(&self, chunk_idx: usize) -> u64 {
        let file_idx = self.file_idx_for_chunk(chunk_idx);
        let range = self.file_chunk_range(file_idx);
        (range.start as usize..chunk_idx)
            .map(|i| self.chunk_size_decompressed[i] as u64)
            .sum()
    }

    /// Precompute file offsets for all chunks in a file. O(chunks_in_file).
    pub fn file_chunk_offsets(&self, file_idx: usize) -> Vec<u64> {
        let range = self.file_chunk_range(file_idx);
        let mut offsets = Vec::with_capacity((range.end - range.start) as usize);
        let mut acc = 0u64;
        for ci in range.start..range.end {
            offsets.push(acc);
            acc += self.chunk_size_decompressed[ci as usize] as u64;
        }
        offsets
    }

    /// Arena byte size for memory logging.
    pub fn arena_bytes(&self) -> usize {
        self.arena.byte_len() + self.arena.len() * std::mem::size_of::<u32>()
    }

    /// Columnar byte size for memory logging.
    pub fn column_bytes(&self) -> usize {
        let n_files = self.file_name_idx.len();
        let n_chunks = self.chunk_name_idx.len();
        n_files * (4 + 4 + 1 + 8 + 4)
            + n_chunks * (4 + 4 + 4 + 4 + 4 + 8)
            + n_files * std::mem::size_of::<u32>() * 2
            + 6 * std::mem::size_of::<Vec<u8>>()
    }
}

impl From<Vec<SophonManifestAssetProperty>> for CompactManifest {
    fn from(properties: Vec<SophonManifestAssetProperty>) -> Self {
        let total_files = properties.len();
        let total_chunks: usize = properties.iter().map(|f| f.asset_chunks.len()).sum();
        let estimated_string_bytes: usize = properties
            .iter()
            .map(|f| {
                f.asset_name.len()
                    + f.asset_hash_md5.len()
                    + f.asset_chunks
                        .iter()
                        .map(|c| c.chunk_name.len())
                        .sum::<usize>()
                    + f.asset_chunks
                        .iter()
                        .map(|c| c.chunk_decompressed_hash_md5.len())
                        .sum::<usize>()
                    + f.asset_chunks
                        .iter()
                        .map(|c| c.chunk_compressed_hash_md5.len())
                        .sum::<usize>()
            })
            .sum();

        let mut arena =
            StringArena::with_capacity(total_files * 2 + total_chunks * 3, estimated_string_bytes);
        let mut file_name_idx = Vec::with_capacity(total_files);
        let mut file_hash_idx = Vec::with_capacity(total_files);
        let mut file_type = Vec::with_capacity(total_files);
        let mut file_size = Vec::with_capacity(total_files);
        let mut file_chunk_start = Vec::with_capacity(total_files + 1);

        let mut chunk_name_idx = Vec::with_capacity(total_chunks);
        let mut chunk_decomp_hash_idx = Vec::with_capacity(total_chunks);
        let mut chunk_comp_hash_idx = Vec::with_capacity(total_chunks);
        let mut chunk_size = Vec::with_capacity(total_chunks);
        let mut chunk_size_decompressed = Vec::with_capacity(total_chunks);
        let mut chunk_old_offset = Vec::with_capacity(total_chunks);

        for file in &properties {
            file_name_idx.push(arena.intern(&file.asset_name));
            file_hash_idx.push(arena.intern_dedup(&file.asset_hash_md5));
            file_type.push(file.asset_type as u8);
            file_size.push(file.asset_size);
            file_chunk_start.push(chunk_name_idx.len() as u32);
            for chunk in &file.asset_chunks {
                chunk_name_idx.push(arena.intern(&chunk.chunk_name));
                chunk_decomp_hash_idx.push(arena.intern_dedup(&chunk.chunk_decompressed_hash_md5));
                chunk_comp_hash_idx.push(arena.intern_dedup(&chunk.chunk_compressed_hash_md5));
                chunk_size.push(chunk.chunk_size as u32);
                chunk_size_decompressed.push(chunk.chunk_size_decompressed as u32);
                chunk_old_offset.push(chunk.chunk_old_offset);
            }
        }

        arena.clear_dedup();

        Self {
            arena,
            file_name_idx,
            file_hash_idx,
            file_type,
            file_size,
            file_chunk_start,
            chunk_name_idx,
            chunk_decomp_hash_idx,
            chunk_comp_hash_idx,
            chunk_size,
            chunk_size_decompressed,
            chunk_old_offset,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto_parse::SophonManifestAssetChunk;

    fn make_chunk(
        name: &str,
        chunk_on_file_offset: u64,
        size: u64,
        decompressed: u64,
    ) -> SophonManifestAssetChunk {
        SophonManifestAssetChunk {
            chunk_name: name.into(),
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset,
            chunk_size: size,
            chunk_size_decompressed: decompressed,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        }
    }

    fn make_file(
        name: &str,
        md5: &str,
        asset_type: u32,
        asset_size: u64,
        chunks: Vec<SophonManifestAssetChunk>,
    ) -> SophonManifestAssetProperty {
        SophonManifestAssetProperty {
            asset_name: name.into(),
            asset_chunks: chunks,
            asset_type,
            asset_size,
            asset_hash_md5: md5.into(),
        }
    }

    #[test]
    fn empty_manifest_has_zero_files_and_chunks() {
        let cm = CompactManifest::from(vec![]);
        assert_eq!(cm.num_files(), 0);
        assert_eq!(cm.num_chunks(), 0);
    }

    #[test]
    fn single_file_no_chunks() {
        let file = make_file("a.pak", "d41d8cd98f00b204e9800998ecf8427e", 0, 1024, vec![]);
        let cm = CompactManifest::from(vec![file]);
        assert_eq!(cm.num_files(), 1);
        assert_eq!(cm.num_chunks(), 0);
        assert_eq!(cm.file_name(0), "a.pak");
        assert_eq!(cm.file_hash_md5(0), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(cm.file_size(0), 1024);
        assert!(!cm.is_directory(0));
        assert_eq!(cm.file_chunk_range(0), 0..0);
    }

    #[test]
    fn directory_detected_by_empty_hash() {
        let dir = make_file("GameData", "", 0, 0, vec![]);
        let cm = CompactManifest::from(vec![dir]);
        assert!(cm.is_directory(0));
    }

    #[test]
    fn directory_detected_by_nonzero_type() {
        let dir = make_file("GameData", "abc", 64, 0, vec![]);
        let cm = CompactManifest::from(vec![dir]);
        assert!(cm.is_directory(0));
    }

    #[test]
    fn multiple_files_with_chunks_have_correct_ranges() {
        let f1 = make_file(
            "a.pak",
            "h1",
            0,
            300,
            vec![
                make_chunk("c1", 0, 100, 100),
                make_chunk("c2", 100, 100, 200),
            ],
        );
        let f2 = make_file("b.pak", "h2", 0, 500, vec![make_chunk("c3", 0, 500, 500)]);
        let cm = CompactManifest::from(vec![f1, f2]);
        assert_eq!(cm.num_files(), 2);
        assert_eq!(cm.num_chunks(), 3);
        assert_eq!(cm.file_chunk_range(0), 0..2);
        assert_eq!(cm.file_chunk_range(1), 2..3);
    }

    #[test]
    fn file_chunk_accessor_returns_correct_data() {
        let f1 = make_file(
            "a.pak",
            "h1",
            0,
            300,
            vec![
                make_chunk("c1", 0, 100, 200),
                make_chunk("c2", 200, 100, 300),
            ],
        );
        let cm = CompactManifest::from(vec![f1]);
        let c = cm.file_chunk(0, 0);
        assert_eq!(c.chunk_name, "c1");
        assert_eq!(c.chunk_size, 100);
        assert_eq!(c.chunk_size_decompressed, 200);
        assert_eq!(cm.chunk_file_offset(0), 0);
        let c = cm.file_chunk(0, 1);
        assert_eq!(c.chunk_name, "c2");
        assert_eq!(cm.chunk_file_offset(1), 200);
    }

    #[test]
    fn chunk_compressed_hash_md5_preserved() {
        let mut chunk = make_chunk("c1", 0, 100, 100);
        chunk.chunk_compressed_hash_md5 = "abcdef1234567890abcdef1234567890".into();
        chunk.chunk_decompressed_hash_md5 = "00000000000000000000000000000000".into();
        let f = make_file("a.pak", "h", 0, 100, vec![chunk]);
        let cm = CompactManifest::from(vec![f]);
        let c = cm.chunk(0);
        assert_eq!(
            c.chunk_compressed_hash_md5,
            "abcdef1234567890abcdef1234567890"
        );
        assert_eq!(
            c.chunk_decompressed_hash_md5,
            "00000000000000000000000000000000"
        );
    }

    #[test]
    fn chunk_old_offset_preserved() {
        let mut chunk = make_chunk("c1", 0, 100, 100);
        chunk.chunk_old_offset = 42;
        let f = make_file("a.pak", "h", 0, 100, vec![chunk]);
        let cm = CompactManifest::from(vec![f]);
        assert_eq!(cm.chunk(0).chunk_old_offset, 42);
    }

    #[test]
    fn arena_stores_all_strings() {
        let f1 = make_file("a.pak", "shared", 0, 100, vec![]);
        let f2 = make_file("b.pak", "shared", 0, 200, vec![]);
        let cm = CompactManifest::from(vec![f1, f2]);
        assert_eq!(cm.arena.len(), 3);
    }

    /// Verifies all stored fields survive Proto -> CompactManifest for 100 assets
    /// with varied chunk counts. Guards against columnar layout changes dropping data.
    #[test]
    fn roundtrip_all_fields_100_assets() {
        let assets: Vec<SophonManifestAssetProperty> = (0..100)
            .map(|f| {
                let chunk_count = 1 + (f % 50);
                let chunks: Vec<SophonManifestAssetChunk> = (0..chunk_count)
                    .map(|c| SophonManifestAssetChunk {
                        chunk_name: format!("chunk_f{f}_c{c}"),
                        chunk_decompressed_hash_md5: format!("{:032x}", f * 1000 + c),
                        chunk_on_file_offset: (c as u64) * 4096,
                        chunk_size: 2048 + c as u64,
                        chunk_size_decompressed: 4096 + c as u64,
                        chunk_compressed_hash_xxh: (f * c) as u64,
                        chunk_compressed_hash_md5: format!("{:032x}", f * 1000 + c + 1),
                        chunk_old_offset: if c % 3 == 0 { -1 } else { (c * 4096) as i64 },
                    })
                    .collect();
                SophonManifestAssetProperty {
                    asset_name: format!("data/bundle_{f:04}.pak"),
                    asset_chunks: chunks,
                    asset_type: if f % 20 == 0 { 64 } else { 0 },
                    asset_size: (chunk_count as u64) * 4096,
                    asset_hash_md5: format!("{f:032x}"),
                }
            })
            .collect();

        let total_chunks: usize = assets.iter().map(|a| a.asset_chunks.len()).sum();
        let cm = CompactManifest::from(assets.clone());

        assert_eq!(cm.num_files(), assets.len());
        assert_eq!(cm.num_chunks(), total_chunks);

        let mut global_chunk_idx = 0usize;
        for (fi, asset) in assets.iter().enumerate() {
            assert_eq!(cm.file_name(fi), asset.asset_name);
            assert_eq!(cm.file_hash_md5(fi), asset.asset_hash_md5);
            assert_eq!(cm.file_type(fi), asset.asset_type);
            assert_eq!(cm.file_size(fi), asset.asset_size);

            let range = cm.file_chunk_range(fi);
            assert_eq!((range.end - range.start) as usize, asset.asset_chunks.len());

            for (ci, src_chunk) in asset.asset_chunks.iter().enumerate() {
                let c = cm.file_chunk(fi, ci);
                assert_eq!(c.chunk_name, src_chunk.chunk_name);
                assert_eq!(
                    c.chunk_decompressed_hash_md5,
                    src_chunk.chunk_decompressed_hash_md5
                );
                assert_eq!(c.chunk_size, src_chunk.chunk_size);
                assert_eq!(c.chunk_size_decompressed, src_chunk.chunk_size_decompressed);
                assert_eq!(
                    c.chunk_compressed_hash_md5,
                    src_chunk.chunk_compressed_hash_md5
                );
                assert_eq!(c.chunk_old_offset, src_chunk.chunk_old_offset);
                global_chunk_idx += 1;
            }
        }
        assert_eq!(global_chunk_idx, total_chunks);
    }

    /// Documents that chunk_size and chunk_size_decompressed are truncated to
    /// u32 during CompactManifest construction. If someone widens the column
    /// without updating the cast, this test catches the change.
    #[test]
    fn truncation_chunk_size_to_u32() {
        let big_value = (u32::MAX as u64) + 1;
        let chunk = SophonManifestAssetChunk {
            chunk_name: "big".into(),
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: 0,
            chunk_size: big_value,
            chunk_size_decompressed: big_value,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        };
        let f = make_file("a.pak", "h", 0, big_value, vec![chunk]);
        let cm = CompactManifest::from(vec![f]);
        let c = cm.chunk(0);
        // Truncation wraps: (u32::MAX + 1) as u32 == 0
        assert_eq!(c.chunk_size, 0);
        assert_eq!(c.chunk_size_decompressed, 0);
    }

    /// Documents that file_type is truncated to u8. Values > 255 wrap.
    #[test]
    fn truncation_file_type_to_u8() {
        let f = make_file("dir.pak", "h", 256, 0, vec![]);
        let cm = CompactManifest::from(vec![f]);
        // 256u32 as u8 == 0
        assert_eq!(cm.file_type(0), 0);
    }

    /// Verifies file_idx_for_chunk resolves correctly across multiple files.
    #[test]
    fn file_idx_for_chunk_multi_file() {
        let f0 = make_file(
            "a.pak",
            "h1",
            0,
            200,
            vec![
                make_chunk("c0", 0, 100, 100),
                make_chunk("c1", 100, 100, 100),
            ],
        );
        let f1 = make_file(
            "b.pak",
            "h2",
            0,
            300,
            vec![
                make_chunk("c2", 0, 100, 100),
                make_chunk("c3", 100, 100, 100),
                make_chunk("c4", 200, 100, 100),
            ],
        );
        let f2 = make_file("c.pak", "h3", 0, 100, vec![make_chunk("c5", 0, 100, 100)]);
        let cm = CompactManifest::from(vec![f0, f1, f2]);

        assert_eq!(cm.file_idx_for_chunk(0), 0);
        assert_eq!(cm.file_idx_for_chunk(1), 0);
        assert_eq!(cm.file_idx_for_chunk(2), 1);
        assert_eq!(cm.file_idx_for_chunk(3), 1);
        assert_eq!(cm.file_idx_for_chunk(4), 1);
        assert_eq!(cm.file_idx_for_chunk(5), 2);
    }

    /// Verifies chunk_file_offset computes prefix sums correctly across files
    /// with varying chunk sizes.
    #[test]
    fn chunk_file_offset_multi_file_prefix_sum() {
        let f0 = make_file(
            "a.pak",
            "h1",
            0,
            700,
            vec![
                make_chunk("c0", 0, 100, 200),
                make_chunk("c1", 200, 100, 500),
            ],
        );
        let f1 = make_file(
            "b.pak",
            "h2",
            0,
            900,
            vec![
                make_chunk("c2", 0, 100, 300),
                make_chunk("c3", 300, 100, 400),
                make_chunk("c4", 700, 100, 200),
            ],
        );
        let cm = CompactManifest::from(vec![f0, f1]);

        // File 0: chunk 0 at offset 0, chunk 1 at offset 200
        assert_eq!(cm.chunk_file_offset(0), 0);
        assert_eq!(cm.chunk_file_offset(1), 200);
        // File 1: chunk 2 at offset 0, chunk 3 at offset 300, chunk 4 at offset 700
        assert_eq!(cm.chunk_file_offset(2), 0);
        assert_eq!(cm.chunk_file_offset(3), 300);
        assert_eq!(cm.chunk_file_offset(4), 700);
    }
}
