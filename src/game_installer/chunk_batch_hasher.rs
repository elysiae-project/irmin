//! Per-file SIMD batch hasher for chunk-decompressed MD5 verification.
//!
//! In VerifyMode::Full every assembled chunk's decompressed bytes are MD5
//! verified. Hashing inside each worker is a scalar stream that the
//! release-profile gate measured as a dominant share (~a quarter to a
//! third) of the assembly pipeline's CPU. MD5 cannot parallelize one
//! message, but a file's chunks are independent messages, so one
//! batch-hasher thread verifies them in lockstep through the `md5-simd`
//! multi-buffer kernel — up to 8 chunks per core on AVX2, multi-GiB/s
//! aggregate — with digests byte-identical to `fast_md5`. Reproduce the
//! ratio with the `assembly_hash_gate` benchmark.
//!
//! Design: workers submit hash jobs after successfully writing a chunk
//! and move on without waiting; the hasher thread — spawned in the same
//! `std::thread::scope` as the workers, mirroring the parallel file
//! hasher — holds the output file's shared read-only mapping, slices each
//! job's range from the page cache, groups equal-length jobs (the SIMD
//! kernel accelerates equal-length runs), and verifies them in batches.
//! Partial batches flush on queue drain so lanes never stall and
//! producers never wait on stragglers.
//!
//! Error and eviction semantics match the in-worker layout:
//! - The hasher records into the assembly's own `first_error` slot
//!   (first-recorded wins, same as worker errors) and sets the shared
//!   abort flag, so error precedence is unchanged. Deferred chunks keep
//!   the zstd frame checksum ignored, exactly as the in-worker MD5 path
//!   did (see `decompress_chunk_optimized_with_defer`).
//! - The hasher owns eviction for deferred ranges: workers skip their
//!   per-chunk `sync_and_evict_range`, and the hasher issues that call
//!   after each verified range — one eviction owner per file, and only
//!   when the worker path would have evicted (`skip_evict` false).
//!
//! Only 16-byte (MD5) decompressed hashes are deferred; other lengths
//! keep the in-worker comparison, preserving existing behavior exactly.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Mutex;

use md5_simd::Md5Engine;

use super::assembly_opt::{self, SharedMmapAdvice, sync_and_evict_range};
use super::error::SophonError;

/// Hash jobs buffered between workers and the hasher thread. Together
/// with in-flight worker chunks this bounds the un-evicted output ranges
/// awaiting verification.
pub(crate) const QUEUE_CAP: usize = 16;

/// One deferred chunk verification: the decompressed bytes live at
/// `offset..offset+len` of the output file and must match `expected`.
pub(crate) struct HashJob {
    /// Chunk path (with `.zstd` extension), matching the in-worker
    /// mismatch report.
    pub(crate) name: String,
    pub(crate) offset: u64,
    pub(crate) len: u64,
    pub(crate) expected: [u8; 16],
}

/// True when a decompressed hash is deferrable to the batch hasher: a
/// real 16-byte MD5 digest. This is `chunk_hash_required` (non-empty, not
/// the zero placeholder) restricted to the one digest length the SIMD
/// engine produces, so 8-byte xxh64-style hashes keep their in-worker
/// comparison unchanged.
pub(crate) fn defer_eligible(chunk_decompressed_hash_md5: &[u8]) -> bool {
    chunk_decompressed_hash_md5.len() == 16 && chunk_decompressed_hash_md5 != [0u8; 16]
}

/// Hasher thread body. Spawn inside the workers' scope; `receiver`
/// disconnects when every producer (each worker's cloned sender, dropped
/// at worker exit) has closed, which drains and flushes the final batch.
pub(crate) fn run(
    output_path: &Path,
    file_len: u64,
    receiver: Receiver<HashJob>,
    first_error: &Mutex<Option<SophonError>>,
    has_error: &AtomicBool,
) {
    let file = match File::open(output_path) {
        Ok(f) => f,
        Err(e) => {
            record(first_error, has_error, SophonError::Io(e));
            // Drain so producers never block on a full queue.
            while receiver.recv().is_ok() {}
            return;
        }
    };
    if file_len == 0 {
        // No mapping exists for a zero-length file; every deferred job
        // (always len > 0) is out-of-range and fails verification with
        // the same report the bounds check in `flush` produces.
        // Unreachable in production: the hasher spawns only when some
        // eligible chunk has size > 0, so the file has bytes.
        while let Ok(job) = receiver.recv() {
            record(
                first_error,
                has_error,
                SophonError::Io(std::io::Error::other(format!(
                    "chunk {} range outside zero-length output file; manifest inconsistent",
                    job.name
                ))),
            );
        }
        return;
    }
    let map = match assembly_opt::mmap_shared_read_only(
        &file,
        file_len as usize,
        SharedMmapAdvice::DemandSequential,
    ) {
        Ok(m) => m,
        Err(e) => {
            record(first_error, has_error, SophonError::Io(e));
            while receiver.recv().is_ok() {}
            return;
        }
    };
    let fd = file.as_raw_fd();
    let engine = Md5Engine::new();
    let mut pending: Vec<HashJob> = Vec::with_capacity(QUEUE_CAP);

    // Collect, then flush on lane satisfaction or queue drain; never wait
    // for stragglers to fill a batch.
    while let Ok(job) = receiver.recv() {
        pending.push(job);
        loop {
            match receiver.try_recv() {
                Ok(job) => pending.push(job),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    flush(&map, fd, &engine, &mut pending, first_error, has_error);
                    return;
                }
            }
            if pending.len() >= QUEUE_CAP {
                flush(&map, fd, &engine, &mut pending, first_error, has_error);
            }
        }
        flush(&map, fd, &engine, &mut pending, first_error, has_error);
    }
    flush(&map, fd, &engine, &mut pending, first_error, has_error);
}

