use super::HDiff;
use std::fs;

const NEW_FROM_EMPTY: &[u8] = b"This is brand new content created from nothing!";
const DIFF_EMPTY_TO_NEW: &[u8] = b"HDIFF13&zstd\x00/\x00\x00\x00\x00\x02\x00\x00\x00/\x00 .This is brand new content created from nothing!";

#[test]
fn hdiff_v13_zstd_empty_original() {
    let result = hdiff_e2e_apply(b"", DIFF_EMPTY_TO_NEW);
    assert_eq!(result, NEW_FROM_EMPTY);
}

#[test]
fn hdiff_v13_zstd_zero_cover_outputs_match_declared_size() {
    let old = vec![0x6d; 512];
    let mut changed = old.clone();
    changed[0] ^= 0xff;
    for (old, new) in [
        (&[][..], &[][..]),
        (&[][..], &[0xa5][..]),
        (old.as_slice(), changed.as_slice()),
    ] {
        let patch = build_v13_zstd_patch(old, new);
        if !old.is_empty() {
            assert!(patch_has_v13_covers(&patch));
        }
        let result = hdiff_e2e_apply(old, &patch);
        assert_eq!(result, new);
    }
}

#[test]
fn hdiff_existing_empty_source_applies_newfile_patch() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("empty-old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, b"").unwrap();
    fs::write(&diff_path, DIFF_EMPTY_TO_NEW).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );

    assert!(hdiff.apply(None));
    assert_eq!(fs::read(out_path).unwrap(), NEW_FROM_EMPTY);
}

#[test]
fn hdiff_wrong_old_size_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, b"old content").unwrap();
    fs::write(&diff_path, b"HDIFF13&zstd\0\x80").unwrap();

    let op = old_path.to_string_lossy().to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    assert!(
        !hdiff.apply(None),
        "should fail when old file size doesn't match"
    );
}

#[test]
fn hdiff_v13_cover_past_source_end_fails_without_replacing_destination() {
    let old = test_prng_data(87, 16);
    let cover_buf = [
        encode_tagged_1bit(0, 1),
        encode_7bit_varint(0),
        encode_7bit_varint(old.len() as i64),
    ]
    .concat();
    let rle_ctrl = encode_tagged_2bit(0, old.len() as i64 - 1);
    let mut patch = b"HDIFF13&nocomp\0".to_vec();
    for value in [
        old.len() as i64,
        old.len() as i64,
        1,
        cover_buf.len() as i64,
        0,
        rle_ctrl.len() as i64,
        0,
        0,
        0,
        0,
        0,
    ] {
        patch.extend_from_slice(&encode_7bit_varint(value));
    }
    patch.extend_from_slice(&cover_buf);
    patch.extend_from_slice(&rle_ctrl);

    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("bad-cover.hdiff");
    let out_path = dir.path().join("existing.bin");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, patch).unwrap();
    fs::write(&out_path, b"keep existing destination").unwrap();
    let mut patcher = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        patch_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(!patcher.apply(None));
    assert_eq!(fs::read(out_path).unwrap(), b"keep existing destination");
}

#[test]
fn hdiff_rejects_truncated_patch_stream() {
    let old = test_prng_data(35, 8192);
    let new = old.clone();
    let mut patch = build_v13_zstd_patch(&old, &new);
    let data_start = b"HDIFF13&zstd\0".len() + 2;
    patch.truncate(data_start + (patch.len() - data_start) / 2);

    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, &old).unwrap();
    fs::write(&diff_path, patch).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(!hdiff.apply(None));
    assert!(!out_path.exists(), "incomplete output should be removed");
}

#[test]
fn hdiff_invalid_diff_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, b"old content").unwrap();
    fs::write(&diff_path, b"NOT_A_HDIFF_FILE_CONTENTS").unwrap();

    let op = old_path.to_string_lossy().to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    assert!(!hdiff.apply(None), "should fail for invalid diff file");
}

#[test]
fn hdiff_truncated_header_fails_without_panicking() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, b"").unwrap();
    fs::write(&diff_path, b"HDIFF13&zstd\0\x80").unwrap();
    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );

    assert!(!hdiff.apply(None));
}

#[test]
fn hdiff_varint_overflow_in_old_data_size_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, b"").unwrap();

    let mut patch = b"HDIFF13&nocomp\0".to_vec();
    patch.extend_from_slice(&[
        0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f,
    ]);
    patch.extend_from_slice(&[0x00; 9]);
    fs::write(&diff_path, patch).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(!hdiff.apply(None));
}

#[test]
fn hdiff_varint_overflow_in_new_data_size_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, b"").unwrap();

    let mut patch = b"HDIFF13&nocomp\0".to_vec();
    patch.extend_from_slice(&[
        0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f, 0x00,
    ]);
    patch.extend_from_slice(&[0x00; 9]);
    fs::write(&diff_path, patch).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );
    assert!(!hdiff.apply(None));
}

#[test]
fn hdiff_detect_hdiff_magic_bytes() {
    assert!(DIFF_EMPTY_TO_NEW.starts_with(b"HDIFF"));
}

#[test]
fn hdiff_nonexistent_source_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&diff_path, b"HDIFF13&zstd\0\x80").unwrap();

    let op = dir
        .path()
        .join("nonexistent.bin")
        .to_string_lossy()
        .to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    assert!(
        !hdiff.apply(None),
        "should fail for nonexistent source file"
    );
}

#[test]
fn hdiff_missing_source_with_nonzero_old_size_fails_before_output_creation() {
    let dir = tempfile::tempdir().unwrap();
    let missing_old_path = dir.path().join("missing.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(
        &diff_path,
        b"HDIFF13&nocomp\0\x01\x01\x00\x00\x00\x00\x00\x00\x00\x00\x00",
    )
    .unwrap();

    let mut hdiff = HDiff::new(
        missing_old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );

    assert!(!hdiff.apply(None));
    assert!(!out_path.exists());
}
/// Newfile hdiff patches work without a source file.
#[test]
fn hdiff_newfile_no_source_file() {
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    // Write a newfile hdiff patch (old_data_size == 0)
    fs::write(&diff_path, DIFF_EMPTY_TO_NEW).unwrap();

    // Source file doesn't exist - should succeed for newfile hdiff
    let op = dir
        .path()
        .join("nonexistent.bin")
        .to_string_lossy()
        .to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    // old_data_size == 0 means the source is optional; the patcher should
    // not fail due to a missing source file.
    let result = hdiff.apply(None);
    assert!(result, "valid newfile patch should apply without a source");
    let output = fs::read(&out_path).unwrap();
    assert_eq!(output, NEW_FROM_EMPTY, "newfile hdiff output mismatch");
}

#[cfg(unix)]
#[test]
fn hdiff_newfile_dangling_source_symlink_fails() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let missing_target = dir.path().join("missing-target.bin");
    let source_link = dir.path().join("source-link.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    symlink(&missing_target, &source_link).unwrap();
    fs::write(&diff_path, DIFF_EMPTY_TO_NEW).unwrap();

    let mut hdiff = HDiff::new(
        source_link.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );

    assert!(!hdiff.apply(None));
    assert!(!out_path.exists());
}

#[test]
fn hdiff_nonexistent_diff_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, b"old content").unwrap();

    let op = old_path.to_string_lossy().to_string();
    let dp = dir
        .path()
        .join("nonexistent.hdiff")
        .to_string_lossy()
        .to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    assert!(!hdiff.apply(None), "should fail for nonexistent diff file");
}

#[test]
fn hdiff_same_source_and_dest_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("file.bin");
    let diff_path = dir.path().join("diff.hdiff");

    fs::write(&old_path, b"old content").unwrap();
    fs::write(&diff_path, DIFF_EMPTY_TO_NEW).unwrap();

    // Same source and dest path should fail.
    let path_str = old_path.to_string_lossy().to_string();
    let mut hdiff = HDiff::new(
        path_str.clone(),
        diff_path.to_string_lossy().to_string(),
        path_str,
    );
    assert!(
        !hdiff.apply(None),
        "should fail when source and destination are the same file"
    );
}

#[cfg(unix)]
#[test]
fn hdiff_hardlinked_source_and_destination_fail_without_truncation() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let alias_path = dir.path().join("alias.bin");
    let diff_path = dir.path().join("diff.hdiff");
    fs::write(&old_path, b"source bytes remain intact").unwrap();
    fs::hard_link(&old_path, &alias_path).unwrap();
    fs::write(&diff_path, DIFF_EMPTY_TO_NEW).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        alias_path.to_string_lossy().into_owned(),
    );

    assert!(!hdiff.apply(None));
    assert_eq!(fs::read(&old_path).unwrap(), b"source bytes remain intact");
    assert_eq!(
        fs::read(&alias_path).unwrap(),
        b"source bytes remain intact"
    );
}

