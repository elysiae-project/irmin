use super::*;
use fast_md5;

#[test]
fn patch_method_serialization() {
    let method = PatchMethod::CopyOver;
    let json = serde_json::to_string(&method).unwrap();
    assert_eq!(json, "\"copyOver\"");

    let method = PatchMethod::Patch;
    let json = serde_json::to_string(&method).unwrap();
    assert_eq!(json, "\"patch\"");

    let method = PatchMethod::DownloadOver;
    let json = serde_json::to_string(&method).unwrap();
    assert_eq!(json, "\"downloadOver\"");

    let method = PatchMethod::Skip;
    let json = serde_json::to_string(&method).unwrap();
    assert_eq!(json, "\"skip\"");
}

#[test]
fn patch_asset_info_serialization() {
    let info = PatchAssetInfo {
        target_file_path: "GameData/Data.pak".to_string(),
        target_file_size: 1024,
        target_file_hash: "abc123".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "chunk_001".to_string(),
        patch_hash: "def456".to_string(),
        patch_offset: 0,
        patch_size: 500,
        patch_chunk_length: 500,
        original_file_path: Some("GameData/Data_old.pak".to_string()),
        original_file_hash: Some("ghi789".to_string()),
        original_file_size: Some(900),
        matching_field: "game".to_string(),
    };
    let json = serde_json::to_string(&info).unwrap();
    let deserialized: PatchAssetInfo = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.target_file_path, "GameData/Data.pak");
    assert_eq!(deserialized.patch_method, PatchMethod::Patch);
    assert_eq!(
        deserialized.original_file_path,
        Some("GameData/Data_old.pak".to_string())
    );
}

#[test]
fn preinstall_state_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let state = PreinstallState {
        tag: "5.0.0".to_string(),
        game_id: "hk4e".to_string(),
        vo_lang: "en".to_string(),
        installed_tag: "4.8.0".to_string(),
        patch_assets: vec![PatchAssetInfo {
            target_file_path: "test.pak".to_string(),
            target_file_size: 100,
            target_file_hash: "hash".to_string(),
            patch_method: PatchMethod::CopyOver,
            patch_name: "chunk_0".to_string(),
            patch_hash: "chunkhash".to_string(),
            patch_offset: 0,
            patch_size: 50,
            patch_chunk_length: 50,
            original_file_path: None,
            original_file_hash: None,
            original_file_size: None,
            matching_field: "game".to_string(),
        }],
        deleted_files: vec!["old_file.pak".to_string()],
        downloaded_chunks: ["chunk_0".to_string()].into_iter().collect(),
        diff_downloads: rustc_hash::FxHashMap::from_iter([(
            "game".to_string(),
            Arc::new(make_download_info()),
        )]),
        main_chunk_download: DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: crate::api_scrape::Compression::None,
            url_prefix: "https://example.com/".to_string(),
            url_suffix: "v2".to_string(),
        },
        main_manifest_ids: vec![("game".to_string(), "manifest_123".to_string())],
    };

    save_preinstall_state(dir.path(), &state).unwrap();
    let loaded = load_preinstall_state(dir.path(), "5.0.0").unwrap();
    assert_eq!(loaded.tag, "5.0.0");
    assert_eq!(loaded.installed_tag, "4.8.0");
    assert_eq!(loaded.patch_assets.len(), 1);
    assert_eq!(loaded.deleted_files.len(), 1);
    assert!(loaded.downloaded_chunks.contains("chunk_0"));
}

#[test]
fn hdiff_magic_detection() {
    assert!(b"HDIFF13".starts_with(HDIFF_MAGIC));
    assert!(!b"NORMAL".starts_with(HDIFF_MAGIC));
}

#[test]
fn preinstall_state_paths() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = PreinstallState::state_file_path(dir.path(), "5.0.0");
    let marker_path = PreinstallState::marker_file_path(dir.path(), "5.0.0");
    assert!(state_path
        .to_string_lossy()
        .contains(".sophon_preinstall_5.0.0.json"));
    assert!(marker_path
        .to_string_lossy()
        .contains(".sophon_preinstall_5.0.0"));
    assert!(!marker_path.to_string_lossy().contains(".json"));
}

#[test]
fn verify_chunk_md5_correct() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_chunk");
    let data = b"hello world";
    let md5_hex = hex::encode(fast_md5::digest(data));
    fs::write(&path, data).unwrap();
    assert!(verify_chunk_md5(&path, &md5_hex));
}

#[test]
fn verify_chunk_md5_wrong() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_chunk");
    fs::write(&path, b"hello world").unwrap();
    assert!(!verify_chunk_md5(&path, "wrong_md5_hash_here"));
}

#[test]
fn verify_chunk_md5_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent");
    assert!(!verify_chunk_md5(&path, "any_hash"));
}

#[test]
fn verify_file_hash_md5_uppercase_expected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_md5_file");
    let data = b"hello world";
    let md5_hex_lower = hex::encode(fast_md5::digest(data));
    fs::write(&path, data).unwrap();
    assert!(verify_file_hash(&path, &md5_hex_lower.to_uppercase()));
}

#[test]
fn verify_file_hash_xxh64_uppercase_expected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_xxh64_file");
    let data = b"hello world";
    fs::write(&path, data).unwrap();
    let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
    hasher.update(data);
    let xxh64_lower = format!("{:016x}", hasher.digest());
    assert!(verify_file_hash(&path, &xxh64_lower.to_uppercase()));
}

#[test]
fn verify_file_hash_empty_returns_true() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_empty");
    fs::write(&path, b"some data").unwrap();
    assert!(verify_file_hash(&path, ""));
}

#[test]
fn verify_file_hash_whitespace_only_returns_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_whitespace");
    fs::write(&path, b"some data").unwrap();
    assert!(!verify_file_hash(&path, "   "));
    assert!(!verify_file_hash(&path, "\t"));
    assert!(!verify_file_hash(&path, "\n"));
}

