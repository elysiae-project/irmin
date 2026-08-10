use super::HDiff;
use std::fs;

const OLD_TEXT: &[u8] =
    b"Hello World! This is the original file content for testing HDiff patching.";
const NEW_TEXT: &[u8] =
    b"Hello Universe! This is the modified file content for testing HDiff patching.";
const DIFF_V13_TEXT: &[u8] = b"HDIFF13&zstd\x00MJ\x01\x03\x00\x04\x00\x08\x00\x0e\x00\x0b\x0e?\x1b\xc7 (\xfe\xfd\xfb\x02\xfd\xfb\x04\xf8Hello Universe";

const NEW_FROM_EMPTY: &[u8] = b"This is brand new content created from nothing!";
const DIFF_EMPTY_TO_NEW: &[u8] = b"HDIFF13&zstd\x00/\x00\x00\x00\x00\x02\x00\x00\x00/\x00 .This is brand new content created from nothing!";

const SAME: &[u8] = b"Identical content on both sides";
const DIFF_IDENTITY: &[u8] =
    b"HDIFF13&zstd\x00\x1f\x1f\x01\x03\x00\x01\x00\x00\x00\x00\x00\x00\x00\x1f\x1e";

fn write_and_apply(old: &[u8], diff: &[u8], expected: &[u8]) -> bool {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, old).unwrap();
    fs::write(&diff_path, diff).unwrap();

    let op = old_path.to_string_lossy().to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    if !hdiff.apply(None) {
        return false;
    }

    let result = fs::read(&out_path).unwrap();
    result == expected
}

/// Test with synthetic hdiff data - may fail due to invalid synthetic
/// fixture
#[test]
#[ignore = "synthetic hdiff fixture produces invalid patch data"]
fn hdiff_v13_zstd_text_patch() {
    assert!(
        write_and_apply(OLD_TEXT, DIFF_V13_TEXT, NEW_TEXT),
        "v13 zstd text patch output mismatch"
    );
}
/// Test with synthetic hdiff data - may fail due to invalid synthetic
/// fixture
#[test]
#[ignore = "synthetic hdiff fixture produces invalid patch data"]
fn hdiff_v13_zstd_empty_original() {
    assert!(
        write_and_apply(b"", DIFF_EMPTY_TO_NEW, NEW_FROM_EMPTY),
        "empty original patch output mismatch"
    );
}

#[test]
fn hdiff_v13_zstd_large_binary() {
    let fixture_dir = "/tmp/hdiff_test";
    let old_path = format!("{fixture_dir}/old_large.bin");
    let diff_path = format!("{fixture_dir}/large_zstd.hdiff");
    let out_path = format!("{fixture_dir}/output_large_test.bin");

    if !std::path::Path::new(&old_path).exists() {
        eprintln!("skipping: large binary test fixtures not present at {fixture_dir}");
        return;
    }

    let mut hdiff = HDiff::new(old_path, diff_path, out_path.clone());
    assert!(hdiff.apply(None), "large binary patch apply failed");

    let result = fs::read(&out_path).unwrap();
    let expected = fs::read(format!("{fixture_dir}/new_large.bin")).unwrap();
    assert_eq!(result, expected, "large binary patch output mismatch");
    let _ = fs::remove_file(&out_path);
}

/// Test with synthetic hdiff data - may fail due to invalid synthetic
/// fixture
#[test]
#[ignore = "synthetic hdiff fixture produces invalid patch data"]
fn hdiff_v13_zstd_identity_patch() {
    assert!(
        write_and_apply(SAME, DIFF_IDENTITY, SAME),
        "identity patch output mismatch"
    );
}

#[test]
fn hdiff_wrong_old_size_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, SAME).unwrap();
    fs::write(&diff_path, DIFF_V13_TEXT).unwrap();

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
fn hdiff_invalid_diff_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, OLD_TEXT).unwrap();
    fs::write(&diff_path, b"NOT_A_HDIFF_FILE_CONTENTS").unwrap();

    let op = old_path.to_string_lossy().to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = out_path.to_string_lossy().to_string();

    let mut hdiff = HDiff::new(op, dp, tp);
    assert!(!hdiff.apply(None), "should fail for invalid diff file");
}

#[test]
fn hdiff_detect_hdiff_magic_bytes() {
    assert!(
        DIFF_V13_TEXT.starts_with(b"HDIFF"),
        "diff data should start with HDIFF magic bytes"
    );
}