#[cfg(unix)]
#[test]
fn hdiff_symlink_destination_is_rejected_without_changing_target() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let target_path = dir.path().join("target.bin");
    let symlink_path = dir.path().join("out.bin");
    let diff_path = dir.path().join("diff.hdiff");
    fs::write(&old_path, b"source bytes").unwrap();
    fs::write(&target_path, b"target bytes remain").unwrap();
    fs::write(&diff_path, DIFF_EMPTY_TO_NEW).unwrap();
    symlink(&target_path, &symlink_path).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        symlink_path.to_string_lossy().into_owned(),
    );

    assert!(!hdiff.apply(None));
    assert_eq!(fs::read(&target_path).unwrap(), b"target bytes remain");
    assert!(
        fs::symlink_metadata(&symlink_path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn hdiff_failed_patch_preserves_existing_destination() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, test_prng_data(91, 8192)).unwrap();
    fs::write(&out_path, b"previous output stays").unwrap();
    let mut patch = build_v13_zstd_patch(&test_prng_data(91, 8192), &test_prng_data(91, 8192));
    patch.truncate(b"HDIFF13&zstd\0".len() + 4);
    fs::write(&diff_path, patch).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );

    assert!(!hdiff.apply(None));
    assert_eq!(fs::read(&out_path).unwrap(), b"previous output stays");
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

#[test]
fn hdiff_success_replaces_destination_with_patch_output() {
    let old = test_prng_data(92, 8192);
    let new = old.clone();
    let patch = build_v13_zstd_patch(&old, &new);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, &old).unwrap();
    fs::write(&diff_path, patch).unwrap();
    fs::write(&out_path, b"previous output").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&out_path, fs::Permissions::from_mode(0o640)).unwrap();
    }

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().into_owned(),
        diff_path.to_string_lossy().into_owned(),
        out_path.to_string_lossy().into_owned(),
    );

    assert!(hdiff.apply(None));
    assert_eq!(fs::read(&out_path).unwrap(), new);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(&out_path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 3);
}

/// enumerate_cover_headers rejects cover_count > 0 with cover_size == 0.
#[test]
fn enumerate_cover_headers_cover_count_gt_zero_cover_size_zero() {
    use std::io::Cursor;
    let result = super::patch_core::enumerate_cover_headers(
        &mut Cursor::new(Vec::new()),
        0, // cover_size == 0
        1, // cover_count > 0
    );
    assert!(
        result.is_err(),
        "should return error when cover_count > 0 but cover_size == 0"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string()
            .contains("cover_count > 0 but cover_size is 0"),
        "error message should mention the specific condition, got: {}",
        err
    );
}

/// Test that negative cover_count is rejected
#[test]
fn enumerate_cover_headers_negative_cover_count_fails() {
    use std::io::Cursor;
    let result = super::patch_core::enumerate_cover_headers(&mut Cursor::new(Vec::new()), 100, -1);
    assert!(
        result.is_err(),
        "should return error for negative cover_count"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cover_count is negative"),
        "error message should mention negative cover_count, got: {}",
        err
    );
}

/// Test that negative cover_size is rejected
#[test]
fn enumerate_cover_headers_negative_cover_size_fails() {
    use std::io::Cursor;
    let result = super::patch_core::enumerate_cover_headers(&mut Cursor::new(Vec::new()), -10, 0);
    assert!(
        result.is_err(),
        "should return error for negative cover_size"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cover_size is negative"),
        "error message should mention negative cover_size, got: {}",
        err
    );
}

/// cover_count exceeding MAX_COVER_COUNT is rejected.
#[test]
fn enumerate_cover_headers_cover_count_exceeds_max() {
    use std::io::Cursor;
    // Use the same limit as patch_core.rs for the boundary test.
    const MAX_COVER_COUNT: i64 = 50_000_000;
    let result = super::patch_core::enumerate_cover_headers(
        &mut Cursor::new(Vec::new()),
        100,
        MAX_COVER_COUNT + 1,
    );
    assert!(
        result.is_err(),
        "should return error when cover_count exceeds maximum"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("cover_count exceeds safe maximum"),
        "error message should mention safe maximum, got: {}",
        err
    );
}

/// Test that enumerate_cover_headers accepts valid zero values.
/// When cover_count == 0, cover_size should be allowed to be 0 (no data to
/// read).
#[test]
fn enumerate_cover_headers_zero_count_zero_size_is_valid() {
    use std::io::Cursor;
    // When cover_count is 0, cover_size being 0 is valid (nothing to read)
    let result = super::patch_core::enumerate_cover_headers(
        &mut Cursor::new(Vec::new()),
        0, // cover_size == 0
        0, // cover_count == 0
    );
    assert!(
        result.is_ok(),
        "cover_count=0 with cover_size=0 should be valid"
    );
    let headers = result.unwrap();
    assert!(
        headers.is_empty(),
        "should have no headers when cover_count is 0"
    );
}

#[test]
fn enumerate_cover_headers_truncated_data_returns_error() {
    use std::io::Cursor;
    // Buffer has data but not enough for the first cover header entry.
    // cover_count=3 but only 2 bytes of data provided.
    let truncated = b"\x01\x02";
    let result = super::patch_core::enumerate_cover_headers(
        &mut Cursor::new(truncated.as_slice()),
        truncated.len() as i64,
        3,
    );
    assert!(result.is_err(), "should fail for truncated cover data");
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("underflow") || err.to_string().contains("truncated"),
        "error should mention underflow or truncation, got: {}",
        err
    );
}

/// Streaming iterator variant yields headers without collecting.
#[test]
fn enumerate_cover_headers_checked_streams_same_headers() {
    use std::io::Cursor;

    // Two headers: p_sign=0x01 (inc_old_pos=1), copy_length=0, cover_length=0.
    let data = [0x01u8, 0x00, 0x00, 0x01, 0x00, 0x00];
    let mut cursor = Cursor::new(&data[..]);
    let mut iter =
        super::patch_core::enumerate_cover_headers_checked(&mut cursor, data.len() as i64, 2)
            .expect("iterator construction");
    let h0 = iter.next().unwrap().expect("first header");
    assert_eq!(h0.old_pos, 1);
    assert_eq!(h0.new_pos, 0);
    assert_eq!(h0.cover_length, 0);
    let h1 = iter.next().unwrap().expect("second header");
    assert_eq!(h1.old_pos, 2);
    assert_eq!(h1.new_pos, 0);
    assert_eq!(h1.cover_length, 0);
    assert!(iter.next().is_none(), "should be exhausted after 2 headers");
}

/// zstd buffered mode decodes with a tight window log.
#[test]
fn get_clip_stream_zstd_buffered_tight_window_decodes_small_frame() {
    let original = b"small zstd frame for tight window test";
    let compressed = zstd::encode_all(std::io::Cursor::new(&original[..]), 3).unwrap();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("tight.zst");
    std::fs::write(&path, &compressed).unwrap();

    let file = std::fs::File::open(&path).unwrap();
    let (mut reader, file_bytes) = super::compression::get_clip_stream(
        file,
        super::CompressionMode::Zstd,
        0,
        original.len() as u64,
        compressed.len() as u64,
        true,
    )
    .unwrap();
    assert_eq!(file_bytes, compressed.len() as u64);
    let mut output = Vec::new();
    reader.read_to_end(&mut output).unwrap();
    assert_eq!(output, original);
}

/// LZ4 compression mode parses from string.
#[test]
fn compression_mode_lz4_parsing() {
    let mode: Result<super::CompressionMode, _> = "lz4".parse();
    assert!(mode.is_ok(), "lz4 should be a valid compression mode");
    assert_eq!(mode.unwrap(), super::CompressionMode::Lz4);
}

/// Test that Nocomp compression mode can be parsed
#[test]
fn compression_mode_nocomp_parsing() {
    let mode: Result<super::CompressionMode, _> = "nocomp".parse();
    assert!(mode.is_ok(), "nocomp should be a valid compression mode");
    assert_eq!(mode.unwrap(), super::CompressionMode::Nocomp);
}

/// Test that empty string defaults to Nocomp
#[test]
fn compression_mode_empty_defaults_to_nocomp() {
    let mode: Result<super::CompressionMode, _> = "".parse();
    assert!(mode.is_ok(), "empty string should be valid");
    assert_eq!(mode.unwrap(), super::CompressionMode::Nocomp);
}

/// Test that invalid compression mode returns error
#[test]
fn compression_mode_invalid_fails() {
    let mode: Result<super::CompressionMode, _> = "invalid_compression".parse();
    assert!(mode.is_err(), "invalid compression mode should fail");
}

/// Test all supported compression modes can be parsed
#[test]
fn compression_mode_all_supported_parse() {
    let modes = vec![
        ("nocomp", super::CompressionMode::Nocomp),
        ("", super::CompressionMode::Nocomp),
        ("zstd", super::CompressionMode::Zstd),
        ("zlib", super::CompressionMode::Zlib),
        ("lz4", super::CompressionMode::Lz4),
        ("LZ4", super::CompressionMode::Lz4), // case insensitive
        ("ZSTD", super::CompressionMode::Zstd),
    ];
    for (input, expected) in modes {
        let mode: Result<super::CompressionMode, _> = input.parse();
        assert!(
            mode.is_ok(),
            "parsing '{}' should succeed, got: {:?}",
            input,
            mode
        );
        assert_eq!(
            mode.unwrap(),
            expected,
            "parsed mode for '{}' should be {:?}",
            input,
            expected
        );
    }
}