/// Verify one batch of jobs through the SIMD engine, then release each
/// verified range's page-cache residency. After an abort is flagged, jobs
/// drain without further work: the output file is discarded anyway.
fn flush(
    map: &assembly_opt::MmapGuard,
    fd: std::os::unix::io::RawFd,
    engine: &Md5Engine,
    jobs: &mut Vec<HashJob>,
    first_error: &Mutex<Option<SophonError>>,
    has_error: &AtomicBool,
) {
    if jobs.is_empty() {
        return;
    }
    if has_error.load(Ordering::Relaxed) {
        jobs.clear();
        return;
    }

    // Stable group by exact length: equal-length jobs keep arrival order
    // within their group and the engine accelerates equal-length runs.
    jobs.sort_by_key(|job| job.len);

    let bytes = map.as_slice();
    let mut inputs: Vec<&[u8]> = Vec::with_capacity(jobs.len());
    let mut hashable: Vec<usize> = Vec::with_capacity(jobs.len());
    for (i, job) in jobs.iter().enumerate() {
        let start = job.offset as usize;
        // Offsets and lengths come from the manifest and the written
        // ranges; a range outside the file cannot verify.
        if let Some(range) = bytes.get(start..start + job.len as usize) {
            inputs.push(range);
            hashable.push(i);
        } else {
            // Offsets and lengths are manifest-derived; a range outside
            // the file is an inconsistent manifest, not a digest failure.
            record(
                first_error,
                has_error,
                SophonError::Io(std::io::Error::other(format!(
                    "chunk {} range outside output file; manifest inconsistent",
                    job.name
                ))),
            );
        }
    }

    if !inputs.is_empty() {
        let mut digests = vec![[0u8; 16]; inputs.len()];
        engine.hash_many(&inputs, &mut digests);
        for (slot, &job_index) in hashable.iter().enumerate() {
            let job = &jobs[job_index];
            if digests[slot] != job.expected {
                record(
                    first_error,
                    has_error,
                    SophonError::Md5Mismatch {
                        item: job.name.clone(),
                        expected: assembly_opt::bytes_to_hex(&job.expected),
                        actual: assembly_opt::md5_to_hex(&digests[slot]),
                    },
                );
            }
        }
    }

    // Eviction (single owner): after a range is verified, unmap its PTEs
    // from this hasher's own mapping first — posix_fadvise(POSIX_FADV_DONTNEED)
    // drops only unmapped pages, so leaving them mapped would pin the very
    // pages the worker path used to release. The fd-based sync+advise below
    // then flushes dirty pages and drops the now-unmapped clean ones, the
    // same net syscall sequence the worker path issued per chunk.
    for job in jobs.iter() {
        // Offsets/lengths are manifest-derived and clamped by evict_prefix;
        // the fd-level call below uses the raw range, matching the worker
        // path exactly.
        let start = job.offset as usize;
        let end = start + job.len as usize;
        let _ = map.evict_prefix(start, end);
        sync_and_evict_range(fd, job.offset, job.len);
    }
    jobs.clear();
}