#[test]
fn verify_file_hash_unknown_length_returns_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_unknown");
    fs::write(&path, b"some data").unwrap();
    assert!(!verify_file_hash(&path, "short"));
    assert!(!verify_file_hash(
        &path,
        "1234567890abcdef1234567890abcdef1234567890abcdef"
    ));
}

#[test]
fn verify_file_hash_missing_file_returns_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent_file");
    assert!(!verify_file_hash(&path, "0123456789abcdef0123456789abcdef"));
    assert!(!verify_file_hash(&path, "0123456789abcdef"));
}

/// Lowercase MD5 hash must be accepted (verify_file_hash normalizes to
/// lowercase).
#[test]
fn verify_file_hash_md5_lowercase_expected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_md5_lower");
    let data = b"hello world";
    let md5_lower = hex::encode(fast_md5::digest(data));
    fs::write(&path, data).unwrap();
    assert!(verify_file_hash(&path, &md5_lower));
}

/// A real MD5 mismatch must return false (verify_file_hash dispatcher).
#[test]
fn verify_file_hash_md5_wrong_hash_returns_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_md5_wrong");
    fs::write(&path, b"hello world").unwrap();
    // MD5 of "hello world"
    assert!(!verify_file_hash(&path, "00000000000000000000000000000000"));
}

/// Lowercase XXH64 hash must be accepted (verify_file_hash normalizes to
/// lowercase).
#[test]
fn verify_file_hash_xxh64_lowercase_expected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_xxh64_lower");
    let data = b"hello world";
    let mut hasher = xxhash_rust::xxh64::Xxh64::new(0);
    hasher.update(data);
    let xxh64_lower = format!("{:016x}", hasher.digest());
    fs::write(&path, data).unwrap();
    assert!(verify_file_hash(&path, &xxh64_lower));
}

/// A real XXH64 mismatch must return false (verify_file_hash dispatcher).
#[test]
fn verify_file_hash_xxh64_wrong_hash_returns_false() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test_xxh64_wrong");
    fs::write(&path, b"hello world").unwrap();
    // All-zero string is not a valid XXH64.
    assert!(!verify_file_hash(&path, "0000000000000000"));
}

#[test]
fn delete_preinstall_state_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = PreinstallState::state_file_path(dir.path(), "5.0.0");
    let marker_path = PreinstallState::marker_file_path(dir.path(), "5.0.0");
    fs::write(&state_path, "{}").unwrap();
    fs::write(&marker_path, "5.0.0").unwrap();
    assert!(state_path.exists());
    assert!(marker_path.exists());
    delete_preinstall_state(dir.path(), "5.0.0");
    assert!(!state_path.exists());
    assert!(!marker_path.exists());
}

#[test]
fn validate_asset_path_rejects_dotdot() {
    let dir = tempfile::tempdir().unwrap();
    let result = validate_asset_path(dir.path(), "../../../etc/passwd");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(matches!(err, SophonError::PathTraversal(_)));
}

#[test]
fn validate_asset_path_rejects_absolute() {
    let dir = tempfile::tempdir().unwrap();
    let result = validate_asset_path(dir.path(), "/etc/passwd");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_asset_path_rejects_backslash_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let result = validate_asset_path(dir.path(), "\\Windows\\System32");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_asset_path_accepts_normal_relative() {
    let dir = tempfile::tempdir().unwrap();
    let result = validate_asset_path(dir.path(), "GameData/Data.pak");
    assert!(result.is_ok());
    assert_eq!(result.unwrap(), dir.path().join("GameData/Data.pak"));
}

#[test]
fn validate_asset_path_accepts_nested_relative() {
    let dir = tempfile::tempdir().unwrap();
    let result = validate_asset_path(dir.path(), "a/b/c/file.pkg");
    assert!(result.is_ok());
}

#[test]
fn validate_asset_path_rejects_windows_drive_letter() {
    let dir = tempfile::tempdir().unwrap();
    let result = validate_asset_path(dir.path(), "C:/Windows/System32");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_patch_name_rejects_empty() {
    let result = validate_patch_name("");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        SophonError::InvalidAssetName(_)
    ));
}

#[test]
fn validate_patch_name_rejects_null_byte() {
    let result = validate_patch_name("file\0.txt");
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        SophonError::InvalidAssetName(_)
    ));
}

#[test]
fn validate_patch_name_rejects_dotdot() {
    let result = validate_patch_name("../../etc/passwd");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_patch_name_rejects_drive_letter() {
    let result = validate_patch_name("C:/path/to/chunk.bin");
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_patch_name_accepts_valid() {
    let result = validate_patch_name("chunk_001.bin");
    assert!(result.is_ok());
}

#[test]
fn validate_patch_name_rejects_relative_parent_component() {
    // Leading `..` must be rejected as traversal (not InvalidAssetName).
    let name = "../etc/passwd";
    let result = validate_patch_name(name);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_patch_name_rejects_parent_component_in_middle() {
    // `..` as a middle directory component must be rejected as traversal.
    let name = "path/../file";
    let result = validate_patch_name(name);
    assert!(result.is_err());
    assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
}

#[test]
fn validate_patch_name_accepts_consecutive_dots_in_filename() {
    // Consecutive dots within a single filename component are not path traversal.
    assert!(validate_patch_name("foo..bar").is_ok());
}

#[test]
fn validate_patch_name_accepts_version_like_name() {
    // Version-like names with consecutive dots are legitimate patch filenames.
    assert!(validate_patch_name("2.0..hotfix.pak").is_ok());
}

#[test]
fn validate_patch_name_rejects_dot_prefix() {
    // Reject ./ and .\\ prefixes for consistency with validate_asset_name
    assert!(validate_patch_name("./foo").is_err());
    assert!(validate_patch_name(".\\foo").is_err());
    assert!(validate_patch_name("./foo/bar").is_err());
    assert!(validate_patch_name(".\\foo\\bar").is_err());
    // Consecutive dots in a filename are allowed.
    assert!(validate_patch_name("foo..bar").is_ok());
}

#[test]
fn is_file_already_patched_size_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.bin");
    fs::write(&path, b"short").unwrap();
    assert!(!is_file_already_patched(&path, 9999, "any_hash"));
}

#[test]
fn is_file_already_patched_md5_mismatch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.bin");
    let data = b"hello world";
    fs::write(&path, data).unwrap();
    assert!(!is_file_already_patched(
        &path,
        data.len() as u64,
        "wrong_hash"
    ));
}

#[test]
fn is_file_already_patched_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("test.bin");
    let data = b"hello world";
    let md5_hex = hex::encode(fast_md5::digest(data));
    fs::write(&path, data).unwrap();
    assert!(is_file_already_patched(&path, data.len() as u64, &md5_hex));
}

