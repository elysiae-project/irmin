//! Columnar manifest storage.
//!
//! Replaces the protobuf `Vec<SophonManifestAssetProperty>` representation
//! with per-field `Vec` columns and a shared string arena. This eliminates
//! per-string and per-Vec heap allocation overhead and improves jemalloc
//! fragmentation by co-locating fields in contiguous allocations.

use crate::proto_parse::SophonManifestAssetProperty;
use rustc_hash::FxHashMap;
use std::hash::{Hash, Hasher};

const INITIAL_STRING_INDEX_CAPACITY: usize = 1 << 20;
const INITIAL_STRING_ARENA_CAPACITY: usize = 16 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
#[error("manifest exceeds compact u32 index limits: {0}")]
pub struct CompactManifestLimitError(&'static str);

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
    /// Allocate an arena sized for approximately `spans` strings and
    /// `total_bytes` UTF-8 bytes; capacities may grow if estimates are exceeded.
    pub fn with_capacity(spans: usize, total_bytes: usize) -> Self {
        let spans = spans
            .min(u32::MAX as usize - 1)
            .min(INITIAL_STRING_INDEX_CAPACITY);
        let total_bytes = total_bytes
            .min(u32::MAX as usize)
            .min(INITIAL_STRING_ARENA_CAPACITY);
        Self {
            data: String::with_capacity(total_bytes),
            offsets: Vec::with_capacity(spans + 1),
            dedup: FxHashMap::with_capacity_and_hasher(spans / 2, Default::default()),
        }
    }

    /// Append `s` and return its zero-based index. Duplicate values are stored
    /// separately; indices are valid only while this arena's ordering is stable.
    pub fn intern(&mut self, s: &str) -> u32 {
        self.try_intern(s)
            .expect("string exceeds compact u32 index limits")
    }

    pub fn try_intern(&mut self, s: &str) -> Result<u32, CompactManifestLimitError> {
        let index = u32::try_from(self.offsets.len())
            .map_err(|_| CompactManifestLimitError("string index count"))?;
        let offset = u32::try_from(self.data.len())
            .map_err(|_| CompactManifestLimitError("string byte offset"))?;
        let max_string_len = (u32::MAX as usize).min(isize::MAX as usize);
        self.data
            .len()
            .checked_add(s.len())
            .filter(|&length| length <= max_string_len)
            .ok_or(CompactManifestLimitError("string byte length"))?;
        // Reserve fallibly only when capacity is insufficient, avoiding the
        // per-call try_reserve overhead for the common pre-allocated case.
        if self.data.capacity() < self.data.len() + s.len() {
            self.data
                .try_reserve(s.len())
                .map_err(|_| CompactManifestLimitError("string allocation failed"))?;
        }
        if self.offsets.capacity() == self.offsets.len() {
            self.offsets
                .try_reserve(1)
                .map_err(|_| CompactManifestLimitError("string index allocation failed"))?;
        }
        self.offsets.push(offset);
        self.data.push_str(s);
        Ok(index)
    }

    /// Intern a string, returning an existing index if an identical string
    /// was already interned. Uses a hash key to avoid per-entry String allocs.
    /// Intern `s`, reusing an identical existing value when present. Hash
    /// collisions are checked against the stored string before reuse.
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

    /// Resolve an index returned by an interning operation.
    ///
    /// Panics when `idx` is outside the arena; returned text borrows the arena.
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

    /// Number of strings interned, including duplicates.
    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    /// UTF-8 byte length of all concatenated strings, excluding offsets.
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    /// Drop the deduplication map to free memory after all interning is done.
    /// Release the temporary hash map used by `intern_dedup`; arena contents remain.
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

/// Binary blob arena with deduplication, mirroring `StringArena` for `&[u8]`.
/// Stores MD5/xxh64 hashes as binary (16 or 8 bytes) instead of hex (32 or 16),
/// halving the memory for hash strings.
#[derive(Default)]
pub struct ByteArena {
    data: Vec<u8>,
    offsets: Vec<u32>,
    dedup: FxHashMap<u64, u32>,
}

impl ByteArena {
    pub fn with_capacity(entries: usize, total_bytes: usize) -> Self {
        let entries = entries
            .min(u32::MAX as usize - 1)
            .min(INITIAL_STRING_INDEX_CAPACITY);
        let total_bytes = total_bytes
            .min(u32::MAX as usize)
            .min(INITIAL_STRING_ARENA_CAPACITY);
        Self {
            data: Vec::with_capacity(total_bytes),
            offsets: Vec::with_capacity(entries + 1),
            dedup: FxHashMap::with_capacity_and_hasher(entries / 2, Default::default()),
        }
    }

    /// Intern binary data, returning an existing index if identical data was
    /// already stored. Handles hash collisions by comparing the stored bytes.
    pub fn intern_dedup(&mut self, bytes: &[u8]) -> u32 {
        let mut h = rustc_hash::FxHasher::default();
        bytes.hash(&mut h);
        let key = h.finish();
        if let Some(&idx) = self.dedup.get(&key)
            && self.get(idx) == bytes
        {
            return idx;
        }
        let idx = self.try_intern(bytes).expect("byte arena overflow");
        self.dedup.insert(key, idx);
        idx
    }

    fn try_intern(&mut self, bytes: &[u8]) -> Result<u32, CompactManifestLimitError> {
        let index = u32::try_from(self.offsets.len())
            .map_err(|_| CompactManifestLimitError("byte index count"))?;
        let offset =
            u32::try_from(self.data.len()).map_err(|_| CompactManifestLimitError("byte offset"))?;
        self.data
            .len()
            .checked_add(bytes.len())
            .filter(|&l| l <= (u32::MAX as usize).min(isize::MAX as usize))
            .ok_or(CompactManifestLimitError("byte length"))?;
        if self.data.capacity() < self.data.len() + bytes.len() {
            self.data
                .try_reserve(bytes.len())
                .map_err(|_| CompactManifestLimitError("byte allocation failed"))?;
        }
        if self.offsets.capacity() == self.offsets.len() {
            self.offsets
                .try_reserve(1)
                .map_err(|_| CompactManifestLimitError("byte index allocation failed"))?;
        }
        self.offsets.push(offset);
        self.data.extend_from_slice(bytes);
        Ok(index)
    }

    /// Resolve an index. Panics when `idx` is out of range.
    #[inline]
    pub fn get(&self, idx: u32) -> &[u8] {
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

    pub fn is_empty(&self) -> bool {
        self.offsets.is_empty()
    }

    #[inline]
    pub fn byte_len(&self) -> usize {
        self.data.len()
    }

    pub fn clear_dedup(&mut self) {
        self.dedup.clear();
        self.dedup.shrink_to_fit();
    }
}

/// Zero-copy view into a single chunk row in `CompactManifest`.
#[derive(Clone, Copy)]
/// Borrowed chunk metadata row returned by [`CompactManifest`] accessors.
pub struct ChunkRef<'a> {
    pub chunk_name: &'a str,
    pub chunk_decompressed_hash_md5: &'a [u8],
    pub chunk_size: u64,
    pub chunk_size_decompressed: u64,
    pub chunk_compressed_hash_md5: &'a [u8],
    pub chunk_old_offset: i64,
}

/// Columnar manifest storage.
///
/// Chunks are stored in a single flat array indexed globally. Each file
/// owns a contiguous slice `[file_chunk_start[i]..file_chunk_start[i+1])`.
/// Compact columnar representation of a decoded manifest.
///
/// File and chunk indices are zero-based; accessor indices are expected to be
/// in range and generally panic when out of bounds. Chunk rows are contiguous
/// per file, allowing each file's chunk range to be represented compactly.
pub struct CompactManifest {
    arena: StringArena,
    hash_arena: ByteArena,
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
    /// Number of asset records.
    #[inline]
    pub fn num_files(&self) -> usize {
        self.file_name_idx.len()
    }

    /// Number of chunk records across all assets.
    #[inline]
    pub fn num_chunks(&self) -> usize {
        self.chunk_name_idx.len()
    }

    #[inline]
    /// Asset path for `file_idx`.
    pub fn file_name(&self, file_idx: usize) -> &str {
        self.arena.get(self.file_name_idx[file_idx])
    }

    /// Expected assembled-file MD5 for `file_idx`.
    #[inline]
    pub fn file_hash_md5(&self, file_idx: usize) -> &[u8] {
        self.hash_arena.get(self.file_hash_idx[file_idx])
    }

    #[cfg(test)]
    #[inline]
    /// Raw asset type code for `file_idx` (available in test builds).
    pub fn file_type(&self, file_idx: usize) -> u32 {
        self.file_type[file_idx] as u32
    }

    #[inline]
    /// Expected assembled byte length for `file_idx`.
    pub fn file_size(&self, file_idx: usize) -> u64 {
        self.file_size[file_idx]
    }

    /// Whether the asset is classified as a directory.
    #[inline(always)]
    pub fn is_directory(&self, file_idx: usize) -> bool {
        self.file_type[file_idx] != 0 || self.file_hash_md5(file_idx).is_empty()
    }

    #[inline]
    /// Global half-open chunk-index range belonging to `file_idx`.
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
    /// Owning file index for a global chunk index; caller must pass a valid index.
    pub fn file_idx_for_chunk(&self, chunk_idx: usize) -> usize {
        match self.file_chunk_start.binary_search(&(chunk_idx as u32)) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        }
    }

    /// Borrowed metadata for a global chunk row.
    #[inline(always)]
    pub fn chunk(&self, chunk_idx: usize) -> ChunkRef<'_> {
        ChunkRef {
            chunk_name: self.arena.get(self.chunk_name_idx[chunk_idx]),
            chunk_decompressed_hash_md5: self.hash_arena.get(self.chunk_decomp_hash_idx[chunk_idx]),
            chunk_size: self.chunk_size[chunk_idx] as u64,
            chunk_size_decompressed: self.chunk_size_decompressed[chunk_idx] as u64,
            chunk_compressed_hash_md5: self.hash_arena.get(self.chunk_comp_hash_idx[chunk_idx]),
            chunk_old_offset: self.chunk_old_offset[chunk_idx],
        }
    }

    /// Access a chunk by its `(file_idx, chunk_idx_in_file)` pair.
    #[inline]
    /// Borrowed metadata for a file-local chunk index; indices are not bounds-checked.
    pub fn file_chunk(&self, file_idx: usize, chunk_idx_in_file: usize) -> ChunkRef<'_> {
        let range = self.file_chunk_range(file_idx);
        self.chunk(range.start as usize + chunk_idx_in_file)
    }

    #[inline]
    /// String-arena index of a chunk's CDN object name.
    pub fn chunk_name_arena_idx(&self, chunk_idx: usize) -> u32 {
        self.chunk_name_idx[chunk_idx]
    }

    /// Resolves a raw arena index (as returned by `chunk_name_arena_idx` or
    /// `file_chunk_name_id_at`) to its borrowed string.
    #[inline]
    pub fn str_at(&self, arena_id: u32) -> &str {
        self.arena.get(arena_id)
    }

    /// Returns the byte range `[start, start+len)` of the string at `arena_id`
    /// within the manifest arena data, for callers that store offsets inline.
    #[inline]
    pub(crate) fn str_offsets_at(&self, arena_id: u32) -> (u32, u32) {
        let idx = arena_id as usize;
        let start = self.arena.offsets[idx];
        let end = if idx + 1 < self.arena.offsets.len() {
            self.arena.offsets[idx + 1]
        } else {
            self.arena.data.len() as u32
        };
        debug_assert!(end >= start, "arena offsets must be monotonic");
        (start, end - start)
    }

    /// Returns the arena's contiguous string storage as a `&str`. Callers that
    /// cache offsets via `str_offsets_at` can slice this without re-validating
    /// UTF-8 (the arena is a `String`, valid UTF-8 by construction).
    #[inline]
    pub(crate) fn arena_str(&self) -> &str {
        &self.arena.data
    }

    /// String-arena index of a file-local chunk's CDN object name, without
    /// constructing a `ChunkRef` or resolving the string.
    #[inline]
    pub fn file_chunk_name_id_at(&self, file_idx: usize, chunk_idx_in_file: usize) -> u32 {
        let start = self.file_chunk_start[file_idx];
        self.chunk_name_idx[start as usize + chunk_idx_in_file]
    }

    /// Returns a chunk name by (file_idx, chunk_idx_in_file) without constructing a `ChunkRef`.
    #[inline]
    pub fn file_chunk_name_at(&self, file_idx: usize, chunk_idx_in_file: usize) -> &str {
        let range = self.file_chunk_range(file_idx);
        self.chunk_name_at(range.start as usize + chunk_idx_in_file)
    }

    /// Returns the chunk's CDN object name.
    #[inline]
    pub fn chunk_name_at(&self, chunk_idx: usize) -> &str {
        self.arena.get(self.chunk_name_idx[chunk_idx])
    }

    /// Returns the chunk's old offset; negative when the chunk is a pure download.
    #[inline]
    pub fn chunk_old_offset_at(&self, chunk_idx: usize) -> i64 {
        self.chunk_old_offset[chunk_idx]
    }

    /// Returns the chunk's compressed size.
    #[inline]
    pub fn chunk_size_at(&self, chunk_idx: usize) -> u64 {
        self.chunk_size[chunk_idx] as u64
    }

    /// Returns the chunk's decompressed size.
    #[inline]
    pub fn chunk_size_decompressed_at(&self, chunk_idx: usize) -> u64 {
        self.chunk_size_decompressed[chunk_idx] as u64
    }

    /// Returns the chunk's compressed hash (binary).
    #[inline]
    pub fn chunk_compressed_hash_at(&self, chunk_idx: usize) -> &[u8] {
        self.hash_arena.get(self.chunk_comp_hash_idx[chunk_idx])
    }

    /// Returns the chunk's decompressed hash (binary).
    #[inline]
    pub fn chunk_decompressed_hash_at(&self, chunk_idx: usize) -> &[u8] {
        self.hash_arena.get(self.chunk_decomp_hash_idx[chunk_idx])
    }

    /// Compute a chunk's offset within its file by summing decompressed sizes
    /// of all preceding chunks in the same file. O(chunks_before) per call.
    #[inline]
    /// Byte offset within the owning file, summing preceding decompressed chunk sizes.
    pub fn chunk_file_offset(&self, chunk_idx: usize) -> u64 {
        let file_idx = self.file_idx_for_chunk(chunk_idx);
        let range = self.file_chunk_range(file_idx);
        (range.start as usize..chunk_idx)
            .map(|i| self.chunk_size_decompressed[i] as u64)
            .sum()
    }

    /// Compute the byte offset of a single chunk within its file. O(chunk_idx).
    #[inline]
    /// Byte offset of the file-local chunk, summing preceding decompressed sizes.
    pub fn file_chunk_offset(&self, file_idx: usize, chunk_idx: usize) -> u64 {
        let range = self.file_chunk_range(file_idx);
        (range.start as usize..range.start as usize + chunk_idx)
            .map(|i| self.chunk_size_decompressed[i] as u64)
            .sum()
    }

    /// Precompute file offsets for all chunks in a file. O(chunks_in_file).
    /// Offsets of all chunks in the file, in file order.
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
    /// Approximate arena storage bytes, including its u32 offsets.
    pub fn arena_bytes(&self) -> usize {
        self.arena.byte_len() + self.arena.len() * std::mem::size_of::<u32>()
    }

    /// Columnar byte size for memory logging.
    /// Approximate memory occupied by column allocations, excluding allocator overhead.
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
        #[allow(clippy::expect_used)]
        Self::try_from_properties(properties).expect("manifest exceeds compact u32 index limits")
    }
}

