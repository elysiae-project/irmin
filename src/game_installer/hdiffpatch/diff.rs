use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
#[cfg(all(feature = "benchmark", unix))]
use std::os::unix::fs::FileExt;
#[cfg(unix)]
use std::os::unix::io::AsRawFd as _;
use std::path::PathBuf;

use super::header::{DiffChunkInfo, DiffSingleChunkInfo, HeaderInfo};
use super::parser::BinaryExtensions;

/// Applies an HDiff patch from a source and diff file into a destination file.
pub struct HDiff {
    pub source_path: String,
    pub diff_path: String,
    pub dest_path: String,
    pub diff_offset: u64,
}

#[cfg(feature = "benchmark")]
#[derive(Clone, Debug)]
pub enum SourceBackend {
    Mmap,
    File,
    Windowed {
        window_size: usize,
        stats: std::sync::Arc<WindowStats>,
    },
}

#[cfg(feature = "benchmark")]
#[derive(Debug, Default)]
pub struct WindowStats {
    pub hits: std::sync::atomic::AtomicU64,
    pub misses: std::sync::atomic::AtomicU64,
    pub bytes_read: std::sync::atomic::AtomicU64,
}

#[cfg(not(feature = "benchmark"))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SourceBackend {
    #[default]
    Mmap,
}

impl HDiff {
    /// Test/benchmark constructor with a zero diff offset.
    #[cfg(any(test, feature = "benchmark"))]
    pub fn new(source_path: String, diff_path: String, dest_path: String) -> Self {
        Self::with_offset(source_path, diff_path, dest_path, 0)
    }

    /// Create a patch operation with explicit source, diff, and destination offsets.
    pub fn with_offset(
        source_path: String,
        diff_path: String,
        dest_path: String,
        diff_offset: u64,
    ) -> Self {
        HDiff {
            source_path,
            diff_path,
            dest_path,
            diff_offset,
        }
    }

    /// Apply the patch, optionally reporting completed output bytes; returns
    /// `true` only when the patch completes successfully.
    pub fn apply(&mut self, on_progress: Option<Box<dyn Fn(u64)>>) -> bool {
        self.apply_with_source_backend(on_progress, SourceBackend::Mmap)
    }

    #[cfg(feature = "benchmark")]
    pub(crate) fn apply_with_source_backend(
        &mut self,
        on_progress: Option<Box<dyn Fn(u64)>>,
        source_backend: SourceBackend,
    ) -> bool {
        match self.apply_inner(on_progress.as_ref().map(|cb| cb.as_ref()), source_backend) {
            Ok(()) => true,
            Err(err) => {
                log::error!("[HDiff::apply] Error: {err}");
                false
            }
        }
    }

    #[cfg(not(feature = "benchmark"))]
    fn apply_with_source_backend(
        &mut self,
        on_progress: Option<Box<dyn Fn(u64)>>,
        source_backend: SourceBackend,
    ) -> bool {
        match self.apply_inner(on_progress.as_ref().map(|cb| cb.as_ref()), source_backend) {
            Ok(()) => true,
            Err(err) => {
                log::error!("[HDiff::apply] Error: {err}");
                false
            }
        }
    }