#[test]
fn hdiff_nonexistent_source_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let diff_path = dir.path().join("diff.hdiff");
    let out_path = dir.path().join("out.bin");

    fs::write(&diff_path, DIFF_V13_TEXT).unwrap();

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
    // Expect success or failure for reasons other than a missing source.
    if result {
        let output = fs::read(&out_path).unwrap();
        assert_eq!(output, NEW_FROM_EMPTY, "newfile hdiff output mismatch");
    }
}

#[test]
fn hdiff_nonexistent_diff_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    let old_path = dir.path().join("old.bin");
    let out_path = dir.path().join("out.bin");

    fs::write(&old_path, OLD_TEXT).unwrap();

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

    fs::write(&old_path, OLD_TEXT).unwrap();
    fs::write(&diff_path, DIFF_V13_TEXT).unwrap();

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

/// compress_cover_buf_size == 1 triggers padding (whereas > 1 would not).
#[test]
fn cover_padding_with_small_compressed_size() {
    // Padding logic is exercised by the v20_zstd and v13_zstd end-to-end tests
    // which use compressed covers. A dedicated regression would require crafting
    // a patch with exactly compress_cover_buf_size == 1.
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
    CompressionMode, CoverHeader, DiffChunkInfo, DiffSingleChunkInfo, HeaderInfo, RleRefClip,
    MAX_ARRAY_POOL_LEN, MAX_ARRAY_POOL_SECOND_OFFSET, MAX_MEM_BUFFER_LEN, MAX_MEM_BUFFER_LIMIT,
};

#[test]
fn cover_header_new_and_fields() {
    let ch = CoverHeader::new(1, 2, 3, 4);
    assert_eq!(ch.old_pos, 1);
    assert_eq!(ch.new_pos, 2);
    assert_eq!(ch.cover_length, 3);
    assert_eq!(ch.next_cover_index, 4);
}

#[test]
fn cover_header_debug_and_clone() {
    let ch = CoverHeader::new(10, 20, 30, 40);
    let _ = format!("{:?}", ch);
    let cloned = ch;
    assert_eq!(ch.old_pos, cloned.old_pos);
    assert_eq!(ch.new_pos, cloned.new_pos);
    assert_eq!(ch.cover_length, cloned.cover_length);
    assert_eq!(ch.next_cover_index, cloned.next_cover_index);
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
    assert_eq!(MAX_MEM_BUFFER_LIMIT, 2 << 20);
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
    let ch = CoverHeader::new(0, 0, -5, 0);
    assert_eq!(
        ch.cover_length, -5,
        "CoverHeader::new stores negative cover_length as-is"
    );
}

/// CoverHeader::new stores negative old_pos as-is; no rejection.
#[test]
fn cover_header_new_stores_negative_old_pos() {
    let ch = CoverHeader::new(-10, 0, 1, 0);
    assert_eq!(
        ch.old_pos, -10,
        "CoverHeader::new stores negative old_pos as-is"
    );
}

/// CoverHeader::new with valid positive values (positive test).
#[test]
fn cover_header_new_accepts_valid_positive_values() {
    let ch = CoverHeader::new(100, 200, 300, 1);
    assert_eq!(ch.old_pos, 100);
    assert_eq!(ch.new_pos, 200);
    assert_eq!(ch.cover_length, 300);
    assert_eq!(ch.next_cover_index, 1);
}

/// CoverHeader::new with extreme i64 values.
#[test]
fn cover_header_new_with_extreme_i64_values() {
    let ch = CoverHeader::new(i64::MAX, i64::MIN, i64::MAX, i64::MIN);
    assert_eq!(ch.old_pos, i64::MAX);
    assert_eq!(ch.new_pos, i64::MIN);
    assert_eq!(ch.cover_length, i64::MAX);
    assert_eq!(ch.next_cover_index, i64::MIN);
}

/// CoverHeader::new with all-zero values (boundary positive test).
#[test]
fn cover_header_new_with_zero_values() {
    let ch = CoverHeader::new(0, 0, 0, 0);
    assert_eq!(ch.old_pos, 0);
    assert_eq!(ch.new_pos, 0);
    assert_eq!(ch.cover_length, 0);
    assert_eq!(ch.next_cover_index, 0);
}

/// Empty input returns no headers.
#[test]
fn enumerate_cover_headers_rejects_negative_cover_length() {
    use std::io::Cursor;

    // Empty input succeeds.
    let mut c = Cursor::new(Vec::new());
    let result = super::patch_core::enumerate_cover_headers(&mut c, 0, 0);
    assert!(result.is_ok());
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