impl CompactManifest {
    pub fn try_from_properties(
        mut properties: Vec<SophonManifestAssetProperty>,
    ) -> Result<Self, CompactManifestLimitError> {
        let mut builder = CompactManifestBuilder::for_properties(&properties)?;
        for file in properties.drain(..) {
            builder.push(&file)?;
        }
        builder.finish()
    }

    /// Builds a compact manifest containing one borrowed asset property.
    pub(crate) fn try_from_property_ref(
        property: &SophonManifestAssetProperty,
    ) -> Result<Self, CompactManifestLimitError> {
        let mut builder = CompactManifestBuilder::for_properties(std::slice::from_ref(property))?;
        builder.push(property)?;
        builder.finish()
    }

    /// Wrap a bare `StringArena` as a minimal manifest whose only populated
    /// column is the arena. Only `arena_str`/`str_offsets_at` are valid on the
    /// result; all file/chunk accessors panic on the empty columns.
    pub(crate) fn wrap_arena(arena: StringArena) -> Self {
        Self {
            arena,
            hash_arena: ByteArena::default(),
            file_name_idx: Vec::new(),
            file_hash_idx: Vec::new(),
            file_type: Vec::new(),
            file_size: Vec::new(),
            file_chunk_start: Vec::new(),
            chunk_name_idx: Vec::new(),
            chunk_decomp_hash_idx: Vec::new(),
            chunk_comp_hash_idx: Vec::new(),
            chunk_size: Vec::new(),
            chunk_size_decompressed: Vec::new(),
            chunk_old_offset: Vec::new(),
        }
    }
}

