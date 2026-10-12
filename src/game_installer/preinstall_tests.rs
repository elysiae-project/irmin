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
        target_snapshot: None,
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
    assert!(
        state_path
            .to_string_lossy()
            .contains(".sophon_preinstall_5.0.0.json")
    );
    assert!(
        marker_path
            .to_string_lossy()
            .contains(".sophon_preinstall_5.0.0")
    );
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
        target_snapshot: None,
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
        target_snapshot: None,
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
        target_snapshot: None,
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
    assert!(state.target_snapshot.is_none());
    assert!(
        !serde_json::to_string(&state)
            .unwrap()
            .contains("targetSnapshot")
    );
}

fn snapshot_state(fields: &[(&str, &str, &[u8])], url: &str) -> PreinstallState {
    let mut state = PreinstallState {
        tag: "2.0".into(),
        game_id: "nap".into(),
        vo_lang: "en".into(),
        installed_tag: "1.0".into(),
        patch_assets: vec![],
        deleted_files: vec![],
        downloaded_chunks: Default::default(),
        diff_downloads: Default::default(),
        main_chunk_download: make_download_info(),
        main_manifest_ids: vec![],
        target_snapshot: None,
    };
    let mut manifests = vec![];
    for &(field, path, data) in fields {
        let hash = hex::encode(fast_md5::digest(data));
        let chunk_download = DownloadInfo {
            url_prefix: url.into(),
            url_suffix: field.into(),
            ..make_download_info()
        };
        let manifest_id = format!("manifest-{field}");
        state
            .main_manifest_ids
            .push((field.into(), manifest_id.clone()));
        state.patch_assets.push(PatchAssetInfo {
            target_file_path: path.into(),
            target_file_size: data.len() as u64,
            target_file_hash: hash.clone(),
            patch_method: PatchMethod::DownloadOver,
            patch_name: String::new(),
            patch_hash: String::new(),
            patch_offset: 0,
            patch_size: 0,
            patch_chunk_length: 0,
            original_file_path: None,
            original_file_hash: None,
            original_file_size: None,
            matching_field: field.into(),
        });
        manifests.push(TargetManifest {
            matching_field: field.into(),
            manifest_id,
            chunk_download,
            manifest: SophonManifestProto {
                assets: vec![SophonManifestAssetProperty {
                    asset_name: path.into(),
                    asset_size: data.len() as u64,
                    asset_type: 0,
                    asset_hash_md5: hash.clone(),
                    asset_chunks: if data.is_empty() {
                        vec![]
                    } else {
                        vec![SophonManifestAssetChunk {
                            chunk_name: format!("chunk-{field}"),
                            chunk_on_file_offset: 0,
                            chunk_size: data.len() as u64,
                            chunk_size_decompressed: data.len() as u64,
                            chunk_compressed_hash_md5: hash.clone(),
                            chunk_decompressed_hash_md5: hash,
                            chunk_compressed_hash_xxh: 0,
                            chunk_old_offset: 0,
                        }]
                    },
                }],
            },
        });
    }
    state.target_snapshot = Some(TargetSnapshot {
        version: 1,
        game_id: state.game_id.clone(),
        vo_lang: state.vo_lang.clone(),
        installed_tag: state.installed_tag.clone(),
        tag: state.tag.clone(),
        build_id: "exact-build".into(),
        legacy_coverage: false,
        manifests,
        digest: String::new(),
    });
    reseal(&mut state);
    state
}

fn reseal(state: &mut PreinstallState) {
    state
        .target_snapshot
        .as_mut()
        .unwrap()
        .seal(&state.patch_assets, &state.deleted_files)
        .unwrap();
}

fn plan_from_state(state: PreinstallState, unique_chunks: Vec<PatchChunkInfo>) -> PreinstallPlan {
    PreinstallPlan {
        patch_assets: state.patch_assets,
        deleted_files: state.deleted_files,
        unique_chunks,
        diff_downloads: state.diff_downloads,
        main_chunk_download: state.main_chunk_download,
        tag: state.tag,
        installed_tag: state.installed_tag,
        main_manifest_ids: state.main_manifest_ids,
        target_snapshot: state.target_snapshot.unwrap(),
    }
}