#[test]
fn is_file_already_patched_missing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("nonexistent.bin");
    assert!(!is_file_already_patched(&path, 100, "any_hash"));
}

#[test]
fn is_file_already_patched_returns_false_when_file_missing() {
    // Return false when the file handle cannot be opened (TOCTOU fix).
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("does_not_exist.bin");
    // Keep tempdir alive for the duration of the test.
    let _keep_dir_alive = &dir;

    let result = is_file_already_patched(&path, 0, "d41d8cd98f00b204e9800998ecf8427e");

    assert!(!result);
}

#[test]
fn is_file_already_patched_returns_false_when_size_mismatch() {
    // Size check must short-circuit before hashing.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("size_mismatch.bin");
    let data = b"short";
    fs::write(&path, data).unwrap();
    let actual_size = data.len() as u64;

    let result = is_file_already_patched(
        &path,
        actual_size + 9_999,
        "00000000000000000000000000000000",
    );

    assert!(!result);
}

#[test]
fn is_file_already_patched_returns_true_when_size_and_md5_match() {
    // Happy path: both size and MD5 match on the same file handle.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("match.bin");
    let data = b"hello world";
    let expected_md5 = hex::encode(fast_md5::digest(data));
    fs::write(&path, data).unwrap();

    let result = is_file_already_patched(&path, data.len() as u64, &expected_md5);

    assert!(result);
}

#[test]
fn is_file_already_patched_returns_false_when_only_md5_mismatch() {
    // Return false when the MD5 computed from the file handle does not match.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("md5_mismatch.bin");
    let data = b"hello world";
    fs::write(&path, data).unwrap();

    let result =
        is_file_already_patched(&path, data.len() as u64, "00000000000000000000000000000000");

    assert!(!result);
}

#[test]
fn patch_method_remove_serialization() {
    let method = PatchMethod::Remove;
    let json = serde_json::to_string(&method).unwrap();
    assert_eq!(json, "\"remove\"");
    let back: PatchMethod = serde_json::from_str(&json).unwrap();
    assert_eq!(back, PatchMethod::Remove);
}

#[test]
fn patch_method_all_roundtrip() {
    for method in [
        PatchMethod::CopyOver,
        PatchMethod::Patch,
        PatchMethod::DownloadOver,
        PatchMethod::Remove,
        PatchMethod::Skip,
    ] {
        let json = serde_json::to_string(&method).unwrap();
        let back: PatchMethod = serde_json::from_str(&json).unwrap();
        assert_eq!(back, method);
    }
}

#[test]
fn filter_patch_assets_skips_filtered_download_over() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(format!("{NAP_GAME_DATA_DIR}/Persistent"))).unwrap();
    fs::write(
        dir.path()
            .join(format!("{NAP_GAME_DATA_DIR}/Persistent/KDelResource")),
        "cutscenes",
    )
    .unwrap();

    let state = PreinstallState {
        tag: "2.0.0".to_string(),
        game_id: "nap".to_string(),
        vo_lang: "en".to_string(),
        installed_tag: "1.0.0".to_string(),
        patch_assets: vec![
            PatchAssetInfo {
                target_file_path: "game_data.bin".to_string(),
                target_file_size: 100,
                target_file_hash: "h1".to_string(),
                patch_method: PatchMethod::DownloadOver,
                patch_name: String::new(),
                patch_hash: String::new(),
                patch_offset: 0,
                patch_size: 0,
                patch_chunk_length: 0,
                original_file_path: None,
                original_file_hash: None,
                original_file_size: None,
                matching_field: "cutscenes".to_string(),
            },
            PatchAssetInfo {
                target_file_path: "core_data.bin".to_string(),
                target_file_size: 200,
                target_file_hash: "h2".to_string(),
                patch_method: PatchMethod::Patch,
                patch_name: "chunk_0".to_string(),
                patch_hash: "ph".to_string(),
                patch_offset: 0,
                patch_size: 200,
                patch_chunk_length: 200,
                original_file_path: Some("core_data_old.bin".to_string()),
                original_file_hash: Some("oh".to_string()),
                original_file_size: Some(180),
                matching_field: "cutscenes".to_string(),
            },
            PatchAssetInfo {
                target_file_path: "main_game.bin".to_string(),
                target_file_size: 300,
                target_file_hash: "h3".to_string(),
                patch_method: PatchMethod::CopyOver,
                patch_name: "chunk_1".to_string(),
                patch_hash: "ph2".to_string(),
                patch_offset: 0,
                patch_size: 300,
                patch_chunk_length: 300,
                original_file_path: None,
                original_file_hash: None,
                original_file_size: None,
                matching_field: "game".to_string(),
            },
        ],
        deleted_files: vec![],
        downloaded_chunks: rustc_hash::FxHashSet::default(),
        diff_downloads: rustc_hash::FxHashMap::from_iter([(
            "game".to_string(),
            Arc::new(make_download_info()),
        )]),
        main_chunk_download: make_download_info(),
        main_manifest_ids: vec![],
    };

    let cache = FilterCache::new(dir.path());
    let mut assets = state.patch_assets.clone();
    filter_patch_assets_for_removed_features(&cache, &mut assets);
    assert_eq!(assets.len(), 3);
    assert_eq!(assets[0].patch_method, PatchMethod::Skip);
    assert_eq!(assets[1].patch_method, PatchMethod::Skip);
    assert_eq!(assets[2].patch_method, PatchMethod::CopyOver);
}