    fn apply_inner(
        &self,
        on_progress: Option<&dyn Fn(u64)>,
        source_backend: SourceBackend,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Reject in-place patching: canonicalize detects same file on disk
        // when both exist; string comparison catches same-path-string when
        // the destination does not yet exist.
        let source_canonical: Option<PathBuf> = std::fs::canonicalize(&self.source_path).ok();
        let dest_canonical: Option<PathBuf> = match std::fs::canonicalize(&self.dest_path) {
            Ok(path) => Some(path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        if source_canonical.is_some()
            && dest_canonical.is_some()
            && source_canonical == dest_canonical
        {
            return Err(format!(
                "source and destination paths resolve to the same file: {path}",
                path = self.source_path
            )
            .into());
        }

        let destination_symlink_metadata = std::fs::symlink_metadata(&self.dest_path);
        let destination_metadata = match destination_symlink_metadata {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err("destination path must not be a symbolic link".into());
            }
            Ok(metadata) => Some(metadata),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => None,
            Err(err) => return Err(err.into()),
        };
        let mut diff_file = File::open(&self.diff_path)?;
        if self.diff_offset > 0 {
            diff_file.seek(SeekFrom::Start(self.diff_offset))?;
        }
        let mut header_info = HeaderInfo::default();
        let header_info_line = diff_file.read_string_to_null(512)?;

        if header_info_line.len() > 64 || !header_info_line.starts_with("HDIFF") {
            return Err("not a HDiff file format".into());
        }
        let h_info_arr: Vec<&str> = header_info_line.split('&').collect();
        if h_info_arr.len() < 2 || h_info_arr.len() > 3 {
            return Err(format!(
                "unsupported HDiff header format: expected 2 or 3 parts, got {parts} (raw: {header_info_line})",
                parts = h_info_arr.len()
            )
            .into());
        }

        let p_file_ver = Self::try_get_version(h_info_arr[0])?;
        if p_file_ver == 19 {
            return Err(
                "directory patches (HDIFF19) are not supported by the single-file patcher".into(),
            );
        }
        if p_file_ver != 13 && p_file_ver != 20 {
            return Err(format!(
                "unsupported HDiff version {p_file_ver} (only 13 and 20 supported)"
            )
            .into());
        }

        // 3-part header: "HDIFF13&zstd&fadler64" or "HDIFF20&zstd&fadler64"
        // The third field is a checksum mode string; validate it if present.
        if h_info_arr.len() == 3 {
            let checksum_name = h_info_arr[2];
            match checksum_name {
                "crc32" | "fadler64" | "nochecksum" | "Crc32" | "Fadler64" | "Nochecksum" => {}
                _ => {
                    return Err(format!("unsupported HDiff checksum mode: {checksum_name}").into());
                }
            }
        }

        header_info.comp_mode = h_info_arr[1].parse()?;
        header_info.is_single_compressed_diff = p_file_ver == 20;

        if header_info.is_single_compressed_diff {
            Self::read_single_file_header(&mut diff_file, &mut header_info)?;
        } else {
            Self::read_non_single_file_header(&mut diff_file, &mut header_info)?;
        }

        // Newfile patch: old_data_size == 0 means source is optional.
        let mut source_mmap = None;
        let mut old_file_fallback: Option<File> = None;
        #[cfg(all(feature = "benchmark", unix))]
        let mut windowed_source = None;
        let mut source_cursor: Option<Cursor<&[u8]>> = None;

        let source_file = match File::open(&self.source_path) {
            Ok(file) => Some(file),
            Err(err)
                if header_info.old_data_size == 0
                    && err.kind() == std::io::ErrorKind::NotFound
                    && matches!(
                        std::fs::symlink_metadata(&self.source_path),
                        Err(ref metadata_err)
                            if metadata_err.kind() == std::io::ErrorKind::NotFound
                    ) =>
            {
                None
            }
            Err(err) => return Err(err.into()),
        };
        if let Some(f) = source_file {
            let source_metadata = f.metadata()?;
            if destination_metadata
                .as_ref()
                .is_some_and(|destination| same_file_identity(&source_metadata, destination))
            {
                return Err(format!(
                    "source and destination refer to the same file: {path}",
                    path = self.source_path
                )
                .into());
            }
            let old_len = source_metadata.len();
            let expected_old_len = u64::try_from(header_info.old_data_size).map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "patch header has negative old data size",
                )
            })?;
            if old_len != expected_old_len {
                return Err(format!(
                    "input file size mismatch: expected {expected} bytes, got {old_len} bytes",
                    expected = expected_old_len
                )
                .into());
            }
            // mmap avoids per-cover seek/read calls while kernel readahead stays adaptive.
            // Falls back to File if mmap fails; page residency stays demand-driven.
            let use_mmap = matches!(source_backend, SourceBackend::Mmap);
            if old_len > 0 && use_mmap {
                let map_len = usize::try_from(old_len).map_err(|_| {
                    std::io::Error::other("source file is too large to map on this platform")
                })?;
                match unsafe {
                    crate::game_installer::assembly_opt::mmap_read_only_unchecked(&f, map_len)
                } {
                    Ok(guard) => {
                        source_mmap = Some(guard);
                        source_cursor = Some(Cursor::new(source_mmap.as_ref().unwrap().as_slice()));
                    }
                    Err(err) => {
                        log::debug!("source mmap failed, using File fallback: {err}");
                        old_file_fallback = Some(f);
                    }
                }
            } else {
                #[cfg(all(feature = "benchmark", unix))]
                if let SourceBackend::Windowed { window_size, stats } = source_backend {
                    windowed_source = Some(WindowedSource::new(f, window_size, 4, stats)?);
                } else {
                    old_file_fallback = Some(f);
                }
                #[cfg(not(all(feature = "benchmark", unix)))]
                {
                    old_file_fallback = Some(f);
                }
            }
        } else {
            old_file_fallback = Some(File::open("/dev/null")?);
        }

        let expected_size = u64::try_from(header_info.new_data_size).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "new_data_size is negative")
        })?;

        let destination_path = PathBuf::from(&self.dest_path);
        let parent = destination_path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| std::path::Path::new("."));
        let mut temp_builder = tempfile::Builder::new();
        temp_builder.prefix(".hdiffpatch-");
        #[cfg(unix)]
        temp_builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o666));
        let mut temp_output = temp_builder.tempfile_in(parent)?;
        if let Some(metadata) = destination_metadata.as_ref() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                temp_output
                    .as_file()
                    .set_permissions(std::fs::Permissions::from_mode(
                        metadata.permissions().mode() & 0o777,
                    ))?;
            }
            #[cfg(not(unix))]
            temp_output
                .as_file()
                .set_permissions(metadata.permissions())?;
        }
        let patch_result: Result<(), Box<dyn std::error::Error>> = (|| {
            let out_file = temp_output.as_file_mut();
            if expected_size > 0 {
                out_file.set_len(expected_size)?;
                if let Err(err) = crate::game_installer::sysio::preallocate(out_file, expected_size)
                {
                    log::debug!("destination preallocation failed: {err}");
                }
            }
            let mut out_writer = CountingWriter::new(BufWriter::with_capacity(
                super::super::FILE_WRITE_BUFFER_SIZE,
                BackgroundFlushWriter::new(out_file.try_clone()?),
            ));
            let old_source: &mut dyn SeekableRead = if let Some(cursor) = source_cursor.as_mut() {
                cursor
            } else {
                #[cfg(all(feature = "benchmark", unix))]
                if let Some(windowed) = windowed_source.as_mut() {
                    windowed
                } else {
                    old_file_fallback.as_mut().unwrap()
                }
                #[cfg(not(all(feature = "benchmark", unix)))]
                {
                    old_file_fallback.as_mut().unwrap()
                }
            };

            if header_info.is_single_compressed_diff {
                // Direct slice path avoids seek+read_exact vtable dispatch per cover chunk.
                let old_slice = source_mmap.as_ref().map(|g| g.as_slice());
                if let Some(slice) = old_slice {
                    super::patch_sf::PatchSF::new(header_info).patch_with_slice(
                        slice,
                        &mut out_writer,
                        &self.diff_path,
                        on_progress,
                    )?;
                } else {
                    super::patch_sf::PatchSF::new(header_info).patch(
                        old_source,
                        &mut out_writer,
                        &self.diff_path,
                        on_progress,
                    )?;
                }
            } else {
                let old_slice = source_mmap.as_ref().map(|g| g.as_slice());
                if let Some(slice) = old_slice {
                    super::patch_single::PatchSingle::new(header_info).patch_with_slice(
                        slice,
                        &mut out_writer,
                        &self.diff_path,
                        on_progress,
                    )?;
                } else {
                    super::patch_single::PatchSingle::new(header_info).patch(
                        old_source,
                        &mut out_writer,
                        &self.diff_path,
                        on_progress,
                    )?;
                }
            }
            out_writer.flush()?;

            let actual_size = out_writer.written();
            if actual_size != expected_size {
                return Err(std::io::Error::other(format!(
                    "Patch output size mismatch: expected {expected_size} bytes, got {actual_size} bytes",
                ))
                .into());
            }
            Ok(())
        })();
        source_cursor.take();
        source_mmap.take();
        if patch_result.is_err() {
            drop(temp_output);
            return patch_result;
        }
        patch_result?;
        temp_output.as_file().sync_all()?;
        temp_output
            .persist(&destination_path)
            .map_err(|err| err.error)?;
        #[cfg(unix)]
        if let Err(err) = File::open(parent).and_then(|directory| directory.sync_all()) {
            log::debug!("destination directory sync failed after atomic patch replacement: {err}");
        }

        Ok(())
    }

    pub(crate) fn try_get_version(str_val: &str) -> Result<i64, Box<dyn std::error::Error>> {
        let idx = str_val
            .find("HDIFF")
            .ok_or_else(|| format!("cannot find 'HDIFF' in: {str_val}"))?;
        let rest = &str_val[idx + "HDIFF".len()..];
        let num_str = rest.trim_start_matches(|c: char| !c.is_ascii_digit());
        num_str
            .parse::<i64>()
            .map_err(|_| format!("invalid version string: {num_str} (raw: {str_val})").into())
    }

    pub(crate) fn read_single_file_header(
        sr: &mut (impl Read + Seek),
        header_info: &mut HeaderInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        header_info.single_chunk_info = DiffSingleChunkInfo::default();
        header_info.new_data_size = sr.read_long_7bit()?;
        header_info.old_data_size = sr.read_long_7bit()?;
        if header_info.new_data_size < 0 || header_info.old_data_size < 0 {
            return Err("new_data_size or old_data_size is negative".into());
        }

        header_info.chunk_info.cover_count = sr.read_long_7bit()?;
        header_info.step_mem_size = sr.read_long_7bit()?;
        if header_info.chunk_info.cover_count < 0 {
            return Err("cover_count is negative".into());
        }
        if header_info.step_mem_size < 0 {
            return Err("step_mem_size is negative".into());
        }
        header_info.single_chunk_info.uncompressed_size = sr.read_long_7bit()?;
        header_info.single_chunk_info.compressed_size = sr.read_long_7bit()?;
        if header_info.single_chunk_info.uncompressed_size < 0 {
            return Err("uncompressed_size is negative".into());
        }
        if header_info.single_chunk_info.compressed_size < 0 {
            return Err("compressed_size is negative".into());
        }

        let pos = sr.stream_position()? as i64;
        header_info.single_chunk_info.diff_data_pos = pos;
        header_info.compressed_count = if header_info.single_chunk_info.compressed_size > 0 {
            1
        } else {
            0
        };
        Ok(())
    }

    pub(crate) fn read_non_single_file_header(
        sr: &mut (impl Read + Seek),
        header_info: &mut HeaderInfo,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let type_end_pos = sr.stream_position()? as i64;
        header_info.new_data_size = sr.read_long_7bit()?;
        header_info.old_data_size = sr.read_long_7bit()?;
        if header_info.new_data_size < 0 || header_info.old_data_size < 0 {
            return Err("new_data_size or old_data_size is negative".into());
        }

        Self::get_diff_chunk_info(sr, &mut header_info.chunk_info, type_end_pos)?;
        header_info.compressed_count = ((header_info.chunk_info.compress_cover_buf_size > 0)
            as i64)
            + ((header_info.chunk_info.compress_rle_ctrl_buf_size > 0) as i64)
            + ((header_info.chunk_info.compress_rle_code_buf_size > 0) as i64)
            + ((header_info.chunk_info.compress_new_data_diff_size > 0) as i64);
        Ok(())
    }

    pub(crate) fn get_diff_chunk_info(
        sr: &mut (impl Read + Seek),
        chunk_info: &mut DiffChunkInfo,
        type_end_pos: i64,
    ) -> Result<(), Box<dyn std::error::Error>> {
        *chunk_info = DiffChunkInfo::default();
        chunk_info.types_end_pos = type_end_pos;
        chunk_info.cover_count = sr.read_long_7bit()?;
        chunk_info.cover_buf_size = sr.read_long_7bit()?;
        chunk_info.compress_cover_buf_size = sr.read_long_7bit()?;
        chunk_info.rle_ctrl_buf_size = sr.read_long_7bit()?;
        chunk_info.compress_rle_ctrl_buf_size = sr.read_long_7bit()?;
        chunk_info.rle_code_buf_size = sr.read_long_7bit()?;
        chunk_info.compress_rle_code_buf_size = sr.read_long_7bit()?;
        chunk_info.new_data_diff_size = sr.read_long_7bit()?;
        chunk_info.compress_new_data_diff_size = sr.read_long_7bit()?;

        let fields: &[(&str, i64)] = &[
            ("cover_buf_size", chunk_info.cover_buf_size),
            (
                "compress_cover_buf_size",
                chunk_info.compress_cover_buf_size,
            ),
            ("rle_ctrl_buf_size", chunk_info.rle_ctrl_buf_size),
            (
                "compress_rle_ctrl_buf_size",
                chunk_info.compress_rle_ctrl_buf_size,
            ),
            ("rle_code_buf_size", chunk_info.rle_code_buf_size),
            (
                "compress_rle_code_buf_size",
                chunk_info.compress_rle_code_buf_size,
            ),
            ("new_data_diff_size", chunk_info.new_data_diff_size),
            (
                "compress_new_data_diff_size",
                chunk_info.compress_new_data_diff_size,
            ),
        ];
        for (name, val) in fields {
            if *val < 0 {
                return Err(format!("{name} is negative in diff chunk info").into());
            }
        }

        chunk_info.head_end_pos = sr.stream_position()? as i64;
        chunk_info.cover_end_pos = chunk_info
            .head_end_pos
            .checked_add(if chunk_info.compress_cover_buf_size > 0 {
                chunk_info.compress_cover_buf_size
            } else {
                chunk_info.cover_buf_size
            })
            .ok_or_else(|| -> Box<dyn std::error::Error> {
                "cover_end_pos overflow in diff chunk info".into()
            })?;
        Ok(())
    }
}