#[test]
fn snapshot_roundtrip_integrity_identity_and_coverage() {
    let dir = tempfile::tempdir().unwrap();
    let state = snapshot_state(
        &[
            ("game", "data.bin", b"target"),
            ("en-us", "vo.bin", b"voice"),
        ],
        "http://localhost",
    );
    save_preinstall_state(dir.path(), &state).unwrap();
    let loaded = load_preinstall_state(dir.path(), "2.0").unwrap();
    assert_eq!(loaded.target_snapshot.as_ref().unwrap().manifests.len(), 2);
    assert_eq!(
        loaded.target_snapshot.as_ref().unwrap().manifests[0]
            .manifest
            .assets[0]
            .asset_chunks[0]
            .chunk_old_offset,
        -1
    );
    let mut corrupt = state.clone();
    corrupt.target_snapshot.as_mut().unwrap().manifests[0]
        .chunk_download
        .url_suffix = "wrong".into();
    assert!(save_preinstall_state(dir.path(), &corrupt).is_err());
    corrupt = state.clone();
    corrupt.target_snapshot.as_mut().unwrap().tag = "3.0".into();
    reseal(&mut corrupt);
    assert!(save_preinstall_state(dir.path(), &corrupt).is_err());
    corrupt = state.clone();
    corrupt.target_snapshot.as_mut().unwrap().manifests.pop();
    reseal(&mut corrupt);
    assert!(save_preinstall_state(dir.path(), &corrupt).is_err());
    corrupt = state.clone();
    corrupt.patch_assets[0].target_file_hash = BLANK_FILE_MD5.into();
    reseal(&mut corrupt);
    assert!(save_preinstall_state(dir.path(), &corrupt).is_err());
    corrupt = state.clone();
    corrupt.patch_assets[0].target_file_size += 1;
    reseal(&mut corrupt);
    assert!(save_preinstall_state(dir.path(), &corrupt).is_err());
    // Wrong filename cannot redirect the installed target tag.
    fs::copy(
        PreinstallState::state_file_path(dir.path(), "2.0"),
        PreinstallState::state_file_path(dir.path(), "3.0"),
    )
    .unwrap();
    assert!(load_preinstall_state(dir.path(), "3.0").is_err());
}

#[test]
fn snapshot_rejects_paths_gaps_overflows_and_missing_chunks() {
    let state = snapshot_state(&[("game", "data.bin", b"target")], "http://localhost");
    for case in 0..5 {
        let mut bad = state.clone();
        let file = &mut bad.target_snapshot.as_mut().unwrap().manifests[0]
            .manifest
            .assets[0];
        match case {
            0 => file.asset_name = "../escape".into(),
            1 => file.asset_chunks[0].chunk_name = "../escape".into(),
            2 => file.asset_chunks[0].chunk_on_file_offset = 1,
            3 => file.asset_chunks[0].chunk_size_decompressed = u64::MAX,
            _ => file.asset_chunks.clear(),
        }
        reseal(&mut bad);
        assert!(
            bad.target_snapshot
                .as_ref()
                .unwrap()
                .validate(&bad)
                .is_err(),
            "case {case}"
        );
    }
}