#[test]
fn filter_patch_assets_no_filter_when_no_kdel() {
    let dir = tempfile::tempdir().unwrap();
    let state = PreinstallState {
        tag: "2.0.0".to_string(),
        game_id: "nap".to_string(),
        vo_lang: "en".to_string(),
        installed_tag: "1.0.0".to_string(),
        patch_assets: vec![PatchAssetInfo {
            target_file_path: "data.bin".to_string(),
            target_file_size: 100,
            target_file_hash: "h".to_string(),
            patch_method: PatchMethod::DownloadOver,
            patch_name: String::new(),
            patch_hash: String::new(),
            patch_offset: 0,
            patch_size: 0,
            patch_chunk_length: 0,
            original_file_path: None,
            original_file_hash: None,
            original_file_size: None,
            matching_field: "cutscenes".to_string(),
        }],
        deleted_files: vec![],
        downloaded_chunks: rustc_hash::FxHashSet::default(),
        diff_downloads: rustc_hash::FxHashMap::from_iter([(
            "game".to_string(),
            Arc::new(make_download_info()),
        )]),
        main_chunk_download: make_download_info(),
        main_manifest_ids: vec![],
    };

    let cache = FilterCache::new(dir.path());
    let mut assets = state.patch_assets.clone();
    filter_patch_assets_for_removed_features(&cache, &mut assets);
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0].patch_method, PatchMethod::DownloadOver);
}

#[test]
fn apply_copy_over_writes_file() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let data = b"new file content";
    let md5_hex = hex::encode(fast_md5::digest(data));
    fs::write(chunks_dir.join("patch_0"), data).unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/output.bin".to_string(),
        target_file_size: data.len() as u64,
        target_file_hash: md5_hex,
        patch_method: PatchMethod::CopyOver,
        patch_name: "patch_0".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: data.len() as u64,
        patch_chunk_length: data.len() as u64,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    apply_copy_over(dir.path(), &chunks_dir, &asset).unwrap();

    let written = fs::read(dir.path().join("GameData/output.bin")).unwrap();
    assert_eq!(written, data);
}

#[test]
fn apply_copy_over_with_offset_reads_subrange() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let full_data = b"AAAA_target_content_BBBB";
    fs::write(chunks_dir.join("patch_1"), full_data).unwrap();

    let target_data = &full_data[5..21];
    let md5_hex = hex::encode(fast_md5::digest(target_data));

    let asset = PatchAssetInfo {
        target_file_path: "GameData/sliced.bin".to_string(),
        target_file_size: target_data.len() as u64,
        target_file_hash: md5_hex,
        patch_method: PatchMethod::CopyOver,
        patch_name: "patch_1".to_string(),
        patch_hash: String::new(),
        patch_offset: 5,
        patch_size: 16,
        patch_chunk_length: 16,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    apply_copy_over(dir.path(), &chunks_dir, &asset).unwrap();

    let written = fs::read(dir.path().join("GameData/sliced.bin")).unwrap();
    assert_eq!(written, target_data);
}

#[test]
fn apply_copy_over_missing_chunk_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/missing.bin".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::CopyOver,
        patch_name: "nonexistent_chunk".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 100,
        patch_chunk_length: 100,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    let result = apply_copy_over(dir.path(), &chunks_dir, &asset);
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        SophonError::PatchChunkNotFound(_)
    ));
}

#[test]
fn apply_copy_over_small_chunk_below_magic_len() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let data = b"ab";
    let md5_hex = hex::encode(fast_md5::digest(data));
    fs::write(chunks_dir.join("small_chunk"), data).unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/small.bin".to_string(),
        target_file_size: data.len() as u64,
        target_file_hash: md5_hex,
        patch_method: PatchMethod::CopyOver,
        patch_name: "small_chunk".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: data.len() as u64,
        patch_chunk_length: data.len() as u64,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    apply_copy_over(dir.path(), &chunks_dir, &asset).unwrap();

    let written = fs::read(dir.path().join("GameData/small.bin")).unwrap();
    assert_eq!(written, data);
}

#[test]
fn apply_copy_over_exact_magic_len_non_hdiff() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let data = b"ABCDE";
    assert_eq!(data.len(), HDIFF_MAGIC.len());
    assert_ne!(&data[..], HDIFF_MAGIC.as_ref());
    let md5_hex = hex::encode(fast_md5::digest(data));
    fs::write(chunks_dir.join("exact_chunk"), data).unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/exact.bin".to_string(),
        target_file_size: data.len() as u64,
        target_file_hash: md5_hex,
        patch_method: PatchMethod::CopyOver,
        patch_name: "exact_chunk".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: data.len() as u64,
        patch_chunk_length: data.len() as u64,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    apply_copy_over(dir.path(), &chunks_dir, &asset).unwrap();

    let written = fs::read(dir.path().join("GameData/exact.bin")).unwrap();
    assert_eq!(written, data);
}

#[test]
fn apply_copy_over_with_offset_non_hdiff_seeks_back() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let full_data = b"__PREFIX__HelloHDIFFWorld__SUFFIX__";
    fs::write(chunks_dir.join("offset_chunk"), full_data).unwrap();

    let target_data = &full_data[10..25];
    assert!(target_data.starts_with(b"HelloHDIFF"));
    let md5_hex = hex::encode(fast_md5::digest(target_data));

    let asset = PatchAssetInfo {
        target_file_path: "GameData/offset.bin".to_string(),
        target_file_size: target_data.len() as u64,
        target_file_hash: md5_hex,
        patch_method: PatchMethod::CopyOver,
        patch_name: "offset_chunk".to_string(),
        patch_hash: String::new(),
        patch_offset: 10,
        patch_size: 15,
        patch_chunk_length: 15,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    apply_copy_over(dir.path(), &chunks_dir, &asset).unwrap();

    let written = fs::read(dir.path().join("GameData/offset.bin")).unwrap();
    assert_eq!(written, target_data);
}

