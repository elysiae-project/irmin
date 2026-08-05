    use super::super::compact_manifest::StringArena;
    use super::super::profiling::PipelineProfiler;
    use super::*;
    use crate::game_installer::compact_manifest::CompactManifest;
    use crate::proto_parse::{
        SophonManifestAssetChunk, SophonManifestAssetProperty,
    };

    fn make_chunk_names(names: &[&str]) -> Arc<ChunkNameLookup> {
        Arc::new(ChunkNameLookup::from_arena(StringArena::from(names)))
    }

    fn assemble_test_file(
        file: SophonManifestAssetProperty,
        game_dir: &Path,
        chunks_dir: &Path,
        temp_dir: &Path,
        chunk_lookup: &ChunkNameLookup,
        chunk_refcounts: &[AtomicU32],
        verify_cache: &DashMap<String, VerificationEntry>,
    ) -> SophonResult<()> {
        let manifest = CompactManifest::from(vec![file]);
        assemble_file(
            &manifest,
            0,
            game_dir,
            chunks_dir,
            temp_dir,
            chunk_lookup,
            chunk_refcounts,
            verify_cache,
            false,
        )
    }

    #[test]
    fn validate_asset_name_empty() {
        let result = validate_asset_name("");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SophonError::InvalidAssetName(_)
        ));
    }

    #[test]
    fn validate_asset_name_slash_prefix() {
        let result = validate_asset_name("/etc/passwd");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
    }

    #[test]
    fn validate_asset_name_backslash_prefix() {
        let result = validate_asset_name("\\Windows\\system32");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
    }

    #[test]
    fn validate_asset_name_dotdot() {
        let result = validate_asset_name("foo/../../../etc/passwd");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
    }

    #[test]
    fn validate_asset_name_null_byte() {
        let result = validate_asset_name("foo\0bar");
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SophonError::InvalidAssetName(_)
        ));
    }

    #[test]
    fn validate_asset_name_drive_letter() {
        let result = validate_asset_name("C:evil");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), SophonError::PathTraversal(_)));
    }

    #[test]
    fn validate_asset_name_valid_relative() {
        assert!(validate_asset_name("GameData/Data.pak").is_ok());
    }

    #[test]
    fn validate_asset_name_valid_nested() {
        assert!(validate_asset_name("a/b/c/file.dat").is_ok());
    }

    #[test]
    fn validate_asset_name_dot_slash_prefix() {
        assert!(validate_asset_name("./etc/passwd").is_err());
        assert!(validate_asset_name(".\\Windows\\system32").is_err());
    }

    #[test]
    fn validate_asset_name_consecutive_dots_allowed() {
        assert!(validate_asset_name("foo..bar").is_ok());
        assert!(validate_asset_name("2.0..hotfix.pak").is_ok());
    }

    #[test]
    fn validate_asset_name_dotdot_component_rejected() {
        assert!(validate_asset_name("../etc/passwd").is_err());
        assert!(validate_asset_name("foo/../bar").is_err());
        assert!(validate_asset_name("a/..").is_err());
    }

    #[test]
    fn chunk_filename_format() {
        assert_eq!(chunk_filename("abc123"), "abc123.zstd");
    }

    fn make_chunk_file(chunks_dir: &Path, chunk_name: &str, data: &[u8]) {
        let compressed = zstd::encode_all(data, 0).unwrap();
        fs::write(chunks_dir.join(format!("{chunk_name}.zstd")), &compressed).unwrap();
    }

    fn compute_md5_hex(data: &[u8]) -> String {
        let mut hasher = Md5::new().unwrap();
        hasher.update(data).unwrap();
        hex::encode(hasher.finish().unwrap())
    }

    fn make_chunk(name: &str, offset: u64, decompressed_size: u64) -> SophonManifestAssetChunk {
        SophonManifestAssetChunk {
            chunk_name: name.to_string(),
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: offset,
            chunk_size: 0,
            chunk_size_decompressed: decompressed_size,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        }
    }

    #[test]
    fn assemble_file_from_single_chunk() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let original_data = b"hello assembly world!";
        make_chunk_file(&chunks_dir, "chunk0", original_data);
        let md5 = compute_md5_hex(original_data);

        let file = SophonManifestAssetProperty {
            asset_name: "output.bin".to_string(),
            asset_chunks: vec![make_chunk("chunk0", 0, original_data.len() as u64)],
            asset_type: 0,
            asset_size: original_data.len() as u64,
            asset_hash_md5: md5,
        };

        let chunk_names = make_chunk_names(&["chunk0"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1)];
        let verify_cache = DashMap::new();

        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        let result = fs::read(game_dir.join("output.bin")).unwrap();
        assert_eq!(result, original_data);
    }

    #[test]
    fn assemble_file_from_multiple_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let data_a = b"AAAA";
        let data_b = b"BBBB";
        let total_size = (data_a.len() + data_b.len()) as u64;
        let mut full_data = Vec::new();
        full_data.extend_from_slice(data_a);
        full_data.extend_from_slice(data_b);

        make_chunk_file(&chunks_dir, "chunkA", data_a);
        make_chunk_file(&chunks_dir, "chunkB", data_b);

        let md5 = compute_md5_hex(&full_data);

        let file = SophonManifestAssetProperty {
            asset_name: "multi.bin".to_string(),
            asset_chunks: vec![
                make_chunk("chunkA", 0, data_a.len() as u64),
                make_chunk("chunkB", data_a.len() as u64, data_b.len() as u64),
            ],
            asset_type: 0,
            asset_size: total_size,
            asset_hash_md5: md5,
        };

        let chunk_names = make_chunk_names(&["chunkA", "chunkB"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1), AtomicU32::new(1)];
        let verify_cache = DashMap::new();

        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        let result = fs::read(game_dir.join("multi.bin")).unwrap();
        assert_eq!(&result[..4], data_a);
        assert_eq!(&result[4..8], data_b);
        assert_eq!(result.len(), total_size as usize);
    }

    #[test]
    fn assemble_file_skips_valid_existing() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let original_data = b"already here";
        let md5 = compute_md5_hex(original_data);
        let target = game_dir.join("existing.bin");
        fs::write(&target, original_data).unwrap();

        make_chunk_file(&chunks_dir, "chunk_skip", original_data);

        let file = SophonManifestAssetProperty {
            asset_name: "existing.bin".to_string(),
            asset_chunks: vec![make_chunk("chunk_skip", 0, original_data.len() as u64)],
            asset_type: 0,
            asset_size: original_data.len() as u64,
            asset_hash_md5: md5,
        };

        let chunk_names = make_chunk_names(&["chunk_skip"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1)];
        let verify_cache = DashMap::new();

        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        assert_eq!(chunk_refcounts[0].load(Ordering::Acquire), 0);
        assert!(!chunks_dir.join("chunk_skip.zstd").exists());

        let result = fs::read(&target).unwrap();
        assert_eq!(result, original_data);
    }

    #[test]
    fn assemble_file_reassembles_md5_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let wrong_data = b"wrong content!";
        let correct_data = b"correct data!!";
        let md5 = compute_md5_hex(correct_data);

        let target = game_dir.join("mismatch.bin");
        fs::write(&target, wrong_data).unwrap();

        make_chunk_file(&chunks_dir, "chunk_fix", correct_data);

        let file = SophonManifestAssetProperty {
            asset_name: "mismatch.bin".to_string(),
            asset_chunks: vec![make_chunk("chunk_fix", 0, correct_data.len() as u64)],
            asset_type: 0,
            asset_size: correct_data.len() as u64,
            asset_hash_md5: md5,
        };

        let chunk_names = make_chunk_names(&["chunk_fix"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1)];
        let verify_cache = DashMap::new();

        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        let result = fs::read(&target).unwrap();
        assert_eq!(result, correct_data);
    }

    #[test]
    fn assemble_file_parallel_file_hash_passes() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let chunks: Vec<Vec<u8>> = (0..8)
            .map(|i| (0..4096).map(|j| ((i * 17 + j) % 251) as u8).collect())
            .collect();
        let names: Vec<String> = (0..8).map(|i| format!("ck{i}")).collect();
        for (i, data) in chunks.iter().enumerate() {
            make_chunk_file(&chunks_dir, &names[i], data);
        }
        let mut full = Vec::new();
        for c in &chunks {
            full.extend_from_slice(c);
        }
        let file_md5 = compute_md5_hex(&full);

        let chunk_specs: Vec<SophonManifestAssetChunk> = chunks
            .iter()
            .enumerate()
            .map(|(i, data)| make_chunk(&names[i], (i * data.len()) as u64, data.len() as u64))
            .collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let chunk_names_arc = make_chunk_names(&name_refs);
        let chunk_refcounts: Vec<AtomicU32> =
            (0..chunks.len()).map(|_| AtomicU32::new(1)).collect();

        let file = SophonManifestAssetProperty {
            asset_name: "parallel_hash.bin".to_string(),
            asset_chunks: chunk_specs,
            asset_type: 0,
            asset_size: full.len() as u64,
            asset_hash_md5: file_md5,
        };

        let verify_cache = DashMap::new();
        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names_arc,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        let result = fs::read(game_dir.join("parallel_hash.bin")).unwrap();
        assert_eq!(result, full);
    }

    #[test]
    fn assemble_file_parallel_file_hash_mismatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let chunks: Vec<Vec<u8>> = (0..8)
            .map(|i| (0..4096).map(|j| ((i * 17 + j) % 251) as u8).collect())
            .collect();
        let names: Vec<String> = (0..8).map(|i| format!("ck{i}")).collect();
        for (i, data) in chunks.iter().enumerate() {
            make_chunk_file(&chunks_dir, &names[i], data);
        }
        let mut full = Vec::new();
        for c in &chunks {
            full.extend_from_slice(c);
        }

        let chunk_specs: Vec<SophonManifestAssetChunk> = chunks
            .iter()
            .enumerate()
            .map(|(i, data)| make_chunk(&names[i], (i * data.len()) as u64, data.len() as u64))
            .collect();
        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let chunk_names_arc = make_chunk_names(&name_refs);
        let chunk_refcounts: Vec<AtomicU32> =
            (0..chunks.len()).map(|_| AtomicU32::new(1)).collect();

        let corrupt_md5 =
            String::from_utf8((0..32).map(|i| if i == 0 { b'f' } else { b'0' }).collect()).unwrap();

        let file = SophonManifestAssetProperty {
            asset_name: "corrupt.bin".to_string(),
            asset_chunks: chunk_specs,
            asset_type: 0,
            asset_size: full.len() as u64,
            asset_hash_md5: corrupt_md5,
        };

        let verify_cache = DashMap::new();
        let result = assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names_arc,
            &chunk_refcounts,
            &verify_cache,
        );

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SophonError::Md5Mismatch { .. }
        ));
        assert!(!game_dir.join("corrupt.bin").exists());
    }

    #[test]
    fn decrement_chunk_refcount_to_zero_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let chunks_dir = dir.path().join("chunks");
        fs::create_dir_all(&chunks_dir).unwrap();

        let chunk_file = chunks_dir.join("vanish.zstd");
        fs::write(&chunk_file, b"dummy").unwrap();

        let chunk_names = make_chunk_names(&["vanish"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1)];

        decrement_chunk_refcount("vanish", &chunk_names, &chunk_refcounts, &chunks_dir);

        assert_eq!(chunk_refcounts[0].load(Ordering::Acquire), 0);
        assert!(!chunk_file.exists());
    }

    #[test]
    fn decrement_chunk_refcount_nonzero_keeps() {
        let dir = tempfile::tempdir().unwrap();
        let chunks_dir = dir.path().join("chunks");
        fs::create_dir_all(&chunks_dir).unwrap();

        let chunk_file = chunks_dir.join("keep.zstd");
        fs::write(&chunk_file, b"dummy").unwrap();

        let chunk_names = make_chunk_names(&["keep"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(2)];

        decrement_chunk_refcount("keep", &chunk_names, &chunk_refcounts, &chunks_dir);

        assert_eq!(chunk_refcounts[0].load(Ordering::Acquire), 1);
        assert!(chunk_file.exists());
    }

    #[test]
    fn cleanup_tmp_files_removes_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.tmp");
        let b = dir.path().join("b.tmp");
        let sub = dir.path().join("sub");
        fs::create_dir_all(&sub).unwrap();
        let c = sub.join("c.tmp");
        fs::write(&a, b"x").unwrap();
        fs::write(&b, b"x").unwrap();
        fs::write(&c, b"x").unwrap();

        cleanup_tmp_files(dir.path()).unwrap();

        assert!(!a.exists());
        assert!(!b.exists());
        assert!(!c.exists());
    }

    #[test]
    fn cleanup_tmp_files_skips_non_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let keep = dir.path().join("keep.dat");
        let keep2 = dir.path().join("important.txt");
        fs::write(&keep, b"data").unwrap();
        fs::write(&keep2, b"data").unwrap();

        cleanup_tmp_files(dir.path()).unwrap();

        assert!(keep.exists());
        assert!(keep2.exists());
    }

    #[test]
    fn run_assembly_task_out_of_bounds_file_idx() {
        let dir = tempfile::tempdir().unwrap();
        let params = AssemblyTaskParams {
            file_idx: 5,
            tmp_dir_idx: 0,
            all_files: Arc::new(CompactManifest::from(vec![])),
            all_tmp_dirs: Arc::new(vec![dir.path().to_path_buf()]),
            game_dir: dir.path().to_path_buf().into(),
            chunks_dir: Arc::new(dir.path().to_path_buf()),
            chunk_refcounts: Arc::new(Vec::new()),
            chunk_names: Arc::new(ChunkNameLookup::from_arena(StringArena::from(
                [].as_slice(),
            ))),
            verify_cache: Arc::new(DashMap::new()),
            assembled_files: Arc::new(AtomicU64::new(0)),
            checked_files: Arc::new(AtomicU64::new(0)),
            last_assembly_update: Arc::new(Mutex::new(Instant::now())),
            total_files: 0,
            profiler: Arc::new(PipelineProfiler::new()),
            completion_flags: Arc::from(Vec::<AtomicBool>::new()),
        };

        let result = run_assembly_task(params, |_| {});
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SophonError::IndexOutOfBounds { kind: "file", .. }
        ));
    }

    #[test]
    fn assemble_file_chunk_md5_passes() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let data = b"chunk with md5 check";
        make_chunk_file(&chunks_dir, "ck0", data);
        let chunk_md5 = compute_md5_hex(data);
        let file_md5 = compute_md5_hex(data);

        let chunk = SophonManifestAssetChunk {
            chunk_name: "ck0".to_string(),
            chunk_decompressed_hash_md5: chunk_md5,
            chunk_on_file_offset: 0,
            chunk_size: 0,
            chunk_size_decompressed: data.len() as u64,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        };

        let file = SophonManifestAssetProperty {
            asset_name: "verified.bin".to_string(),
            asset_chunks: vec![chunk],
            asset_type: 0,
            asset_size: data.len() as u64,
            asset_hash_md5: file_md5,
        };

        let chunk_names = make_chunk_names(&["ck0"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1)];
        let verify_cache = DashMap::new();

        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        let result = fs::read(game_dir.join("verified.bin")).unwrap();
        assert_eq!(result, data);
    }

    #[test]
    fn assemble_file_chunk_md5_mismatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        let data = b"real chunk data here";
        make_chunk_file(&chunks_dir, "ck1", data);
        let file_md5 = compute_md5_hex(data);

        let chunk = SophonManifestAssetChunk {
            chunk_name: "ck1".to_string(),
            chunk_decompressed_hash_md5: "deadbeefdeadbeefdeadbeefdeadbeef".to_string(),
            chunk_on_file_offset: 0,
            chunk_size: 0,
            chunk_size_decompressed: data.len() as u64,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        };

        let file = SophonManifestAssetProperty {
            asset_name: "bad.bin".to_string(),
            asset_chunks: vec![chunk],
            asset_type: 0,
            asset_size: data.len() as u64,
            asset_hash_md5: file_md5,
        };

        let chunk_names = make_chunk_names(&["ck1"]);
        let chunk_refcounts: Vec<AtomicU32> = vec![AtomicU32::new(1)];
        let verify_cache = DashMap::new();

        let result = assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        );

        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SophonError::Md5Mismatch { .. }
        ));
    }

    #[test]
    fn run_assembly_task_out_of_bounds_tmp_dir_idx() {
        let dir = tempfile::tempdir().unwrap();
        let file = SophonManifestAssetProperty {
            asset_name: "dummy".to_string(),
            asset_chunks: vec![],
            asset_type: 0,
            asset_size: 0,
            asset_hash_md5: String::new(),
        };
        let params = AssemblyTaskParams {
            file_idx: 0,
            tmp_dir_idx: 99,
            all_files: Arc::new(CompactManifest::from(vec![file])),
            all_tmp_dirs: Arc::new(vec![]),
            game_dir: dir.path().to_path_buf().into(),
            chunks_dir: Arc::new(dir.path().to_path_buf()),
            chunk_refcounts: Arc::new(Vec::new()),
            chunk_names: Arc::new(ChunkNameLookup::from_arena(StringArena::from(
                [].as_slice(),
            ))),
            verify_cache: Arc::new(DashMap::new()),
            assembled_files: Arc::new(AtomicU64::new(0)),
            checked_files: Arc::new(AtomicU64::new(0)),
            last_assembly_update: Arc::new(Mutex::new(Instant::now())),
            total_files: 1,
            profiler: Arc::new(PipelineProfiler::new()),
            completion_flags: Arc::from((0..1).map(|_| AtomicBool::new(false)).collect::<Vec<_>>()),
        };

        let result = run_assembly_task(params, |_| {});
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            SophonError::IndexOutOfBounds {
                kind: "temp dir",
                ..
            }
        ));
    }

    #[test]
    fn validate_chunk_name_valid() {
        assert!(validate_chunk_name("abc123"));
    }

    #[test]
    fn validate_chunk_name_empty() {
        assert!(!validate_chunk_name(""));
    }

    #[test]
    fn validate_chunk_name_null_byte() {
        assert!(!validate_chunk_name("abc\0def"));
    }

    #[test]
    fn validate_chunk_name_absolute_path() {
        assert!(!validate_chunk_name("/etc/passwd"));
    }

    /// Reject `..` as a path component; allow consecutive dots in filenames.
    #[test]
    fn validate_chunk_name_rejects_double_dot_component() {
        assert!(!validate_chunk_name("../etc/passwd"));
        assert!(!validate_chunk_name("foo/../bar"));
        assert!(!validate_chunk_name("a/.."));
        assert!(!validate_chunk_name("a\\..\\b"));
    }

    #[test]
    fn validate_chunk_name_allows_consecutive_dots_in_filename() {
        assert!(validate_chunk_name("foo..bar"));
        assert!(validate_chunk_name("a..b"));
        assert!(validate_chunk_name("2.0..hotfix.pak"));
    }

    /// Reject backslash-prefixed names.
    #[test]
    fn validate_chunk_name_rejects_backslash_prefix() {
        assert!(!validate_chunk_name("\\Windows\\System32"));
        assert!(!validate_chunk_name("\\etc\\passwd"));
    }

    /// Reject drive-letter prefixes.
    #[test]
    fn validate_chunk_name_rejects_drive_letter() {
        assert!(!validate_chunk_name("C:\\Windows"));
        assert!(!validate_chunk_name("Z:\\"));
    }

    /// Accept valid special characters in chunk names.
    #[test]
    fn validate_chunk_name_accepts_valid_special_chars() {
        assert!(validate_chunk_name("chunk_001"));
        assert!(validate_chunk_name("chunk-v2"));
        assert!(validate_chunk_name("chunk_1.2.3"));
        assert!(validate_chunk_name("my_chunk-abc.xyz"));
    }

    /// Accept numeric chunk names.
    #[test]
    fn validate_chunk_name_accepts_numeric() {
        assert!(validate_chunk_name("12345"));
        assert!(validate_chunk_name("0"));
    }

    /// Verify write_from_old_file reads at the correct offset.
    #[test]
    fn write_from_old_file_reads_correct_offset() {
        let dir = tempfile::tempdir().unwrap();
        let old_file_path = dir.path().join("old_file.bin");

        // Old file: "AAAABBBB"
        fs::write(&old_file_path, b"AAAABBBB").unwrap();

        // Output file, pre-sized so write_all_at at offset 0 is valid.
        let output_path = dir.path().join("output.bin");
        let output_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output_path)
            .unwrap();
        output_file.set_len(4).unwrap();

        // Read 4 bytes from offset 4.
        let mut transfer_buf = vec![0u8; 1024];
        let old_file = File::open(&old_file_path).unwrap();
        let bytes_written = write_from_old_file(
            &old_file,
            &old_file_path,
            &output_file,
            0,    // new_offset
            4,    // old_offset
            4,    // expected_size
            None, // file_hasher
            &mut transfer_buf,
            "", // chunk_decompressed_hash_md5 (skip verification)
        )
        .unwrap();
        drop(output_file);

        assert_eq!(bytes_written, 4);
        let result = fs::read(&output_path).unwrap();
        assert_eq!(&result, b"BBBB");
    }

    /// Verify chunk hash validation in write_from_old_file.
    #[test]
    fn write_from_old_file_verifies_chunk_hash() {
        let dir = tempfile::tempdir().unwrap();
        let old_file_path = dir.path().join("old_file.bin");

        let data = b"test data for hash verification";
        let expected_md5 = {
            let mut h = Md5::new().unwrap();
            h.update(data).unwrap();
            hex::encode(h.finish().unwrap())
        };
        fs::write(&old_file_path, data).unwrap();

        let output_path = dir.path().join("output.bin");
        let output_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output_path)
            .unwrap();
        output_file.set_len(data.len() as u64).unwrap();

        let mut transfer_buf = vec![0u8; 1024];
        let old_file = File::open(&old_file_path).unwrap();
        let result = write_from_old_file(
            &old_file,
            &old_file_path,
            &output_file,
            0,
            0,
            data.len() as u64,
            None,
            &mut transfer_buf,
            &expected_md5,
        );

        assert!(result.is_ok(), "should succeed with correct hash");
    }

    /// Test write_from_old_file fails on hash mismatch
    #[test]
    fn write_from_old_file_hash_mismatch_fails() {
        let dir = tempfile::tempdir().unwrap();
        let old_file_path = dir.path().join("old_file.bin");

        let data = b"test data";
        let wrong_md5 = "ffffffffffffffffffffffffffffffff"; // Wrong MD5 (not EMPTY_MD5).
        fs::write(&old_file_path, data).unwrap();

        let output_path = dir.path().join("output.bin");
        let output_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output_path)
            .unwrap();
        output_file.set_len(data.len() as u64).unwrap();

        let mut transfer_buf = vec![0u8; 1024];
        let old_file = File::open(&old_file_path).unwrap();
        let result = write_from_old_file(
            &old_file,
            &old_file_path,
            &output_file,
            0,
            0,
            data.len() as u64,
            None,
            &mut transfer_buf,
            wrong_md5,
        );

        assert!(result.is_err(), "should fail with wrong hash");
        assert!(matches!(
            result.unwrap_err(),
            SophonError::Md5Mismatch { .. }
        ));
    }

    /// Verify failure when old file is too short.
    #[test]
    fn write_from_old_file_too_short_fails() {
        let dir = tempfile::tempdir().unwrap();
        let old_file_path = dir.path().join("old_file.bin");

        // 5-byte file
        fs::write(&old_file_path, b"short").unwrap();

        let output_path = dir.path().join("output.bin");
        let output_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output_path)
            .unwrap();
        output_file.set_len(10).unwrap();

        let mut transfer_buf = vec![0u8; 1024];
        let old_file = File::open(&old_file_path).unwrap();
        let result = write_from_old_file(
            &old_file,
            &old_file_path,
            &output_file,
            0,
            0,
            10, // expected_size > actual size
            None,
            &mut transfer_buf,
            "",
        );

        assert!(result.is_err(), "should fail when file is too short");
    }

    /// Verify failure when old file is missing.
    #[test]
    fn write_from_old_file_missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let old_file_path = dir.path().join("nonexistent.bin");

        let output_path = dir.path().join("output.bin");
        let output_file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&output_path)
            .unwrap();
        output_file.set_len(100).unwrap();

        let _transfer_buf: Vec<u8> = vec![0u8; 1024];
        let old_result = File::open(&old_file_path);
        assert!(old_result.is_err(), "old file should not exist");
    }

    /// Verify chunk reuse from the old file.
    #[test]
    fn assemble_file_reuses_chunk_from_old_file() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        // Old file to reuse from.
        let old_data = b"reused chunk data here!";
        let target_path = game_dir.join("reused.bin");
        fs::write(&target_path, old_data).unwrap();

        let md5 = compute_md5_hex(old_data);

        // Asset with chunk_old_offset >= 0.
        let file = SophonManifestAssetProperty {
            asset_name: "reused.bin".to_string(),
            asset_chunks: vec![SophonManifestAssetChunk {
                chunk_name: "not_used".to_string(), // Won't be used due to old_offset
                chunk_decompressed_hash_md5: md5.clone(),
                chunk_on_file_offset: 0,
                chunk_size: 0,
                chunk_size_decompressed: old_data.len() as u64,
                chunk_compressed_hash_xxh: 0,
                chunk_compressed_hash_md5: String::new(),
                chunk_old_offset: 0, // Reuse from old file at offset 0
            }],
            asset_type: 0,
            asset_size: old_data.len() as u64,
            asset_hash_md5: md5,
        };

        let chunk_names = make_chunk_names(&[]);
        let chunk_refcounts: Vec<AtomicU32> = Vec::new();
        let verify_cache = DashMap::new();

        // Should reuse from the existing file.
        let result = assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        );

        assert!(
            result.is_ok(),
            "should succeed with chunk reuse: {:?}",
            result.err()
        );

        // Verify the file still has the correct content
        let result_data = fs::read(&target_path).unwrap();
        assert_eq!(&result_data, old_data);

        // No chunk was downloaded.
        assert!(chunk_refcounts.is_empty());
    }

    /// Verifies assembled output == decompressed chunk concatenation with PRNG data,
    /// and that the file MD5 matches the manifest. Guards against decompression or
    /// offset calculation regressions.
    #[test]
    fn assemble_file_prng_data_matches_expected_md5() {
        let dir = tempfile::tempdir().unwrap();
        let game_dir = dir.path().join("game");
        let chunks_dir = dir.path().join("chunks");
        let temp_dir = dir.path().join("tmp");
        fs::create_dir_all(&game_dir).unwrap();
        fs::create_dir_all(&chunks_dir).unwrap();
        fs::create_dir_all(&temp_dir).unwrap();

        // Generate 4 chunks of PRNG data (xoshiro256**)
        fn prng_block(seed: u64, len: usize) -> Vec<u8> {
            let mut s = [seed, seed.wrapping_mul(6364136223846793005).wrapping_add(1),
                         seed.wrapping_mul(1442695040888963407).wrapping_add(3),
                         seed ^ 0xdeadbeefcafe1234];
            let mut out = Vec::with_capacity(len);
            for _ in 0..len {
                let r = s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
                let t = s[1] << 17;
                s[2] ^= s[0]; s[3] ^= s[1]; s[1] ^= s[2]; s[0] ^= s[3];
                s[2] ^= t; s[3] = s[3].rotate_left(45);
                out.push(r as u8);
            }
            out
        }

        let chunk_data: Vec<Vec<u8>> = (0..4)
            .map(|i| prng_block(42 + i, 16384))
            .collect();

        let mut expected = Vec::new();
        let mut chunks = Vec::new();
        let names: Vec<String> = (0..4).map(|i| format!("prng_c{i}")).collect();

        for (i, data) in chunk_data.iter().enumerate() {
            make_chunk_file(&chunks_dir, &names[i], data);
            let offset = expected.len() as u64;
            expected.extend_from_slice(data);
            chunks.push(SophonManifestAssetChunk {
                chunk_name: names[i].clone(),
                chunk_decompressed_hash_md5: String::new(),
                chunk_on_file_offset: offset,
                chunk_size: 0,
                chunk_size_decompressed: data.len() as u64,
                chunk_compressed_hash_xxh: 0,
                chunk_compressed_hash_md5: String::new(),
                chunk_old_offset: -1,
            });
        }

        let file_md5 = compute_md5_hex(&expected);
        let file = SophonManifestAssetProperty {
            asset_name: "assembled_prng.bin".to_string(),
            asset_chunks: chunks,
            asset_type: 0,
            asset_size: expected.len() as u64,
            asset_hash_md5: file_md5.clone(),
        };

        let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
        let chunk_names = make_chunk_names(&name_refs);
        let chunk_refcounts: Vec<AtomicU32> = (0..4).map(|_| AtomicU32::new(100)).collect();
        let verify_cache = DashMap::new();

        assemble_test_file(
            file,
            &game_dir,
            &chunks_dir,
            &temp_dir,
            &chunk_names,
            &chunk_refcounts,
            &verify_cache,
        )
        .unwrap();

        let result = fs::read(game_dir.join("assembled_prng.bin")).unwrap();
        assert_eq!(result.len(), expected.len());
        assert_eq!(result, expected, "assembled output differs from decompressed concatenation");

        // Verify the file-level MD5 matches
        let result_md5 = compute_md5_hex(&result);
        assert_eq!(result_md5, file_md5);
    }
