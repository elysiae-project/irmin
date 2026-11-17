//! Snapshot of downloaded chunk state, extracted to a separate compilation
//! unit so codegen changes here do not shift [`super::installer`]'s
//! instruction layout (which is sensitive to the state_save canary).

use std::sync::atomic::AtomicU32;
use std::sync::atomic::Ordering;

use super::installer::ChunkNameLookup;

/// Collect the names and sizes of completed (non-zero) chunks in one pass.
/// Pre-allocates at half the slice length — a typical completion ratio —
/// to reduce peak allocation while avoiding the counting pass's extra
/// atomic loads. The Vec grows if the actual ratio exceeds the estimate.
#[inline]
pub fn snapshot_downloaded_chunks<'a>(
    downloaded_chunks: &[AtomicU32],
    chunk_names: &'a ChunkNameLookup,
) -> Vec<(&'a str, u64)> {
    debug_assert_eq!(downloaded_chunks.len(), chunk_names.name_locs.len());
    let mut entries = Vec::with_capacity(downloaded_chunks.len() / 2 + 1);
    for (i, value) in downloaded_chunks.iter().enumerate() {
        let value = value.load(Ordering::Relaxed);
        if value > 0 {
            entries.push((chunk_names.get(i), value as u64));
        }
    }
    entries
}