#[test]
fn apply_copy_over_detects_hdiff_magic() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    let hdiff_data = b"HDIFF13patchpayload";
    fs::write(chunks_dir.join("patch_hdiff"), hdiff_data).unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/hdiff.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::CopyOver,
        patch_name: "patch_hdiff".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: hdiff_data.len() as u64,
        patch_chunk_length: hdiff_data.len() as u64,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    let result = apply_copy_over(dir.path(), &chunks_dir, &asset);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, SophonError::HDiffPatchFailed { .. }),
        "expected HDiffPatchFailed, got: {err:?}"
    );
}

#[test]
fn apply_copy_over_cleans_up_diff_temp_on_failure() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Chunk with HDIFF magic but invalid data.
    let hdiff_data = b"HDIFFinvalid_data_that_cannot_be_patched";
    fs::write(chunks_dir.join("patch_hdiff_fail"), hdiff_data).unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/hdiff_fail.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::CopyOver,
        patch_name: "patch_hdiff_fail".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: hdiff_data.len() as u64,
        patch_chunk_length: hdiff_data.len() as u64,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    let result = apply_copy_over(dir.path(), &chunks_dir, &asset);

    // Should fail because the HDiff data is invalid.
    assert!(result.is_err());

    // No diff_temp file is created when patching directly from the chunk file.
    let diff_temp = dir.path().join("patching/patch_hdiff_fail.diff");
    assert!(
        !diff_temp.exists(),
        "no diff_temp file should be created when patching from chunk file directly"
    );
}

#[test]
fn apply_hdiff_patch_size_mismatch_returns_error() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Original file with the wrong size.
    let original_path = dir.path().join("original.bin");
    fs::write(&original_path, b"short data that has wrong size").unwrap();
    let actual_size = fs::metadata(&original_path).unwrap().len();

    // Dummy chunk file so the patch chunk exists check passes.
    fs::write(chunks_dir.join("patch_chunk.bin"), b"dummy chunk data").unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "original.bin".to_string(),
        target_file_size: 100,
        target_file_hash: "target_hash".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "patch_chunk.bin".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 20,
        patch_chunk_length: 20,
        original_file_path: Some("original.bin".to_string()),
        original_file_hash: Some("original_hash".to_string()),
        original_file_size: Some(999), // Expected size is 999, but actual is different
        matching_field: "game".to_string(),
    };

    let cache = FilterCache::new(dir.path());
    let result = apply_hdiff_patch(dir.path(), &chunks_dir, &asset, &cache);

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        matches!(err, SophonError::OriginalFileMissing(_)),
        "Expected OriginalFileMissing, got: {err:?}"
    );
    let err_str = err.to_string();
    assert!(
        err_str.contains("Size mismatch"),
        "Error should mention 'Size mismatch': {err_str}"
    );
    // Error message includes expected and actual sizes.
    assert!(
        err_str.contains("999") && err_str.contains(&actual_size.to_string()),
        "Error should include expected and actual sizes: {err_str}"
    );
}

#[test]
fn apply_hdiff_patch_blank_original_creates_diff_ref() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Dummy chunk file so the patch chunk exists check passes.
    fs::write(chunks_dir.join("patch_chunk.bin"), b"dummy chunk data").unwrap();

    // Original file does not exist but hash indicates blank file.
    let asset = PatchAssetInfo {
        target_file_path: "new_file.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::Patch,
        patch_name: "patch_chunk.bin".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 20,
        patch_chunk_length: 20,
        original_file_path: Some("nonexistent.bin".to_string()),
        original_file_hash: Some(BLANK_FILE_MD5.to_string()),
        original_file_size: Some(0),
        matching_field: "game".to_string(),
    };

    let cache = FilterCache::new(dir.path());
    let result = apply_hdiff_patch(dir.path(), &chunks_dir, &asset, &cache);

    // Should not return OriginalFileMissing; creates blank diff_ref and proceeds.
    assert!(
        !matches!(result, Err(SophonError::OriginalFileMissing(_))),
        "Should not return OriginalFileMissing for blank original: {:?}",
        result
    );
    // Patch fails because the chunk is not a real HDiff, but the blank-file check
    // passes.
    let diff_ref_path = dir.path().join("patching/patch_chunk.bin.diff_ref");
    assert!(
        !diff_ref_path.exists(),
        "diff_ref file should have been cleaned up after patching"
    );
}