#[cfg(unix)]
fn same_file_identity(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;

    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_identity(_left: &std::fs::Metadata, _right: &std::fs::Metadata) -> bool {
    false
}

struct CountingWriter<W> {
    inner: W,
    written: u64,
}

/// File wrapper that submits completed output ranges for writeback while
/// the patcher keeps decoding, so the final durability fsync waits only
/// for the in-flight tail instead of writing the whole output. Each
/// `BACKGROUND_FLUSH_INTERVAL` bytes delivered to the kernel, the covered
/// range is handed to `sync_file_range` with `SYNC_FILE_RANGE_WRITE`,
/// which queues writeback without waiting for it to finish (it may still
/// block briefly on device-queue congestion). Durability semantics are
/// unchanged — the final `sync_all` before rename still flushes
/// everything, including metadata and the unsubmitted tail — this only
/// moves when the disk does the work, overlapping it with CPU-bound
/// decode. Earlier submission also caps dirty-page accumulation, keeping
/// the patcher under the kernel's dirty thresholds.
struct BackgroundFlushWriter {
    file: std::fs::File,
    /// Offset of the first byte not yet submitted for writeback.
    submitted_until: u64,
    /// File offset of the next byte this handle will write. The handle is
    /// the only writer, appends sequentially, and shares the fd's file
    /// description, so this tracks the kernel position after each write.
    next_offset: u64,
}

/// Bytes of output between background writeback submissions. The size
/// trades writeback-queue granularity against syscall count; it matches
/// the patcher's region cache flushes so submissions align with large
/// writes the BufWriter already passes through.
const BACKGROUND_FLUSH_INTERVAL: u64 = 4 << 20;

impl BackgroundFlushWriter {
    fn new(file: std::fs::File) -> Self {
        Self {
            file,
            submitted_until: 0,
            next_offset: 0,
        }
    }

    /// Queue writeback for all complete intervals written so far. Errors
    /// are non-fatal hints: the final sync_all re-covers the whole file.
    fn submit_pending(&mut self) {
        #[cfg(unix)]
        {
            let ready_end = self.next_offset / BACKGROUND_FLUSH_INTERVAL * BACKGROUND_FLUSH_INTERVAL;
            if ready_end > self.submitted_until {
                let len = ready_end - self.submitted_until;
                unsafe {
                    let _ = libc::sync_file_range(
                        self.file.as_raw_fd(),
                        self.submitted_until.try_into().unwrap_or(i64::MAX),
                        len.try_into().unwrap_or(i64::MAX),
                        libc::SYNC_FILE_RANGE_WRITE,
                    );
                }
                self.submitted_until = ready_end;
            }
        }
    }
}

impl std::io::Write for BackgroundFlushWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.file.write(buf)?;
        self.next_offset += written as u64;
        self.submit_pending();
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

#[cfg(test)]
mod output_tests {
    use super::{BackgroundFlushWriter, CountingWriter, BACKGROUND_FLUSH_INTERVAL};
    use std::io::{Cursor, Write};

    #[test]
    fn counting_writer_tracks_successfully_written_bytes() {
        let mut writer = CountingWriter::new(Cursor::new(Vec::new()));
        writer.write_all(b"patch output").unwrap();
        assert_eq!(writer.written(), 12);
        assert_eq!(writer.inner.into_inner(), b"patch output");
    }

    #[test]
    fn counting_writer_tracks_partial_writes() {
        struct PartialWriter {
            bytes: Vec<u8>,
            limit: usize,
        }

        impl Write for PartialWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let count = buf.len().min(self.limit);
                self.bytes.extend_from_slice(&buf[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let mut writer = CountingWriter::new(PartialWriter {
            bytes: Vec::new(),
            limit: 3,
        });
        writer.write_all(b"1234567").unwrap();
        assert_eq!(writer.written(), 7);
        assert_eq!(writer.inner.bytes, b"1234567");
    }

    /// The flush writer's submission accounting: only complete intervals
    /// are submitted, a single large write crossing several interval
    /// boundaries submits them in one batch, and the unaligned tail is
    /// left for the final sync_all.
    #[test]
    fn background_flush_submits_only_complete_intervals() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");

        let mut writer = BackgroundFlushWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap(),
        );
        assert_eq!(writer.submitted_until, 0);

        // 9 MiB in one write spans two full intervals plus a MiB tail.
        let big = vec![0xA5u8; BACKGROUND_FLUSH_INTERVAL as usize * 2 + (1 << 20)];
        writer.write_all(&big).unwrap();
        assert_eq!(writer.next_offset, big.len() as u64);
        assert_eq!(
            writer.submitted_until,
            BACKGROUND_FLUSH_INTERVAL * 2,
            "two complete intervals submitted; 1 MiB tail must wait"
        );

        // Small writes below the next boundary submit nothing further.
        writer.write_all(&[1, 2, 3, 4]).unwrap();
        assert_eq!(
            writer.submitted_until, BACKGROUND_FLUSH_INTERVAL * 2,
            "sub-interval writes must not advance the submitted range"
        );
        writer.flush().unwrap();

        // Bytes are not corrupted by the wrapper.
        let on_disk = std::fs::read(&path).unwrap();
        assert_eq!(on_disk.len(), big.len() + 4);
        assert_eq!(&on_disk[..8], &[0xA5; 8]);
        assert_eq!(&on_disk[big.len()..], &[1, 2, 3, 4]);
    }

    /// Sub-interval output never triggers a submission call and still
    /// writes correctly (the small-patch fast path has zero overhead
    /// beyond the offset counter).
    #[test]
    fn background_flush_small_output_never_submits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.bin");
        let mut writer = BackgroundFlushWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap(),
        );
        writer.write_all(b"tiny patch").unwrap();
        writer.flush().unwrap();
        assert_eq!(writer.submitted_until, 0);
        assert_eq!(std::fs::read(&path).unwrap(), b"tiny patch");
    }

    /// A boundary crossed by accumulated writes (not one big write)
    /// submits exactly the complete interval that has formed.
    #[test]
    fn background_flush_accumulates_across_writes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accum.bin");
        let mut writer = BackgroundFlushWriter::new(
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .truncate(true)
                .open(&path)
                .unwrap(),
        );
        let half = (BACKGROUND_FLUSH_INTERVAL / 2) as usize;
        writer.write_all(&vec![1u8; half]).unwrap();
        assert_eq!(writer.submitted_until, 0, "half an interval submits nothing");
        writer.write_all(&vec![2u8; half]).unwrap();
        assert_eq!(
            writer.submitted_until, BACKGROUND_FLUSH_INTERVAL,
            "two halves complete one interval"
        );
        writer.write_all(&vec![3u8; half]).unwrap();
        assert_eq!(
            writer.submitted_until, BACKGROUND_FLUSH_INTERVAL,
            "another half does not complete a second interval"
        );
        writer.flush().unwrap();
        assert_eq!(std::fs::read(&path).unwrap().len(), half * 3);
    }
}