struct CompactManifestBuilder {
    arena: StringArena,
    hash_arena: ByteArena,
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

impl CompactManifestBuilder {
    fn for_properties(
        properties: &[SophonManifestAssetProperty],
    ) -> Result<Self, CompactManifestLimitError> {
        let total_files = properties.len();
        let (total_chunks, estimated_string_bytes) = manifest_storage_estimates(properties).ok_or(
            CompactManifestLimitError("manifest size arithmetic overflow"),
        )?;
        let span_count = validate_limits(total_files, total_chunks)?;
        validate_allocation_sizes(
            total_files,
            total_chunks,
            span_count,
            estimated_string_bytes,
        )?;
        Ok(Self::with_capacity(
            total_files,
            total_chunks,
            span_count,
            estimated_string_bytes,
        ))
    }

    fn with_capacity(
        total_files: usize,
        total_chunks: usize,
        span_count: usize,
        estimated_string_bytes: usize,
    ) -> Self {
        let estimated_hash_bytes = estimated_string_bytes / 2;
        Self {
            arena: StringArena::with_capacity(span_count, estimated_string_bytes),
            hash_arena: ByteArena::with_capacity(span_count, estimated_hash_bytes),
            file_name_idx: Vec::with_capacity(total_files),
            file_hash_idx: Vec::with_capacity(total_files),
            file_type: Vec::with_capacity(total_files),
            file_size: Vec::with_capacity(total_files),
            file_chunk_start: Vec::with_capacity(total_files + 1),
            chunk_name_idx: Vec::with_capacity(total_chunks),
            chunk_decomp_hash_idx: Vec::with_capacity(total_chunks),
            chunk_comp_hash_idx: Vec::with_capacity(total_chunks),
            chunk_size: Vec::with_capacity(total_chunks),
            chunk_size_decompressed: Vec::with_capacity(total_chunks),
            chunk_old_offset: Vec::with_capacity(total_chunks),
        }
    }