/// Verify the padding logic for all compression modes
#[test]
fn padding_applies_only_to_zlib_mode() {
    use super::CompressionMode;

    // Padding is 1 for zlib, 0 for all other modes
    for mode in [
        CompressionMode::Nocomp,
        CompressionMode::Zstd,
        CompressionMode::Lz4,
    ] {
        let padding: u64 = match mode {
            CompressionMode::Zlib => 1,
            _ => 0,
        };
        assert_eq!(padding, 0, "non-zlib mode should have no padding");
    }

    let zlib_padding: u64 = match CompressionMode::Zlib {
        CompressionMode::Zlib => 1,
        _ => 0,
    };
    assert_eq!(zlib_padding, 1, "zlib mode should have padding of 1");
}

use super::{
    CompressionMode, CoverHeader, DiffChunkInfo, DiffSingleChunkInfo, HeaderInfo,
    MAX_ARRAY_POOL_LEN, MAX_ARRAY_POOL_SECOND_OFFSET, MAX_MEM_BUFFER_LEN, RleRefClip,
};

#[test]
fn cover_header_new_and_fields() {
    let ch = CoverHeader::new(1, 2, 3);
    assert_eq!(ch.old_pos, 1);
    assert_eq!(ch.new_pos, 2);
    assert_eq!(ch.cover_length, 3);
}

#[test]
fn cover_header_debug_and_clone() {
    let ch = CoverHeader::new(10, 20, 30);
    let _ = format!("{:?}", ch);
    let cloned = ch;
    assert_eq!(ch.old_pos, cloned.old_pos);
    assert_eq!(ch.new_pos, cloned.new_pos);
    assert_eq!(ch.cover_length, cloned.cover_length);
}

#[test]
fn rle_ref_clip_default_all_zeros() {
    let clip = RleRefClip::default();
    assert_eq!(clip.mem_copy_length, 0);
    assert_eq!(clip.mem_set_length, 0);
    assert_eq!(clip.mem_set_value, 0);
}

#[test]
fn rle_ref_clip_custom_values() {
    let clip = RleRefClip {
        mem_copy_length: 42,
        mem_set_length: 7,
        mem_set_value: 255,
    };
    assert_eq!(clip.mem_copy_length, 42);
    assert_eq!(clip.mem_set_length, 7);
    assert_eq!(clip.mem_set_value, 255);
}

#[test]
fn rle_ref_clip_copy_behavior() {
    let original = RleRefClip {
        mem_copy_length: 100,
        mem_set_length: 200,
        mem_set_value: 50,
    };
    let mut copy = original;
    copy.mem_copy_length = 999;
    copy.mem_set_value = 77;
    assert_eq!(original.mem_copy_length, 100);
    assert_eq!(original.mem_set_value, 50);
    assert_eq!(copy.mem_copy_length, 999);
    assert_eq!(copy.mem_set_value, 77);
}

#[test]
fn compression_mode_from_str_mixed_case() {
    assert_eq!(
        "Zstd".parse::<CompressionMode>().unwrap(),
        CompressionMode::Zstd
    );
    assert_eq!(
        "ZSTD".parse::<CompressionMode>().unwrap(),
        CompressionMode::Zstd
    );
    assert_eq!(
        "Zlib".parse::<CompressionMode>().unwrap(),
        CompressionMode::Zlib
    );
    assert_eq!(
        "ZLIB".parse::<CompressionMode>().unwrap(),
        CompressionMode::Zlib
    );
    assert_eq!(
        "Lz4".parse::<CompressionMode>().unwrap(),
        CompressionMode::Lz4
    );
    assert_eq!(
        "LZ4".parse::<CompressionMode>().unwrap(),
        CompressionMode::Lz4
    );
    assert_eq!(
        "Nocomp".parse::<CompressionMode>().unwrap(),
        CompressionMode::Nocomp
    );
    assert_eq!(
        "NOCOMP".parse::<CompressionMode>().unwrap(),
        CompressionMode::Nocomp
    );
}

#[test]
fn compression_mode_from_str_whitespace_fails() {
    assert!(" nocomp".parse::<CompressionMode>().is_err());
    assert!("nocomp ".parse::<CompressionMode>().is_err());
    assert!("  nocomp ".parse::<CompressionMode>().is_err());
    assert!("\tnocomp".parse::<CompressionMode>().is_err());
}

#[test]
fn compression_mode_debug_format() {
    assert_eq!(format!("{:?}", CompressionMode::Nocomp), "Nocomp");
    assert_eq!(format!("{:?}", CompressionMode::Zstd), "Zstd");
    assert_eq!(format!("{:?}", CompressionMode::Zlib), "Zlib");
    assert_eq!(format!("{:?}", CompressionMode::Lz4), "Lz4");
}

#[test]
fn compression_mode_default_equals_nocomp() {
    assert_eq!(CompressionMode::default(), CompressionMode::Nocomp);
}

#[test]
fn constants_correct_values() {
    assert_eq!(MAX_MEM_BUFFER_LEN, 1 << 20);
    assert_eq!(MAX_ARRAY_POOL_LEN, 1 << 20);
    assert_eq!(MAX_ARRAY_POOL_SECOND_OFFSET, MAX_ARRAY_POOL_LEN / 2);
}

#[test]
fn diff_chunk_info_default_all_zeros() {
    let info = DiffChunkInfo::default();
    assert_eq!(info.types_end_pos, 0);
    assert_eq!(info.cover_count, 0);
    assert_eq!(info.cover_buf_size, 0);
    assert_eq!(info.compress_cover_buf_size, 0);
    assert_eq!(info.rle_ctrl_buf_size, 0);
    assert_eq!(info.compress_rle_ctrl_buf_size, 0);
    assert_eq!(info.rle_code_buf_size, 0);
    assert_eq!(info.compress_rle_code_buf_size, 0);
    assert_eq!(info.new_data_diff_size, 0);
    assert_eq!(info.compress_new_data_diff_size, 0);
    assert_eq!(info.head_end_pos, 0);
    assert_eq!(info.cover_end_pos, 0);
}

#[test]
fn diff_single_chunk_info_default_all_zeros() {
    let info = DiffSingleChunkInfo::default();
    assert_eq!(info.uncompressed_size, 0);
    assert_eq!(info.compressed_size, 0);
    assert_eq!(info.diff_data_pos, 0);
}

#[test]
fn header_info_default_values() {
    let info = HeaderInfo::default();
    assert_eq!(info.comp_mode, CompressionMode::Nocomp);
    assert!(!info.is_single_compressed_diff);
    assert_eq!(info.step_mem_size, 0);
    assert_eq!(info.old_data_size, 0);
    assert_eq!(info.new_data_size, 0);
    assert_eq!(info.compressed_count, 0);
    assert_eq!(info.single_chunk_info.uncompressed_size, 0);
    assert_eq!(info.chunk_info.types_end_pos, 0);
}

#[test]
fn hdiff_new_stores_paths() {
    let hd = HDiff::new("src".into(), "diff".into(), "dst".into());
    assert_eq!(hd.source_path, "src");
    assert_eq!(hd.diff_path, "diff");
    assert_eq!(hd.dest_path, "dst");
}

#[test]
fn hdiff_is_send_sync() {
    fn assert_send<T: Send>() {}
    fn assert_sync<T: Sync>() {}
    assert_send::<HDiff>();
    assert_sync::<HDiff>();
}

/// CoverHeader::new is a plain constructor; it stores all values
/// including negative cover_length. Rejection of negative lengths
/// happens downstream in enumerate_cover_headers.
#[test]
fn cover_header_new_stores_negative_cover_length() {
    let ch = CoverHeader::new(0, 0, -5);
    assert_eq!(
        ch.cover_length, -5,
        "CoverHeader::new stores negative cover_length as-is"
    );
}

/// CoverHeader::new stores negative old_pos as-is; no rejection.
#[test]
fn cover_header_new_stores_negative_old_pos() {
    let ch = CoverHeader::new(-10, 0, 1);
    assert_eq!(
        ch.old_pos, -10,
        "CoverHeader::new stores negative old_pos as-is"
    );
}

/// CoverHeader::new with valid positive values (positive test).
#[test]
fn cover_header_new_accepts_valid_positive_values() {
    let ch = CoverHeader::new(100, 200, 300);
    assert_eq!(ch.old_pos, 100);
    assert_eq!(ch.new_pos, 200);
    assert_eq!(ch.cover_length, 300);
}

/// CoverHeader::new with extreme i64 values.
#[test]
fn cover_header_new_with_extreme_i64_values() {
    let ch = CoverHeader::new(i64::MAX, i64::MIN, i64::MAX);
    assert_eq!(ch.old_pos, i64::MAX);
    assert_eq!(ch.new_pos, i64::MIN);
    assert_eq!(ch.cover_length, i64::MAX);
}

/// CoverHeader::new with all-zero values (boundary positive test).
#[test]
fn cover_header_new_with_zero_values() {
    let ch = CoverHeader::new(0, 0, 0);
    assert_eq!(ch.old_pos, 0);
    assert_eq!(ch.new_pos, 0);
    assert_eq!(ch.cover_length, 0);
}

/// Empty input returns no headers.
#[test]
fn enumerate_cover_headers_empty_input_with_zero_covers_is_valid() {
    use std::io::Cursor;

    let mut c = Cursor::new(Vec::new());
    let result = super::patch_core::enumerate_cover_headers(&mut c, 0, 0);
    assert!(result.is_ok());
}