#[test]
fn apply_hdiff_patch_blank_original_by_size() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Dummy chunk file so the patch chunk exists check passes.
    fs::write(chunks_dir.join("patch_chunk2.bin"), b"dummy chunk data").unwrap();

    // Original file does not exist and original_file_size is 0.
    let asset = PatchAssetInfo {
        target_file_path: "another_new_file.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::Patch,
        patch_name: "patch_chunk2.bin".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 20,
        patch_chunk_length: 20,
        original_file_path: Some("nonexistent2.bin".to_string()),
        original_file_hash: None,
        original_file_size: Some(0),
        matching_field: "game".to_string(),
    };

    let cache = FilterCache::new(dir.path());
    let result = apply_hdiff_patch(dir.path(), &chunks_dir, &asset, &cache);

    // Should not return OriginalFileMissing; creates blank diff_ref and proceeds.
    assert!(
        !matches!(result, Err(SophonError::OriginalFileMissing(_))),
        "Should not return OriginalFileMissing for blank original (size=0): {:?}",
        result
    );
    // diff_ref was cleaned up after patching.
    let diff_ref_path = dir.path().join("patching/patch_chunk2.bin.diff_ref");
    assert!(
        !diff_ref_path.exists(),
        "diff_ref file should have been cleaned up after patching"
    );
}
/// BLANK_FILE_MD5 triggers blank-file handling even when
/// original_file_size is non-zero. Hash takes precedence over size.
#[test]
fn apply_hdiff_patch_blank_original_by_hash_ignores_size() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Dummy chunk file so the patch chunk exists check passes.
    fs::write(chunks_dir.join("patch_chunk3.bin"), b"dummy chunk data").unwrap();

    // Original file does not exist; hash indicates blank but size is inconsistent.
    let asset = PatchAssetInfo {
        target_file_path: "new_file2.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::Patch,
        patch_name: "patch_chunk3.bin".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 20,
        patch_chunk_length: 20,
        original_file_path: Some("nonexistent3.bin".to_string()),
        original_file_hash: Some(BLANK_FILE_MD5.to_string()),
        original_file_size: Some(999), // Inconsistent: hash says blank, size says 999
        matching_field: "game".to_string(),
    };

    let cache = FilterCache::new(dir.path());
    let result = apply_hdiff_patch(dir.path(), &chunks_dir, &asset, &cache);

    // Should not return OriginalFileMissing when hash indicates blank.
    assert!(
        !matches!(result, Err(SophonError::OriginalFileMissing(_))),
        "Should not return OriginalFileMissing when hash indicates blank: {:?}",
        result
    );
    // diff_ref was cleaned up after patching.
    let diff_ref_path = dir.path().join("patching/patch_chunk3.bin.diff_ref");
    assert!(
        !diff_ref_path.exists(),
        "diff_ref file should have been cleaned up after patching"
    );
}

/// New subdirectories introduced by a patch must be created before the
/// temp output is renamed into place; otherwise fs::rename fails with
/// ENOENT ("No such file or directory").
#[test]
fn apply_hdiff_patch_creates_nested_target_dir() {
    let dir = tempfile::tempdir().unwrap();
    let chunks_dir = dir.path().join("patching/chunk");
    fs::create_dir_all(&chunks_dir).unwrap();

    // Dummy chunk file so the patch chunk exists check passes. Patch apply
    // will fail (not real HDiff), but the nested dir creation happens first.
    fs::write(
        chunks_dir.join("patch_chunk_nested.bin"),
        b"dummy chunk data",
    )
    .unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "new_dir/sub_dir/nested.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::Patch,
        patch_name: "patch_chunk_nested.bin".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 20,
        patch_chunk_length: 20,
        original_file_path: Some("nonexistent_nested.bin".to_string()),
        original_file_hash: Some(BLANK_FILE_MD5.to_string()),
        original_file_size: Some(0),
        matching_field: "game".to_string(),
    };

    let cache = FilterCache::new(dir.path());
    let result = apply_hdiff_patch(dir.path(), &chunks_dir, &asset, &cache);

    // Apply itself may fail (dummy chunk), but the nested parent dir must
    // already exist or the later rename would hit ENOENT.
    assert!(
        dir.path().join("new_dir/sub_dir").is_dir(),
        "nested target parent directory not created (result={result:?})"
    );
}

#[test]
fn apply_hdiff_patch_from_files_blank_diff_ref_hash_matches() {
    let dir = tempfile::tempdir().unwrap();

    // Mock diff file.
    let diff_path = dir.path().join("patching/mock.diff");
    fs::create_dir_all(diff_path.parent().unwrap()).unwrap();
    fs::write(&diff_path, b"mock hdiff data").unwrap();

    let asset = PatchAssetInfo {
        target_file_path: "GameData/output.bin".to_string(),
        target_file_size: 0,
        target_file_hash: String::new(),
        patch_method: PatchMethod::Patch,
        patch_name: "blank_verify_patch".to_string(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 20,
        patch_chunk_length: 20,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };

    // Invalid diff data will fail, but the blank diff_ref hash verification
    // should pass (not fail with hash mismatch) before reaching the hdiff step.
    let result = apply_hdiff_patch_from_files(dir.path(), &diff_path, 0, &asset);

    // Did not fail on the blank diff_ref hash verification step.
    if let Err(SophonError::HDiffPatchFailed { error, .. }) = &result {
        assert!(
            !error.contains("hash mismatch"),
            "Should not fail on blank diff_ref hash mismatch. Error: {error}"
        );
    }

    // BLANK_FILE_MD5 matches the hash of an empty file.
    let empty_path = dir.path().join("empty_file.txt");
    fs::write(&empty_path, b"").unwrap();
    assert!(
        verify_file_hash(&empty_path, BLANK_FILE_MD5),
        "BLANK_FILE_MD5 should match the hash of an empty file"
    );
}

#[test]
fn load_preinstall_state_missing_file() {
    let dir = tempfile::tempdir().unwrap();
    let result = load_preinstall_state(dir.path(), "nonexistent");
    assert!(result.is_err());
}

#[test]
fn load_preinstall_state_corrupted_json() {
    let dir = tempfile::tempdir().unwrap();
    let state_path = PreinstallState::state_file_path(dir.path(), "5.0.0");
    fs::write(&state_path, "not valid json{{{{").unwrap();
    let result = load_preinstall_state(dir.path(), "5.0.0");
    assert!(result.is_err());
}

#[test]
fn save_preinstall_state_atomic_write() {
    let dir = tempfile::tempdir().unwrap();
    let state = PreinstallState {
        tag: "5.0.0".to_string(),
        game_id: "hk4e".to_string(),
        vo_lang: "en".to_string(),
        installed_tag: "4.8.0".to_string(),
        patch_assets: vec![],
        deleted_files: vec![],
        downloaded_chunks: rustc_hash::FxHashSet::default(),
        diff_downloads: rustc_hash::FxHashMap::from_iter([(
            "game".to_string(),
            Arc::new(make_download_info()),
        )]),
        main_chunk_download: make_download_info(),
        main_manifest_ids: vec![],
    };

    save_preinstall_state(dir.path(), &state).unwrap();

    let state_path = PreinstallState::state_file_path(dir.path(), "5.0.0");
    assert!(state_path.exists());
    assert!(!state_path.with_extension("json.tmp").exists());

    let loaded = load_preinstall_state(dir.path(), "5.0.0").unwrap();
    assert_eq!(loaded.tag, "5.0.0");
    assert!(loaded.patch_assets.is_empty());
}

#[test]
fn extract_blacklist_filename_valid() {
    let line = r#"  {"fileName":"Audio/Chinese/abc.pak","fileSize":"1234"}"#;
    let result = extract_blacklist_filename(line);
    assert_eq!(result, Some("Audio/Chinese/abc.pak".to_string()));
}

#[test]
fn extract_blacklist_filename_with_backslashes() {
    let line = r#"  {"fileName":"Audio\Chinese\abc.pak","fileSize":"1234"}"#;
    let result = extract_blacklist_filename(line);
    assert_eq!(result, Some("Audio/Chinese/abc.pak".to_string()));
}

#[test]
fn extract_blacklist_filename_no_match() {
    let line = r#"  {"otherField":"value"}"#;
    let result = extract_blacklist_filename(line);
    assert_eq!(result, None);
}

#[test]
fn extract_blacklist_filename_empty() {
    let result = extract_blacklist_filename("");
    assert_eq!(result, None);
}

#[test]
fn is_filtered_asset_nap_kdel() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(format!("{NAP_GAME_DATA_DIR}/Persistent"))).unwrap();
    fs::write(
        dir.path()
            .join(format!("{NAP_GAME_DATA_DIR}/Persistent/KDelResource")),
        "cutscenes|design",
    )
    .unwrap();

    let cache = FilterCache::new(dir.path());

    let asset = PatchAssetInfo {
        target_file_path: "data.bin".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "c".to_string(),
        patch_hash: "p".to_string(),
        patch_offset: 0,
        patch_size: 100,
        patch_chunk_length: 100,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "cutscenes".to_string(),
    };
    assert!(is_filtered_asset(&cache, &asset));

    let asset_game = PatchAssetInfo {
        target_file_path: "data.bin".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "c".to_string(),
        patch_hash: "p".to_string(),
        patch_offset: 0,
        patch_size: 100,
        patch_chunk_length: 100,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };
    assert!(!is_filtered_asset(&cache, &asset_game));
}

#[test]
fn is_filtered_asset_hkrpg_blacklist() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join(format!("{HKRPG_DATA_DIR}/Persistent"))).unwrap();
    fs::write(
        dir.path().join(format!(
            "{HKRPG_DATA_DIR}/Persistent/DownloadBlacklist.json"
        )),
        r#"{"fileName":"Audio/Korean/vo_kr.pak","fileSize":"1000"}"#,
    )
    .unwrap();

    let cache = FilterCache::new(dir.path());

    let asset = PatchAssetInfo {
        target_file_path: "Audio/Korean/vo_kr.pak".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::DownloadOver,
        patch_name: String::new(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 0,
        patch_chunk_length: 0,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "ko-kr".to_string(),
    };
    assert!(is_filtered_asset(&cache, &asset));

    let asset_en = PatchAssetInfo {
        target_file_path: "Audio/English/vo_en.pak".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::DownloadOver,
        patch_name: String::new(),
        patch_hash: String::new(),
        patch_offset: 0,
        patch_size: 0,
        patch_chunk_length: 0,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "en-us".to_string(),
    };
    assert!(!is_filtered_asset(&cache, &asset_en));
}

#[test]
fn is_filtered_asset_hk4e_audio_lang() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(
        dir.path()
            .join(format!("{HK4E_DATA_DIR_GLOBAL}/Persistent")),
    )
    .unwrap();
    fs::write(
        dir.path().join(format!(
            "{HK4E_DATA_DIR_GLOBAL}/Persistent/audio_lang_installed"
        )),
        "English(US)\n",
    )
    .unwrap();

    let cache = FilterCache::new(dir.path());

    let asset_en = PatchAssetInfo {
        target_file_path: "Audio/English(US)/vo_en.pak".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "c".to_string(),
        patch_hash: "p".to_string(),
        patch_offset: 0,
        patch_size: 100,
        patch_chunk_length: 100,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "en-us".to_string(),
    };
    assert!(!is_filtered_asset(&cache, &asset_en));

    let asset_jp = PatchAssetInfo {
        target_file_path: "Audio/Japanese/vo_jp.pak".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "c".to_string(),
        patch_hash: "p".to_string(),
        patch_offset: 0,
        patch_size: 100,
        patch_chunk_length: 100,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "ja-jp".to_string(),
    };
    assert!(is_filtered_asset(&cache, &asset_jp));
}

#[test]
fn is_filtered_asset_no_game_dir_markers() {
    let dir = tempfile::tempdir().unwrap();
    let cache = FilterCache::new(dir.path());
    let asset = PatchAssetInfo {
        target_file_path: "data.bin".to_string(),
        target_file_size: 100,
        target_file_hash: "h".to_string(),
        patch_method: PatchMethod::Patch,
        patch_name: "c".to_string(),
        patch_hash: "p".to_string(),
        patch_offset: 0,
        patch_size: 100,
        patch_chunk_length: 100,
        original_file_path: None,
        original_file_hash: None,
        original_file_size: None,
        matching_field: "game".to_string(),
    };
    assert!(!is_filtered_asset(&cache, &asset));
}

#[test]
fn patching_chunk_dir_path() {
    let dir = tempfile::tempdir().unwrap();
    let chunks = patching_chunk_dir(dir.path());
    assert!(chunks.to_string_lossy().contains("patching"));
    assert!(chunks.to_string_lossy().contains("chunk"));
}

#[test]
fn retry_delay_first_attempt_is_1000ms() {
    // Base 1000 ms + jitter (0-999 ms).
    let delay = retry_delay(0_u32).as_millis();
    assert!(
        (1000..2000).contains(&delay),
        "expected 1000-1999, got {}",
        delay
    );
}

