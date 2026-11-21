//! Assembly worker count, extracted to a separate compilation unit so
//! codegen changes here do not shift [`super::assembly`]'s instruction
//! layout (which is sensitive to the state_save canary).

/// Returns the number of parallel assembly workers, capped at 8 to
/// balance parallelism against per-worker overhead (DCtx pool, compressed
/// buffer, MD5 pool per thread). Falls back to 1 when the CPU count is
/// unavailable.
#[inline]
pub(super) fn assembly_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().min(8))
        .unwrap_or(1)
}
