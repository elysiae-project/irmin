use std::fs::{self, File};
use std::os::unix::io::AsRawFd as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU32;
use std::sync::{Arc, Mutex};

use super::compact_manifest::CompactManifest;
use super::error::{SophonError, SophonResult};
use super::installer::ChunkNameLookup;

/// Batched durability for assembled files.
///
/// Files are staged here after their bytes are fully written and verified,
/// but before they are made durable or renamed into place. Committing an
/// epoch flushes the batch, renames every file, syncs the touched
/// directories, and only then releases the chunk files they were assembled
/// from (chunks are the only recoverable copy until the output data is
/// durable — see
/// [`crate::game_installer::assembly::decrement_chunk_refcount`]). A crash
/// before commit leaves both the chunk files and the sweepable tmp files
/// for a resume to re-assemble.
///
/// The point of the epoch is one flush barrier per batch instead of one
/// per file: per-chunk `sync_file_range` writeback already streams dirty
/// pages out during assembly, so the commit-time flush is the only
/// synchronous barrier a small file pays for.
pub struct AssemblyEpochs {
    state: Mutex<EpochState>,
    chunk_lookup: Arc<ChunkNameLookup>,
    chunk_refcounts: Arc<Vec<AtomicU32>>,
    chunks_dir: Arc<PathBuf>,
    /// Max staged files before a task must commit before staging more.
    max_files: usize,
    /// Max staged bytes before the same.
    max_bytes: u64,
}

struct EpochState {
    staged: Vec<StagedFile>,
    staged_bytes: u64,
    status: Result<(), SophonError>,
}

struct StagedFile {
    file: File,
    tmp_path: PathBuf,
    target_path: PathBuf,
    /// Manifest handle kept alive so commit can re-derive chunk names;
    /// one Arc bump per file beats allocating name copies at stage time.
    manifest: Arc<CompactManifest>,
    /// Global chunk range this file consumed; names are re-derived at commit.
    chunk_start: u32,
    chunk_end: u32,
    /// Device id of the tmp file, captured at stage time so commit can
    /// issue one syncfs per distinct filesystem (all tmp files normally
    /// share one, but a bind-mounted subdirectory would not).
    dev: u64,
}

/// Staged files before a committing task must flush the epoch. Small-file
/// assembly is dominated by per-file flush barriers, so a generous file
/// count keeps the amortization while the byte threshold bounds the
/// writeback debt the page cache can accumulate between commits.
pub const EPOCH_MAX_FILES: usize = 64;
/// Staged bytes before the same; 256 MiB caps dirty-page growth between
/// commits without throttling the per-chunk writeback stream.
pub const EPOCH_MAX_BYTES: u64 = 256 * 1024 * 1024;

/// Whether the stager should commit before returning to its caller.
#[derive(PartialEq, Eq)]
pub enum CommitDirective {
    /// Epoch still under budget.
    NotDue,
    /// Staging pushed the epoch over budget; commit before continuing.
    Due,
}

/// One file's staging request: everything commit needs to flush, rename,
/// and release the file's chunks. Grouped so the `tmp_path`/`target_path`
/// and `chunk_start`/`chunk_end` pairs cannot be transposed at call sites.
pub struct StageRequest {
    pub tmp_path: PathBuf,
    pub target_path: PathBuf,
    pub manifest: Arc<CompactManifest>,
    pub chunk_start: u32,
    pub chunk_end: u32,
    pub bytes: u64,
}

impl CommitDirective {
    /// True when this directive requires an immediate [`AssemblyEpochs::commit`].
    pub fn is_due(&self) -> bool {
        matches!(self, CommitDirective::Due)
    }
}

impl AssemblyEpochs {
    pub fn new(
        chunk_lookup: Arc<ChunkNameLookup>,
        chunk_refcounts: Arc<Vec<AtomicU32>>,
        chunks_dir: Arc<PathBuf>,
        max_files: usize,
        max_bytes: u64,
    ) -> Self {
        Self {
            state: Mutex::new(EpochState {
                staged: Vec::new(),
                staged_bytes: 0,
                status: Ok(()),
            }),
            chunk_lookup,
            chunk_refcounts,
            chunks_dir,
            max_files,
            max_bytes,
        }
    }