#[tokio::test]
async fn publication_saves_valid_state_before_marker_and_rejects_changed_source() {
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let state = snapshot_state(&[("game", "empty", b"")], "http://localhost");
    // No patch bytes required. State publication failure must not create a marker.
    let state_path = PreinstallState::state_file_path(dir.path(), &state.tag);
    fs::create_dir(&state_path).unwrap();
    let result = preinstall_download(
        &Client::new(),
        plan_from_state(state.clone(), vec![]),
        dir.path(),
        "nap",
        "en",
        DownloadHandle::new(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        HashMap::new(),
    )
    .await;
    assert!(result.is_err());
    assert!(!PreinstallState::marker_file_path(dir.path(), &state.tag).exists());
    fs::remove_dir(state_path).unwrap();
    let gd = dir.path().to_owned();
    let change_source = Arc::new(move |_: &HashMap<String, u64>| {
        super::super::write_installed_tag(&gd, "changed").unwrap();
    });
    let result = preinstall_download(
        &Client::new(),
        plan_from_state(state.clone(), vec![]),
        dir.path(),
        "nap",
        "en",
        DownloadHandle::new(),
        Arc::new(|_| {}),
        change_source,
        HashMap::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(SophonError::PreinstallStateInvalid(_))
    ));
    assert!(!PreinstallState::marker_file_path(dir.path(), &state.tag).exists());
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    preinstall_download(
        &Client::new(),
        plan_from_state(state.clone(), vec![]),
        dir.path(),
        "nap",
        "en",
        DownloadHandle::new(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        HashMap::new(),
    )
    .await
    .unwrap();
    assert!(
        load_preinstall_state(dir.path(), &state.tag)
            .unwrap()
            .target_snapshot
            .is_some()
    );
    assert!(PreinstallState::marker_file_path(dir.path(), &state.tag).exists());
}

fn patch_download_fixture(url: &str) -> (PreinstallState, PatchChunkInfo) {
    let mut state = snapshot_state(&[("game", "data.bin", b"target")], url);
    let asset = &mut state.patch_assets[0];
    asset.patch_method = PatchMethod::CopyOver;
    asset.patch_name = "patch".into();
    asset.patch_hash = hex::encode(fast_md5::digest(b"target"));
    asset.patch_size = 6;
    asset.patch_chunk_length = 6;
    let chunk = PatchChunkInfo {
        patch_name: "patch".into(),
        patch_size: 6,
        patch_md5: asset.patch_hash.clone(),
        matching_field: "game".into(),
    };
    state.diff_downloads.insert(
        "game".into(),
        Arc::new(DownloadInfo {
            url_prefix: url.into(),
            url_suffix: "patches".into(),
            ..make_download_info()
        }),
    );
    reseal(&mut state);
    (state, chunk)
}

#[tokio::test]
async fn resumed_missing_or_corrupt_patch_chunks_are_downloaded_before_ready() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    let server = MockServer::start().await;
    Mock::given(path("/patches/patch"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"target".to_vec()))
        .expect(2)
        .mount(&server)
        .await;
    for corrupt in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        super::super::write_installed_tag(dir.path(), "1.0").unwrap();
        fs::create_dir_all(patching_chunk_dir(dir.path())).unwrap();
        if corrupt {
            fs::write(patching_chunk_dir(dir.path()).join("patch"), b"broken").unwrap();
        }
        let (state, chunk) = patch_download_fixture(&server.uri());
        preinstall_download(
            &Client::new(),
            plan_from_state(state, vec![chunk]),
            dir.path(),
            "nap",
            "en",
            DownloadHandle::new(),
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            HashMap::from([("patch".into(), 6)]),
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read(patching_chunk_dir(dir.path()).join("patch")).unwrap(),
            b"target"
        );
        assert!(load_preinstall_state(dir.path(), "2.0").is_ok());
    }
}

#[tokio::test]
async fn download_worker_panic_cannot_publish_ready_state() {
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let (state, chunk) = patch_download_fixture("http://localhost");
    let handle = DownloadHandle::new();
    handle.pause();
    let updater = Arc::new(|p| {
        if matches!(p, SophonProgress::Paused { .. }) {
            panic!("worker callback panic");
        }
    });
    let result = preinstall_download(
        &Client::new(),
        plan_from_state(state, vec![chunk]),
        dir.path(),
        "nap",
        "en",
        handle,
        updater,
        Arc::new(|_| {}),
        HashMap::new(),
    )
    .await;
    assert!(matches!(result, Err(SophonError::JoinError(_))));
    assert!(!PreinstallState::marker_file_path(dir.path(), "2.0").exists());
    assert!(!PreinstallState::state_file_path(dir.path(), "2.0").exists());
}

async fn serve_target_chunks(
    server: &wiremock::MockServer,
    state: &mut PreinstallState,
    fields: &[(&str, &str, &[u8])],
) {
    use wiremock::{Mock, ResponseTemplate, matchers::path};
    for entry in &mut state.target_snapshot.as_mut().unwrap().manifests {
        let data = fields
            .iter()
            .find(|(field, _, _)| *field == entry.matching_field)
            .unwrap()
            .2;
        if data.is_empty() {
            continue;
        }
        let compressed = zstd::encode_all(data, 1).unwrap();
        let chunk = &mut entry.manifest.assets[0].asset_chunks[0];
        chunk.chunk_size = compressed.len() as u64;
        chunk.chunk_compressed_hash_md5 = hex::encode(fast_md5::digest(&compressed));
        entry.chunk_download.compression = crate::api_scrape::Compression::Zstd;
        Mock::given(path(format!(
            "/{}/{}",
            entry.matching_field, chunk.chunk_name
        )))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed))
        .expect(1)
        .mount(server)
        .await;
    }
    reseal(state);
}