#[test]
fn enumerate_cover_headers_decodes_minimal_header() {
    use std::io::Cursor;

    let encoded_header = [0x00, 0x00, 0x02];
    let mut cursor = Cursor::new(encoded_header);
    let headers = super::patch_core::enumerate_cover_headers(&mut cursor, 3, 1).unwrap();
    assert_eq!(headers.len(), 1);
    assert_eq!(headers[0].old_pos, 0);
    assert_eq!(headers[0].new_pos, 0);
    assert_eq!(headers[0].cover_length, 2);
}

/// enumerate_cover_headers rejects negative old_pos from underflow.
#[test]
fn enumerate_cover_headers_old_pos_underflow_fails() {
    use std::io::Cursor;
    // p_sign = 0x81: bit7=1 (sign=1), bits0-5=1 (inc_old_pos=1),
    // bit6=0 (no continuation). old_pos = last_old_pos_back - 1 = -1.
    let buf = vec![
        0x81, // p_sign: sign=1, inc_old_pos=1 (varint tagged)
        0x00, // copy_length = 0
        0x00, // cover_length = 0
    ];
    let buf_len = buf.len();
    let mut c = Cursor::new(buf);
    let result = super::patch_core::enumerate_cover_headers(&mut c, buf_len as i64, 1);
    assert!(
        result.is_err(),
        "should fail for negative old_pos from underflow"
    );
    let err = result.unwrap_err();
    assert!(
        err.to_string().contains("negative old_pos"),
        "error should mention negative old_pos, got: {}",
        err
    );
}

/// Verify that zlib mode produces padding of 1 (used in patch_single for
/// compressed stream alignment).
#[test]
fn compression_mode_zlib_identifies_padding() {
    let padding: u64 = match CompressionMode::Zlib {
        CompressionMode::Zlib => 1,
        _ => 0,
    };
    assert_eq!(padding, 1, "zlib mode should produce padding of 1");
}

/// Verify that zstd mode produces no padding.
#[test]
fn compression_mode_zstd_identifies_no_padding() {
    let padding: u64 = match CompressionMode::Zstd {
        CompressionMode::Zlib => 1,
        _ => 0,
    };
    assert_eq!(padding, 0, "zstd mode should produce padding of 0");
}

/// Verify that nocomp mode produces no padding.
#[test]
fn compression_mode_nocomp_identifies_no_padding() {
    let padding: u64 = match CompressionMode::Nocomp {
        CompressionMode::Zlib => 1,
        _ => 0,
    };
    assert_eq!(padding, 0, "nocomp mode should produce padding of 0");
}

/// Verify that lz4 mode produces no padding.
#[test]
fn compression_mode_lz4_identifies_no_padding() {
    let padding: u64 = match CompressionMode::Lz4 {
        CompressionMode::Zlib => 1,
        _ => 0,
    };
    assert_eq!(padding, 0, "lz4 mode should produce padding of 0");
}

/// Only zlib mode results in padding for compressed streams; all other
/// modes have zero padding.
#[test]
fn compression_mode_only_zlib_has_padding() {
    let modes_and_expected_padding = [
        (CompressionMode::Nocomp, 0u64),
        (CompressionMode::Zstd, 0u64),
        (CompressionMode::Zlib, 1u64),
        (CompressionMode::Lz4, 0u64),
    ];
    for (mode, expected) in modes_and_expected_padding {
        let padding: u64 = match mode {
            CompressionMode::Zlib => 1,
            _ => 0,
        };
        assert_eq!(
            padding, expected,
            "mode {:?} should have padding of {}",
            mode, expected
        );
    }
}

/// Verify that the header line "HDIFF13&zstd" correctly parses to v13
/// with Zstd compression.
#[test]
fn hdiff_header_parses_v13_zstd_mode() {
    let parts: Vec<&str> = "HDIFF13&zstd".split('&').collect();
    assert_eq!(parts.len(), 2);
    let version = HDiff::try_get_version(parts[0]).unwrap();
    assert_eq!(version, 13);
    let mode: CompressionMode = parts[1].parse().unwrap();
    assert_eq!(mode, CompressionMode::Zstd);
}

/// Verify that the header line "HDIFF20&zlib" correctly parses to v20
/// with Zlib compression.
#[test]
fn hdiff_header_parses_v20_zlib_mode() {
    let parts: Vec<&str> = "HDIFF20&zlib".split('&').collect();
    assert_eq!(parts.len(), 2);
    let version = HDiff::try_get_version(parts[0]).unwrap();
    assert_eq!(version, 20);
    let mode: CompressionMode = parts[1].parse().unwrap();
    assert_eq!(mode, CompressionMode::Zlib);
}

/// Verify that "HDIFF13&nocomp" parses to v13 with Nocomp mode.
#[test]
fn hdiff_header_parses_v13_nocomp_mode() {
    let parts: Vec<&str> = "HDIFF13&nocomp".split('&').collect();
    assert_eq!(parts.len(), 2);
    let version = HDiff::try_get_version(parts[0]).unwrap();
    assert_eq!(version, 13);
    let mode: CompressionMode = parts[1].parse().unwrap();
    assert_eq!(mode, CompressionMode::Nocomp);
}

/// Verify that a 3-part header (with checksum) correctly identifies both
/// compression mode and checksum presence.
#[test]
fn hdiff_header_parses_3part_with_checksum() {
    let parts: Vec<&str> = "HDIFF20&zstd&fadler64".split('&').collect();
    assert_eq!(parts.len(), 3);
    let version = HDiff::try_get_version(parts[0]).unwrap();
    assert_eq!(version, 20);
    let mode: CompressionMode = parts[1].parse().unwrap();
    assert_eq!(mode, CompressionMode::Zstd);
    // Third part is the checksum mode
    assert_eq!(parts[2], "fadler64");
}

/// is_single_compressed_diff should be true for version 20 regardless of
/// compression mode.
#[test]
fn compression_mode_v20_sets_single_compressed_flag() {
    let is_single = 20 == 20;
    assert!(is_single, "v20 should set is_single_compressed_diff");
    let is_not_single = 13 == 20;
    assert!(
        !is_not_single,
        "v13 should not set is_single_compressed_diff"
    );
}

/// try_get_version without "HDIFF" in the string should fail.
#[test]
fn try_get_version_no_hdiff_fails() {
    let result = HDiff::try_get_version("random_string");
    assert!(result.is_err(), "should fail without HDIFF marker");
    let err = result.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("cannot find 'HDIFF'"), "msg={msg}");
}

/// try_get_version with "HDIFF" but no trailing digits should fail.
#[test]
fn try_get_version_hdiff_no_digits_fails() {
    let result = HDiff::try_get_version("HDIFF");
    assert!(result.is_err(), "should fail when no digits follow HDIFF");
    let err = result.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("invalid version string"), "msg={msg}");
}

/// try_get_version with non-numeric characters after HDIFF should fail.
#[test]
fn try_get_version_hdiff_non_numeric_fails() {
    let result = HDiff::try_get_version("HDIFFabc");
    assert!(result.is_err(), "should fail with non-numeric version");
    let err = result.unwrap_err();
    let msg = format!("{err}");
    assert!(msg.contains("invalid version string"), "msg={msg}");
}

/// try_get_version with "HDIFF13" parses to version 13.
#[test]
fn try_get_version_hdiff13_parses() {
    let version = HDiff::try_get_version("HDIFF13").unwrap();
    assert_eq!(version, 13);
}

/// try_get_version with "HDIFF20" parses to version 20.
#[test]
fn try_get_version_hdiff20_parses() {
    let version = HDiff::try_get_version("HDIFF20").unwrap();
    assert_eq!(version, 20);
}

/// try_get_version with only digits after HDIFF returns the correct version
/// number (no trailing non-digit characters).
#[test]
fn try_get_version_hdiff_version_only() {
    let version = HDiff::try_get_version("HDIFF42").unwrap();
    assert_eq!(version, 42);
}

/// read_single_file_header with valid data should populate all fields.
#[test]
fn read_single_file_header_success() {
    use std::io::Cursor;
    let data = vec![
        0x64, // new_data_size = 100
        0x32, // old_data_size = 50
        0x00, // cover_count = 0
        0x88, 0x00, // step_mem_size = 1024
        0x64, // uncompressed_size = 100
        0x00, // compressed_size = 0
    ];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    let result = HDiff::read_single_file_header(&mut cursor, &mut header_info);
    assert!(result.is_ok(), "valid header should succeed");
    assert_eq!(header_info.new_data_size, 100);
    assert_eq!(header_info.old_data_size, 50);
    assert_eq!(header_info.chunk_info.cover_count, 0);
    assert_eq!(header_info.step_mem_size, 1024);
    assert_eq!(header_info.single_chunk_info.uncompressed_size, 100);
    assert_eq!(header_info.single_chunk_info.compressed_size, 0);
}

/// read_single_file_header with truncated data should return an error.
#[test]
fn read_single_file_header_truncated_fails() {
    use std::io::Cursor;
    let data = vec![0x64]; // only new_data_size, nothing else
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    let result = HDiff::read_single_file_header(&mut cursor, &mut header_info);
    assert!(result.is_err(), "truncated data should fail");
}

