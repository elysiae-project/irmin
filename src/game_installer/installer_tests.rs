    use super::*;
    use crate::api_scrape::Compression;

    fn make_chunk(name: &str, size: u64) -> SophonManifestAssetChunk {
        SophonManifestAssetChunk {
            chunk_name: name.into(),
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: 0,
            chunk_size: size,
            chunk_size_decompressed: size,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        }
    }

    fn make_chunk_with_hash(
        name: &str,
        size: u64,
        decompressed_hash: &str,
        on_file_offset: u64,
    ) -> SophonManifestAssetChunk {
        SophonManifestAssetChunk {
            chunk_name: name.into(),
            chunk_decompressed_hash_md5: decompressed_hash.into(),
            chunk_on_file_offset: on_file_offset,
            chunk_size: size,
            chunk_size_decompressed: size,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
            chunk_old_offset: -1,
        }
    }

    fn make_file(
        name: &str,
        md5: &str,
        chunks: Vec<SophonManifestAssetChunk>,
    ) -> SophonManifestAssetProperty {
        let size: u64 = chunks.iter().map(|c| c.chunk_size_decompressed).sum();
        SophonManifestAssetProperty {
            asset_name: name.into(),
            asset_chunks: chunks,
            asset_type: 0,
            asset_size: size,
            asset_hash_md5: md5.into(),
        }
    }

    fn make_dir(name: &str) -> SophonManifestAssetProperty {
        SophonManifestAssetProperty {
            asset_name: name.into(),
            asset_chunks: vec![],
            asset_type: 64,
            asset_size: 0,
            asset_hash_md5: String::new(),
        }
    }

    fn make_download_info() -> DownloadInfo {
        DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: Compression::None,
            url_prefix: "https://example.com".into(),
            url_suffix: "chunks".into(),
        }
    }

    #[allow(dead_code)]
    fn make_installer_data(files: Vec<SophonManifestAssetProperty>) -> InstallerData {
        InstallerData {
            chunk_download: Arc::new(make_download_info()),
            file_count: files.len(),
            label: "test".into(),
            matching_field: "game".into(),
        }
    }

    fn make_sophon_installer(hash: &str) -> SophonInstaller {
        SophonInstaller {
            manifest: SophonManifestProto { assets: vec![] },
            chunk_download: make_download_info(),
            label: "test".into(),
            matching_field: "game".into(),
            manifest_hash: hash.into(),
        }
    }

    #[test]
    fn compute_totals_no_dupes() {
        let files = vec![
            make_file(
                "a.pak",
                "aa",
                vec![make_chunk("c1", 100), make_chunk("c2", 200)],
            ),
            make_file("b.pak", "bb", vec![make_chunk("c3", 300)]),
        ];
        let (bytes, files_count) = compute_totals(&CompactManifest::from(files));
        assert_eq!(bytes, 600);
        assert_eq!(files_count, 2);
    }

    #[test]
    fn compute_totals_with_dedup() {
        let files = vec![
            make_file("a.pak", "aa", vec![make_chunk("shared", 500)]),
            make_file("b.pak", "bb", vec![make_chunk("shared", 500)]),
        ];
        let (bytes, files_count) = compute_totals(&CompactManifest::from(files));
        assert_eq!(bytes, 500);
        assert_eq!(files_count, 2);
    }

    #[test]
    fn compute_totals_empty() {
        let files: Vec<SophonManifestAssetProperty> = vec![];
        let (bytes, files_count) = compute_totals(&CompactManifest::from(files));
        assert_eq!(bytes, 0);
        assert_eq!(files_count, 0);
    }

    #[test]
    fn compute_totals_same_name_different_size() {
        let files = vec![
            make_file("a.pak", "aa", vec![make_chunk("shared", 500)]),
            make_file("b.pak", "bb", vec![make_chunk("shared", 600)]),
        ];
        let (bytes, files_count) = compute_totals(&CompactManifest::from(files));
        assert_eq!(bytes, 500);
        assert_eq!(files_count, 2);
    }

    #[test]
    fn compute_diff_files_all_new() {
        let new_manifest = SophonManifestProto {
            assets: vec![
                make_file("a.pak", "aa", vec![]),
                make_file("b.pak", "bb", vec![]),
                make_file("c.pak", "cc", vec![]),
            ],
        };
        let old_md5_map = FxHashMap::default();
        let diff = compute_diff_files(new_manifest, &old_md5_map);
        assert_eq!(diff.len(), 3);
    }

    #[test]
    fn compute_diff_files_unchanged_excluded() {
        let new_manifest = SophonManifestProto {
            assets: vec![make_file("a.pak", "aa", vec![])],
        };
        let mut old_md5_map: FxHashMap<&str, &str> = FxHashMap::default();
        old_md5_map.insert("a.pak", "aa");
        let diff = compute_diff_files(new_manifest, &old_md5_map);
        assert!(diff.is_empty());
    }

    #[test]
    fn compute_diff_files_changed_included() {
        let new_manifest = SophonManifestProto {
            assets: vec![make_file("a.pak", "new_md5", vec![])],
        };
        let mut old_md5_map: FxHashMap<&str, &str> = FxHashMap::default();
        old_md5_map.insert("a.pak", "old_md5");
        let diff = compute_diff_files(new_manifest, &old_md5_map);
        assert_eq!(diff.len(), 1);
    }

    #[test]
    fn compute_diff_files_dirs_included() {
        let new_manifest = SophonManifestProto {
            assets: vec![make_dir("GameData"), make_file("a.pak", "aa", vec![])],
        };
        let diff = compute_diff_files(new_manifest, &FxHashMap::default());
        assert_eq!(diff.len(), 2);
        let names: Vec<&str> = diff.iter().map(|f| f.asset_name.as_str()).collect();
        assert!(names.contains(&"GameData"));
        assert!(names.contains(&"a.pak"));
    }

    #[test]
    fn compute_diff_files_mixed() {
        let new_manifest = SophonManifestProto {
            assets: vec![
                make_file("new.pak", "nn", vec![]),
                make_file("changed.pak", "new_md5", vec![]),
                make_file("unchanged.pak", "same", vec![]),
                make_dir("somedir"),
            ],
        };
        let mut old_md5_map: FxHashMap<&str, &str> = FxHashMap::default();
        old_md5_map.insert("changed.pak", "old_md5");
        old_md5_map.insert("unchanged.pak", "same");
        let diff = compute_diff_files(new_manifest, &old_md5_map);
        assert_eq!(diff.len(), 3);
        let names: Vec<&str> = diff.iter().map(|f| f.asset_name.as_str()).collect();
        assert!(names.contains(&"new.pak"));
        assert!(names.contains(&"changed.pak"));
        assert!(names.contains(&"somedir")); // directories are included in diff
    }

    #[test]
    fn intern_old_chunk_offsets_assigns_unique_ids() {
        let manifest = SophonManifestProto {
            assets: vec![
                make_file(
                    "a.pak",
                    "aa",
                    vec![
                        make_chunk_with_hash("c0", 100, "hash0", 0),
                        make_chunk_with_hash("c1", 100, "hash1", 100),
                    ],
                ),
                make_file(
                    "b.pak",
                    "bb",
                    vec![make_chunk_with_hash("c2", 100, "hash0", 0)],
                ),
                make_dir("skipped"),
            ],
        };
        let (offsets, name_to_id, hash_to_id) = intern_old_chunk_offsets(&manifest);

        assert_eq!(name_to_id.len(), 2);
        assert_eq!(hash_to_id.len(), 2);
        assert_eq!(offsets.len(), 3);

        let a_id = name_to_id["a.pak"];
        let b_id = name_to_id["b.pak"];
        let h0 = hash_to_id["hash0"];
        let h1 = hash_to_id["hash1"];

        assert_eq!(offsets.get(&(a_id, h0)), Some(&0));
        assert_eq!(offsets.get(&(a_id, h1)), Some(&100));
        assert_eq!(offsets.get(&(b_id, h0)), Some(&0));
    }

    #[test]
    fn intern_old_chunk_offsets_empty_manifest() {
        let manifest = SophonManifestProto { assets: vec![] };
        let (offsets, name_to_id, hash_to_id) = intern_old_chunk_offsets(&manifest);
        assert!(offsets.is_empty());
        assert!(name_to_id.is_empty());
        assert!(hash_to_id.is_empty());
    }

    #[test]
    fn intern_old_chunk_offsets_only_dirs() {
        let manifest = SophonManifestProto {
            assets: vec![make_dir("d0"), make_dir("d1")],
        };
        let (offsets, name_to_id, hash_to_id) = intern_old_chunk_offsets(&manifest);
        assert!(offsets.is_empty());
        assert!(name_to_id.is_empty());
        assert!(hash_to_id.is_empty());
    }

    #[test]
    fn assign_chunk_offsets_reuses_matching_old_entries() {
        let old_manifest = SophonManifestProto {
            assets: vec![make_file(
                "a.pak",
                "old_md5",
                vec![make_chunk_with_hash("c0", 100, "hash0", 4096)],
            )],
        };
        let (offsets, name_to_id, hash_to_id) = intern_old_chunk_offsets(&old_manifest);

        let mut diff = vec![make_file(
            "a.pak",
            "new_md5",
            vec![
                make_chunk_with_hash("c0", 100, "hash0", 0),
                make_chunk_with_hash("c1", 100, "hash1", 100),
            ],
        )];

        assign_chunk_offsets(&mut diff, &offsets, &name_to_id, &hash_to_id);

        assert_eq!(diff[0].asset_chunks[0].chunk_old_offset, 4096);
        assert_eq!(diff[0].asset_chunks[1].chunk_old_offset, -1);
    }

    #[test]
    fn assign_chunk_offsets_unknown_file_name_all_minus_one() {
        let old_manifest = SophonManifestProto {
            assets: vec![make_file(
                "old.pak",
                "m",
                vec![make_chunk_with_hash("c0", 100, "hash0", 0)],
            )],
        };
        let (offsets, name_to_id, hash_to_id) = intern_old_chunk_offsets(&old_manifest);

        let mut diff = vec![make_file(
            "new.pak",
            "m",
            vec![make_chunk_with_hash("c0", 100, "hash0", 0)],
        )];

        assign_chunk_offsets(&mut diff, &offsets, &name_to_id, &hash_to_id);
        assert_eq!(diff[0].asset_chunks[0].chunk_old_offset, -1);
    }

    #[test]
    fn assign_chunk_offsets_empty_maps_all_minus_one() {
        let mut diff = vec![make_file(
            "a.pak",
            "aa",
            vec![make_chunk_with_hash("c0", 100, "hash0", 0)],
        )];
        assign_chunk_offsets(
            &mut diff,
            &FxHashMap::default(),
            &FxHashMap::default(),
            &FxHashMap::default(),
        );
        assert_eq!(diff[0].asset_chunks[0].chunk_old_offset, -1);
    }

    /// Same chunk hash at different offsets in different files must resolve
    /// to the per-file offset, not a single global offset.
    #[test]
    fn assign_chunk_offsets_same_hash_different_offsets_per_file() {
        let old_manifest = SophonManifestProto {
            assets: vec![
                make_file(
                    "a.pak",
                    "m1",
                    vec![make_chunk_with_hash("c0", 100, "hash_shared", 1000)],
                ),
                make_file(
                    "b.pak",
                    "m2",
                    vec![make_chunk_with_hash("c0", 100, "hash_shared", 5000)],
                ),
            ],
        };
        let (offsets, name_to_id, hash_to_id) = intern_old_chunk_offsets(&old_manifest);

        let mut diff = vec![
            make_file(
                "a.pak",
                "m1_new",
                vec![make_chunk_with_hash("c0", 100, "hash_shared", 0)],
            ),
            make_file(
                "b.pak",
                "m2_new",
                vec![make_chunk_with_hash("c0", 100, "hash_shared", 0)],
            ),
        ];

        assign_chunk_offsets(&mut diff, &offsets, &name_to_id, &hash_to_id);
        assert_eq!(diff[0].asset_chunks[0].chunk_old_offset, 1000);
        assert_eq!(diff[1].asset_chunks[0].chunk_old_offset, 5000);
    }

    #[test]
    fn collect_deleted_files_basic() {
        let old = SophonManifestProto {
            assets: vec![
                make_file("A", "a1", vec![]),
                make_file("B", "b1", vec![]),
                make_file("C", "c1", vec![]),
            ],
        };
        let mut new_names = rustc_hash::FxHashSet::default();
        new_names.insert("A");
        new_names.insert("D");
        let deleted = collect_deleted_files(&old, &new_names);
        assert_eq!(deleted.len(), 2);
        assert!(deleted.contains(&"B".to_string()));
        assert!(deleted.contains(&"C".to_string()));
    }

    #[test]
    fn collect_deleted_files_none() {
        let old = SophonManifestProto {
            assets: vec![make_file("A", "a1", vec![]), make_file("B", "b1", vec![])],
        };
        let mut new_names = rustc_hash::FxHashSet::default();
        new_names.insert("A");
        new_names.insert("B");
        let deleted = collect_deleted_files(&old, &new_names);
        assert!(deleted.is_empty());
    }

    #[test]
    fn collect_deleted_files_dirs_excluded() {
        let old = SophonManifestProto {
            assets: vec![make_dir("old_dir"), make_file("A", "a1", vec![])],
        };
        let new_names: rustc_hash::FxHashSet<&str> = ["A"].iter().copied().collect();
        let deleted = collect_deleted_files(&old, &new_names);
        assert!(deleted.is_empty());
    }

    #[test]
    fn build_old_md5_map_basic() {
        let manifest = SophonManifestProto {
            assets: vec![
                make_file("a.pak", "md5_a", vec![]),
                make_file("b.pak", "md5_b", vec![]),
                make_dir("dir"),
            ],
        };
        let map = build_old_md5_map(&manifest);
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("a.pak"), Some(&"md5_a"));
        assert_eq!(map.get("b.pak"), Some(&"md5_b"));
    }

    #[test]
    fn combine_manifest_hashes_deterministic() {
        let installers = vec![
            make_sophon_installer("hash_a"),
            make_sophon_installer("hash_b"),
        ];
        let h1 = combine_manifest_hashes(&installers);
        let h2 = combine_manifest_hashes(&installers);
        assert_eq!(h1, h2);
    }

    #[test]
    fn combine_manifest_hashes_order_independent() {
        let installers_ab = vec![
            make_sophon_installer("hash_a"),
            make_sophon_installer("hash_b"),
        ];
        let installers_ba = vec![
            make_sophon_installer("hash_b"),
            make_sophon_installer("hash_a"),
        ];
        assert_eq!(
            combine_manifest_hashes(&installers_ab),
            combine_manifest_hashes(&installers_ba),
        );
    }

    #[tokio::test]
    async fn notify_assembly_ready_single_file_ready() {
        let (tx, mut rx) = mpsc::unbounded_channel::<(usize, usize)>();

        let pending_counts: Vec<AtomicU32> = vec![AtomicU32::new(1)];
        let chunk_entries: Vec<FileEntry> = vec![(0u32, 0u32, 0u32)];
        let chunk_entry_offsets: Vec<u32> = vec![0, 1];
        let tmp = tempfile::tempdir().unwrap();
        let alloc = super::output_allocator::OutputAllocator::new(tmp.path());

        notify_assembly_ready(
            0,
            &chunk_entries,
            &chunk_entry_offsets,
            &pending_counts,
            &tx,
            &alloc,
        );

        let received = rx.try_recv();
        assert!(received.is_ok(), "file should be sent to assembly channel");
        assert_eq!(received.unwrap(), (0, 0));
        assert_eq!(pending_counts[0].load(Ordering::Acquire), 0);
    }

    #[tokio::test]
    async fn notify_assembly_ready_chunk_not_in_map() {
        let chunk_entries: Vec<FileEntry> = vec![];
        let chunk_entry_offsets: Vec<u32> = vec![0];
        let pending_counts: Vec<AtomicU32> = vec![];
        let (tx, rx) = mpsc::unbounded_channel::<(usize, usize)>();
        drop(rx);
        let tmp = tempfile::tempdir().unwrap();
        let alloc = super::output_allocator::OutputAllocator::new(tmp.path());

        notify_assembly_ready(
            999,
            &chunk_entries,
            &chunk_entry_offsets,
            &pending_counts,
            &tx,
            &alloc,
        );
    }

    #[tokio::test]
    async fn check_needs_download_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("does_not_exist.bin");
        let chunk = make_chunk("c1", 100);
        let cache = Arc::new(DashMap::new());

        let all_files = Arc::new(CompactManifest::from(vec![make_file(
            "f1",
            "",
            vec![chunk],
        )]));

        let needs = check_needs_download(
            &dest,
            all_files,
            0,
            0,
            Arc::new(dir.path().to_path_buf()),
            cache,
        )
        .await
        .unwrap();
        assert!(needs.0);
    }

    #[tokio::test]
    async fn check_needs_download_valid_cached_file() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("cached.bin");
        let data = b"hello cache";
        std::fs::write(&file_path, data).unwrap();

        let md5_hex = {
            let mut hasher = fast_md5::Md5::new();
            hasher.update(data);
            hex::encode(hasher.finalize())
        };

        let metadata = std::fs::metadata(&file_path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let cache: Arc<DashMap<String, VerificationEntry>> = Arc::new(DashMap::new());
        let rel_path = file_path
            .strip_prefix(dir.path())
            .unwrap()
            .to_string_lossy()
            .to_string();
        cache.insert(
            rel_path,
            VerificationEntry {
                size: data.len() as u64,
                md5: md5_hex.clone(),
                mtime_secs: mtime,
            },
        );

        let mut chunk = make_chunk("c1", data.len() as u64);
        chunk.chunk_compressed_hash_md5 = md5_hex;

        let all_files = Arc::new(CompactManifest::from(vec![make_file(
            "f1",
            "",
            vec![chunk],
        )]));

        let needs = check_needs_download(
            &file_path,
            all_files,
            0,
            0,
            Arc::new(dir.path().to_path_buf()),
            cache,
        )
        .await
        .unwrap();
        assert!(!needs.0);
    }

    #[tokio::test]
    async fn download_chunk_with_retries_success_on_first() {
        use crate::api_scrape::Compression;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let data = b"chunk payload".to_vec();
        let expected_md5 = hex::encode(fast_md5::digest(&data));

        let data_len = data.len() as u64;
        let chunk = SophonManifestAssetChunk {
            chunk_name: "test_retry_chunk".to_string(),
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: 0,
            chunk_size: data_len,
            chunk_size_decompressed: data_len,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: expected_md5,
            chunk_old_offset: -1,
        };

        let dl_info = Arc::new(DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: Compression::None,
            url_prefix: format!("{uri}/", uri = server.uri()),
            url_suffix: "chunks".to_string(),
        });

        Mock::given(method("GET"))
            .and(path("chunks/test_retry_chunk"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(data.clone())
                    .insert_header("content-length", data.len().to_string()),
            )
            .mount(&server)
            .await;

        let client = Arc::new(Client::new());
        let chunk_download = dl_info;

        Mock::given(method("GET"))
            .and(path("chunks/test_retry_chunk"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(data.clone())
                    .insert_header("content-length", data.len().to_string()),
            )
            .mount(&server)
            .await;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("test_retry_chunk.zstd");

        let ctx = Arc::new(InstallContext {
            installer_clients: Arc::new(vec![Arc::clone(&client)]),
            installer_downloads: Arc::new(vec![Arc::clone(&chunk_download)]),
            chunks_dir: Arc::new(dir.path().to_path_buf()),
            game_dir: dir.path().to_path_buf().into(),
            all_tmp_dirs: Arc::new(vec![]),
            all_files: Arc::new(CompactManifest::from(vec![])),
            downloaded_bytes: Arc::new(AtomicU64::new(0)),
            assembled_files: Arc::new(AtomicU64::new(0)),
            checked_files: Arc::new(AtomicU64::new(0)),
            total_bytes: 0,
            total_files: 0,
            resume_bytes_offset: Arc::new(AtomicU64::new(0)),
            verify_cache: Arc::new(DashMap::new()),
            chunk_refcounts: Arc::new(OnceLock::new()),
            chunk_names: Arc::new(OnceLock::new()),
            last_assembly_update: Arc::new(Mutex::new(Instant::now())),
            last_update: Arc::new(AtomicU64::new(now_nanos())),
            smooth_speed_bps: Arc::new(AtomicU64::new(0)),
            eta_speed_history: Arc::new(Mutex::new(VecDeque::new())),
            last_speed_bytes: Arc::new(AtomicU64::new(0)),
            last_speed_time: Arc::new(AtomicU64::new(now_nanos())),
            updater: Arc::new(|_| {}),
            downloaded_chunks: Arc::new(OnceLock::new()),
            chunks_since_save: Arc::new(AtomicU64::new(0)),
            last_save: Arc::new(Mutex::new(None)),
            state_saver: Arc::new(|_| {}),
            adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
            profiler: Arc::new(super::profiling::PipelineProfiler::new()),
            completion_flags: Arc::from(Vec::<AtomicBool>::new()),
            output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
            chunk_bitmap: Arc::new(super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("test.bitmap"), 1).unwrap()),
        });

        let handle = DownloadHandle::new();

        let result = download_chunk_with_retries(
            (&chunk).into(),
            &client,
            &chunk_download,
            &dest,
            None,
            &ctx,
            &handle,
        )
        .await;
        assert!(result.is_ok());
    }

    /// Discards partial files when MD5 verification fails after a resumed
    /// download. Prevents appending fresh bytes to corrupted data.
    #[tokio::test]
    async fn download_chunk_with_retries_mismatch_discards_partial() {
        use crate::api_scrape::Compression;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let data = b"chunk payload that has the wrong hash".to_vec();
        // Intentionally wrong hash so the chunk always fails MD5 verification.
        let _wrong_md5 = "00000000000000000000000000000000";

        let wrong_md5 = hex::encode(fast_md5::digest(b"wrong_data"));
        let data_len = data.len() as u64;
        let chunk = SophonManifestAssetChunk {
            chunk_name: "discard_partial_chunk".to_string(),
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: 0,
            chunk_size: data_len,
            chunk_size_decompressed: data_len,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: wrong_md5.to_string(),
            chunk_old_offset: -1,
        };

        let server_uri = server.uri();
        let dl_info = Arc::new(DownloadInfo {
            encryption: 0,
            password: String::new(),
            compression: Compression::None,
            url_prefix: format!("{server_uri}/"),
            url_suffix: "chunks".to_string(),
        });

        Mock::given(method("GET"))
            .and(path("chunks/discard_partial_chunk"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(data.clone())
                    .insert_header("content-length", data.len().to_string()),
            )
            .mount(&server)
            .await;

        let client = Arc::new(Client::new());
        let chunk_download = dl_info;

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("discard_partial_chunk.zstd");
        // Pre-create the file so we can check whether it gets deleted.
        tokio::fs::write(&dest, b"corrupted-existing-content")
            .await
            .unwrap();

        let ctx = Arc::new(InstallContext {
            installer_clients: Arc::new(vec![Arc::clone(&client)]),
            installer_downloads: Arc::new(vec![Arc::clone(&chunk_download)]),
            chunks_dir: Arc::new(dir.path().to_path_buf()),
            game_dir: dir.path().to_path_buf().into(),
            all_tmp_dirs: Arc::new(vec![]),
            all_files: Arc::new(CompactManifest::from(vec![])),
            downloaded_bytes: Arc::new(AtomicU64::new(0)),
            assembled_files: Arc::new(AtomicU64::new(0)),
            checked_files: Arc::new(AtomicU64::new(0)),
            total_bytes: 0,
            total_files: 0,
            resume_bytes_offset: Arc::new(AtomicU64::new(0)),
            verify_cache: Arc::new(DashMap::new()),
            chunk_refcounts: Arc::new(OnceLock::new()),
            chunk_names: Arc::new(OnceLock::new()),
            last_assembly_update: Arc::new(Mutex::new(Instant::now())),
            last_update: Arc::new(AtomicU64::new(now_nanos())),
            smooth_speed_bps: Arc::new(AtomicU64::new(0)),
            eta_speed_history: Arc::new(Mutex::new(VecDeque::new())),
            last_speed_bytes: Arc::new(AtomicU64::new(0)),
            last_speed_time: Arc::new(AtomicU64::new(now_nanos())),
            updater: Arc::new(|_| {}),
            downloaded_chunks: Arc::new(OnceLock::new()),
            chunks_since_save: Arc::new(AtomicU64::new(0)),
            last_save: Arc::new(Mutex::new(None)),
            state_saver: Arc::new(|_| {}),
            adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
            profiler: Arc::new(super::profiling::PipelineProfiler::new()),
            completion_flags: Arc::from(Vec::<AtomicBool>::new()),
            output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
            chunk_bitmap: Arc::new(super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("test.bitmap"), 1).unwrap()),
        });

        let handle = DownloadHandle::new();

        let result = download_chunk_with_retries(
            (&chunk).into(),
            &client,
            &chunk_download,
            &dest,
            None,
            &ctx,
            &handle,
        )
        .await;
        // After MAX_HASH_RETRIES, the operation fails. Each retry starts from
        // size 0 because the partial file is discarded on mismatch.
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("hash verification failed"),
            "expected hash failure, got: {err_msg}"
        );
        // The destination file was discarded on every mismatch.
        assert!(
            !dest.exists(),
            "partial file should be deleted after MD5 mismatch retries"
        );
    }

    #[test]
    fn compute_totals_filters_old_reuse_chunks() {
        let chunk_new = SophonManifestAssetChunk {
            chunk_name: "new_chunk".into(),
            chunk_size: 500,
            chunk_size_decompressed: 500,
            chunk_old_offset: -1,
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: 0,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
        };
        let chunk_reuse = SophonManifestAssetChunk {
            chunk_name: "reuse_chunk".into(),
            chunk_size: 300,
            chunk_size_decompressed: 300,
            chunk_old_offset: 42,
            chunk_decompressed_hash_md5: String::new(),
            chunk_on_file_offset: 0,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: String::new(),
        };
        let file1 = SophonManifestAssetProperty {
            asset_name: "a.pak".into(),
            asset_chunks: vec![chunk_new, chunk_reuse],
            asset_type: 0,
            asset_hash_md5: String::new(),
            asset_size: 0,
        };
        let (bytes, files_count) = compute_totals(&CompactManifest::from(vec![file1]));
        assert_eq!(bytes, 500, "old-reuse chunk should be excluded");
        assert_eq!(files_count, 1);
    }

    #[test]
    fn compute_totals_filters_directories() {
        let file1 = make_file("a.pak", "aa", vec![make_chunk("c1", 100)]);
        let dir1 = make_dir("GameData");
        let (bytes, files_count) = compute_totals(&CompactManifest::from(vec![dir1, file1]));
        assert_eq!(bytes, 100);
        assert_eq!(files_count, 2);
    }

    #[test]
    fn build_installer_data_filters_directories() {
        let installer = SophonInstaller {
            manifest: SophonManifestProto {
                assets: vec![
                    make_dir("GameData"),
                    make_file("a.pak", "aa", vec![make_chunk("c1", 100)]),
                ],
            },
            chunk_download: make_download_info(),
            label: "test".into(),
            matching_field: "game".into(),
            manifest_hash: "abc".into(),
        };

        let (data, all_files) = build_installer_data(vec![installer]);
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].file_count, 1);
        assert_eq!(all_files.len(), 1);
        assert_eq!(all_files[0].asset_name, "a.pak");
    }

    #[test]
    fn combine_manifest_hashes_empty() {
        let installers: Vec<SophonInstaller> = vec![];
        let hash = combine_manifest_hashes(&installers);
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 16);
    }

    #[test]
    fn compute_diff_files_all_unchanged() {
        let new_manifest = SophonManifestProto {
            assets: vec![
                make_file("a.pak", "aa", vec![]),
                make_file("b.pak", "bb", vec![]),
            ],
        };
        let mut old_md5_map: FxHashMap<&str, &str> = FxHashMap::default();
        old_md5_map.insert("a.pak", "aa");
        old_md5_map.insert("b.pak", "bb");
        let diff = compute_diff_files(new_manifest, &old_md5_map);
        assert!(diff.is_empty());
    }

    #[test]
    fn chunk_still_valid_for_resume_missing() {
        let tmp = std::env::temp_dir().join(format!(
            "elysiae_resume_missing_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(!chunk_still_valid_for_resume("missing_chunk", 123, &tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn chunk_still_valid_for_resume_size_mismatch() {
        let tmp = std::env::temp_dir().join(format!(
            "elysiae_resume_size_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        let chunk_path = tmp.join("OK_chunk.zstd");
        std::fs::write(&chunk_path, b"abc").unwrap();
        assert!(!chunk_still_valid_for_resume("OK_chunk", 5, &tmp));
        assert!(!chunk_still_valid_for_resume("OK_chunk", 100, &tmp));
        assert!(chunk_still_valid_for_resume("OK_chunk", 3, &tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn chunk_still_valid_for_resume_invalid_name() {
        let tmp = std::env::temp_dir().join(format!(
            "elysiae_resume_invalid_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("ok.zstd"), b"hi").unwrap();
        assert!(!chunk_still_valid_for_resume("../escape", 2, &tmp));
        assert!(!chunk_still_valid_for_resume("", 0, &tmp));
        assert!(!chunk_still_valid_for_resume("name/with/slash", 2, &tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }
