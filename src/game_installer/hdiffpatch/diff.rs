use std::fs::File;
use std::io::{BufWriter, Cursor, Read, Seek, SeekFrom, Write};
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
        match self.apply_inner(on_progress.as_ref().map(|cb| cb.as_ref())) {
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
            if old_len > 0 {
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
                old_file_fallback = Some(f);
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
                out_file.try_clone()?,
            ));
            let old_source: &mut dyn SeekableRead = match source_cursor.as_mut() {
                Some(c) => c,
                None => old_file_fallback.as_mut().unwrap(),
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

#[cfg(test)]
mod output_tests {
    use super::CountingWriter;
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

pub(crate) trait SeekableRead: Read + Seek {}
impl<T: Read + Seek> SeekableRead for T {}
