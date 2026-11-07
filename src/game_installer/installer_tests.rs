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
    fn snapshot_downloaded_chunks_preserves_names_values_and_last_duplicate() {
        let lookup = ChunkNameLookup::from_arena(super::super::compact_manifest::StringArena::from(
            &["shared", "idle", "shared", "other"][..],
        ));
        let downloaded_chunks = [
            AtomicU32::new(11),
            AtomicU32::new(0),
            AtomicU32::new(29),
            AtomicU32::new(7),
        ];

        let snapshot = snapshot_downloaded_chunks(&downloaded_chunks, &lookup);

        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot.get("shared"), Some(&29));
        assert_eq!(snapshot.get("other"), Some(&7));
        assert!(!snapshot.contains_key("idle"));
    }

    #[test]
    fn download_item_index_is_absent_when_deduplication_is_disabled() {
        assert!(uses_chunk_deduplication(super::VerifyMode::Full));
        assert!(!uses_chunk_deduplication(super::VerifyMode::Deferred));
        assert!(!uses_chunk_deduplication(super::VerifyMode::None));
        assert!(new_download_items_index(super::VerifyMode::Deferred, 100).is_none());
        assert!(new_download_items_index(super::VerifyMode::None, 100).is_none());

        let index = new_download_items_index(super::VerifyMode::Full, 100).unwrap();
        assert!(index.is_empty());
        assert!(index.capacity() >= 100);
    }

    #[test]
    fn download_item_claims_are_unique_and_stop_at_item_count() {
        for item_count in [0, 1, 3, 1000] {
            let next_index = Arc::new(AtomicUsize::new(0));
            let claims = Arc::new(Mutex::new(Vec::new()));
            std::thread::scope(|scope| {
                for _ in 0..16 {
                    let next_index = Arc::clone(&next_index);
                    let claims = Arc::clone(&claims);
                    scope.spawn(move || {
                        while let Some(index) = DownloadItem::claim_index(&next_index, item_count) {
                            claims.lock().unwrap().push(index);
                        }
                    });
                }
            });

            let mut claims = Arc::try_unwrap(claims).unwrap().into_inner().unwrap();
            claims.sort_unstable();
            assert_eq!(claims, (0..item_count).collect::<Vec<_>>());
            assert_eq!(next_index.load(Ordering::Relaxed), item_count);
        }
    }

    #[test]
    fn adaptive_download_scaling_checks_for_unclaimed_items() {
        assert_eq!(next_download_worker_count(4, 16, 2), 6);
        assert_eq!(next_download_worker_count(4, 16, 20), 16);
        assert_eq!(next_download_worker_count(4, 4, 20), 4);
    }

    #[test]
    fn download_cancellation_is_recorded_for_finalization() {
        let cancelled = AtomicU8::new(0);

        record_download_cancellation(&cancelled);

        assert_eq!(cancelled.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn cancelled_worker_result_requires_user_cancellation() {
        let cancelled = AtomicU8::new(0);
        let first_error = Mutex::new(None);
        let handle = DownloadHandle::new();

        record_download_cancelled_result(&cancelled, &first_error, &handle);

        assert_eq!(cancelled.load(Ordering::Relaxed), 0);
        assert!(matches!(
            first_error.into_inner().unwrap(),
            Some(SophonError::Io(_))
        ));

        let cancelled = AtomicU8::new(0);
        let first_error = Mutex::new(None);
        let handle = DownloadHandle::new();
        handle.cancel();

        record_download_cancelled_result(&cancelled, &first_error, &handle);

        assert_eq!(cancelled.load(Ordering::Relaxed), 1);
        assert!(first_error.into_inner().unwrap().is_none());
    }

    #[tokio::test]
    async fn scheduler_cancellation_wait_handles_current_and_new_cancellation() {
        let handle = DownloadHandle::new();
        let waiting_handle = handle.clone();
        let waiting = tokio::spawn(async move {
            wait_for_download_cancellation(&waiting_handle).await;
        });
        tokio::task::yield_now().await;
        handle.cancel();
        tokio::time::timeout(std::time::Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();

        wait_for_download_cancellation(&handle).await;
    }

    #[test]
    fn download_summary_observes_recorded_and_handle_cancellation() {
        let recorded = AtomicU8::new(0);
        let handle = DownloadHandle::new();
        assert!(!download_was_cancelled(&recorded, &handle));

        record_download_cancellation(&recorded);
        assert!(download_was_cancelled(&recorded, &handle));

        let recorded = AtomicU8::new(0);
        let handle = DownloadHandle::new();
        handle.cancel();
        assert!(download_was_cancelled(&recorded, &handle));
    }

    #[test]
    fn download_worker_error_keeps_first_error() {
        let first_error = Mutex::new(None);

        record_download_worker_error(&first_error, SophonError::Cancelled);
        record_download_worker_error(
            &first_error,
            SophonError::Io(std::io::Error::other("later error")),
        );

        assert!(matches!(
            first_error.into_inner().unwrap(),
            Some(SophonError::Cancelled)
        ));
    }

    #[test]
    fn download_work_completion_requires_zero_remaining_items() {
        let remaining = AtomicUsize::new(1);
        assert!(!download_work_finished(&remaining));
        remaining.store(0, Ordering::Release);
        assert!(download_work_finished(&remaining));
    }

    #[test]
    fn cancelled_downloads_are_not_reported_as_abandoned_work() {
        let remaining = AtomicUsize::new(1);
        let handle = DownloadHandle::new();
        assert!(download_work_abandoned(&remaining, &handle));

        handle.cancel();
        assert!(!download_work_abandoned(&remaining, &handle));
    }

    #[test]
    fn repair_chunk_name_dedup_borrows_names() {
        let names = [String::from("shared"), String::from("unique")];
        let mut seen = rustc_hash::FxHashSet::default();

        assert!(insert_new_chunk_name(&mut seen, &names[0]));
        assert!(!insert_new_chunk_name(&mut seen, &names[0]));
        assert!(insert_new_chunk_name(&mut seen, &names[1]));
        assert_eq!(seen.len(), 2);
    }

    #[tokio::test]
    async fn bounded_stoppable_stream_bounds_futures_and_stops_after_error() {
        let stopped = Arc::new(AtomicBool::new(false));
        let active = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let started = Arc::new(AtomicUsize::new(0));
        let mapped = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(4));
        let mut stream = bounded_stoppable_stream(
            vec![0usize; 100],
            4,
            Arc::clone(&stopped),
            {
                let active = Arc::clone(&active);
                let max_active = Arc::clone(&max_active);
                let started = Arc::clone(&started);
                let mapped = Arc::clone(&mapped);
                let barrier = Arc::clone(&barrier);
                move |item| {
                    mapped.fetch_add(1, Ordering::Relaxed);
                    let active = Arc::clone(&active);
                    let max_active = Arc::clone(&max_active);
                    let started = Arc::clone(&started);
                    let stopped = Arc::clone(&stopped);
                    let barrier = Arc::clone(&barrier);
                    async move {
                        started.fetch_add(1, Ordering::Relaxed);
                        let current = active.fetch_add(1, Ordering::AcqRel) + 1;
                        max_active.fetch_max(current, Ordering::Relaxed);
                        barrier.wait().await;
                        active.fetch_sub(1, Ordering::AcqRel);
                        if item == 0 {
                            stopped.store(true, Ordering::Release);
                            Err(SophonError::Io(std::io::Error::other("synthetic failure")))
                        } else {
                            Ok(())
                        }
                    }
                }
            },
            "test",
        )
        .unwrap();

        let mut first_error = None;
        while let Some(result) = stream.next().await {
            if let Err(err) = result {
                first_error.get_or_insert(err);
            }
        }

        assert!(matches!(first_error, Some(SophonError::Io(_))));
        assert!(started.load(Ordering::Relaxed) <= 4);
        assert_eq!(mapped.load(Ordering::Relaxed), 4);
        assert!(max_active.load(Ordering::Relaxed) <= 4);
        assert_eq!(active.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn bounded_stoppable_stream_processes_all_items_without_errors() {
        let stopped = Arc::new(AtomicBool::new(false));
        let started = Arc::new(AtomicUsize::new(0));
        let mut stream = bounded_stoppable_stream(
            (0..37usize).collect(),
            5,
            stopped,
            {
                let started = Arc::clone(&started);
                move |_| {
                    let started = Arc::clone(&started);
                    async move {
                        started.fetch_add(1, Ordering::Relaxed);
                        Ok::<_, SophonError>(())
                    }
                }
            },
            "test",
        )
        .unwrap();

        while let Some(result) = stream.next().await {
            result.unwrap();
        }

        assert_eq!(started.load(Ordering::Relaxed), 37);
    }

    #[test]
    fn bounded_stoppable_stream_rejects_zero_concurrency() {
        let result = bounded_stoppable_stream(
            Vec::<usize>::new(),
            0,
            Arc::new(AtomicBool::new(false)),
            |_| async { Ok::<_, SophonError>(()) },
            "test",
        );

        assert!(matches!(result, Err(SophonError::Io(_))));
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
            save_chunk_names: Arc::new(OnceLock::new()),
            adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
            profiler: Arc::new(super::profiling::PipelineProfiler::new()),
            completion_flags: Arc::from(Vec::<AtomicBool>::new()),
            output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
            chunk_bitmap: Arc::new(super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("test.bitmap"), 1).unwrap()),
            verify_mode: super::VerifyMode::Full,
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
            save_chunk_names: Arc::new(OnceLock::new()),
            adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
            profiler: Arc::new(super::profiling::PipelineProfiler::new()),
            completion_flags: Arc::from(Vec::<AtomicBool>::new()),
            output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
            chunk_bitmap: Arc::new(super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("test.bitmap"), 1).unwrap()),
            verify_mode: super::VerifyMode::Full,
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