impl<W> CountingWriter<W> {
    fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }

    fn written(&self) -> u64 {
        self.written
    }
}

impl<W: Write> Write for CountingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        self.written = self
            .written
            .checked_add(written as u64)
            .ok_or_else(|| std::io::Error::other("patch output byte count overflow"))?;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(all(test, feature = "benchmark"))]
mod windowed_source_tests {
    use super::{WindowStats, WindowedSource};
    use std::io::{Cursor, Read, Seek, SeekFrom};

    #[test]
    fn matches_cursor_across_windows_and_backtracking() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        let data = (0..97).map(|value| value as u8).collect::<Vec<_>>();
        std::fs::write(&path, &data).unwrap();
        let mut source = WindowedSource::new(
            std::fs::File::open(&path).unwrap(),
            7,
            3,
            std::sync::Arc::new(WindowStats::default()),
        )
        .unwrap();
        let mut oracle = Cursor::new(data);

        for seek in [
            SeekFrom::Start(65),
            SeekFrom::Start(3),
            SeekFrom::Current(18),
            SeekFrom::End(-11),
            SeekFrom::Start(0),
        ] {
            assert_eq!(source.seek(seek).unwrap(), oracle.seek(seek).unwrap());
            let mut actual = [0; 19];
            let mut expected = [0; 19];
            assert_eq!(
                source.read(&mut actual).unwrap(),
                oracle.read(&mut expected).unwrap()
            );
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn randomized_reads_and_evictions_match_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        let data = (0usize..4093)
            .map(|index| (index.wrapping_mul(37) % 251) as u8)
            .collect::<Vec<_>>();
        std::fs::write(&path, &data).unwrap();
        let mut source = WindowedSource::new(
            std::fs::File::open(&path).unwrap(),
            31,
            3,
            std::sync::Arc::new(WindowStats::default()),
        )
        .unwrap();
        let mut oracle = Cursor::new(data);
        let mut state = 0x1234_5678u32;
        for _ in 0..500 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let position = (state as usize % 4300) as u64;
            assert_eq!(
                source.seek(SeekFrom::Start(position)).unwrap(),
                oracle.seek(SeekFrom::Start(position)).unwrap()
            );
            let length = ((state >> 16) as usize % 97) + 1;
            let mut actual = vec![0; length];
            let mut expected = vec![0; length];
            let actual_len = source.read(&mut actual).unwrap();
            let expected_len = oracle.read(&mut expected).unwrap();
            assert_eq!(actual_len, expected_len);
            assert_eq!(&actual[..actual_len], &expected[..expected_len]);
        }
    }

    #[test]
    fn seek_past_end_reads_eof_and_negative_seek_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        std::fs::write(&path, b"abc").unwrap();
        let mut source = WindowedSource::new(
            std::fs::File::open(&path).unwrap(),
            2,
            1,
            std::sync::Arc::new(WindowStats::default()),
        )
        .unwrap();
        assert_eq!(source.seek(SeekFrom::Start(7)).unwrap(), 7);
        assert_eq!(source.read(&mut [0; 1]).unwrap(), 0);
        assert!(source.seek(SeekFrom::Current(-8)).is_err());
    }

    #[test]
    fn detects_source_truncation_after_open() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut source =
            WindowedSource::new(file, 4, 2, std::sync::Arc::new(WindowStats::default())).unwrap();
        std::fs::write(&path, b"a").unwrap();
        assert_eq!(
            source.read_exact(&mut [0; 4]).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn detects_truncation_mid_window_with_read_exact() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        std::fs::write(&path, b"abcdefgh").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let mut source =
            WindowedSource::new(file, 8, 1, std::sync::Arc::new(WindowStats::default())).unwrap();
        std::fs::write(&path, b"abcd").unwrap();
        assert_eq!(
            source.read_exact(&mut [0; 8]).unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof
        );
    }

    #[test]
    fn rejects_zero_window_or_slot_count() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        std::fs::write(&path, b"x").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let stats = std::sync::Arc::new(WindowStats::default());
        assert!(WindowedSource::new(file.try_clone().unwrap(), 0, 1, stats.clone()).is_err());
        assert!(WindowedSource::new(file, 1, 0, stats).is_err());
    }

    #[test]
    fn empty_source_is_immediately_at_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.bin");
        std::fs::write(&path, []).unwrap();
        let mut source = WindowedSource::new(
            std::fs::File::open(&path).unwrap(),
            4,
            1,
            std::sync::Arc::new(WindowStats::default()),
        )
        .unwrap();
        assert_eq!(source.read(&mut [0; 1]).unwrap(), 0);
    }
}