/// read_single_file_header sets diff_data_pos to the stream position after
/// reading all header fields.
#[test]
fn read_single_file_header_sets_diff_data_pos() {
    use std::io::Cursor;
    let data = vec![0x64, 0x32, 0x00, 0x88, 0x00, 0x64, 0x00];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    HDiff::read_single_file_header(&mut cursor, &mut header_info).unwrap();
    assert_eq!(
        header_info.single_chunk_info.diff_data_pos, 7,
        "diff_data_pos should be at end of header (7 bytes)"
    );
}

/// When compressed_size is 0, compressed_count should be 0.
#[test]
fn read_single_file_header_compressed_count_zero() {
    use std::io::Cursor;
    let data = vec![0x64, 0x32, 0x00, 0x88, 0x00, 0x64, 0x00];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    HDiff::read_single_file_header(&mut cursor, &mut header_info).unwrap();
    assert_eq!(header_info.compressed_count, 0);
}

/// When compressed_size > 0, compressed_count should be 1.
#[test]
fn read_single_file_header_compressed_count_one() {
    use std::io::Cursor;
    let data = vec![0x64, 0x32, 0x00, 0x88, 0x00, 0x64, 0x01];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    HDiff::read_single_file_header(&mut cursor, &mut header_info).unwrap();
    assert_eq!(header_info.compressed_count, 1);
}

/// read_single_file_header with minimal non-zero values.
#[test]
fn read_single_file_header_minimal_values() {
    use std::io::Cursor;
    let data = vec![
        0x01, // new_data_size = 1
        0x01, // old_data_size = 1
        0x01, // cover_count = 1
        0x01, // step_mem_size = 1
        0x01, // uncompressed_size = 1
        0x01, // compressed_size = 1
    ];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    let result = HDiff::read_single_file_header(&mut cursor, &mut header_info);
    assert!(result.is_ok(), "minimal values should succeed");
    assert_eq!(header_info.new_data_size, 1);
    assert_eq!(header_info.old_data_size, 1);
    assert_eq!(header_info.chunk_info.cover_count, 1);
    assert_eq!(header_info.step_mem_size, 1);
    assert_eq!(header_info.single_chunk_info.uncompressed_size, 1);
    assert_eq!(header_info.single_chunk_info.compressed_size, 1);
    assert_eq!(header_info.compressed_count, 1);
}

/// read_non_single_file_header with valid data and all-zero chunk info.
#[test]
fn read_non_single_file_header_success() {
    use std::io::Cursor;
    let data = vec![
        0x64, 0x32, // new_data_size=100, old_data_size=50
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // chunk info all zeros
    ];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    let result = HDiff::read_non_single_file_header(&mut cursor, &mut header_info);
    assert!(result.is_ok(), "valid header should succeed");
    assert_eq!(header_info.new_data_size, 100);
    assert_eq!(header_info.old_data_size, 50);
    assert_eq!(header_info.chunk_info.cover_count, 0);
    assert_eq!(header_info.chunk_info.cover_buf_size, 0);
    assert_eq!(header_info.compressed_count, 0);
}

/// read_non_single_file_header with truncated data (only sizes, no chunk
/// info) should fail.
#[test]
fn read_non_single_file_header_truncated_fails() {
    use std::io::Cursor;
    let data = vec![0x64, 0x32]; // only sizes, no chunk info
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    let result = HDiff::read_non_single_file_header(&mut cursor, &mut header_info);
    assert!(result.is_err(), "truncated chunk info should fail");
}

/// read_non_single_file_header with all-zero chunk info yields
/// compressed_count = 0.
#[test]
fn read_non_single_file_header_compressed_count_zero() {
    use std::io::Cursor;
    let data = vec![
        0x64, 0x32, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    ];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut cursor, &mut header_info).unwrap();
    assert_eq!(header_info.compressed_count, 0);
}

/// read_non_single_file_header with one compress_*_size > 1 should report
/// compressed_count = 1.
#[test]
fn read_non_single_file_header_one_compressed_stream() {
    use std::io::Cursor;
    // cover_count=0, cover_buf_size=0, compress_cover_buf_size=2 (so > 1)
    let data = vec![
        0x64, 0x32, // new_data_size=100, old_data_size=50
        0x00, // cover_count = 0
        0x00, // cover_buf_size = 0
        0x02, // compress_cover_buf_size = 2 (> 1 -> counts as 1)
        0x00, // rle_ctrl_buf_size = 0
        0x00, // compress_rle_ctrl_buf_size = 0
        0x00, // rle_code_buf_size = 0
        0x00, // compress_rle_code_buf_size = 0
        0x00, // new_data_diff_size = 0
        0x00, // compress_new_data_diff_size = 0
    ];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut cursor, &mut header_info).unwrap();
    assert_eq!(
        header_info.compressed_count, 1,
        "one compress field > 1 should give count of 1"
    );
}

/// Per the HDiff format, any non-zero compress_*_size counts as a
/// compressed section. A value of 1 is valid (e.g., when a compressor
/// collapses 2+ bytes to a single byte). The compressed_count must
/// reflect this.
#[test]
fn read_non_single_file_header_one_byte_compressed_counts() {
    use std::io::Cursor;
    let data = vec![
        0x64, 0x32, // new_data_size=100, old_data_size=50
        0x00, // cover_count = 0
        0x00, // cover_buf_size = 0
        0x01, // compress_cover_buf_size = 1 (single compressed byte)
        0x00, // rle_ctrl_buf_size = 0
        0x00, // compress_rle_ctrl_buf_size = 0
        0x00, // rle_code_buf_size = 0
        0x00, // compress_rle_code_buf_size = 0
        0x00, // new_data_diff_size = 0
        0x00, // compress_new_data_diff_size = 0
    ];
    let mut cursor = Cursor::new(data);
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut cursor, &mut header_info).unwrap();
    assert_eq!(
        header_info.compressed_count, 1,
        "compress_cover_buf_size == 1 must count as compressed"
    );
}

// E2E patch application tests with generated fixtures.
// Uses the same encoding approach as the benchmark fixture generators.

/// Encode a non-negative i64 as a 7-bit varint (tag_bit=0).
fn encode_7bit_varint(v: i64) -> Vec<u8> {
    assert!(v >= 0);
    if v < 0x80 {
        return vec![v as u8];
    }
    let bits = 64 - (v as u64).leading_zeros() as usize;
    let groups = bits.div_ceil(7);
    let mut out = Vec::with_capacity(groups);
    for i in (0..groups).rev() {
        let chunk = ((v >> (i * 7)) & 0x7F) as u8;
        if i > 0 {
            out.push(chunk | 0x80);
        } else {
            out.push(chunk);
        }
    }
    out[0] |= 0x80;
    out
}

/// Encode tagged varint with tag_bit=1 (cover header delta format).
fn encode_tagged_1bit(sign: u8, value: i64) -> Vec<u8> {
    assert!(value >= 0);
    let mut out = Vec::new();
    if value < 64 {
        out.push((sign << 7) | (value as u8));
    } else {
        let bits = 64 - (value as u64).leading_zeros() as usize;
        let remaining_bits = bits - 6;
        let extra_groups = remaining_bits.div_ceil(7);
        let total_shift = extra_groups * 7;
        let first_payload = ((value >> total_shift) & 0x3F) as u8;
        out.push((sign << 7) | 0x40 | first_payload);
        for g in (0..extra_groups).rev() {
            let chunk = ((value >> (g * 7)) & 0x7F) as u8;
            if g > 0 {
                out.push(chunk | 0x80);
            } else {
                out.push(chunk);
            }
        }
    }
    out
}

/// Encode tagged varint with tag_bit=2 (RLE ctrl format).
fn encode_tagged_2bit(rle_type: u8, value: i64) -> Vec<u8> {
    assert!(value >= 0 && rle_type < 4);
    let mut out = Vec::new();
    if value < 32 {
        out.push((rle_type << 6) | (value as u8));
    } else {
        let bits = 64 - (value as u64).leading_zeros() as usize;
        let remaining_bits = bits - 5;
        let extra_groups = remaining_bits.div_ceil(7);
        let total_shift = extra_groups * 7;
        let first_payload = ((value >> total_shift) & 0x1F) as u8;
        out.push((rle_type << 6) | 0x20 | first_payload);
        for g in (0..extra_groups).rev() {
            let chunk = ((value >> (g * 7)) & 0x7F) as u8;
            if g > 0 {
                out.push(chunk | 0x80);
            } else {
                out.push(chunk);
            }
        }
    }
    out
}

/// Generate deterministic pseudo-random data.
fn test_prng_data(seed: u64, len: usize) -> Vec<u8> {
    let mut s = [
        seed,
        seed.wrapping_mul(6364136223846793005).wrapping_add(1),
        seed.wrapping_mul(1442695040888963407).wrapping_add(3),
        seed ^ 0xdeadbeefcafe1234,
    ];
    let mut out = Vec::with_capacity(len);
    for _ in 0..len {
        let result = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        out.push(result as u8);
    }
    out
}