#[tokio::test]
async fn completed_snapshot_applies_without_metadata_even_after_branch_removal_or_advance() {
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    for advanced in [false, true] {
        let server = MockServer::start().await;
        let dir = tempfile::tempdir().unwrap();
        super::super::write_installed_tag(dir.path(), "1.0").unwrap();
        let fields: &[(&str, &str, &[u8])] = &[
            ("game", "data.bin", b"new-game"),
            ("en-us", "voice.bin", b"new-voice"),
            ("cutscenes", "movie.bin", b"new-movie"),
        ];
        let mut state = snapshot_state(fields, &server.uri());
        serve_target_chunks(&server, &mut state, fields).await;
        preinstall_download(
            &Client::new(),
            plan_from_state(state, vec![]),
            dir.path(),
            "nap",
            "en",
            DownloadHandle::new(),
            Arc::new(|_| {}),
            Arc::new(|_| {}),
            HashMap::new(),
        )
        .await
        .unwrap();
        // Every modern metadata dependency is behind the legacy-only resolver.
        Mock::given(path("/metadata"))
            .respond_with(if advanced {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"tag":"3.0"}))
            } else {
                ResponseTemplate::new(404)
            })
            .expect(0)
            .mount(&server)
            .await;
        let calls = AtomicUsize::new(0);
        apply_preinstall_with_resolver(
            &Client::new(),
            dir.path(),
            "2.0",
            Arc::new(|_| {}),
            &DownloadHandle::new(),
            |_| async {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(invalid_state("metadata must not be consulted"))
            },
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for &(_, path, data) in fields {
            assert_eq!(fs::read(dir.path().join(path)).unwrap(), data);
        }
        assert_eq!(read_installed_tag(dir.path()).as_deref(), Some("2.0"));
        assert!(!PreinstallState::state_file_path(dir.path(), "2.0").exists());
    }
}

#[tokio::test]
async fn snapshot_fallbacks_ignore_damaged_old_offsets_and_regenerate_hk4e_indexes() {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let fields: &[(&str, &str, &[u8])] = &[
        ("game", "data.bin", b"new-game"),
        ("en-us", "Audio/English(US)/voice.bin", b"new-voice"),
        ("cutscenes", "movie.bin", b"new-movie"),
    ];
    let mut state = snapshot_state(fields, &server.uri());
    state.game_id = "hk4e".into();
    state.target_snapshot.as_mut().unwrap().game_id = "hk4e".into();
    serve_target_chunks(&server, &mut state, fields).await;
    state.patch_assets[0].patch_method = PatchMethod::CopyOver;
    state.patch_assets[0].patch_name = "missing-copy".into();
    state.patch_assets[1].patch_method = PatchMethod::Patch;
    state.patch_assets[1].patch_name = "missing-hdiff".into();
    state.patch_assets[1].original_file_path = Some("missing-old-voice".into());
    // Force an old-file temptation: downloaded data must win, not offset zero.
    fs::write(dir.path().join("data.bin"), b"bad-game").unwrap();
    reseal(&mut state);
    save_preinstall_state(dir.path(), &state).unwrap();
    apply_preinstall(
        &Client::new(),
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &DownloadHandle::new(),
    )
    .await
    .unwrap();
    for &(_, path, data) in fields {
        assert_eq!(fs::read(dir.path().join(path)).unwrap(), data);
    }
    let pkg = fs::read_to_string(dir.path().join("pkg_version")).unwrap();
    assert!(pkg.contains("data.bin") && pkg.contains("movie.bin") && pkg.contains("voice.bin"));
    let audio = fs::read_to_string(dir.path().join("Audio_English(US)_pkg_version")).unwrap();
    assert!(audio.contains("voice.bin") && !audio.contains("movie.bin"));
    assert_eq!(read_installed_tag(dir.path()).as_deref(), Some("2.0"));
}

fn test_branch(name: &str, tag: &str) -> PackageBranch {
    PackageBranch {
        package_id: "package".into(),
        branch: name.into(),
        password: String::new(),
        tag: tag.into(),
    }
}

fn build_from_snapshot(snapshot: &TargetSnapshot, url: &str) -> SophonBuildData {
    SophonBuildData {
        build_id: snapshot.build_id.clone(),
        tag: snapshot.tag.clone(),
        manifests: snapshot
            .manifests
            .iter()
            .map(|entry| SophonManifestMeta {
                category_id: String::new(),
                category_name: String::new(),
                matching_field: entry.matching_field.clone(),
                manifest: crate::api_scrape::ManifestFileInfo {
                    id: entry.manifest_id.clone(),
                    checksum: String::new(),
                    compressed_size: "0".into(),
                    uncompressed_size: "0".into(),
                },
                chunk_download: entry.chunk_download.clone(),
                manifest_download: DownloadInfo {
                    url_prefix: url.into(),
                    url_suffix: "manifests".into(),
                    ..make_download_info()
                },
                stats: crate::api_scrape::Stats {
                    compressed_size: "0".into(),
                    uncompressed_size: "0".into(),
                    file_count: "0".into(),
                    chunk_count: "0".into(),
                },
                deduplicated_stats: None,
            })
            .collect(),
    }
}

