//! Output file pre-allocation and handle caching for eager decompression.
//!
//! Manages open file descriptors for output files that receive concurrent
//! pwrite calls from multiple download workers. Pre-allocates files to their
//! declared size before downloads begin.

use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dashmap::DashMap;

use super::error::{SophonError, SophonResult};
use super::sysio;

pub struct OutputAllocator {
    handles: DashMap<usize, Arc<File>>,
    game_dir: PathBuf,
}

impl OutputAllocator {
    pub fn new(game_dir: &Path) -> Self {
        Self {
            handles: DashMap::new(),
            game_dir: game_dir.to_path_buf(),
        }
    }

    /// Pre-allocates an output file at its full size. Creates parent directories.
    /// Stores the open fd in the handle cache for later pwrite access.
    /// Thread-safe: only the first caller for a given file_idx creates the file.
    pub fn preallocate_file(
        &self,
        file_idx: usize,
        relative_path: &str,
        size: u64,
    ) -> SophonResult<()> {
        let game_dir = &self.game_dir;

        // Atomic check-and-insert: closure runs only if key is absent.
        self.handles
            .entry(file_idx)
            .or_try_insert_with(|| -> SophonResult<Arc<File>> {
                let full_path = game_dir.join(relative_path);

                if let Some(parent) = full_path.parent() {
                    fs::create_dir_all(parent).map_err(SophonError::Io)?;
                }

                let file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(&full_path)
                    .map_err(SophonError::Io)?;

                // Only extend; never shrink (preserves data on resume).
                let current_len = file.metadata().map_err(SophonError::Io)?.len();
                if current_len < size {
                    file.set_len(size).map_err(SophonError::Io)?;
                    let _ = sysio::preallocate(&file, size);
                }

                use std::os::unix::io::AsRawFd;
                super::assembly_opt::posix_advise(
                    file.as_raw_fd(),
                    0,
                    size,
                    libc::POSIX_FADV_RANDOM,
                );

                Ok(Arc::new(file))
            })?;

        Ok(())
    }

    /// Returns the cached file handle for a given file index.
    pub fn get_handle(&self, file_idx: usize) -> Option<Arc<File>> {
        self.handles.get(&file_idx).map(|r| Arc::clone(r.value()))
    }

    /// Closes and removes the file handle from cache.
    pub fn close_file(&self, file_idx: usize) {
        self.handles.remove(&file_idx);
    }

    /// Returns the number of open file handles.
    pub fn open_count(&self) -> usize {
        self.handles.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preallocate_and_get_handle() {
        let dir = tempfile::tempdir().unwrap();
        let alloc = OutputAllocator::new(dir.path());

        alloc
            .preallocate_file(0, "subdir/test_file.bin", 1024 * 1024)
            .unwrap();

        let handle = alloc.get_handle(0).unwrap();
        let meta = handle.metadata().unwrap();
        assert_eq!(meta.len(), 1024 * 1024);

        // Verify parent dir was created
        assert!(dir.path().join("subdir").is_dir());
    }

    #[test]
    fn close_file_removes_handle() {
        let dir = tempfile::tempdir().unwrap();
        let alloc = OutputAllocator::new(dir.path());

        alloc.preallocate_file(5, "a.bin", 100).unwrap();
        assert!(alloc.get_handle(5).is_some());

        alloc.close_file(5);
        assert!(alloc.get_handle(5).is_none());
    }

    #[test]
    fn concurrent_access() {
        let dir = tempfile::tempdir().unwrap();
        let alloc = Arc::new(OutputAllocator::new(dir.path()));

        // Pre-allocate a file
        alloc.preallocate_file(0, "concurrent.bin", 4096).unwrap();

        // Multiple threads write to different offsets via pwrite
        let handles: Vec<_> = (0..4)
            .map(|i| {
                let alloc = Arc::clone(&alloc);
                std::thread::spawn(move || {
                    let file = alloc.get_handle(0).unwrap();
                    let data = vec![i as u8; 1024];
                    use std::os::unix::fs::FileExt;
                    file.write_all_at(&data, i as u64 * 1024).unwrap();
                })
            })
            .collect();

        for h in handles {
            h.join().unwrap();
        }

        // Verify all writes landed
        let file = alloc.get_handle(0).unwrap();
        use std::os::unix::fs::FileExt;
        for i in 0..4u8 {
            let mut buf = [0u8; 1024];
            file.read_at(&mut buf, i as u64 * 1024).unwrap();
            assert!(buf.iter().all(|&b| b == i));
        }
    }
}