pub(crate) trait SeekableRead: Read + Seek {}
impl<T: Read + Seek> SeekableRead for T {}

#[cfg(feature = "benchmark")]
struct WindowSlot {
    base: Option<u64>,
    valid: usize,
    stamp: u64,
    data: Vec<u8>,
}

#[cfg(feature = "benchmark")]
/// Bounded positional-read cache for benchmark comparisons.
/// The source file must remain unchanged while the patch is applied.
struct WindowedSource {
    file: File,
    len: u64,
    position: u64,
    window_size: usize,
    max_slots: usize,
    clock: u64,
    slots: Vec<WindowSlot>,
    stats: std::sync::Arc<WindowStats>,
}

#[cfg(feature = "benchmark")]
impl WindowedSource {
    fn new(
        file: File,
        window_size: usize,
        slot_count: usize,
        stats: std::sync::Arc<WindowStats>,
    ) -> std::io::Result<Self> {
        if window_size == 0 || slot_count == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "window size and slot count must be positive",
            ));
        }
        let len = file.metadata()?.len();
        Ok(Self {
            file,
            len,
            position: 0,
            window_size,
            max_slots: slot_count,
            clock: 0,
            slots: Vec::with_capacity(slot_count),
            stats,
        })
    }

    fn slot_for_position(&mut self) -> std::io::Result<usize> {
        let base = self.position / self.window_size as u64 * self.window_size as u64;
        self.clock = self.clock.wrapping_add(1);
        if let Some(index) = self.slots.iter().position(|slot| slot.base == Some(base)) {
            self.stats
                .hits
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            self.slots[index].stamp = self.clock;
            return Ok(index);
        }
        self.stats
            .misses
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let index = if self.slots.len() < self.max_slots {
            self.slots.push(WindowSlot {
                base: None,
                valid: 0,
                stamp: self.clock,
                data: vec![0; self.window_size],
            });
            self.slots.len() - 1
        } else {
            self.slots
                .iter()
                .enumerate()
                .min_by_key(|(_, slot)| slot.stamp)
                .map(|(index, _)| index)
                .expect("positive slot count")
        };
        let slot = &mut self.slots[index];
        slot.base = None;
        slot.valid = 0;
        let mut filled = 0;
        while filled < self.window_size {
            let count = loop {
                match self
                    .file
                    .read_at(&mut slot.data[filled..], base + filled as u64)
                {
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    result => break result?,
                }
            };
            if count == 0 {
                break;
            }
            filled += count;
        }
        if base < self.len && filled == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "source file ended before its recorded length",
            ));
        }
        slot.base = Some(base);
        slot.valid = filled;
        slot.stamp = self.clock;
        self.stats
            .bytes_read
            .fetch_add(filled as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(index)
    }
}