/// Build a valid HDIFF20&nocomp patch from old/new byte slices.
fn build_v20_nocomp_patch(old: &[u8], new: &[u8]) -> Vec<u8> {
    let block_size = 256usize;
    let num_blocks = old.len() / block_size;

    let mut covers: Vec<(u64, u64, u64)> = Vec::new();
    let mut run_start: Option<usize> = None;
    for i in 0..num_blocks {
        let off = i * block_size;
        let same = old[off..off + block_size] == new[off..off + block_size];
        if same {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start {
            covers.push((
                (start * block_size) as u64,
                (start * block_size) as u64,
                ((i - start) * block_size) as u64,
            ));
            run_start = None;
        }
    }
    if let Some(start) = run_start {
        covers.push((
            (start * block_size) as u64,
            (start * block_size) as u64,
            ((num_blocks - start) * block_size) as u64,
        ));
    }

    let cover_count = covers.len();

    // Encode covers
    let mut cover_buf = Vec::new();
    let mut last_old_end = 0u64;
    let mut last_new_end = 0u64;
    for &(old_pos, new_pos, length) in &covers {
        let delta = old_pos.abs_diff(last_old_end);
        let sign = if old_pos >= last_old_end { 0 } else { 1 };
        cover_buf.extend_from_slice(&encode_tagged_1bit(sign, delta as i64));
        cover_buf.extend_from_slice(&encode_7bit_varint((new_pos - last_new_end) as i64));
        cover_buf.extend_from_slice(&encode_7bit_varint(length as i64));
        last_old_end = old_pos + length;
        last_new_end = new_pos + length;
    }

    // RLE: (len0=cover_length, lenv=0) per cover
    let mut rle_buf = Vec::new();
    for &(_, _, length) in &covers {
        rle_buf.extend_from_slice(&encode_7bit_varint(length as i64));
        rle_buf.extend_from_slice(&encode_7bit_varint(0));
    }

    // Diff stream
    let mut diff_stream = Vec::new();
    diff_stream.extend_from_slice(&encode_7bit_varint(cover_buf.len() as i64));
    diff_stream.extend_from_slice(&encode_7bit_varint(rle_buf.len() as i64));
    diff_stream.extend_from_slice(&cover_buf);
    diff_stream.extend_from_slice(&rle_buf);
    let mut prev_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_end {
            diff_stream.extend_from_slice(&new[prev_end as usize..new_pos as usize]);
        }
        prev_end = new_pos + length;
    }
    if prev_end < new.len() as u64 {
        diff_stream.extend_from_slice(&new[prev_end as usize..]);
    }

    let uncompressed_size = diff_stream.len() as i64;
    let step_mem_size = (cover_buf.len() + rle_buf.len()) as i64;

    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF20&nocomp\0");
    patch.extend_from_slice(&encode_7bit_varint(new.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(old.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(cover_count as i64));
    patch.extend_from_slice(&encode_7bit_varint(step_mem_size));
    patch.extend_from_slice(&encode_7bit_varint(uncompressed_size));
    patch.extend_from_slice(&encode_7bit_varint(0));
    patch.extend_from_slice(&diff_stream);
    patch
}

/// Build a valid HDIFF13&zstd patch from old/new byte slices.
fn build_v13_zstd_patch(old: &[u8], new: &[u8]) -> Vec<u8> {
    let block_size = 256usize;
    let num_blocks = old.len() / block_size;

    let mut covers: Vec<(u64, u64, u64)> = Vec::new();
    let mut run_start: Option<usize> = None;
    for i in 0..num_blocks {
        let off = i * block_size;
        let same = old[off..off + block_size] == new[off..off + block_size];
        if same {
            if run_start.is_none() {
                run_start = Some(i);
            }
        } else if let Some(start) = run_start {
            covers.push((
                (start * block_size) as u64,
                (start * block_size) as u64,
                ((i - start) * block_size) as u64,
            ));
            run_start = None;
        }
    }
    if let Some(start) = run_start {
        covers.push((
            (start * block_size) as u64,
            (start * block_size) as u64,
            ((num_blocks - start) * block_size) as u64,
        ));
    }

    let cover_count = covers.len() as i64;

    // Cover buffer
    let mut cover_buf = Vec::new();
    let mut last_old_pos_back = 0i64;
    let mut last_new_pos_back = 0i64;
    for &(old_pos, new_pos, length) in &covers {
        let (old_pos, new_pos, length) = (old_pos as i64, new_pos as i64, length as i64);
        let inc = (old_pos - last_old_pos_back).unsigned_abs() as i64;
        let sign: u8 = if old_pos >= last_old_pos_back { 0 } else { 1 };
        cover_buf.extend_from_slice(&encode_tagged_1bit(sign, inc));
        cover_buf.extend_from_slice(&encode_7bit_varint(new_pos - last_new_pos_back));
        cover_buf.extend_from_slice(&encode_7bit_varint(length));
        last_old_pos_back = old_pos + length;
        last_new_pos_back = new_pos + length;
    }

    // RLE ctrl: type-0 entries for gaps and covers
    let mut rle_ctrl_buf = Vec::new();
    let mut prev_new_end = 0i64;
    for &(_, new_pos, length) in &covers {
        let (new_pos, length) = (new_pos as i64, length as i64);
        let gap = new_pos - prev_new_end;
        if gap > 0 {
            rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, gap - 1));
        }
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, length - 1));
        prev_new_end = new_pos + length;
    }
    let tail_gap = new.len() as i64 - prev_new_end;
    if tail_gap > 0 {
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, tail_gap - 1));
    }

    // new_data_diff
    let mut new_data_diff = Vec::new();
    let mut prev_end = 0u64;
    for &(_, new_pos, length) in &covers {
        if new_pos > prev_end {
            new_data_diff.extend_from_slice(&new[prev_end as usize..new_pos as usize]);
        }
        prev_end = new_pos + length;
    }
    if prev_end < new.len() as u64 {
        new_data_diff.extend_from_slice(&new[prev_end as usize..]);
    }

    // Compress each section
    let cover_compressed = zstd::encode_all(cover_buf.as_slice(), 3).unwrap();
    let rle_ctrl_compressed = zstd::encode_all(rle_ctrl_buf.as_slice(), 3).unwrap();
    let new_data_compressed = zstd::encode_all(new_data_diff.as_slice(), 3).unwrap();

    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF13&zstd\0");
    patch.extend_from_slice(&encode_7bit_varint(new.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(old.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(cover_count));
    patch.extend_from_slice(&encode_7bit_varint(cover_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(cover_compressed.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(rle_ctrl_buf.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(rle_ctrl_compressed.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(0i64)); // rle_code_buf_size
    patch.extend_from_slice(&encode_7bit_varint(0i64)); // compress_rle_code_buf_size
    patch.extend_from_slice(&encode_7bit_varint(new_data_diff.len() as i64));
    patch.extend_from_slice(&encode_7bit_varint(new_data_compressed.len() as i64));
    patch.extend_from_slice(&cover_compressed);
    patch.extend_from_slice(&rle_ctrl_compressed);
    patch.extend_from_slice(&new_data_compressed);
    patch
}

fn build_v13_zstd_patch_with_single_cover(
    old: &[u8],
    new: &[u8],
    old_pos: usize,
    new_pos: usize,
    cover_length: usize,
) -> Vec<u8> {
    assert!(
        old_pos
            .checked_add(cover_length)
            .is_some_and(|end| end <= old.len())
    );
    assert!(
        new_pos
            .checked_add(cover_length)
            .is_some_and(|end| end <= new.len())
    );
    assert_eq!(
        &old[old_pos..old_pos + cover_length],
        &new[new_pos..new_pos + cover_length]
    );
    let old_pos_i64 = i64::try_from(old_pos).unwrap();
    let new_pos_i64 = i64::try_from(new_pos).unwrap();
    let cover_length_i64 = i64::try_from(cover_length).unwrap();
    let new_size = i64::try_from(new.len()).unwrap();
    let old_size = i64::try_from(old.len()).unwrap();

    let cover_buf = [
        encode_tagged_1bit(0, old_pos_i64),
        encode_7bit_varint(new_pos_i64),
        encode_7bit_varint(cover_length_i64),
    ]
    .concat();

    let mut rle_ctrl_buf = Vec::new();
    if new_pos > 0 {
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, new_pos_i64 - 1));
    }
    if cover_length > 0 {
        rle_ctrl_buf.extend_from_slice(&encode_tagged_2bit(0, cover_length_i64 - 1));
    }
    assert_eq!(new_pos_i64 + cover_length_i64, new_size);

    let mut new_data_diff = Vec::with_capacity(new.len() - cover_length);
    new_data_diff.extend_from_slice(&new[..new_pos]);
    assert_eq!(new_data_diff.len(), new_pos);

    let cover_compressed = zstd::encode_all(cover_buf.as_slice(), 3).unwrap();
    let rle_ctrl_compressed = zstd::encode_all(rle_ctrl_buf.as_slice(), 3).unwrap();
    let new_data_compressed = zstd::encode_all(new_data_diff.as_slice(), 3).unwrap();
    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF13&zstd\0");
    for value in [
        new_size,
        old_size,
        1,
        cover_buf.len() as i64,
        cover_compressed.len() as i64,
        rle_ctrl_buf.len() as i64,
        rle_ctrl_compressed.len() as i64,
        0,
        0,
        new_data_diff.len() as i64,
        new_data_compressed.len() as i64,
    ] {
        patch.extend_from_slice(&encode_7bit_varint(value));
    }
    patch.extend_from_slice(&cover_compressed);
    patch.extend_from_slice(&rle_ctrl_compressed);
    patch.extend_from_slice(&new_data_compressed);
    patch
}

fn build_v13_zstd_single_cover_rle_patch(
    old: &[u8],
    rle_type: u8,
    deltas: &[u8],
) -> (Vec<u8>, Vec<u8>) {
    assert!(matches!(rle_type, 2 | 3));
    assert!(!old.is_empty());
    if rle_type == 3 {
        assert_eq!(deltas.len(), old.len());
    } else {
        assert_eq!(deltas.len(), 1);
    }
    let new = old
        .iter()
        .enumerate()
        .map(|(index, byte)| byte.wrapping_add(deltas[if rle_type == 3 { index } else { 0 }]))
        .collect::<Vec<_>>();
    let cover_buf = [
        encode_tagged_1bit(0, 0),
        encode_7bit_varint(0),
        encode_7bit_varint(old.len() as i64),
    ]
    .concat();
    let rle_ctrl_buf = encode_tagged_2bit(rle_type, old.len() as i64 - 1);
    let cover_compressed = zstd::encode_all(cover_buf.as_slice(), 3).unwrap();
    let rle_ctrl_compressed = zstd::encode_all(rle_ctrl_buf.as_slice(), 3).unwrap();
    let mut patch = Vec::new();
    patch.extend_from_slice(b"HDIFF13&zstd\0");
    for value in [
        new.len() as i64,
        old.len() as i64,
        1,
        cover_buf.len() as i64,
        cover_compressed.len() as i64,
        rle_ctrl_buf.len() as i64,
        rle_ctrl_compressed.len() as i64,
        deltas.len() as i64,
        0,
        0,
        0,
    ] {
        patch.extend_from_slice(&encode_7bit_varint(value));
    }
    patch.extend_from_slice(&cover_compressed);
    patch.extend_from_slice(&rle_ctrl_compressed);
    patch.extend_from_slice(deltas);
    (patch, new)
}

fn patch_has_v13_covers(patch: &[u8]) -> bool {
    use super::parser::BinaryExtensions;

    let mut input = std::io::Cursor::new(patch);
    let header = input.read_string_to_null(512).unwrap();
    assert!(header.starts_with("HDIFF13&"));
    let mut info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut input, &mut info).unwrap();
    info.chunk_info.cover_count > 0
}

#[test]
fn hdiff_e2e_v20_nocomp_applies_correctly() {
    let old = test_prng_data(100, 8192);
    let mut new = old.clone();
    // Modify ~25% of blocks to exercise gap/new_data paths
    for i in (0..32).filter(|i| i % 4 == 0) {
        let off = i * 256;
        let replacement = test_prng_data(2000 + i as u64, 256);
        new[off..off + 256].copy_from_slice(&replacement);
    }

    let patch = build_v20_nocomp_patch(&old, &new);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("patch.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().to_string(),
        patch_path.to_string_lossy().to_string(),
        out_path.to_string_lossy().to_string(),
    );
    assert!(hdiff.apply(None), "v20 nocomp patch failed");

    let result = fs::read(&out_path).unwrap();
    assert_eq!(result, new, "v20 nocomp output mismatch");
}

/// An RLE run longer than the 1MB decode buffer must be drained in multiple
/// steps before the next ctrl byte is read. Regression: single 3MB cover
/// produced from an identical old/new pair previously desynchronized the
/// ctrl stream and failed with "failed to fill whole buffer".
#[test]
fn hdiff_e2e_v13_zstd_rle_run_exceeds_buffer_applies_correctly() {
    let old = test_prng_data(4000, 3 * 1024 * 1024);
    let new = old.clone();

    let patch = build_v13_zstd_patch(&old, &new);
    assert_eq!(
        hdiff_e2e_apply(&old, &patch),
        new,
        "oversized RLE run output mismatch"
    );
}

#[test]
fn hdiff_e2e_v13_zstd_large_gap_before_cover_applies_correctly() {
    let old = test_prng_data(4001, 1024);
    let gap = test_prng_data(4002, 3 * 1024 * 1024 + 17);
    let mut new = gap.clone();
    new.extend_from_slice(&old[256..768]);
    let patch = build_v13_zstd_patch_with_single_cover(&old, &new, 256, gap.len(), 512);
    let result = hdiff_e2e_apply(&old, &patch);
    assert_eq!(result, new);
}

#[test]
fn hdiff_v13_zero_length_final_cover_flushes_tail_progress() {
    use super::parser::BinaryExtensions;
    use super::patch_single::PatchSingle;
    use std::io::Cursor;

    let old = test_prng_data(4005, 16);
    let new = test_prng_data(4006, 64 * 1024 + 13);
    let patch = build_v13_zstd_patch_with_single_cover(&old, &new, old.len(), new.len(), 0);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old-zero-cover.bin");
    let patch_path = dir.path().join("zero-cover-tail.hdiff");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();
    let mut header_reader = Cursor::new(patch.as_slice());
    header_reader.read_string_to_null(512).unwrap();
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut header_reader, &mut header_info).unwrap();
    header_info.comp_mode = super::CompressionMode::Zstd;
    let patcher = PatchSingle::new(header_info);
    let mut source = fs::File::open(&old_path).unwrap();
    let mut output = Cursor::new(Vec::new());
    let progress = std::sync::Mutex::new(Vec::new());
    let callback = |written| progress.lock().unwrap().push(written);
    patcher
        .patch_with_chunk_size(
            &mut source,
            &mut output,
            patch_path.to_str().unwrap(),
            256,
            Some(&callback),
        )
        .unwrap();
    assert_eq!(output.into_inner(), new);
    assert_eq!(progress.into_inner().unwrap(), [new.len() as u64]);
}

#[test]
fn hdiff_v13_chunked_regions_match_across_rle_and_cover_boundaries() {
    use super::parser::BinaryExtensions;
    use super::patch_single::PatchSingle;
    use std::io::Cursor;

    let old = test_prng_data(4010, 1024);
    let mut new = old.clone();
    new[256..512].copy_from_slice(&test_prng_data(4011, 256));
    new.extend_from_slice(&test_prng_data(4012, 13));
    let patch = build_v13_zstd_patch(&old, &new);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("patch.hdiff");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();

    let mut header_reader = Cursor::new(patch.as_slice());
    header_reader.read_string_to_null(512).unwrap();
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut header_reader, &mut header_info).unwrap();
    header_info.comp_mode = super::CompressionMode::Zstd;
    assert_eq!(header_info.old_data_size, old.len() as i64);
    assert_eq!(header_info.new_data_size, new.len() as i64);
    assert_eq!(header_info.chunk_info.cover_count, 2);
    let patcher = PatchSingle::new(header_info);

    for chunk_size in [1, 7, 255, 256, 257, 4096] {
        let mut file_source = fs::File::open(&old_path).unwrap();
        let mut file_output = Cursor::new(Vec::new());
        let file_progress = std::sync::Mutex::new(Vec::new());
        let file_callback = |written| file_progress.lock().unwrap().push(written);
        patcher
            .patch_with_chunk_size(
                &mut file_source,
                &mut file_output,
                patch_path.to_str().unwrap(),
                chunk_size,
                Some(&file_callback),
            )
            .unwrap_or_else(|error| panic!("file chunk={chunk_size}: {error:?}"));
        assert_eq!(file_output.into_inner(), new, "file chunk={chunk_size}");
        let file_progress = file_progress.into_inner().unwrap();
        assert!(file_progress.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(file_progress.last(), Some(&(new.len() as u64)));
        assert_eq!(
            file_progress
                .iter()
                .filter(|&&written| written == new.len() as u64)
                .count(),
            1
        );

        let mut slice_output = Cursor::new(Vec::new());
        let slice_progress = std::sync::Mutex::new(Vec::new());
        let slice_callback = |written| slice_progress.lock().unwrap().push(written);
        patcher
            .patch_with_slice_and_chunk_size(
                &old,
                &mut slice_output,
                patch_path.to_str().unwrap(),
                chunk_size,
                Some(&slice_callback),
            )
            .unwrap_or_else(|error| panic!("slice chunk={chunk_size}: {error:?}"));
        assert_eq!(slice_output.into_inner(), new, "slice chunk={chunk_size}");
        let slice_progress = slice_progress.into_inner().unwrap();
        assert!(slice_progress.windows(2).all(|pair| pair[0] <= pair[1]));
        assert_eq!(slice_progress.last(), Some(&(new.len() as u64)));
        assert_eq!(
            slice_progress
                .iter()
                .filter(|&&written| written == new.len() as u64)
                .count(),
            1
        );
    }
    let mut source = fs::File::open(&old_path).unwrap();
    let error = patcher
        .patch_with_chunk_size(
            &mut source,
            &mut Cursor::new(Vec::new()),
            patch_path.to_str().unwrap(),
            0,
            None,
        )
        .unwrap_err();
    assert!(error.to_string().contains("region chunk size is zero"));
    let error = patcher
        .patch_with_chunk_size(
            &mut fs::File::open(&old_path).unwrap(),
            &mut Cursor::new(Vec::new()),
            patch_path.to_str().unwrap(),
            4 * 1024 * 1024 + 1,
            None,
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exceeds the production cache limit")
    );
}

#[test]
fn hdiff_v13_chunked_large_cover_preserves_long_rle_state() {
    use super::parser::BinaryExtensions;
    use super::patch_single::PatchSingle;
    use std::io::{Cursor, Seek};

    let old = test_prng_data(4020, 4 * 1024 * 1024);
    let patch = build_v13_zstd_patch(&old, &old);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old-large.bin");
    let patch_path = dir.path().join("identity-large.hdiff");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();
    let mut header_reader = Cursor::new(patch.as_slice());
    header_reader.read_string_to_null(512).unwrap();
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut header_reader, &mut header_info).unwrap();
    header_info.comp_mode = super::CompressionMode::Zstd;
    assert_eq!(header_info.old_data_size, old.len() as i64);
    assert_eq!(header_info.new_data_size, old.len() as i64);
    assert_eq!(header_info.chunk_info.cover_count, 1);
    assert_eq!(
        header_info.chunk_info.cover_buf_size,
        (encode_tagged_1bit(0, 0).len()
            + encode_7bit_varint(0).len()
            + encode_7bit_varint(old.len() as i64).len()) as i64
    );
    let mut cover_clips = PatchSingle::new(header_info.clone())
        .open_clips(patch_path.to_str().unwrap())
        .unwrap();
    let mut decoded_cover = Vec::new();
    std::io::Read::read_to_end(&mut *cover_clips[0], &mut decoded_cover).unwrap();
    assert_eq!(
        decoded_cover,
        [
            encode_tagged_1bit(0, 0),
            encode_7bit_varint(0),
            encode_7bit_varint(old.len() as i64),
        ]
        .concat()
    );
    drop(cover_clips);
    let mut cover_clips = PatchSingle::new(header_info.clone())
        .open_clips(patch_path.to_str().unwrap())
        .unwrap();
    let mut cover_headers = super::patch_core::enumerate_cover_headers_checked(
        &mut *cover_clips[0],
        header_info.chunk_info.cover_buf_size,
        header_info.chunk_info.cover_count,
    )
    .unwrap();
    let cover = cover_headers.next().unwrap().unwrap();
    assert_eq!(cover.new_pos, 0);
    assert_eq!(cover.cover_length, old.len() as i64);
    let patcher = PatchSingle::new(header_info);

    for chunk_size in [512 * 1024, 1024 * 1024] {
        let mut source = fs::File::open(&old_path).unwrap();
        let mut output = Cursor::new(Vec::new());
        let progress = std::sync::Mutex::new(Vec::new());
        let callback = |written| progress.lock().unwrap().push(written);
        patcher
            .patch_with_chunk_size(
                &mut source,
                &mut output,
                patch_path.to_str().unwrap(),
                chunk_size,
                Some(&callback),
            )
            .unwrap_or_else(|error| {
                panic!(
                    "large cover chunk={chunk_size}, source_pos={}: {error:?}",
                    source.stream_position().unwrap()
                )
            });
        assert_eq!(output.into_inner(), old);
        let progress = progress.into_inner().unwrap();
        assert_eq!(progress.last(), Some(&(old.len() as u64)));
        assert_eq!(
            progress
                .iter()
                .filter(|&&value| value == old.len() as u64)
                .count(),
            1
        );
    }
}