#[test]
fn retry_delay_doubles_up_to_fifth_attempt() {
    // attempt 0 -> 1 s + jitter
    let d0 = retry_delay(0_u32).as_millis();
    assert!(
        (1000..2000).contains(&d0),
        "attempt 0: expected 1000-1999, got {}",
        d0
    );
    // attempt 1 -> 2 s + jitter
    let d1 = retry_delay(1_u32).as_millis();
    assert!(
        (2000..3000).contains(&d1),
        "attempt 1: expected 2000-2999, got {}",
        d1
    );
    // attempt 2 -> 4 s + jitter
    let d2 = retry_delay(2_u32).as_millis();
    assert!(
        (4000..5000).contains(&d2),
        "attempt 2: expected 4000-4999, got {}",
        d2
    );
    // attempt 3 -> 8 s + jitter
    let d3 = retry_delay(3_u32).as_millis();
    assert!(
        (8000..9000).contains(&d3),
        "attempt 3: expected 8000-8999, got {}",
        d3
    );
    // attempt 4 -> 16 s + jitter
    let d4 = retry_delay(4_u32).as_millis();
    assert!(
        (16000..17000).contains(&d4),
        "attempt 4: expected 16000-16999, got {}",
        d4
    );
    // attempt 5 -> 32 s, capped to 30 s + jitter
    let d5 = retry_delay(5_u32).as_millis();
    assert!(
        (30000..31000).contains(&d5),
        "attempt 5: expected 30000-30999, got {}",
        d5
    );
}

#[test]
fn retry_delay_caps_at_30_seconds_for_larger_attempts() {
    // Capped at 30000 + jitter (up to ~30999 ms)
    let d5 = retry_delay(5_u32).as_millis();
    assert!(
        (30000..31000).contains(&d5),
        "attempt 5: expected 30000-30999, got {}",
        d5
    );
    let d6 = retry_delay(6_u32).as_millis();
    assert!(
        (30000..31000).contains(&d6),
        "attempt 6: expected 30000-30999, got {}",
        d6
    );
    let d50 = retry_delay(50_u32).as_millis();
    assert!(
        (30000..31000).contains(&d50),
        "attempt 50: expected 30000-30999, got {}",
        d50
    );
}

#[test]
fn retry_delay_max_attempt_caps_before_multiply() {
    let d10 = retry_delay(10).as_millis();
    assert!(
        (30000..31000).contains(&d10),
        "attempt 10: expected 30000-30999, got {}",
        d10
    );
    let d100 = retry_delay(100).as_millis();
    assert!(
        (30000..31000).contains(&d100),
        "attempt 100: expected 30000-30999, got {}",
        d100
    );
}

fn make_download_info() -> DownloadInfo {
    DownloadInfo {
        encryption: 0,
        password: String::new(),
        compression: crate::api_scrape::Compression::None,
        url_prefix: "https://example.com/".to_string(),
        url_suffix: "v1".to_string(),
    }
}

/// Verifies the BufReader-based incremental MD5 loop produces correct
/// digests for files larger than the 256KB internal buffer.
#[test]
fn verify_chunk_md5_large_file_multi_buffer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large_chunk");

    // 512 KiB of patterned data (spans two 256KB buffers)
    let data: Vec<u8> = (0..524288u32)
        .map(|i| (i.wrapping_mul(7) ^ (i >> 3)) as u8)
        .collect();
    let md5_hex = hex::encode(fast_md5::digest(&data));
    fs::write(&path, &data).unwrap();

    assert!(
        verify_chunk_md5(&path, &md5_hex),
        "multi-buffer MD5 should match single-shot digest"
    );
}

/// Pins PreinstallState JSON field names. If camelCase rename_all or field
/// names change, persisted state files from older versions become unloadable.
#[test]
fn preinstall_state_golden_json_field_names() {
    let golden = r#"{
            "tag": "4.8.0_live",
            "gameId": "hk4e_global",
            "voLang": "en-us",
            "installedTag": "4.7.0_live",
            "patchAssets": [{
                "targetFilePath": "data/level.pak",
                "targetFileSize": 1048576,
                "targetFileHash": "abcdef1234567890abcdef1234567890",
                "patchMethod": "patch",
                "patchName": "level.pak.hdiff",
                "patchHash": "fedcba0987654321",
                "patchOffset": 0,
                "patchSize": 524288,
                "patchChunkLength": 1,
                "originalFilePath": null,
                "originalFileHash": null,
                "originalFileSize": null,
                "matchingField": "game"
            }],
            "deletedFiles": ["old/removed.dat"],
            "downloadedChunks": ["chunk_001"],
            "diffDownloads": {},
            "mainChunkDownload": {
                "encryption": 0,
                "password": "",
                "compression": 0,
                "url_prefix": "https://cdn.example.com/",
                "url_suffix": "v1"
            },
            "mainManifestIds": [["game", "manifest_hash_123"]]
        }"#;

    let state: PreinstallState = serde_json::from_str(golden).unwrap();
    assert_eq!(state.tag, "4.8.0_live");
    assert_eq!(state.game_id, "hk4e_global");
    assert_eq!(state.vo_lang, "en-us");
    assert_eq!(state.installed_tag, "4.7.0_live");
    assert_eq!(state.patch_assets.len(), 1);
    assert_eq!(state.patch_assets[0].target_file_path, "data/level.pak");
    assert_eq!(state.patch_assets[0].patch_method, PatchMethod::Patch);
    assert_eq!(state.patch_assets[0].matching_field, "game");
    assert_eq!(state.deleted_files, vec!["old/removed.dat"]);
    assert!(state.downloaded_chunks.contains("chunk_001"));
    assert_eq!(
        state.main_chunk_download.url_prefix,
        "https://cdn.example.com/"
    );
    assert_eq!(
        state.main_manifest_ids,
        vec![("game".to_string(), "manifest_hash_123".to_string())]
    );
}