fn record(
    first_error: &Mutex<Option<SophonError>>,
    has_error: &AtomicBool,
    err: SophonError,
) {
    let mut guard = first_error.lock().unwrap();
    if guard.is_none() {
        *guard = Some(err);
    }
    drop(guard);
    has_error.store(true, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::unix::fs::FileExt;
    use std::sync::mpsc::sync_channel;

    /// Deterministic pseudo-random bytes (xorshift), keeping tests
    /// dependency-free.
    fn fake_bytes(seed: u64, len: usize) -> Vec<u8> {
        let mut v = Vec::with_capacity(len);
        let mut state = seed | 1;
        for _ in 0..len {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            v.push((state & 0xff) as u8);
        }
        v
    }

    /// Reference digest via the production scalar hasher.
    fn scalar_md5(data: &[u8]) -> [u8; 16] {
        let mut h = assembly_opt::Md5::new().unwrap();
        h.update(data).unwrap();
        h.finish().unwrap()
    }

    fn temp_file(content: &[u8]) -> (std::path::PathBuf, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        std::fs::write(&path, content).unwrap();
        (path, dir)
    }

    /// Spawn the hasher the way assembly does (own thread inside the
    /// producers' scope) and block until it drains and exits. Returns the
    /// first recorded error, if any.
    fn drive(
        path: &Path,
        file_len: u64,
        jobs: impl FnOnce(std::sync::mpsc::SyncSender<HashJob>),
        pre_flagged: bool,
    ) -> Option<SophonError> {
        let (tx, rx) = sync_channel(QUEUE_CAP);
        let first_error = Mutex::new(None);
        let has_error = AtomicBool::new(pre_flagged);
        std::thread::scope(|s| {
            s.spawn({
                let path = path.to_path_buf();
                let slot = &first_error;
                let flag = &has_error;
                move || run(&path, file_len, rx, slot, flag)
            });
            // The original sender drops here at scope-body end…
            jobs(tx);
        });
        // …and the scope join guarantees the hasher has drained fully.
        first_error.into_inner().unwrap()
    }

    /// Differential test: digests must equal the scalar production hasher
    /// over mixed lengths, spanning the SIMD engine's lane-batching
    /// thresholds (both below and above).
    #[test]
    fn batch_digests_match_scalar_across_lengths() {
        let lens = [0usize, 1, 31, 55, 64, 64, 100, 4096, 1 << 16];
        let mut content = Vec::new();
        let mut expected = Vec::new();
        for (i, &len) in lens.iter().enumerate() {
            let bytes = fake_bytes(i as u64 + 1, len);
            expected.push(scalar_md5(&bytes));
            content.extend_from_slice(&bytes);
        }
        let (path, _dir) = temp_file(&content);

        let err = drive(&path, content.len() as u64, |tx| {
            let mut offset = 0u64;
            for (i, &len) in lens.iter().enumerate() {
                tx.send(HashJob {
                    name: format!("chunk_{i}.zstd"),
                    offset,
                    len: len as u64,
                    expected: expected[i],
                })
                .unwrap();
                offset += len as u64;
            }
        }, false);
        assert!(err.is_none());
    }

    /// A tampered range must produce exactly one Md5Mismatch naming the
    /// chunk.
    #[test]
    fn mismatch_is_detected_and_named() {
        let bytes = fake_bytes(7, 3 * 4096);
        let (path, _dir) = temp_file(&bytes);

        let err = drive(&path, bytes.len() as u64, |tx| {
            tx.send(HashJob {
                name: "evil.zstd".into(),
                offset: 0,
                len: bytes.len() as u64,
                expected: [0x42; 16],
            })
            .unwrap();
        }, false);
        match err {
            Some(SophonError::Md5Mismatch { item, .. }) => assert_eq!(item, "evil.zstd"),
            other => panic!("expected Md5Mismatch, got {other:?}"),
        }
    }

    /// Sub-capacity trailing batches must all flush on disconnect — no
    /// job may be silently dropped when the queue drains partial.
    #[test]
    fn partial_batch_flushes_on_drain() {
        let lens = [900usize, 1500, 777];
        let mut content = Vec::new();
        let mut expected = Vec::new();
        for (i, &len) in lens.iter().enumerate() {
            let bytes = fake_bytes(i as u64 + 100, len);
            expected.push(scalar_md5(&bytes));
            content.extend_from_slice(&bytes);
        }
        let (path, _dir) = temp_file(&content);

        let err = drive(&path, content.len() as u64, |tx| {
            let mut offset = 0u64;
            for (i, &len) in lens.iter().enumerate() {
                tx.send(HashJob {
                    name: format!("c{i}.zstd"),
                    offset,
                    len: len as u64,
                    expected: expected[i],
                })
                .unwrap();
                offset += len as u64;
            }
        }, false);
        assert!(err.is_none());
    }

    /// First error wins across forced mid-run flushes (more jobs than
    /// QUEUE_CAP), matching worker error precedence.
    #[test]
    fn first_recorded_error_wins() {
        let bytes = fake_bytes(3, 8192);
        let (path, _dir) = temp_file(&bytes);

        let err = drive(&path, 8192, |tx| {
            for i in 0..QUEUE_CAP + 4 {
                tx.send(HashJob {
                    name: format!("bad{i}.zstd"),
                    offset: 0,
                    len: 8192,
                    expected: [i as u8; 16],
                })
                .unwrap();
            }
        }, false);
        let SophonError::Md5Mismatch { item, .. } = err.unwrap() else {
            panic!("expected Md5Mismatch")
        };
        assert_eq!(item, "bad0.zstd");
    }

    /// A job whose range lies outside the file must fail verification
    /// rather than panic (manifest corruption defense).
    #[test]
    fn out_of_range_job_fails_not_panics() {
        let bytes = fake_bytes(11, 4096);
        let (path, _dir) = temp_file(&bytes);

        let err = drive(&path, 4096, |tx| {
            tx.send(HashJob {
                name: "oob.zstd".into(),
                offset: 4096,
                len: 4096,
                expected: [0; 16],
            })
            .unwrap();
        }, false);
        assert!(matches!(err, Some(SophonError::Io(_))));
    }

    /// The hasher must drain a closed empty channel on an empty file
    /// without errors.
    #[test]
    fn empty_channel_on_empty_file_drains_cleanly() {
        let (path, _dir) = temp_file(b"");
        let err = drive(&path, 0, |_| {}, false);
        assert!(err.is_none());
    }

    /// Many jobs (multiple QUEUE_CAP rounds) force mid-run flushes;
    /// results must stay complete and correct.
    #[test]
    fn many_jobs_flush_repeatedly() {
        let per = 4096usize;
        let n = QUEUE_CAP * 3;
        let mut content = Vec::with_capacity(per * n);
        let mut expected = Vec::new();
        for i in 0..n {
            let bytes = fake_bytes(i as u64 + 1000, per);
            expected.push(scalar_md5(&bytes));
            content.extend_from_slice(&bytes);
        }
        let (path, _dir) = temp_file(&content);

        let err = drive(&path, content.len() as u64, |tx| {
            for (i, e) in expected.iter().enumerate() {
                tx.send(HashJob {
                    name: format!("m{i}.zstd"),
                    offset: (i * per) as u64,
                    len: per as u64,
                    expected: *e,
                })
                .unwrap();
            }
        }, false);
        assert!(err.is_none());
    }

    /// With an abort pre-flagged, the hasher must keep draining a full
    /// queue (producers must never block) and exit cleanly.
    #[test]
    fn pre_flagged_abort_still_drains_full_queue() {
        let bytes = fake_bytes(5, 2048);
        let (path, _dir) = temp_file(&bytes);

        // Cap smaller than the job count to force sender blocking while
        // the hasher drains-without-working. The channel lives inside the
        // scope body so the sender drops before the join and the hasher's
        // recv() sees Disconnected.
        let first_error = Mutex::new(None);
        let has_error = AtomicBool::new(true);
        std::thread::scope(|s| {
            let (tx, rx) = sync_channel(2);
            s.spawn({
                let path = path.clone();
                let slot = &first_error;
                let flag = &has_error;
                move || run(&path, 2048, rx, slot, flag)
            });
            for i in 0..6 {
                tx.send(HashJob {
                    name: format!("d{i}.zstd"),
                    offset: 0,
                    len: 2048,
                    expected: [9; 16],
                })
                .unwrap();
            }
        });
        // Drained without deadlock; no new error recorded (flag was
        // already set, jobs were dropped without work).
        assert!(first_error.into_inner().unwrap().is_none());
    }

    /// `defer_eligible` is exactly "16 bytes, not all zero".
    #[test]
    fn defer_eligible_predicate() {
        assert!(defer_eligible(&[1u8; 16]));
        assert!(!defer_eligible(&[0u8; 16]));
        assert!(!defer_eligible(&[1u8; 8]));
        assert!(!defer_eligible(b""));
    }

    /// Writes from a "worker" into the output file must be visible to the
    /// hasher's MAP_SHARED read-only mapping — the visibility contract
    /// the deferral relies on. Hash the tail half after writing it into a
    /// pre-sized file.
    #[test]
    fn shared_map_sees_concurrent_writes() {
        let per = 8192usize;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("live.bin");
        std::fs::write(&path, vec![0u8; per * 2]).unwrap();
        let f = OpenOptions::new().write(true).open(&path).unwrap();
        let first_error = Mutex::new(None);
        let has_error = AtomicBool::new(false);
        std::thread::scope(|s| {
            let (tx, rx) = sync_channel(QUEUE_CAP);
            s.spawn({
                let path = path.clone();
                let slot = &first_error;
                let flag = &has_error;
                move || run(&path, (per * 2) as u64, rx, slot, flag)
            });
            // Concurrent "worker" write and submit.
            let tail = fake_bytes(99, per);
            f.write_all_at(&tail, per as u64).unwrap();
            tx.send(HashJob {
                name: "tail.zstd".into(),
                offset: per as u64,
                len: per as u64,
                expected: scalar_md5(&tail),
            })
            .unwrap();
        });
        assert!(first_error.into_inner().unwrap().is_none());
    }
}