    /// Stage one fully-written tmp file. The caller must have finished all
    /// writes and (parallel) verification of the staged bytes; ownership of
    /// the file handle transfers here so commit can flush it.
    /// On past-error epochs the file is discarded (tmp removed) and the
    /// caller sees the sticky error.
    pub fn stage(&self, file: File, req: StageRequest) -> SophonResult<CommitDirective> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(err) = &state.status {
            let _ = fs::remove_file(&req.tmp_path);
            return Err(SophonError::AssemblyFailed {
                file: req.target_path.display().to_string(),
                error: err.to_string(),
            });
        }
        // st_dev captured from the live fd: cheap fstat, errors treated as
        // dev 0 (single-device assumption, the overwhelmingly common case).
        let dev = fstat_dev(&file).unwrap_or(0);
        state.staged.push(StagedFile {
            file,
            tmp_path: req.tmp_path,
            target_path: req.target_path,
            manifest: req.manifest,
            chunk_start: req.chunk_start,
            chunk_end: req.chunk_end,
            dev,
        });
        state.staged_bytes += req.bytes;
        // staged.len() is bounded by max_files (each Due is committed by the
        // staging thread before it stages again), so a linear scan in commit
        // stays cheap.
        let due = state.staged.len() >= self.max_files || state.staged_bytes >= self.max_bytes;
        Ok(if due {
            CommitDirective::Due
        } else {
            CommitDirective::NotDue
        })
    }

    /// Commit every staged file: flush → rename → fsync parent dirs →
    /// deferred refcount decrements. Idempotent; an empty epoch is a no-op.
    /// The first error is sticky: later `stage`/`commit` calls return the
    /// ORIGINAL error (kind and path preserved) until the manager is
    /// dropped, so the surrounding install fails with the real cause.
    /// The state lock is only held to take the batch — never across the
    /// flush syscalls — so concurrent stagers keep running during a commit.
    pub fn commit(&self) -> SophonResult<()> {
        let staged = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if let Err(err) = &state.status {
                return Err(clone_sticky(err));
            }
            state.staged_bytes = 0;
            std::mem::take(&mut state.staged)
        };
        let n = staged.len();
        let result =
            commit_staged(staged, &self.chunk_lookup, &self.chunk_refcounts, &self.chunks_dir);
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        match (&state.status, result) {
            (Err(sticky), _) => Err(clone_sticky(sticky)),
            (_, Err(err)) => {
                state.status = Err(err);
                Err(clone_sticky(state.status.as_ref().unwrap_err()))
            }
            (Ok(()), Ok(())) => Ok(()),
        }
        .map_err(|err| annotate_epoch(err, n))
    }
}

/// The sticky error is boxed behind the Mutex, so re-exposing it to callers
/// needs a copy; `io::Error` is cheaply cloneable via its string form and
/// the SophonError variants involved are all cheap value types.
fn clone_sticky(err: &SophonError) -> SophonError {
    match err {
        SophonError::Io(e) => SophonError::Io(clone_io(e)),
        other => {
            // Fallback path for variants without cheap clones: preserve
            // kind + message. Only reachable if a future variant is stored.
            SophonError::AssemblyFailed {
                file: String::new(),
                error: other.to_string(),
            }
        }
    }
}

fn clone_io(e: &std::io::Error) -> std::io::Error {
    match e.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::new(e.kind(), e.to_string()),
    }
}

/// Attach the epoch context to an error reported by `commit`, unless the
/// error already names a file.
fn annotate_epoch(err: SophonError, staged_count: usize) -> SophonError {
    match err {
        SophonError::AssemblyFailed { file, error } if file.is_empty() => {
            SophonError::AssemblyFailed {
                file: format!("assembly epoch ({staged_count} files)"),
                error,
            }
        }
        other => other,
    }
}