#[cfg(feature = "benchmark")]
impl Read for WindowedSource {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        if output.is_empty() || self.position >= self.len {
            return Ok(0);
        }
        let mut copied = 0;
        while copied < output.len() && self.position < self.len {
            let index = self.slot_for_position()?;
            let offset = (self.position - self.slots[index].base.unwrap()) as usize;
            let count = (output.len() - copied)
                .min(self.slots[index].valid.saturating_sub(offset))
                .min(usize::try_from(self.len - self.position).unwrap_or(usize::MAX));
            if count == 0 {
                return if copied == 0 {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::UnexpectedEof,
                        "source file ended before its recorded length",
                    ))
                } else {
                    Ok(copied)
                };
            }
            output[copied..copied + count]
                .copy_from_slice(&self.slots[index].data[offset..offset + count]);
            self.position += count as u64;
            copied += count;
        }
        Ok(copied)
    }
}

#[cfg(feature = "benchmark")]
impl Seek for WindowedSource {
    fn seek(&mut self, from: SeekFrom) -> std::io::Result<u64> {
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(offset) => i128::from(self.position) + i128::from(offset),
            SeekFrom::End(offset) => i128::from(self.len) + i128::from(offset),
        };
        if position < 0 || position > i128::from(u64::MAX) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            ));
        }
        self.position = position as u64;
        Ok(self.position)
    }
}