#[test]
fn hdiff_v13_chunked_large_tail_matches_expected_output() {
    use super::parser::BinaryExtensions;
    use super::patch_single::PatchSingle;
    use std::io::Cursor;

    let old = test_prng_data(4021, 3 * 1024 * 1024);
    let mut new = old.clone();
    new[256..].fill(0x5a);
    let patch = build_v13_zstd_patch(&old, &new);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old-tail.bin");
    let patch_path = dir.path().join("tail.hdiff");
    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();
    let mut header_reader = Cursor::new(patch.as_slice());
    header_reader.read_string_to_null(512).unwrap();
    let mut header_info = HeaderInfo::default();
    HDiff::read_non_single_file_header(&mut header_reader, &mut header_info).unwrap();
    header_info.comp_mode = super::CompressionMode::Zstd;
    let patcher = PatchSingle::new(header_info);

    for chunk_size in [512 * 1024, 1024 * 1024] {
        let mut source = fs::File::open(&old_path).unwrap();
        let mut output = Cursor::new(Vec::new());
        patcher
            .patch_with_chunk_size(
                &mut source,
                &mut output,
                patch_path.to_str().unwrap(),
                chunk_size,
                None,
            )
            .unwrap();
        assert_eq!(output.into_inner(), new);
    }
}