/// Flush → rename → sync → release. Split from [`AssemblyEpochs::commit`]
/// so the lock is not held across syscalls.
fn commit_staged(
    staged: Vec<StagedFile>,
    chunk_lookup: &ChunkNameLookup,
    chunk_refcounts: &[AtomicU32],
    chunks_dir: &Path,
) -> SophonResult<()> {
    let mut first_err = None;
    // One syncfs per distinct filesystem holding staged files: per-chunk
    // sync_file_range calls during assembly already queued each file's
    // writeback, so a single barrier per device waits for all of it in one
    // transaction commit (per-file fdatasync would start one commit per
    // file — the exact serialization the epoch exists to remove).
    let mut synced_devs: Vec<u64> = Vec::new();
    for sf in &staged {
        if synced_devs.contains(&sf.dev) {
            continue;
        }
        synced_devs.push(sf.dev);
        if let Err(err) = syncfs(sf.file.as_raw_fd()) {
            first_err.get_or_insert(SophonError::AssemblyFailed {
                file: sf.target_path.display().to_string(),
                error: format!("syncfs: {err}"),
            });
            break;
        }
    }
    for sf in &staged {
        if first_err.is_some() {
            break;
        }
        if let Err(err) = fs::rename(&sf.tmp_path, &sf.target_path) {
            if err.raw_os_error() == Some(libc::EXDEV)
                || err.kind() == std::io::ErrorKind::CrossesDevices
            {
                log::warn!("rename EXDEV; falling back to copy + unlink: {err}");
                if let Err(err2) = exdev_copy_sync(&sf.tmp_path, &sf.target_path) {
                    first_err.get_or_insert(err2);
                    continue;
                }
            } else {
                first_err.get_or_insert(SophonError::AssemblyFailed {
                    file: sf.target_path.display().to_string(),
                    error: format!("rename: {err}"),
                });
                continue;
            }
        }
    }
    // Rename durability: fsync each distinct parent directory once so the
    // new names survive a crash. Sync AFTER all renames so a single commit
    // pays one fsync per directory, not one per file. A dir-fsync failure
    // fails the commit — silent best-effort here would hide the durability
    // gap — but the already-flushed data keeps the files recoverable.
    if first_err.is_none() {
        let mut dirs: Vec<&Path> = Vec::new();
        for sf in &staged {
            if let Some(parent) = sf.target_path.parent() {
                dirs.push(parent);
            }
        }
        dirs.sort_unstable();
        dirs.dedup();
        for dir in dirs {
            if let Err(err) = fs::File::open(dir).and_then(|d| d.sync_all()) {
                log::warn!(
                    "failed to fsync directory {} after rename: {err}",
                    dir.display()
                );
                first_err.get_or_insert(SophonError::Io(err));
                break;
            }
        }
    } else {
        // Best effort: discard any tmp files that never reached rename so
        // a retry does not collide with stale tmp content.
        for sf in &staged {
            if sf.target_path.exists() {
                // File was renamed before the error? Only possible if the
                // error came from a later file; keep earlier renames.
                continue;
            }
            let _ = fs::remove_file(&sf.tmp_path);
        }
    }
    // Deferred chunk release: ONLY on full success. A failed commit must
    // leave refcounts untouched so the chunk files survive for a resume
    // re-assembly — deleting them here would abandon the only copy of
    // data whose assembled form was never made durable.
    if first_err.is_none() {
        for sf in &staged {
            for ci in sf.chunk_start..sf.chunk_end {
                let name = sf.manifest.chunk_name_at(ci as usize);
                crate::game_installer::assembly::decrement_chunk_refcount(
                    name,
                    chunk_lookup,
                    chunk_refcounts,
                    chunks_dir,
                );
            }
        }
    }
    match first_err {
        Some(err) => Err(err),
        None => Ok(()),
    }
}

/// Device id of an open file, used to group the epoch flush by filesystem.
fn fstat_dev(file: &File) -> std::io::Result<u64> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `st` is a valid, correctly-aligned out-pointer for fstat(2);
    // on success the kernel fully initializes it.
    let ret = unsafe { libc::fstat(file.as_raw_fd(), st.as_mut_ptr()) };
    if ret == 0 {
        // SAFETY: fstat succeeded, so `st` is initialized.
        Ok(unsafe { st.assume_init() }.st_dev)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Whole-filesystem sync barrier via `syncfs(2)`: waits until the
/// filesystem containing the staged files has flushed all dirty data.
fn syncfs(fd: std::os::unix::io::RawFd) -> std::io::Result<()> {
    // SAFETY: `fd` is a live file descriptor owned by a staged file.
    let ret = unsafe { libc::syncfs(fd) };
    if ret == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn exdev_copy_sync(tmp_path: &Path, target_path: &Path) -> SophonResult<()> {
    fs::copy(tmp_path, target_path)?;
    fs::File::open(target_path)?.sync_data()?;
    let _ = fs::remove_file(tmp_path);
    Ok(())
}