    fn push(
        &mut self,
        file: &SophonManifestAssetProperty,
    ) -> Result<(), CompactManifestLimitError> {
        self.file_name_idx.push(checked_intern(
            &mut self.arena,
            file.asset_name_str(),
            false,
        )?);
        self.file_hash_idx.push(intern_hash(
            &mut self.hash_arena,
            file.asset_hash_md5_str(),
        )?);
        self.file_type.push(file.asset_type as u8);
        self.file_size.push(file.asset_size);
        self.file_chunk_start.push(
            u32::try_from(self.chunk_name_idx.len())
                .map_err(|_| CompactManifestLimitError("chunk index count"))?,
        );
        for chunk in &file.asset_chunks {
            self.chunk_name_idx.push(checked_intern(
                &mut self.arena,
                chunk.chunk_name_str(),
                false,
            )?);
            self.chunk_decomp_hash_idx.push(intern_hash(
                &mut self.hash_arena,
                chunk.chunk_decompressed_hash_md5_str(),
            )?);
            self.chunk_comp_hash_idx.push(intern_hash(
                &mut self.hash_arena,
                chunk.chunk_compressed_hash_md5_str(),
            )?);
            self.chunk_size.push(chunk.chunk_size as u32);
            self.chunk_size_decompressed
                .push(chunk.chunk_size_decompressed as u32);
            self.chunk_old_offset.push(chunk.chunk_old_offset);
        }
        Ok(())
    }