#[test]
fn hdiff_v13_chunked_additive_and_literal_rle_runs_preserve_state() {
    use super::parser::BinaryExtensions;
    use super::patch_single::PatchSingle;
    use std::io::Cursor;

    let old = test_prng_data(4030, 1027);
    for (rle_type, deltas) in [
        (2, vec![0x37]),
        (
            3,
            (0..old.len())
                .map(|index| (index as u8).wrapping_mul(13))
                .collect(),
        ),
    ] {
        let (patch, expected) = build_v13_zstd_single_cover_rle_patch(&old, rle_type, &deltas);
        let dir = tempfile::tempdir().unwrap();
        let old_path = dir.path().join("old-rle.bin");
        let patch_path = dir.path().join("rle.hdiff");
        fs::write(&old_path, &old).unwrap();
        fs::write(&patch_path, &patch).unwrap();
        let mut header_reader = Cursor::new(patch.as_slice());
        header_reader.read_string_to_null(512).unwrap();
        let mut header_info = HeaderInfo::default();
        HDiff::read_non_single_file_header(&mut header_reader, &mut header_info).unwrap();
        header_info.comp_mode = super::CompressionMode::Zstd;
        let patcher = PatchSingle::new(header_info);

        for chunk_size in [1, 7, 127, 128, 255, 256, 257, 1024] {
            let mut source = fs::File::open(&old_path).unwrap();
            let mut output = Cursor::new(Vec::new());
            patcher
                .patch_with_chunk_size(
                    &mut source,
                    &mut output,
                    patch_path.to_str().unwrap(),
                    chunk_size,
                    None,
                )
                .unwrap_or_else(|error| {
                    panic!("RLE type={rle_type}, chunk={chunk_size}: {error:?}")
                });
            assert_eq!(
                output.into_inner(),
                expected,
                "RLE type={rle_type}, chunk={chunk_size}"
            );
        }
    }
}

#[test]
fn hdiff_e2e_v13_zstd_identity_patch_applies_correctly() {
    let original = test_prng_data(31, 16 * 1024);
    let patch = build_v13_zstd_patch(&original, &original);
    assert_eq!(hdiff_e2e_apply(&original, &patch), original);
}

#[test]
fn hdiff_e2e_v13_zstd_sparse_source_covers() {
    let old = test_prng_data(34, 64 * 1024);
    let mut new = old.clone();
    for block in (0..old.len() / 256).step_by(6) {
        new[block * 256] ^= 0xff;
    }
    let patch = build_v13_zstd_patch(&old, &new);
    assert!(patch_has_v13_covers(&patch));
    assert_eq!(hdiff_e2e_apply(&old, &patch), new);
}

#[test]
fn hdiff_e2e_v13_zstd_handles_new_data_tail() {
    let old = test_prng_data(32, 8192);
    let mut new = old.clone();
    new[4096..].fill(0x5a);
    new.extend_from_slice(b"partial tail");
    let patch = build_v13_zstd_patch(&old, &new);
    assert_eq!(hdiff_e2e_apply(&old, &patch), new);
}

/// Shared apply helper for e2e tests.
#[cfg(test)]
fn hdiff_e2e_apply(old: &[u8], patch: &[u8]) -> Vec<u8> {
    use std::io::Read;
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("patch.hdiff");
    let out_path = dir.path().join("out.bin");
    fs::write(&old_path, old).unwrap();
    fs::write(&patch_path, patch).unwrap();
    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().to_string(),
        patch_path.to_string_lossy().to_string(),
        out_path.to_string_lossy().to_string(),
    );
    assert!(
        hdiff.apply(None),
        "hdiff apply failed: {}/{}",
        old.len(),
        patch.len()
    );
    let mut result = Vec::new();
    fs::File::open(&out_path)
        .unwrap()
        .read_to_end(&mut result)
        .unwrap();
    result
}

#[test]
fn hdiff_e2e_v13_zstd_applies_correctly() {
    let old = test_prng_data(200, 8192);
    let mut new = old.clone();
    // Modify ~25% of blocks
    for i in (0..32).filter(|i| i % 4 == 1) {
        let off = i * 256;
        let replacement = test_prng_data(3000 + i as u64, 256);
        new[off..off + 256].copy_from_slice(&replacement);
    }

    let patch = build_v13_zstd_patch(&old, &new);
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let patch_path = dir.path().join("patch.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, &old).unwrap();
    fs::write(&patch_path, &patch).unwrap();

    let mut hdiff = HDiff::new(
        old_path.to_string_lossy().to_string(),
        patch_path.to_string_lossy().to_string(),
        out_path.to_string_lossy().to_string(),
    );
    assert!(hdiff.apply(None), "v13 zstd patch failed");

    let result = fs::read(&out_path).unwrap();
    assert_eq!(result, new, "v13 zstd output mismatch");
}