#[tokio::test]
async fn legacy_exact_recovery_preserves_en_us_omitted_voice_and_migrates_before_mutation() {
    use prost::Message;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let fields: &[(&str, &str, &[u8])] = &[
        ("game", "data.bin", b"new-game"),
        ("cutscenes", "movie.bin", b"new-movie"),
    ];
    let mut state = snapshot_state(fields, &server.uri());
    state.vo_lang = "en-US".into();
    state.game_id = "hk4e".into();
    let snapshot = state.target_snapshot.as_mut().unwrap();
    snapshot.vo_lang = "en-US".into();
    snapshot.game_id = "hk4e".into();
    serve_target_chunks(&server, &mut state, fields).await;
    // An available English target must not broaden this old package's historical scope.
    let extra = snapshot_state(
        &[("game", "unused", b""), ("en-us", "voice.bin", b"new")],
        &server.uri(),
    );
    let mut build = build_from_snapshot(state.target_snapshot.as_ref().unwrap(), &server.uri());
    build.manifests.push(
        build_from_snapshot(extra.target_snapshot.as_ref().unwrap(), &server.uri())
            .manifests
            .pop()
            .unwrap(),
    );
    for entry in &state.target_snapshot.as_ref().unwrap().manifests {
        Mock::given(path(format!("/manifests/{}", entry.manifest_id)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(entry.manifest.encode_to_vec()))
            .expect(1)
            .mount(&server)
            .await;
    }
    Mock::given(path("/manifests/manifest-en-us"))
        .respond_with(ResponseTemplate::new(404))
        .expect(0)
        .mount(&server)
        .await;
    state.target_snapshot = None;
    state.deleted_files.push("Audio/English(US)/old.bin".into());
    fs::create_dir_all(dir.path().join("Audio/English(US)")).unwrap();
    fs::write(
        dir.path().join("Audio/English(US)/old.bin"),
        b"keep old voice",
    )
    .unwrap();
    fs::write(
        dir.path().join("Audio_English(US)_pkg_version"),
        b"keep old index",
    )
    .unwrap();
    save_preinstall_state(dir.path(), &state).unwrap();
    let requests = Mutex::new(Vec::new());
    let client = Client::new();
    // Cancel immediately after exact recovery: migration must persist before the
    // first game-file mutation, and the retry must need no metadata at all.
    let handle = DownloadHandle::new();
    let result = apply_preinstall_with_resolver(
        &client,
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &handle,
        |old| {
            let client = &client;
            let requests = &requests;
            let handle = &handle;
            async move {
                let recovered = recover_legacy_from_branches(
                    client,
                    &old,
                    vec![test_branch("main", "2.0"), test_branch("pre", "3.0")],
                    |branch, tag| {
                        requests
                            .lock()
                            .unwrap()
                            .push((branch.branch.clone(), tag.clone()));
                        let build = build.clone();
                        async move {
                            if branch.branch == "main" {
                                Err(SophonError::NoManifests)
                            } else {
                                Ok(build)
                            }
                        }
                    },
                )
                .await?;
                handle.cancel();
                Ok(recovered)
            }
        },
    )
    .await;
    assert!(matches!(result, Err(SophonError::Cancelled)));
    assert_eq!(
        *requests.lock().unwrap(),
        vec![("main".into(), "2.0".into()), ("pre".into(), "2.0".into())]
    );
    let migrated = load_preinstall_state(dir.path(), "2.0").unwrap();
    assert_eq!(migrated.main_manifest_ids.len(), 2);
    assert!(migrated.target_snapshot.as_ref().unwrap().legacy_coverage);
    assert!(!dir.path().join("data.bin").exists());
    apply_preinstall(
        &client,
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &DownloadHandle::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read(dir.path().join("Audio/English(US)/old.bin")).unwrap(),
        b"keep old voice"
    );
    assert_eq!(
        fs::read(dir.path().join("Audio_English(US)_pkg_version")).unwrap(),
        b"keep old index"
    );
    assert!(!dir.path().join("voice.bin").exists());
}

#[tokio::test]
async fn missing_legacy_metadata_and_failed_migration_keep_payloads_and_version() {
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let mut state = snapshot_state(&[("game", "data.bin", b"target")], "http://localhost");
    let mut snapshot = state.target_snapshot.take().unwrap();
    snapshot.legacy_coverage = true;
    snapshot
        .seal(&state.patch_assets, &state.deleted_files)
        .unwrap();
    save_preinstall_state(dir.path(), &state).unwrap();
    fs::create_dir_all(patching_chunk_dir(dir.path())).unwrap();
    let payload = patching_chunk_dir(dir.path()).join("keep");
    fs::write(&payload, b"cached payload").unwrap();
    let result = apply_preinstall_with_resolver(
        &Client::new(),
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &DownloadHandle::new(),
        |_| async { Err(invalid_state("exact manifest no longer available")) },
    )
    .await;
    assert!(
        result
            .unwrap_err()
            .to_string()
            .contains("normal update/repair")
    );
    // Force the atomic migration write to fail without touching the original JSON.
    fs::create_dir(PreinstallState::state_file_path(dir.path(), "2.0").with_extension("json.tmp"))
        .unwrap();
    assert!(
        apply_preinstall_with_resolver(
            &Client::new(),
            dir.path(),
            "2.0",
            Arc::new(|_| {}),
            &DownloadHandle::new(),
            |_| async { Ok(snapshot) }
        )
        .await
        .is_err()
    );
    assert!(
        load_preinstall_state(dir.path(), "2.0")
            .unwrap()
            .target_snapshot
            .is_none()
    );
    assert!(payload.exists());
    assert!(!dir.path().join("data.bin").exists());
    assert_eq!(read_installed_tag(dir.path()).as_deref(), Some("1.0"));
}

#[tokio::test]
async fn snapshot_apply_failure_and_cancellation_keep_metadata_and_payloads() {
    let server = wiremock::MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let fields: &[(&str, &str, &[u8])] = &[("game", "data.bin", b"target")];
    let mut state = snapshot_state(fields, &server.uri());
    serve_target_chunks(&server, &mut state, fields).await;
    let wrong_hash = hex::encode(fast_md5::digest(b"wrong!"));
    state.patch_assets[0].target_file_hash = wrong_hash.clone();
    state.target_snapshot.as_mut().unwrap().manifests[0]
        .manifest
        .assets[0]
        .asset_hash_md5 = wrong_hash;
    reseal(&mut state);
    save_preinstall_state(dir.path(), &state).unwrap();
    fs::create_dir_all(patching_chunk_dir(dir.path())).unwrap();
    fs::write(patching_chunk_dir(dir.path()).join("keep"), b"patch").unwrap();
    let handle = DownloadHandle::new();
    handle.cancel();
    assert!(matches!(
        apply_preinstall(&Client::new(), dir.path(), "2.0", Arc::new(|_| {}), &handle).await,
        Err(SophonError::Cancelled)
    ));
    assert!(load_preinstall_state(dir.path(), "2.0").is_ok());
    let result = apply_preinstall(
        &Client::new(),
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &DownloadHandle::new(),
    )
    .await;
    assert!(
        matches!(result, Err(SophonError::AssemblyFailed { .. })),
        "{result:?}"
    );
    assert!(load_preinstall_state(dir.path(), "2.0").is_ok());
    assert!(patching_chunk_dir(dir.path()).join("keep").exists());
    assert!(dir.path().join("chunks/chunk-game.zstd").exists());
    assert_eq!(read_installed_tag(dir.path()).as_deref(), Some("1.0"));
}

#[tokio::test]
async fn discovery_prefers_valid_live_package_and_keeps_its_sizes_when_queries_fail() {
    let dir = tempfile::tempdir().unwrap();
    let state = snapshot_state(&[("game", "data.bin", b"target")], "http://localhost");
    save_preinstall_state(dir.path(), &state).unwrap();
    assert!(!PreinstallState::marker_file_path(dir.path(), "2.0").exists());
    for pre in [None, Some(test_branch("pre", "3.0"))] {
        let queries = AtomicUsize::new(0);
        let info = super::super::update::check_update_with_branches(
            "nap",
            "en",
            dir.path(),
            Some("1.0".into()),
            test_branch("main", "2.0"),
            pre,
            |branch, from| {
                queries.fetch_add(1, Ordering::SeqCst);
                assert_eq!(branch.tag, "2.0");
                assert_eq!(from.as_deref(), Some("1.0"));
                async { Err(SophonError::NoManifests) }
            },
        )
        .await
        .unwrap();
        assert!(info.update_available && info.preinstall_available && info.preinstall_downloaded);
        assert_eq!(info.preinstall_tag.as_deref(), Some("2.0"));
        assert_eq!(info.remote_tag, "2.0");
        assert_eq!(
            (
                info.preinstall_compressed_size,
                info.preinstall_decompressed_size
            ),
            (6, 6)
        );
        assert_eq!(
            (info.update_compressed_size, info.update_decompressed_size),
            (6, 6)
        );
        assert_eq!(queries.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn discovery_rejects_markers_arbitrary_json_corrupt_snapshots_and_wrong_identity() {
    let dir = tempfile::tempdir().unwrap();
    let state = snapshot_state(&[("game", "data.bin", b"target")], "http://localhost");
    let path = PreinstallState::state_file_path(dir.path(), "2.0");
    fs::write(PreinstallState::marker_file_path(dir.path(), "2.0"), "2.0").unwrap();
    for case in 0..5 {
        match case {
            0 => {}
            1 => fs::write(&path, "{}").unwrap(),
            2 => {
                let mut corrupt = state.clone();
                corrupt.target_snapshot.as_mut().unwrap().digest = "broken".into();
                fs::write(&path, serde_json::to_vec(&corrupt).unwrap()).unwrap();
            }
            3 => {
                let mut wrong_game = state.clone();
                wrong_game.game_id = "hk4e".into();
                wrong_game.target_snapshot.as_mut().unwrap().game_id = "hk4e".into();
                reseal(&mut wrong_game);
                save_preinstall_state(dir.path(), &wrong_game).unwrap();
            }
            _ => {
                let mut wrong_source = state.clone();
                wrong_source.installed_tag = "0.9".into();
                wrong_source.target_snapshot.as_mut().unwrap().installed_tag = "0.9".into();
                reseal(&mut wrong_source);
                save_preinstall_state(dir.path(), &wrong_source).unwrap();
            }
        }
        let info = super::super::update::check_update_with_branches(
            "nap",
            "en",
            dir.path(),
            Some("1.0".into()),
            test_branch("main", "2.0"),
            Some(test_branch("pre", "3.0")),
            |branch, _| async move {
                Ok(if branch.tag == "3.0" {
                    (300, 900)
                } else {
                    (200, 600)
                })
            },
        )
        .await
        .unwrap();
        assert!(!info.preinstall_downloaded, "case {case}");
        assert_eq!(info.preinstall_tag.as_deref(), Some("3.0"));
        assert_eq!(
            (
                info.preinstall_compressed_size,
                info.preinstall_decompressed_size
            ),
            (300, 900)
        );
    }
}

#[tokio::test]
async fn successful_apply_removes_only_its_package_state_not_shared_patch_payloads() {
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let (mut state, _) = patch_download_fixture("http://localhost");
    state.downloaded_chunks.insert("patch".into());
    // A legitimate patch-only asset need not appear in the normal manifest.
    state.target_snapshot.as_mut().unwrap().manifests[0]
        .manifest
        .assets
        .clear();
    reseal(&mut state);
    save_preinstall_state(dir.path(), &state).unwrap();
    fs::write(PreinstallState::marker_file_path(dir.path(), "2.0"), "2.0").unwrap();
    let mut future = state.clone();
    future.tag = "3.0".into();
    future.target_snapshot.as_mut().unwrap().tag = "3.0".into();
    reseal(&mut future);
    save_preinstall_state(dir.path(), &future).unwrap();
    let future_path = PreinstallState::state_file_path(dir.path(), "3.0");
    let future_before = fs::read(&future_path).unwrap();
    fs::create_dir_all(patching_chunk_dir(dir.path())).unwrap();
    fs::write(patching_chunk_dir(dir.path()).join("patch"), b"target").unwrap();
    fs::write(
        patching_chunk_dir(dir.path()).join("another-incomplete-package"),
        b"keep",
    )
    .unwrap();
    apply_preinstall(
        &Client::new(),
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &DownloadHandle::new(),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(dir.path().join("data.bin")).unwrap(), b"target");
    assert!(!PreinstallState::state_file_path(dir.path(), "2.0").exists());
    assert!(!PreinstallState::marker_file_path(dir.path(), "2.0").exists());
    assert_eq!(fs::read(future_path).unwrap(), future_before);
    assert_eq!(
        fs::read(patching_chunk_dir(dir.path()).join("patch")).unwrap(),
        b"target"
    );
    assert_eq!(
        fs::read(patching_chunk_dir(dir.path()).join("another-incomplete-package")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn planning_captures_each_selected_manifest_once_with_its_download_descriptor() {
    use prost::Message;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    super::super::write_installed_tag(dir.path(), "1.0").unwrap();
    let fields: &[(&str, &str, &[u8])] = &[
        ("game", "data.bin", b"new-game"),
        ("en-us", "voice.bin", b"new-voice"),
        ("cutscenes", "movie.bin", b"new-movie"),
    ];
    let mut fixture = snapshot_state(fields, &server.uri());
    serve_target_chunks(&server, &mut fixture, fields).await;
    let snapshot = fixture.target_snapshot.as_mut().unwrap();
    for entry in &mut snapshot.manifests {
        entry.manifest.assets[0].asset_chunks[0].chunk_old_offset = 0;
        Mock::given(path(format!("/manifests/{}", entry.manifest_id)))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(entry.manifest.encode_to_vec()))
            .expect(1)
            .mount(&server)
            .await;
    }
    let main = build_from_snapshot(snapshot, &server.uri());
    let patch_dl = DownloadInfo {
        url_prefix: server.uri(),
        url_suffix: "patch-manifests".into(),
        ..make_download_info()
    };
    Mock::given(path("/patch-manifests/empty-patch"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(crate::proto_parse::SophonPatchProto::default().encode_to_vec()),
        )
        .expect(1)
        .mount(&server)
        .await;
    let patch = SophonPatchBuildData {
        build_id: "exact-build".into(),
        patch_id: "patch-id".into(),
        tag: "2.0".into(),
        manifests: vec![SophonPatchManifestMeta {
            category_id: String::new(),
            category_name: String::new(),
            matching_field: "game".into(),
            manifest: crate::api_scrape::ManifestFileInfo {
                id: "empty-patch".into(),
                checksum: String::new(),
                compressed_size: "0".into(),
                uncompressed_size: "0".into(),
            },
            diff_download: patch_dl.clone(),
            manifest_download: patch_dl,
            stats: HashMap::new(),
        }],
    };
    let mut wrong = main.clone();
    wrong.tag = "3.0".into();
    assert!(
        build_preinstall_plan_from_builds(
            &Client::new(),
            "nap",
            "en",
            "1.0".into(),
            patch.clone(),
            wrong
        )
        .await
        .is_err()
    );
    let plan =
        build_preinstall_plan_from_builds(&Client::new(), "nap", "en", "1.0".into(), patch, main)
            .await
            .unwrap();
    assert_eq!(plan.installed_tag, "1.0");
    assert_eq!(plan.target_snapshot.manifests.len(), 3);
    for entry in &plan.target_snapshot.manifests {
        assert_eq!(entry.chunk_download.url_suffix, entry.matching_field);
        assert_eq!(
            entry.manifest.assets[0].asset_chunks[0].chunk_old_offset,
            -1
        );
    }
    preinstall_download(
        &Client::new(),
        plan,
        dir.path(),
        "nap",
        "en",
        DownloadHandle::new(),
        Arc::new(|_| {}),
        Arc::new(|_| {}),
        HashMap::new(),
    )
    .await
    .unwrap();
    apply_preinstall(
        &Client::new(),
        dir.path(),
        "2.0",
        Arc::new(|_| {}),
        &DownloadHandle::new(),
    )
    .await
    .unwrap();
    for &(_, path, data) in fields {
        assert_eq!(fs::read(dir.path().join(path)).unwrap(), data);
    }
}

#[tokio::test]
async fn legacy_recovery_rejects_substituted_target_and_manifest_ids() {
    let state = snapshot_state(&[("game", "empty", b"")], "http://localhost");
    let build = build_from_snapshot(state.target_snapshot.as_ref().unwrap(), "http://localhost");
    for wrong_tag in [true, false] {
        let mut bad_build = build.clone();
        if wrong_tag {
            bad_build.tag = "newer".into();
        } else {
            bad_build.manifests[0].manifest.id = "different-id".into();
        }
        let result = recover_legacy_from_branches(
            &Client::new(),
            &state,
            vec![test_branch("main", "3.0")],
            |_, tag| {
                assert_eq!(tag, "2.0");
                let build = bad_build.clone();
                async { Ok(build) }
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(SophonError::PreinstallStateInvalid(_))
        ));
    }
}