    fn finish(mut self) -> Result<CompactManifest, CompactManifestLimitError> {
        self.file_chunk_start.push(
            u32::try_from(self.chunk_name_idx.len())
                .map_err(|_| CompactManifestLimitError("chunk index sentinel"))?,
        );
        self.arena.clear_dedup();
        self.hash_arena.clear_dedup();
        Ok(CompactManifest {
            arena: self.arena,
            hash_arena: self.hash_arena,
            file_name_idx: self.file_name_idx,
            file_hash_idx: self.file_hash_idx,
            file_type: self.file_type,
            file_size: self.file_size,
            file_chunk_start: self.file_chunk_start,
            chunk_name_idx: self.chunk_name_idx,
            chunk_decomp_hash_idx: self.chunk_decomp_hash_idx,
            chunk_comp_hash_idx: self.chunk_comp_hash_idx,
            chunk_size: self.chunk_size,
            chunk_size_decompressed: self.chunk_size_decompressed,
            chunk_old_offset: self.chunk_old_offset,
        })
    }
}

fn validate_limits(
    total_files: usize,
    total_chunks: usize,
) -> Result<usize, CompactManifestLimitError> {
    let span_count = total_files
        .checked_mul(2)
        .and_then(|files| {
            total_chunks
                .checked_mul(3)
                .and_then(|chunks| files.checked_add(chunks))
        })
        .ok_or(CompactManifestLimitError("string entry count overflow"))?;
    if span_count >= u32::MAX as usize
        || total_files >= u32::MAX as usize
        || total_chunks >= u32::MAX as usize
    {
        return Err(CompactManifestLimitError("file or string index count"));
    }
    total_files
        .checked_add(1)
        .ok_or(CompactManifestLimitError("file index sentinel overflow"))?;
    Ok(span_count)
}

fn validate_allocation_sizes(
    total_files: usize,
    total_chunks: usize,
    span_count: usize,
    estimated_string_bytes: usize,
) -> Result<(), CompactManifestLimitError> {
    let max_capacity = isize::MAX as usize;
    let file_capacity = total_files
        .checked_add(1)
        .ok_or(CompactManifestLimitError("file index capacity overflow"))?;
    let arena_index_capacity = span_count
        .checked_add(1)
        .ok_or(CompactManifestLimitError("string index capacity overflow"))?;
    let capacities = [
        (file_capacity, std::mem::size_of::<u32>()),
        (arena_index_capacity, std::mem::size_of::<u32>()),
        (total_chunks, std::mem::size_of::<u32>()),
        (total_chunks, std::mem::size_of::<u32>()),
        (total_chunks, std::mem::size_of::<u32>()),
        (total_chunks, std::mem::size_of::<u32>()),
        (total_chunks, std::mem::size_of::<u32>()),
        (total_chunks, std::mem::size_of::<i64>()),
        (total_files, std::mem::size_of::<u32>()),
        (total_files, std::mem::size_of::<u32>()),
        (total_files, std::mem::size_of::<u8>()),
        (total_files, std::mem::size_of::<u64>()),
    ];
    let arena_bytes = estimated_string_bytes.min(INITIAL_STRING_ARENA_CAPACITY);
    for (count, element_size) in capacities {
        let bytes = count
            .checked_mul(element_size)
            .ok_or(CompactManifestLimitError("allocation size overflow"))?;
        if bytes > max_capacity {
            return Err(CompactManifestLimitError(
                "allocation capacity exceeds isize::MAX",
            ));
        }
    }
    if arena_bytes > max_capacity {
        return Err(CompactManifestLimitError(
            "allocation capacity exceeds isize::MAX",
        ));
    }
    Ok(())
}

#[inline]
fn checked_intern(
    arena: &mut StringArena,
    value: &str,
    deduplicate: bool,
) -> Result<u32, CompactManifestLimitError> {
    if deduplicate {
        let mut hasher = rustc_hash::FxHasher::default();
        value.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(&index) = arena.dedup.get(&key)
            && arena.get(index) == value
        {
            return Ok(index);
        }
        if arena.dedup.len() == arena.dedup.capacity() {
            arena
                .dedup
                .try_reserve(1)
                .map_err(|_| CompactManifestLimitError("string index allocation failed"))?;
        }
        let index = arena.try_intern(value)?;
        arena.dedup.insert(key, index);
        return Ok(index);
    }
    arena.try_intern(value)
}

/// Decode a hex hash to binary and intern it in the `ByteArena`. Empty hex
/// strings produce an empty binary entry.
fn intern_hash(arena: &mut ByteArena, hex: &str) -> Result<u32, CompactManifestLimitError> {
    if hex.is_empty() {
        return Ok(arena.intern_dedup(&[]));
    }
    let bytes = super::assembly_opt::hex_to_bytes(hex).unwrap_or_else(|| {
        log::warn!("intern_hash: non-hex hash '{hex}' stored as raw bytes");
        hex.as_bytes().to_vec()
    });
    Ok(arena.intern_dedup(&bytes))
}

fn manifest_storage_estimates(
    properties: &[SophonManifestAssetProperty],
) -> Option<(usize, usize)> {
    properties
        .iter()
        .try_fold((0usize, 0usize), |(chunks, bytes), file| {
            let file_bytes = file
                .asset_name
                .len()
                .checked_add(file.asset_hash_md5.len())?;
            file.asset_chunks.iter().try_fold(
                (chunks, bytes.checked_add(file_bytes)?),
                |(chunks, bytes), chunk| {
                    let chunk_bytes = chunk
                        .chunk_name
                        .len()
                        .checked_add(chunk.chunk_decompressed_hash_md5.len())?
                        .checked_add(chunk.chunk_compressed_hash_md5.len())?;
                    Some((chunks.checked_add(1)?, bytes.checked_add(chunk_bytes)?))
                },
            )
        })
        .filter(|&(chunks, bytes)| chunks <= u32::MAX as usize && bytes <= u32::MAX as usize)
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
            chunk_name: name.to_string().into(),
            chunk_decompressed_hash_md5: bytes::Bytes::new(),
            chunk_on_file_offset,
            chunk_size: size,
            chunk_size_decompressed: decompressed,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: bytes::Bytes::new(),
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
            asset_name: name.to_string().into(),
            asset_chunks: chunks,
            asset_type,
            asset_size,
            asset_hash_md5: md5.to_string().into(),
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
        assert_eq!(
            &*super::super::assembly_opt::bytes_to_hex(cm.file_hash_md5(0)),
            "d41d8cd98f00b204e9800998ecf8427e"
        );
        assert_eq!(cm.file_size(0), 1024);
        assert!(!cm.is_directory(0));
        assert_eq!(cm.file_chunk_range(0), 0..0);
    }

