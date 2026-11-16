//! Snapshot of downloaded chunk state, extracted to a separate compilation
//! unit so codegen changes here do not shift [`super::installer`]'s
//! instruction layout (which is sensitive to the state_save canary).

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use super::installer::ChunkNameLookup;

/// Collect the names and sizes of completed (non-zero) chunks in one pass.
/// Over-allocates to the full slice length rather than counting first,
/// trading a transient allocation for eliminating the counting pass's
/// extra atomic loads.
pub fn snapshot_downloaded_chunks<'a>(
    downloaded_chunks: &[AtomicU32],
    chunk_names: &'a ChunkNameLookup,
) -> Vec<(&'a str, u64)> {
    debug_assert_eq!(downloaded_chunks.len(), chunk_names.name_locs.len());
    let mut entries = Vec::with_capacity(downloaded_chunks.len());
    for (i, value) in downloaded_chunks.iter().enumerate() {
        let value = value.load(Ordering::Relaxed);
        if value > 0 {
            entries.push((chunk_names.get(i), value as u64));
        }
    }
    entries
}