    #[test]
    fn borrowed_single_file_builder_matches_owned_builder() {
        for chunk_count in [0usize, 1, 3, 50] {
            let chunks = (0..chunk_count)
                .map(|index| SophonManifestAssetChunk {
                    chunk_name: if index % 2 == 0 {
                        "shared-chunk-name".to_owned().into()
                    } else {
                        format!("chunk-{index}").into()
                    },
                    chunk_decompressed_hash_md5: format!("{:032x}", index % 3).into(),
                    chunk_on_file_offset: (index * 1024) as u64,
                    chunk_size: 512 + index as u64,
                    chunk_size_decompressed: 1024 + index as u64,
                    chunk_compressed_hash_xxh: index as u64,
                    chunk_compressed_hash_md5: format!("{:032x}", index % 2).into(),
                    chunk_old_offset: if index % 2 == 0 { -1 } else { index as i64 },
                })
                .collect();
            let property = make_file(
                "GameData/StreamingAssets/bundle.pak",
                "duplicate-file-hash",
                if chunk_count == 0 { 64 } else { 0 },
                (chunk_count * 1024) as u64,
                chunks,
            );
            let owned = CompactManifest::try_from_properties(vec![property.clone()]).unwrap();
            let borrowed = CompactManifest::try_from_property_ref(&property).unwrap();

            assert_eq!(borrowed.num_files(), owned.num_files());
            assert_eq!(borrowed.num_chunks(), owned.num_chunks());
            assert_eq!(borrowed.file_name(0), owned.file_name(0));
            assert_eq!(borrowed.file_hash_md5(0), owned.file_hash_md5(0));
            assert_eq!(borrowed.file_type(0), owned.file_type(0));
            assert_eq!(borrowed.file_size(0), owned.file_size(0));
            assert_eq!(borrowed.is_directory(0), owned.is_directory(0));
            assert_eq!(borrowed.file_chunk_range(0), owned.file_chunk_range(0));
            for chunk_index in 0..chunk_count {
                let borrowed_chunk = borrowed.file_chunk(0, chunk_index);
                let owned_chunk = owned.file_chunk(0, chunk_index);
                assert_eq!(borrowed_chunk.chunk_name, owned_chunk.chunk_name);
                assert_eq!(
                    borrowed_chunk.chunk_decompressed_hash_md5,
                    owned_chunk.chunk_decompressed_hash_md5
                );
                assert_eq!(
                    borrowed_chunk.chunk_compressed_hash_md5,
                    owned_chunk.chunk_compressed_hash_md5
                );
                assert_eq!(borrowed_chunk.chunk_size, owned_chunk.chunk_size);
                assert_eq!(
                    borrowed_chunk.chunk_size_decompressed,
                    owned_chunk.chunk_size_decompressed
                );
                assert_eq!(
                    borrowed_chunk.chunk_old_offset,
                    owned_chunk.chunk_old_offset
                );
                assert_eq!(
                    borrowed.chunk_file_offset(chunk_index),
                    owned.chunk_file_offset(chunk_index)
                );
            }
        }
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
            &*super::super::assembly_opt::bytes_to_hex(c.chunk_compressed_hash_md5),
            "abcdef1234567890abcdef1234567890"
        );
        assert_eq!(
            &*super::super::assembly_opt::bytes_to_hex(c.chunk_decompressed_hash_md5),
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
        assert_eq!(cm.arena.len(), 2);
        assert_eq!(cm.hash_arena.len(), 1);
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
                        chunk_name: format!("chunk_f{f}_c{c}").into(),
                        chunk_decompressed_hash_md5: format!("{:032x}", f * 1000 + c).into(),
                        chunk_on_file_offset: (c as u64) * 4096,
                        chunk_size: 2048 + c as u64,
                        chunk_size_decompressed: 4096 + c as u64,
                        chunk_compressed_hash_xxh: (f * c) as u64,
                        chunk_compressed_hash_md5: format!("{:032x}", f * 1000 + c + 1).into(),
                        chunk_old_offset: if c % 3 == 0 { -1 } else { (c * 4096) as i64 },
                    })
                    .collect();
                SophonManifestAssetProperty {
                    asset_name: format!("data/bundle_{f:04}.pak").into(),
                    asset_chunks: chunks,
                    asset_type: if f % 20 == 0 { 64 } else { 0 },
                    asset_size: (chunk_count as u64) * 4096,
                    asset_hash_md5: format!("{f:032x}").into(),
                }
            })
            .collect();

        let total_chunks: usize = assets.iter().map(|a| a.asset_chunks.len()).sum();
        let cm = CompactManifest::from(assets.clone());

        assert_eq!(cm.num_files(), assets.len());
        assert_eq!(cm.num_chunks(), total_chunks);

        let mut global_chunk_idx = 0usize;
        for (fi, asset) in assets.iter().enumerate() {
            assert_eq!(cm.file_name(fi), asset.asset_name_str());
            assert_eq!(
                &*super::super::assembly_opt::bytes_to_hex(cm.file_hash_md5(fi)),
                asset.asset_hash_md5_str()
            );
            assert_eq!(cm.file_type(fi), asset.asset_type);
            assert_eq!(cm.file_size(fi), asset.asset_size);

            let range = cm.file_chunk_range(fi);
            assert_eq!((range.end - range.start) as usize, asset.asset_chunks.len());

            for (ci, src_chunk) in asset.asset_chunks.iter().enumerate() {
                let c = cm.file_chunk(fi, ci);
                assert_eq!(c.chunk_name, src_chunk.chunk_name_str());
                assert_eq!(
                    &*super::super::assembly_opt::bytes_to_hex(c.chunk_decompressed_hash_md5),
                    src_chunk.chunk_decompressed_hash_md5_str()
                );
                assert_eq!(c.chunk_size, src_chunk.chunk_size);
                assert_eq!(c.chunk_size_decompressed, src_chunk.chunk_size_decompressed);
                assert_eq!(
                    &*super::super::assembly_opt::bytes_to_hex(c.chunk_compressed_hash_md5),
                    src_chunk.chunk_compressed_hash_md5_str()
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
            chunk_name: bytes::Bytes::from("big"),
            chunk_decompressed_hash_md5: bytes::Bytes::new(),
            chunk_on_file_offset: 0,
            chunk_size: big_value,
            chunk_size_decompressed: big_value,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: bytes::Bytes::new(),
            chunk_old_offset: -1,
        };
        let f = make_file("a.pak", "h", 0, big_value, vec![chunk]);
        let cm = CompactManifest::from(vec![f]);
        let c = cm.chunk(0);
        // Truncation wraps: (u32::MAX + 1) as u32 == 0
        assert_eq!(c.chunk_size, 0);
        assert_eq!(c.chunk_size_decompressed, 0);
    }

    #[test]
    fn checked_constructor_rejects_unrepresentable_arena_without_large_allocation() {
        let file = make_file("a", "h", 0, 0, vec![]);
        assert!(validate_limits(usize::MAX, 0).is_err());
        assert!(validate_limits(1, u32::MAX as usize).is_err());
        if usize::BITS > 32 {
            assert!(validate_limits(1, u32::MAX as usize + 1).is_err());
            assert!(validate_allocation_sizes(1, 1, 1, usize::MAX).is_ok());
        }
        assert!(validate_allocation_sizes(usize::MAX, 0, 0, 0).is_err());
        assert!(CompactManifest::try_from_properties(vec![file]).is_ok());
    }

    #[test]
    fn compact_index_limits_accept_last_safe_count_and_reject_sentinel_overflow() {
        if usize::BITS > 32 {
            let max_chunks_for_one_file = (u32::MAX as usize - 3) / 3;
            assert!(validate_limits(1, max_chunks_for_one_file).is_ok());
            assert!(validate_limits(1, max_chunks_for_one_file + 1).is_err());
            let max_files_without_chunks = (u32::MAX as usize - 1) / 2;
            assert!(validate_limits(max_files_without_chunks, 0).is_ok());
            assert!(validate_limits(max_files_without_chunks + 1, 0).is_err());
        }
    }

    #[test]
    fn checked_string_arena_intern_preserves_indices() {
        let mut arena = StringArena::default();
        let first = arena.try_intern("alpha").unwrap();
        let second = arena.try_intern("beta").unwrap();
        assert_eq!(arena.get(first), "alpha");
        assert_eq!(arena.get(second), "beta");
        assert_eq!(arena.intern("gamma"), 2);
    }

    #[test]
    fn deduplicated_empty_strings_share_index_but_regular_intern_appends() {
        let mut arena = StringArena::default();
        let first = arena.intern_dedup("");
        assert_eq!(arena.intern_dedup(""), first);
        assert_eq!(checked_intern(&mut arena, "", true).unwrap(), first);
        assert_eq!(arena.len(), 1);

        let appended = arena.intern("");
        assert_ne!(appended, first);
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.get(first), "");
        assert_eq!(arena.get(appended), "");
    }

    #[test]
    fn clearing_dedup_map_allows_empty_string_to_be_interned_again() {
        let mut arena = StringArena::default();
        let first = arena.intern_dedup("");
        assert_eq!(arena.len(), 1);
        arena.clear_dedup();
        let second = arena.intern_dedup("");
        assert_ne!(first, second);
        assert_eq!(arena.len(), 2);
        assert_eq!(arena.get(first), "");
        assert_eq!(arena.get(second), "");
    }

    #[test]
    fn string_arena_initial_capacities_are_bounded_hints() {
        let arena = StringArena::with_capacity(usize::MAX, usize::MAX);
        assert!(arena.offsets.capacity() <= INITIAL_STRING_INDEX_CAPACITY + 1);
        assert!(arena.data.capacity() <= INITIAL_STRING_ARENA_CAPACITY);
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

    #[test]
    fn shared_chunk_names_stored_separately() {
        let f0 = make_file(
            "a.pak",
            "h1",
            0,
            200,
            vec![make_chunk("shared", 0, 100, 200)],
        );
        let f1 = make_file(
            "b.pak",
            "h2",
            0,
            200,
            vec![make_chunk("shared", 0, 100, 200)],
        );
        let cm = CompactManifest::from(vec![f0, f1]);

        // Both chunks resolve to the same name.
        assert_eq!(cm.chunk_name_at(0), "shared");
        assert_eq!(cm.chunk_name_at(1), "shared");
        // The builder does not dedup chunk names; each gets a distinct arena id.
        assert_ne!(cm.chunk_name_arena_idx(0), cm.chunk_name_arena_idx(1));
        // Arena holds 4 strings: 2 file names + 2 chunk name copies.
        assert_eq!(cm.arena.len(), 4);
    }

    #[test]
    fn str_offsets_at_last_entry_uses_data_len() {
        use super::StringArena;
        let arena = StringArena::from(&["first", "second", "last"][..]);
        let manifest = CompactManifest::wrap_arena(arena);
        // Last entry: end is data.len(), not a sentinel offset.
        let (start, len) = manifest.str_offsets_at(2);
        let arena_str = manifest.arena_str();
        assert_eq!(&arena_str[start as usize..(start + len) as usize], "last");
    }

    #[test]
    fn str_offsets_at_empty_string() {
        use super::StringArena;
        let arena = StringArena::from(&[""][..]);
        let manifest = CompactManifest::wrap_arena(arena);
        let (start, len) = manifest.str_offsets_at(0);
        assert_eq!(len, 0);
        let arena_str = manifest.arena_str();
        assert_eq!(&arena_str[start as usize..(start + len) as usize], "");
    }
}
