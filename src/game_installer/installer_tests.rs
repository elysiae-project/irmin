use super::*;
use crate::api_scrape::Compression;

fn make_chunk(name: &str, size: u64) -> SophonManifestAssetChunk {
    SophonManifestAssetChunk {
        chunk_name: name.to_string().into(),
        chunk_decompressed_hash_md5: bytes::Bytes::new(),
        chunk_on_file_offset: 0,
        chunk_size: size,
        chunk_size_decompressed: size,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: bytes::Bytes::new(),
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
        chunk_name: name.to_string().into(),
        chunk_decompressed_hash_md5: decompressed_hash.to_string().into(),
        chunk_on_file_offset: on_file_offset,
        chunk_size: size,
        chunk_size_decompressed: size,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: bytes::Bytes::new(),
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
        asset_name: name.to_string().into(),
        asset_chunks: chunks,
        asset_type: 0,
        asset_size: size,
        asset_hash_md5: md5.to_string().into(),
    }
}

fn make_dir(name: &str) -> SophonManifestAssetProperty {
    SophonManifestAssetProperty {
        asset_name: name.to_string().into(),
        asset_chunks: vec![],
        asset_type: 64,
        asset_size: 0,
        asset_hash_md5: bytes::Bytes::new(),
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

    // Vec preserves all entries; duplicate names produce multiple rows.
    // Production chunk names are unique, so dedup is unnecessary. The test
    // verifies that the last value for each name is findable.
    assert_eq!(snapshot.len(), 3);
    assert_eq!(
        snapshot
            .iter()
            .rfind(|(k, _)| *k == "shared")
            .map(|(_, v)| *v),
        Some(29)
    );
    assert_eq!(
        snapshot
            .iter()
            .rfind(|(k, _)| *k == "other")
            .map(|(_, v)| *v),
        Some(7)
    );
    assert!(!snapshot.iter().any(|(k, _)| *k == "idle"));
}

#[test]
fn snapshot_downloaded_chunks_keeps_last_duplicate_with_exact_capacity() {
    let lookup = ChunkNameLookup::from_arena(super::super::compact_manifest::StringArena::from(
        &["shared", "shared", "shared"][..],
    ));
    let downloaded_chunks = [AtomicU32::new(29), AtomicU32::new(11), AtomicU32::new(0)];

    let snapshot = snapshot_downloaded_chunks(&downloaded_chunks, &lookup);

    assert_eq!(snapshot.len(), 2);
    assert_eq!(
        snapshot
            .iter()
            .rfind(|(k, _)| *k == "shared")
            .map(|(_, v)| *v),
        Some(11)
    );

    let no_completed_duplicates = [AtomicU32::new(0), AtomicU32::new(0)];
    let no_completed_lookup = ChunkNameLookup::from_arena(
        super::super::compact_manifest::StringArena::from(&["shared", "shared"][..]),
    );
    assert!(snapshot_downloaded_chunks(&no_completed_duplicates, &no_completed_lookup).is_empty());
}

#[cfg(feature = "bench-diagnostics")]
#[test]
#[ignore = "measures resume snapshot allocation strategies"]
fn benchmark_downloaded_chunk_snapshot_capacity() {
    use std::io::Read as _;
    use std::time::Instant;

    fn process_hwm_kib() -> u64 {
        let mut status = String::new();
        std::fs::File::open("/proc/self/status")
            .unwrap()
            .read_to_string(&mut status)
            .unwrap();
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or_default()
    }

    let item_count = std::env::var("IRMIN_SNAPSHOT_ITEMS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(500_000);
    let completed = std::env::var("IRMIN_SNAPSHOT_COMPLETED")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(500)
        .min(item_count);
    let strategy =
        std::env::var("IRMIN_SNAPSHOT_STRATEGY").unwrap_or_else(|_| "production".to_owned());
    let names = (0..item_count)
        .map(|index| format!("chunk-{index:08}"))
        .collect::<Vec<_>>();
    let name_refs = names.iter().map(String::as_str).collect::<Vec<_>>();
    let lookup = ChunkNameLookup::from_arena(super::super::compact_manifest::StringArena::from(
        name_refs.as_slice(),
    ));
    let values = (0..item_count)
        .map(|index| AtomicU32::new(if index < completed { 1 } else { 0 }))
        .collect::<Vec<_>>();

    std::fs::write("/proc/self/clear_refs", b"5").unwrap();
    let before_hwm = process_hwm_kib();
    let started = Instant::now();
    let map = match strategy.as_str() {
        "production" => snapshot_downloaded_chunks(&values, &lookup),
        "empty" => {
            let mut map = Vec::new();
            for (index, value) in values.iter().enumerate() {
                let value = value.load(Ordering::Relaxed);
                if value > 0 {
                    map.push((lookup.get(index), value as u64));
                }
            }
            map
        }
        "exact" => {
            let count = values
                .iter()
                .filter(|value| value.load(Ordering::Relaxed) > 0)
                .count();
            let mut map = Vec::with_capacity(count);
            for (index, value) in values.iter().enumerate() {
                let value = value.load(Ordering::Relaxed);
                if value > 0 {
                    map.push((lookup.get(index), value as u64));
                }
            }
            map
        }
        other => panic!("unknown snapshot strategy {other}"),
    };
    let snapshot_elapsed = started.elapsed();
    assert_eq!(map.len(), completed);
    let before_serialize_hwm = process_hwm_kib();
    let serialize_started = Instant::now();
    let serialized_len = serde_json::to_vec(&map).unwrap().len();
    let serialize_elapsed = serialize_started.elapsed();
    std::hint::black_box(&map);
    std::hint::black_box(serialized_len);
    let after_serialize_hwm = process_hwm_kib();
    println!(
        "snapshot_capacity strategy={strategy} items={item_count} completed={completed} snapshot_ms={:.3} serialize_ms={:.3} hwm_before_kib={before_hwm} hwm_before_serialize_kib={before_serialize_hwm} hwm_after_serialize_kib={after_serialize_hwm} output_bytes={serialized_len} capacity={}",
        snapshot_elapsed.as_secs_f64() * 1000.0,
        serialize_elapsed.as_secs_f64() * 1000.0,
        map.capacity(),
    );
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
fn register_chunks_records_offsets_in_file_order() {
    let manifest = CompactManifest::from(vec![make_file(
        "mixed.pak",
        "hash",
        vec![
            make_chunk("download-0", 11),
            SophonManifestAssetChunk {
                chunk_old_offset: 4096,
                ..make_chunk("old", 13)
            },
            make_chunk("download-1", 17),
        ],
    )]);
    let mut entries = Vec::new();
    let mut items = Vec::new();
    let mut index = new_download_items_index(super::VerifyMode::Full, manifest.num_chunks());
    let mut refcounts = Vec::new();
    let mut pending = Vec::new();
    let mut tainted = rustc_hash::FxHashSet::default();

    register_chunks_for_file(
        &manifest,
        0,
        0,
        |item_idx, entry| push_nested_chunk_entry(&mut entries, item_idx, entry),
        &mut items,
        &mut index,
        &mut refcounts,
        &mut pending,
        0,
        &FxHashMap::default(),
        &mut tainted,
        false,
    )
    .unwrap();

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].chunk_idx, 0);
    assert_eq!(items[0].output_offset, 0);
    assert_eq!(items[1].chunk_idx, 2);
    assert_eq!(items[1].output_offset, 24);
    for item in &items {
        assert_eq!(
            item.output_offset,
            manifest.file_chunk_offset(0, item.chunk_idx as usize)
        );
    }
}

#[test]
fn register_chunks_records_offsets_after_leading_old_and_empty_chunks() {
    let manifest = CompactManifest::from(vec![make_file(
        "leading-old.pak",
        "hash",
        vec![
            SophonManifestAssetChunk {
                chunk_old_offset: 4096,
                ..make_chunk("old", 0)
            },
            make_chunk("empty", 0),
            make_chunk("download", 17),
        ],
    )]);
    let mut entries = Vec::new();
    let mut items = Vec::new();
    let mut index = new_download_items_index(super::VerifyMode::Full, manifest.num_chunks());
    let mut refcounts = Vec::new();
    let mut pending = Vec::new();
    let mut tainted = rustc_hash::FxHashSet::default();

    register_chunks_for_file(
        &manifest,
        0,
        0,
        |item_idx, entry| push_nested_chunk_entry(&mut entries, item_idx, entry),
        &mut items,
        &mut index,
        &mut refcounts,
        &mut pending,
        0,
        &FxHashMap::default(),
        &mut tainted,
        false,
    )
    .unwrap();

    assert_eq!(items.len(), 2);
    assert_eq!(items[0].output_offset, 0);
    assert_eq!(items[1].output_offset, 0);
    for item in &items {
        assert_eq!(
            item.output_offset,
            manifest.file_chunk_offset(0, item.chunk_idx as usize)
        );
    }
}

#[test]
fn shared_chunks_are_tainted_out_of_eager_mode() {
    let manifest = CompactManifest::from(vec![
        make_file("a.pak", "hash-a", vec![make_chunk("shared", 11)]),
        make_file("b.pak", "hash-b", vec![make_chunk("shared", 11)]),
    ]);
    let mut entries = Vec::new();
    let mut items = Vec::new();
    let mut index = new_download_items_index(super::VerifyMode::Full, manifest.num_chunks());
    let mut refcounts = Vec::new();
    let mut pending = Vec::new();
    let mut tainted = rustc_hash::FxHashSet::default();

    for file_idx in 0..manifest.num_files() {
        register_chunks_for_file(
            &manifest,
            file_idx,
            file_idx,
            |item_idx, entry| push_nested_chunk_entry(&mut entries, item_idx, entry),
            &mut items,
            &mut index,
            &mut refcounts,
            &mut pending,
            file_idx,
            &FxHashMap::default(),
            &mut tainted,
            false,
        )
        .unwrap();
    }

    assert_eq!(items.len(), 1);
    assert_eq!(tainted.len(), 2);
    disable_eager_for_tainted_files(&mut items, &tainted);
    assert!(!items[0].use_eager);
}

#[test]
fn repeated_chunk_names_within_file_are_tainted_out_of_eager_mode() {
    let manifest = CompactManifest::from(vec![make_file(
        "repeated.pak",
        "hash",
        vec![make_chunk("same", 11), make_chunk("same", 17)],
    )]);
    let mut entries = Vec::new();
    let mut items = Vec::new();
    let mut index = new_download_items_index(super::VerifyMode::Full, manifest.num_chunks());
    let mut refcounts = Vec::new();
    let mut pending = Vec::new();
    let mut tainted = rustc_hash::FxHashSet::default();

    register_chunks_for_file(
        &manifest,
        0,
        0,
        |item_idx, entry| push_nested_chunk_entry(&mut entries, item_idx, entry),
        &mut items,
        &mut index,
        &mut refcounts,
        &mut pending,
        0,
        &FxHashMap::default(),
        &mut tainted,
        false,
    )
    .unwrap();

    assert_eq!(items.len(), 1);
    assert!(tainted.contains(&0));
    disable_eager_for_tainted_files(&mut items, &tainted);
    assert!(!items[0].use_eager);
}

#[test]
fn adaptive_download_scaling_checks_for_unclaimed_items() {
    assert_eq!(next_download_worker_count(4, 16, 2), 6);
    assert_eq!(next_download_worker_count(4, 16, 20), 16);
    assert_eq!(next_download_worker_count(4, 4, 20), 4);
}

#[test]
fn from_manifest_dedups_shared_chunk_names_across_files() {
    use std::sync::Arc;

    use super::compact_manifest::CompactManifest;

    let manifest = Arc::new(CompactManifest::from(vec![
        make_file("a.pak", "hash-a", vec![make_chunk("shared", 11)]),
        make_file("b.pak", "hash-b", vec![make_chunk("shared", 11)]),
    ]));

    // Two files reference the same chunk name "shared". The manifest builder
    // does not dedup names during interning, so the two entries get distinct
    // arena ids. from_manifest must dedup by name so lookup returns one index.
    let items = [(0, 0), (1, 0)];
    let lookup = ChunkNameLookup::from_manifest(Arc::clone(&manifest), items.into_iter());

    assert_eq!(lookup.name_locs.len(), 1, "shared chunk name must dedup to one dense index");
    let idx = lookup
        .lookup("shared")
        .expect("shared chunk name is findable");
    assert_eq!(idx, 0);
    assert_eq!(lookup.get(idx), "shared");
    assert!(lookup.lookup("absent").is_none());
}

#[test]
fn from_arena_round_trips_get_and_lookup() {
    use super::super::compact_manifest::StringArena;
    let names = ["alpha", "beta", "gamma"];
    let arena = StringArena::from(&names[..]);
    let lookup = ChunkNameLookup::from_arena(arena);

    assert_eq!(lookup.name_locs.len(), 3);
    assert_eq!(lookup.get(0), "alpha");
    assert_eq!(lookup.get(1), "beta");
    assert_eq!(lookup.get(2), "gamma");
    assert_eq!(lookup.lookup("beta"), Some(1));
    assert_eq!(lookup.lookup("delta"), None);
}

#[test]
fn from_arena_empty_lookup_returns_none() {
    use super::super::compact_manifest::StringArena;
    let lookup = ChunkNameLookup::from_arena(StringArena::default());
    assert_eq!(lookup.name_locs.len(), 0);
    assert!(lookup.lookup("anything").is_none());
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

#[test]
fn validated_resume_chunks_moves_only_valid_entries() {
    let chunks = vec![
        Some((String::from("kept"), 12)),
        None,
        Some((String::from("also-kept"), 34)),
    ];

    let validated = validated_resume_chunks(chunks);

    assert_eq!(validated.len(), 2);
    assert_eq!(validated.get("kept"), Some(&12));
    assert_eq!(validated.get("also-kept"), Some(&34));
}

#[test]
fn resume_chunk_validation_handles_parallel_batches() {
    let dir = tempfile::tempdir().unwrap();
    let mut chunks = Vec::new();
    for index in 0..70 {
        let name = format!("resume-{index}");
        std::fs::write(dir.path().join(format!("{name}.zstd")), b"x").unwrap();
        chunks.push(Some((name, 1)));
    }
    chunks.push(Some(("missing".to_owned(), 1)));

    let validated = validate_resume_chunk_entries(chunks, dir.path());

    assert_eq!(validated.len(), 70);
    assert_eq!(validated.get("resume-0"), Some(&1));
    assert_eq!(validated.get("resume-69"), Some(&1));
    assert!(!validated.contains_key("missing"));
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
    let names: Vec<&str> = diff.iter().map(|f| f.asset_name_str()).collect();
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
    let names: Vec<&str> = diff.iter().map(|f| f.asset_name_str()).collect();
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
            md5: { let mut m = [0u8; 16]; let b = super::super::assembly_opt::hex_to_bytes(&md5_hex).unwrap_or_default(); if b.len() == 16 { m.copy_from_slice(&b); } m },
            mtime_secs: mtime,
        },
    );

    let mut chunk = make_chunk("c1", data.len() as u64);
    chunk.chunk_compressed_hash_md5 = md5_hex.into();

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

#[cfg(feature = "bench-diagnostics")]
#[tokio::test]
#[ignore = "measures item-budget scheduling for predownloaded chunk verification"]
async fn benchmark_resume_chunk_check_dispatch() {
    use futures_util::stream::{self, StreamExt};
    use std::time::Instant;

    let entry_count = std::env::var("IRMIN_RESUME_CHECK_BENCH_ENTRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(5_000);
    let concurrency = std::env::var("IRMIN_RESUME_CHECK_BENCH_CONCURRENCY")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(48);
    let batch_size = std::env::var("IRMIN_RESUME_CHECK_BENCH_BATCH")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8);
    let cache_entries = std::env::var("IRMIN_RESUME_CHECK_BENCH_CACHE_ENTRIES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or_else(|| entry_count.min(cache::VERIFICATION_CACHE_MAX_ENTRIES));
    let mode =
        std::env::var("IRMIN_RESUME_CHECK_BENCH_MODE").unwrap_or_else(|_| "per-item".to_owned());
    assert!((1..=100_000).contains(&entry_count));
    assert!(cache_entries <= entry_count.min(cache::VERIFICATION_CACHE_MAX_ENTRIES));
    assert!((1..=256).contains(&concurrency));
    assert!((1..=256).contains(&batch_size));
    assert!(concurrency.is_multiple_of(batch_size));
    assert!(matches!(mode.as_str(), "per-item" | "batched"));

    let dir = match std::env::var_os("IRMIN_RESUME_CHECK_BENCH_TEMP_DIR") {
        Some(path) => tempfile::tempdir_in(path).unwrap(),
        None => tempfile::tempdir().unwrap(),
    };
    let data = [0x5au8; 4096];
    let md5 = hex::encode(fast_md5::digest(&data));
    let mut chunks = Vec::with_capacity(entry_count);
    let cache: Arc<DashMap<String, VerificationEntry>> = Arc::new(DashMap::new());
    let game_dir = Arc::new(dir.path().to_path_buf());
    let source_count = entry_count.min(64);
    for index in 0..entry_count {
        let name = format!("resume-{index:08}.zstd");
        let path = dir.path().join(&name);
        if index < source_count {
            std::fs::write(&path, data).unwrap();
        } else {
            let source = dir
                .path()
                .join(format!("resume-{:08}.zstd", index % source_count));
            std::fs::hard_link(source, &path).unwrap();
        }
        if index < cache_entries {
            let mtime_secs = std::fs::metadata(&path)
                .unwrap()
                .modified()
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs();
            cache.insert(
                name,
                VerificationEntry {
                    size: data.len() as u64,
                    md5: { let mut m = [0u8; 16]; let raw = hex::decode(&md5).unwrap(); if raw.len() == 16 { m.copy_from_slice(&raw); } m },
                    mtime_secs,
                },
            );
        }
        let mut chunk = make_chunk_with_hash(
            &format!("resume-{index:08}"),
            data.len() as u64,
            &md5,
            (index * data.len()) as u64,
        );
        chunk.chunk_compressed_hash_md5 = md5.clone().into();
        chunks.push(chunk);
    }
    let manifest = Arc::new(CompactManifest::from(vec![make_file(
        "resume.bin",
        &md5,
        chunks,
    )]));
    let started = Instant::now();
    let verified = if mode == "per-item" {
        stream::iter(0..entry_count)
            .map(|chunk_idx| {
                let manifest = Arc::clone(&manifest);
                let game_dir = Arc::clone(&game_dir);
                let cache = Arc::clone(&cache);
                let path = dir.path().join(format!("resume-{chunk_idx:08}.zstd"));
                async move {
                    let (needs_download, _) =
                        check_needs_download(&path, manifest, 0, chunk_idx, game_dir, cache)
                            .await
                            .unwrap();
                    !needs_download
                }
            })
            .buffer_unordered(concurrency)
            .fold(0usize, |verified, hit| async move {
                verified + usize::from(hit)
            })
            .await
    } else {
        let chunk_count = manifest.file_chunk_range(0).len();
        let batches = (0..chunk_count)
            .step_by(batch_size)
            .map(|start| (start, (start + batch_size).min(chunk_count)))
            .collect::<Vec<_>>();
        stream::iter(batches)
            .map(|(start, end)| {
                let manifest = Arc::clone(&manifest);
                let game_dir = Arc::clone(&game_dir);
                let cache = Arc::clone(&cache);
                let paths = (start..end)
                    .map(|chunk_idx| dir.path().join(format!("resume-{chunk_idx:08}.zstd")))
                    .collect::<Vec<_>>();
                async move {
                    tokio::task::spawn_blocking(move || {
                        let mut hits = 0;
                        for (offset, path) in paths.into_iter().enumerate() {
                            let chunk = manifest.file_chunk(0, start + offset);
                            let (valid, _) = cache::check_file_md5_cached_with_size(
                                &path,
                                chunk.chunk_size,
                                chunk.chunk_compressed_hash_md5,
                                &game_dir,
                                &cache,
                            )
                            .unwrap_or((false, None));
                            hits += usize::from(valid);
                        }
                        hits
                    })
                    .await
                    .unwrap()
                }
            })
            .buffer_unordered(concurrency / batch_size)
            .fold(0usize, |verified, batch_hits| async move {
                verified + batch_hits
            })
            .await
    };
    let elapsed = started.elapsed();
    assert_eq!(verified, entry_count);
    let job_slots = if mode == "per-item" {
        concurrency
    } else {
        concurrency / batch_size
    };
    println!(
        "resume_chunk_check mode={mode} entries={entry_count} cache_entries={cache_entries} concurrency={concurrency} batch_size={batch_size} job_slots={job_slots} elapsed_us={} verified={verified} final_cache_entries={}",
        elapsed.as_micros(),
        cache.len(),
    );
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
        chunk_name: bytes::Bytes::from("test_retry_chunk"),
        chunk_decompressed_hash_md5: bytes::Bytes::new(),
        chunk_on_file_offset: 0,
        chunk_size: data_len,
        chunk_size_decompressed: data_len,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: expected_md5.into(),
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
        downloaded_bytes: AtomicU64::new(0),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: 0,
        total_files: 0,
        resume_bytes_offset: AtomicU64::new(0),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: AtomicU64::new(now_nanos()),
        smooth_speed_bps: AtomicU64::new(0),
        eta_speed_history: Mutex::new(VecDeque::new()),
        last_speed_bytes: AtomicU64::new(0),
        last_speed_time: AtomicU64::new(now_nanos()),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: AtomicU64::new(0),
        assemblies_since_save: AtomicU64::new(0),
        save_slot: SaveSlot::new(),
        state_saver: Arc::new(|_| {}),
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(Vec::<AtomicBool>::new()),
        output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
        chunk_bitmap: Arc::new(
            super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("test.bitmap"), 1).unwrap(),
        ),
        verify_mode: super::VerifyMode::Full,
        durability_tx: Arc::new(std::sync::Mutex::new(None)),
    });

    let handle = DownloadHandle::new();

    let result = download_chunk_with_retries(
        crate::game_installer::compact_manifest::ChunkRef {
            chunk_name: chunk.chunk_name_str(),
            chunk_decompressed_hash_md5: crate::game_installer::assembly_opt::hex_to_bytes(
                chunk.chunk_decompressed_hash_md5_str(),
            )
            .unwrap_or_default()
            .leak(),
            chunk_size: chunk.chunk_size,
            chunk_size_decompressed: chunk.chunk_size_decompressed,
            chunk_compressed_hash_md5: crate::game_installer::assembly_opt::hex_to_bytes(
                chunk.chunk_compressed_hash_md5_str(),
            )
            .unwrap_or_default()
            .leak(),
            chunk_old_offset: chunk.chunk_old_offset,
        },
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
        chunk_name: bytes::Bytes::from("discard_partial_chunk"),
        chunk_decompressed_hash_md5: bytes::Bytes::new(),
        chunk_on_file_offset: 0,
        chunk_size: data_len,
        chunk_size_decompressed: data_len,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: wrong_md5.to_string().into(),
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
        downloaded_bytes: AtomicU64::new(0),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: 0,
        total_files: 0,
        resume_bytes_offset: AtomicU64::new(0),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: AtomicU64::new(now_nanos()),
        smooth_speed_bps: AtomicU64::new(0),
        eta_speed_history: Mutex::new(VecDeque::new()),
        last_speed_bytes: AtomicU64::new(0),
        last_speed_time: AtomicU64::new(now_nanos()),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: AtomicU64::new(0),
        assemblies_since_save: AtomicU64::new(0),
        save_slot: SaveSlot::new(),
        state_saver: Arc::new(|_| {}),
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(Vec::<AtomicBool>::new()),
        output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
        chunk_bitmap: Arc::new(
            super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("test.bitmap"), 1).unwrap(),
        ),
        verify_mode: super::VerifyMode::Full,
        durability_tx: Arc::new(std::sync::Mutex::new(None)),
    });

    let handle = DownloadHandle::new();

    let result = download_chunk_with_retries(
        crate::game_installer::compact_manifest::ChunkRef {
            chunk_name: chunk.chunk_name_str(),
            chunk_decompressed_hash_md5: crate::game_installer::assembly_opt::hex_to_bytes(
                chunk.chunk_decompressed_hash_md5_str(),
            )
            .unwrap_or_default()
            .leak(),
            chunk_size: chunk.chunk_size,
            chunk_size_decompressed: chunk.chunk_size_decompressed,
            chunk_compressed_hash_md5: crate::game_installer::assembly_opt::hex_to_bytes(
                chunk.chunk_compressed_hash_md5_str(),
            )
            .unwrap_or_default()
            .leak(),
            chunk_old_offset: chunk.chunk_old_offset,
        },
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
        chunk_name: bytes::Bytes::from("new_chunk"),
        chunk_size: 500,
        chunk_size_decompressed: 500,
        chunk_old_offset: -1,
        chunk_decompressed_hash_md5: bytes::Bytes::new(),
        chunk_on_file_offset: 0,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: bytes::Bytes::new(),
    };
    let chunk_reuse = SophonManifestAssetChunk {
        chunk_name: bytes::Bytes::from("reuse_chunk"),
        chunk_size: 300,
        chunk_size_decompressed: 300,
        chunk_old_offset: 42,
        chunk_decompressed_hash_md5: bytes::Bytes::new(),
        chunk_on_file_offset: 0,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: bytes::Bytes::new(),
    };
    let file1 = SophonManifestAssetProperty {
        asset_name: bytes::Bytes::from("a.pak"),
        asset_chunks: vec![chunk_new, chunk_reuse],
        asset_type: 0,
        asset_hash_md5: bytes::Bytes::new(),
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

    let game_dir = tempfile::tempdir().unwrap();
    let (data, all_files) = build_installer_data(
        vec![installer],
        "unknown",
        game_dir.path(),
        &[],
    );
    assert_eq!(data.len(), 1);
    assert_eq!(data[0].file_count, 1);
    assert_eq!(all_files.len(), 1);
    assert_eq!(all_files[0].asset_name, "a.pak");
}

fn write_hkrpg_blacklist(game_dir: &std::path::Path, entry: &str) {
    let blacklist_dir = game_dir.join("StarRail_Data/Persistent");
    std::fs::create_dir_all(&blacklist_dir).unwrap();
    std::fs::write(
        blacklist_dir.join("DownloadBlacklist.json"),
        format!("{{\"fileName\":\"{entry}\"}}"),
    )
    .unwrap();
}

#[test]
fn build_installer_data_counts_surviving_assets_per_installer() {
    // Two installers with disjoint segments: counts must track each
    // segment with no name-keyed mapping.
    let first = SophonInstaller {
        manifest: SophonManifestProto {
            assets: vec![
                make_file("keep_a.pak", "aa", vec![make_chunk("k1", 100)]),
                make_file("keep_b.pak", "bb", vec![make_chunk("k2", 100)]),
            ],
        },
        chunk_download: make_download_info(),
        label: "first".into(),
        matching_field: "game".into(),
        manifest_hash: "h1".into(),
    };
    let second = SophonInstaller {
        manifest: SophonManifestProto {
            assets: vec![
                make_file("mix_a.pak", "cc", vec![make_chunk("m1", 100)]),
                make_file("mix_b.pak", "dd", vec![make_chunk("m2", 100)]),
            ],
        },
        chunk_download: make_download_info(),
        label: "second".into(),
        matching_field: "game".into(),
        manifest_hash: "h2".into(),
    };
    let game_dir = tempfile::tempdir().unwrap();
    write_hkrpg_blacklist(game_dir.path(), "totally_unrelated_name");

    let (data, all_files) = build_installer_data(
        vec![first, second],
        "hkrpg",
        game_dir.path(),
        &[],
    );
    assert_eq!(data.len(), 2);
    assert_eq!(data[0].file_count, 2);
    assert_eq!(data[1].file_count, 2, "non-matching blacklist removes nothing");
    assert_eq!(all_files.len(), 4);

    // A removal inside one segment must decrement only that segment.
    write_hkrpg_blacklist(game_dir.path(), "mix_a.pak");
    let first = SophonInstaller {
        manifest: SophonManifestProto {
            assets: vec![
                make_file("keep_a.pak", "aa", vec![make_chunk("k1", 100)]),
                make_file("keep_b.pak", "bb", vec![make_chunk("k2", 100)]),
            ],
        },
        chunk_download: make_download_info(),
        label: "first".into(),
        matching_field: "game".into(),
        manifest_hash: "h1".into(),
    };
    let second = SophonInstaller {
        manifest: SophonManifestProto {
            assets: vec![
                make_file("mix_a.pak", "cc", vec![make_chunk("m1", 100)]),
                make_file("mix_b.pak", "dd", vec![make_chunk("m2", 100)]),
            ],
        },
        chunk_download: make_download_info(),
        label: "second".into(),
        matching_field: "game".into(),
        manifest_hash: "h2".into(),
    };
    let (data, all_files) = build_installer_data(
        vec![first, second],
        "hkrpg",
        game_dir.path(),
        &[],
    );
    assert_eq!(data[0].file_count, 2, "first segment untouched");
    assert_eq!(data[1].file_count, 1, "blacklisted asset must not be counted");
    assert_eq!(all_files.len(), 3);
    assert_eq!(all_files[2].asset_name, "mix_b.pak");
}

#[test]
fn flat_chunk_entry_collector_preserves_group_and_entry_order() {
    if usize::BITS == 64 {
        assert_eq!(
            std::mem::size_of::<(u32, FileEntry)>(),
            std::mem::size_of::<u32>() * 4
        );
        assert!(
            std::mem::size_of::<(u32, FileEntry)>() < std::mem::size_of::<(usize, FileEntry)>()
        );
    }
    let mut collector = super::ChunkEntryCollector::with_capacity(5, 3).unwrap();
    collector.push(1, (10, 2, 3)).unwrap();
    collector.push(0, (20, 1, 4)).unwrap();
    collector.push(1, (11, 2, 5)).unwrap();
    collector.push(0, (21, 1, 6)).unwrap();
    collector.push(2, (30, 3, 7)).unwrap();
    assert_eq!(collector.counts, vec![2, 2, 1]);

    let (flat, offsets) = collector.finish(3).unwrap();

    assert_eq!(offsets, vec![0, 2, 4, 5]);
    assert_eq!(
        flat,
        vec![(20, 1, 4), (21, 1, 6), (10, 2, 3), (11, 2, 5), (30, 3, 7)]
    );
}

#[test]
fn flat_chunk_entry_collector_preserves_interleaved_groups() {
    let mut collector = super::ChunkEntryCollector::with_capacity(5, 3).unwrap();
    collector.push(2, (20, 2, 0)).unwrap();
    collector.push(0, (0, 0, 0)).unwrap();
    collector.push(2, (21, 2, 1)).unwrap();
    collector.push(1, (10, 1, 0)).unwrap();
    collector.push(0, (1, 0, 1)).unwrap();

    let (flat, offsets) = collector.finish(3).unwrap();

    assert_eq!(offsets, vec![0, 2, 3, 5]);
    assert_eq!(
        flat,
        vec![(0, 0, 0), (1, 0, 1), (10, 1, 0), (20, 2, 0), (21, 2, 1)]
    );
}

#[test]
fn chunk_entry_offsets_reject_u32_overflow_without_mutating_offsets() {
    let mut offsets = vec![u32::MAX - 2];
    push_chunk_entry_offset(&mut offsets, 2).unwrap();
    assert_eq!(offsets, vec![u32::MAX - 2, u32::MAX]);

    let error = push_chunk_entry_offset(&mut offsets, 1).unwrap_err();
    assert!(error.to_string().contains("u32 index range"));
    assert_eq!(offsets, vec![u32::MAX - 2, u32::MAX]);
}

#[test]
fn flat_chunk_entry_collector_keeps_empty_item_groups() {
    let mut collector = super::ChunkEntryCollector::with_capacity(2, 5).unwrap();
    collector.push(1, (10, 2, 3)).unwrap();
    collector.push(3, (30, 4, 5)).unwrap();

    let (flat, offsets) = collector.finish(5).unwrap();

    assert_eq!(flat, vec![(10, 2, 3), (30, 4, 5)]);
    assert_eq!(offsets, vec![0, 0, 1, 1, 2, 2]);

    let mut extra_group = super::ChunkEntryCollector::with_capacity(1, 2).unwrap();
    extra_group.push(1, (1, 1, 1)).unwrap();
    assert!(extra_group.finish(1).is_err());

    let empty = super::ChunkEntryCollector::with_capacity(0, 0)
        .unwrap()
        .finish(0)
        .unwrap();
    assert_eq!(empty, (Vec::new(), vec![0]));
}

#[test]
fn flat_registration_matches_nested_entries_for_deduplicated_and_independent_chunks() {
    let mut old_chunk = make_chunk("old-source", 7);
    old_chunk.chunk_old_offset = 12;
    let manifest = CompactManifest::from(vec![
        make_file("old-only.pak", "old-only-hash", vec![old_chunk.clone()]),
        make_file("empty-before.pak", "empty-before-hash", vec![]),
        make_file(
            "first.pak",
            "hash-a",
            vec![
                make_chunk("shared", 11),
                make_chunk("first-only", 13),
                old_chunk,
            ],
        ),
        make_file(
            "second.pak",
            "hash-b",
            vec![make_chunk("shared", 17), make_chunk("second-only", 19)],
        ),
        make_file("empty.pak", "hash-empty", vec![]),
    ]);

    let run = |flat_output: bool, force_eager: bool| {
        let total_chunks = manifest.num_chunks();
        let mut nested_entries = Vec::with_capacity(total_chunks);
        let mut flat_collector =
            super::ChunkEntryCollector::with_capacity(total_chunks, total_chunks).unwrap();
        let mut items = Vec::new();
        let mut index = new_download_items_index(super::VerifyMode::Full, total_chunks);
        let mut refcounts = Vec::new();
        let mut pending = Vec::new();
        let mut tainted = rustc_hash::FxHashSet::default();
        let mut pre_downloaded = FxHashMap::default();
        pre_downloaded.insert(3, 13);

        for file_idx in 0..manifest.num_files() {
            let mut add_entry = |item_idx, entry| {
                if flat_output {
                    flat_collector.push(item_idx, entry)
                } else {
                    push_nested_chunk_entry(&mut nested_entries, item_idx, entry)
                }
            };
            register_chunks_for_file(
                &manifest,
                file_idx,
                file_idx,
                &mut add_entry,
                &mut items,
                &mut index,
                &mut refcounts,
                &mut pending,
                file_idx,
                &pre_downloaded,
                &mut tainted,
                force_eager,
            )
            .unwrap();
        }

        let (flat_entries, offsets) = if flat_output {
            flat_collector.finish(items.len()).unwrap()
        } else {
            let total_entries = nested_entries.iter().map(Vec::len).sum();
            let mut offsets = Vec::with_capacity(nested_entries.len() + 1);
            let mut entries = Vec::with_capacity(total_entries);
            offsets.push(0);
            for group in nested_entries {
                entries.extend(group);
                offsets.push(entries.len() as u32);
            }
            (entries, offsets)
        };
        let item_fields = items
            .iter()
            .map(|item| {
                (
                    item.file_idx,
                    item.chunk_idx,
                    item.output_offset,
                    item.installer_idx,
                    item.is_pre_downloaded,
                    item.use_eager,
                )
            })
            .collect::<Vec<_>>();
        let refcount_values = refcounts
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .collect::<Vec<_>>();
        let pending_values = pending
            .iter()
            .map(|count| count.load(Ordering::Relaxed))
            .collect::<Vec<_>>();
        (
            item_fields,
            flat_entries,
            offsets,
            refcount_values,
            pending_values,
            tainted,
        )
    };

    for force_eager in [false, true] {
        assert_eq!(run(false, force_eager), run(true, force_eager));
    }
}

#[cfg(feature = "bench-diagnostics")]
#[test]
#[ignore = "measures download-state registration and flatten allocations"]
fn benchmark_chunk_entry_flatten_baseline() {
    use std::io::Read as _;
    use std::time::Instant;

    fn process_hwm_kib() -> u64 {
        let mut status = String::new();
        std::fs::File::open("/proc/self/status")
            .unwrap()
            .read_to_string(&mut status)
            .unwrap();
        status
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse::<u64>().ok())
            })
            .unwrap_or_default()
    }

    let file_count = std::env::var("IRMIN_ENTRY_BENCH_FILES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(10_000);
    let chunks_per_file = std::env::var("IRMIN_ENTRY_BENCH_CHUNKS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8);
    let unique_chunk_count = std::env::var("IRMIN_ENTRY_BENCH_UNIQUE_CHUNKS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(file_count * chunks_per_file);
    let mode = std::env::var("IRMIN_ENTRY_BENCH_MODE").unwrap_or_else(|_| "nested".to_owned());
    assert!(file_count > 0 && chunks_per_file > 0 && unique_chunk_count > 0);
    let total_chunk_occurrences = file_count
        .checked_mul(chunks_per_file)
        .expect("diagnostic occurrence count overflow");
    let assets: Vec<SophonManifestAssetProperty> = (0..file_count)
        .map(|file_index| {
            let chunks = (0..chunks_per_file)
                .map(|chunk_index| {
                    let occurrence = file_index * chunks_per_file + chunk_index;
                    make_chunk(
                        &format!("chunk-{:08}", occurrence % unique_chunk_count),
                        1024,
                    )
                })
                .collect();
            make_file(
                &format!("asset-{file_index:08}.bundle"),
                "file-hash",
                chunks,
            )
        })
        .collect();
    let manifest = CompactManifest::from(assets);
    let total_chunks = manifest.num_chunks();
    let mut items = Vec::with_capacity(total_chunks);
    let mut index = new_download_items_index(super::VerifyMode::Full, total_chunks);
    let mut refcounts = Vec::with_capacity(total_chunks);
    let mut pending = Vec::with_capacity(manifest.num_files());
    let mut tainted = rustc_hash::FxHashSet::default();
    let empty_pre_downloaded = FxHashMap::default();

    std::fs::write("/proc/self/clear_refs", b"5").unwrap();
    let before_hwm = process_hwm_kib();
    let started = Instant::now();
    let (
        registration_elapsed,
        flatten_elapsed,
        unique_items,
        association_count,
        after_register_hwm,
    ) = match mode.as_str() {
        "nested" => {
            let mut entries = Vec::with_capacity(total_chunks);
            for file_index in 0..manifest.num_files() {
                register_chunks_for_file(
                    &manifest,
                    file_index,
                    file_index % 4,
                    |item_idx, entry| push_nested_chunk_entry(&mut entries, item_idx, entry),
                    &mut items,
                    &mut index,
                    &mut refcounts,
                    &mut pending,
                    file_index % 4,
                    &empty_pre_downloaded,
                    &mut tainted,
                    false,
                )
                .unwrap();
            }
            let registration_elapsed = started.elapsed();
            let after_register_hwm = process_hwm_kib();
            let total_flat_entries = entries.iter().map(Vec::len).sum::<usize>();
            let flatten_started = Instant::now();
            let mut offsets = Vec::with_capacity(entries.len() + 1);
            let mut flat = Vec::with_capacity(total_flat_entries);
            offsets.push(0);
            for inner in entries.drain(..) {
                flat.extend(inner);
                offsets.push(flat.len() as u32);
            }
            assert_eq!(flat.len(), total_flat_entries);
            assert_eq!(offsets.len(), items.len() + 1);
            let flatten_elapsed = flatten_started.elapsed();
            (
                registration_elapsed,
                flatten_elapsed,
                items.len(),
                total_flat_entries,
                after_register_hwm,
            )
        }
        "flat" => {
            let mut collector =
                super::ChunkEntryCollector::with_capacity(total_chunks, total_chunks).unwrap();
            for file_index in 0..manifest.num_files() {
                register_chunks_for_file(
                    &manifest,
                    file_index,
                    file_index % 4,
                    |item_idx, entry| collector.push(item_idx, entry),
                    &mut items,
                    &mut index,
                    &mut refcounts,
                    &mut pending,
                    file_index % 4,
                    &empty_pre_downloaded,
                    &mut tainted,
                    false,
                )
                .unwrap();
            }
            let registration_elapsed = started.elapsed();
            let after_register_hwm = process_hwm_kib();
            let scatter_started = Instant::now();
            let (_flat, offsets) = collector.finish(items.len()).unwrap();
            let flatten_elapsed = scatter_started.elapsed();
            assert_eq!(offsets.len(), items.len() + 1);
            (
                registration_elapsed,
                flatten_elapsed,
                items.len(),
                total_chunk_occurrences,
                after_register_hwm,
            )
        }
        other => panic!("unsupported entry benchmark mode: {other}"),
    };
    let after_flatten_hwm = process_hwm_kib();
    assert_eq!(association_count, total_chunk_occurrences);
    println!(
        "chunk_entry_flatten mode={mode} files={file_count} chunks_per_file={chunks_per_file} occurrences={total_chunk_occurrences} unique_items={unique_items} associations={association_count} registration_ms={:.3} flatten_ms={:.3} before_hwm_kib={before_hwm} after_register_hwm_kib={after_register_hwm} after_flatten_hwm_kib={after_flatten_hwm}",
        registration_elapsed.as_secs_f64() * 1000.0,
        flatten_elapsed.as_secs_f64() * 1000.0,
    );
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

#[tokio::test]
async fn save_assembly_state_gates_on_completion_interval() {
    use std::sync::atomic::AtomicUsize;

    let dir = tempfile::tempdir().unwrap();
    let save_calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&save_calls);

    let ctx = Arc::new(InstallContext {
        installer_clients: Arc::new(Vec::new()),
        installer_downloads: Arc::new(Vec::new()),
        chunks_dir: Arc::new(dir.path().to_path_buf()),
        game_dir: dir.path().to_path_buf().into(),
        all_tmp_dirs: Arc::new(Vec::new()),
        all_files: Arc::new(CompactManifest::from(Vec::new())),
        downloaded_bytes: AtomicU64::new(0),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: 0,
        total_files: 0,
        resume_bytes_offset: AtomicU64::new(0),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: AtomicU64::new(now_nanos()),
        smooth_speed_bps: AtomicU64::new(0),
        eta_speed_history: Mutex::new(VecDeque::new()),
        last_speed_bytes: AtomicU64::new(0),
        last_speed_time: AtomicU64::new(now_nanos()),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: AtomicU64::new(0),
        assemblies_since_save: AtomicU64::new(0),
        save_slot: SaveSlot::new(),
        state_saver: Arc::new(move |entries: &[(&str, u64)]| {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            assert!(entries.is_empty(), "empty manifest snapshots no chunks");
        }),
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(Vec::<AtomicBool>::new()),
        output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
        chunk_bitmap: Arc::new(
            super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("gate.bitmap"), 1).unwrap(),
        ),
        verify_mode: super::VerifyMode::Full,
        durability_tx: Arc::new(std::sync::Mutex::new(None)),
    });

    // Boundary checks: the save fires exactly on the interval — firing one
    // call early or one call late must fail. Each step drives the
    // completion counter to an exact multiple of the interval, drains,
    // then asserts, so an off-by-one cannot hide inside a long loop.
    let interval = crate::CHUNK_STATE_SAVE_INTERVAL;

    // Stay below the first boundary: no saves.
    for _ in 0..(interval - 1) {
        save_assembly_state_if_due(&ctx);
    }
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        0,
        "completions below the interval must not save"
    );

    // The exact first boundary saves once.
    save_assembly_state_if_due(&ctx);
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "the first boundary must save exactly once"
    );

    // Calls past the boundary hold until the next exact boundary; this
    // also proves a drained slot never owed a save it skipped.
    for _ in 0..7 {
        save_assembly_state_if_due(&ctx);
    }
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "calls between boundaries must not save"
    );

    // The exact second boundary saves once more.
    for _ in 0..(interval - 7) {
        save_assembly_state_if_due(&ctx);
    }
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the second boundary must save exactly once"
    );

    // The partial tail does not save.
    for _ in 0..7 {
        save_assembly_state_if_due(&ctx);
    }
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "the partial tail must not save"
    );

    // The terminal save path (save_assembly_state) bypasses the gate.
    save_assembly_state(&ctx);
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        3,
        "two interval saves plus the terminal save must be recorded"
    );
    assert!(
        !ctx.save_slot.is_in_flight(),
        "the slot must be idle once drained"
    );
}

#[tokio::test]
async fn save_slot_runs_save_on_the_blocking_pool_and_drains() {
    let slot = SaveSlot::new();
    let caller = std::thread::current().id();
    let save_thread = Arc::new(std::sync::Mutex::new(None));
    let recorded = Arc::clone(&save_thread);
    slot.trigger(move || {
        *recorded.lock().unwrap() = Some(std::thread::current().id());
    });
    slot.drain().await;
    let save_thread = save_thread.lock().unwrap().take();
    // The caller (a runtime worker) must never run the blocking save itself.
    assert_ne!(save_thread.expect("save must have run"), caller);
}

#[tokio::test]
async fn save_slot_coalesces_triggers_while_a_save_is_in_flight() {
    let slot = SaveSlot::new();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let calls_in_save = Arc::clone(&calls);
    slot.trigger(move || {
        calls_in_save.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        started_tx.send(()).unwrap();
        // Park the in-flight save so the skip window is deterministic.
        release_rx.recv().unwrap();
        calls_in_save.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    started_rx.recv().unwrap();

    // A second trigger while the first is parked must be skipped entirely.
    slot.trigger(|| panic!("skipped trigger must never run"));
    assert!(started_rx.try_recv().is_err());

    release_tx.send(()).unwrap();
    slot.drain().await;
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "exactly one save ran: one start and one completion marker"
    );

    // With the slot idle again the next trigger is accepted.
    let accepted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let recorded_accept = Arc::clone(&accepted);
    slot.trigger(move || {
        recorded_accept.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    slot.drain().await;
    assert!(accepted.load(std::sync::atomic::Ordering::Relaxed));
}

#[tokio::test]
async fn save_slot_drain_returns_after_the_in_flight_save_completes() {
    let slot = SaveSlot::new();
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    slot.trigger(move || {
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    started_rx.recv().unwrap();

    // Drain blocks (asynchronously) until the parked save finishes; only then
    // may the terminal save order itself after this one.
    let drain = tokio::time::timeout(std::time::Duration::from_secs(10), slot.drain());
    tokio::task::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        release_tx.send(()).unwrap();
    });
    drain.await.expect("drain must complete once released");

    // A second drain with nothing in flight returns immediately.
    tokio::time::timeout(std::time::Duration::from_secs(10), slot.drain())
        .await
        .expect("idle drain must complete");
}

#[tokio::test]
async fn save_assembly_state_coalesces_triggers_while_a_save_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    // Gate for parking the first save: a (started, release) flag pair with
    // a condvar. Both halves are Sync, so they can live in the Send+Sync
    // state_saver closure; every trigger that arrives while the save is
    // parked deterministically takes the coalescing skip branch.
    let gate = Arc::new((
        std::sync::Mutex::new((false, false)),
        std::sync::Condvar::new(),
    ));
    let save_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let ctx = Arc::new(InstallContext {
        installer_clients: Arc::new(Vec::new()),
        installer_downloads: Arc::new(Vec::new()),
        chunks_dir: Arc::new(dir.path().to_path_buf()),
        game_dir: dir.path().to_path_buf().into(),
        all_tmp_dirs: Arc::new(Vec::new()),
        all_files: Arc::new(CompactManifest::from(Vec::new())),
        downloaded_bytes: AtomicU64::new(0),
        assembled_files: Arc::new(AtomicU64::new(0)),
        checked_files: Arc::new(AtomicU64::new(0)),
        total_bytes: 0,
        total_files: 0,
        resume_bytes_offset: AtomicU64::new(0),
        verify_cache: Arc::new(DashMap::new()),
        chunk_refcounts: Arc::new(OnceLock::new()),
        chunk_names: Arc::new(OnceLock::new()),
        last_assembly_update: Arc::new(Mutex::new(Instant::now())),
        last_update: AtomicU64::new(now_nanos()),
        smooth_speed_bps: AtomicU64::new(0),
        eta_speed_history: Mutex::new(VecDeque::new()),
        last_speed_bytes: AtomicU64::new(0),
        last_speed_time: AtomicU64::new(now_nanos()),
        updater: Arc::new(|_| {}),
        downloaded_chunks: Arc::new(OnceLock::new()),
        chunks_since_save: AtomicU64::new(0),
        assemblies_since_save: AtomicU64::new(0),
        save_slot: SaveSlot::new(),
        state_saver: {
            let counter = Arc::clone(&save_calls);
            let gate = Arc::clone(&gate);
            Arc::new(move |_entries: &[(&str, u64)]| {
                counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let (lock, cvar) = &*gate;
                let mut state = lock.lock().unwrap();
                state.0 = true;
                cvar.notify_all();
                // Runs on the blocking pool inside the save, so parking
                // here is legal.
                while !state.1 {
                    state = cvar.wait(state).unwrap();
                }
            })
        },
        save_chunk_names: Arc::new(OnceLock::new()),
        adaptive_assembly: Arc::new(AdaptiveAssembly::new()),
        profiler: Arc::new(super::profiling::PipelineProfiler::new()),
        completion_flags: Arc::from(Vec::<AtomicBool>::new()),
        output_allocator: Arc::new(super::output_allocator::OutputAllocator::new(dir.path())),
        chunk_bitmap: Arc::new(
            super::chunk_bitmap::ChunkBitmap::create(&dir.path().join("gate.bitmap"), 1).unwrap(),
        ),
        verify_mode: super::VerifyMode::Full,
        durability_tx: Arc::new(std::sync::Mutex::new(None)),
    });

    // The parked save is in flight; wait for it to signal. The condvar
    // wait runs on the blocking pool (not the runtime thread) under a
    // timeout, so a save that never starts fails the test instead of
    // hanging it.
    save_assembly_state(&ctx);
    {
        let gate = Arc::clone(&gate);
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let (lock, cvar) = &*gate;
                let mut state = lock.lock().unwrap();
                while !state.0 {
                    state = cvar.wait(state).unwrap();
                }
            }),
        )
        .await
        .expect("parked save must start within the timeout")
        .expect("gate wait task must not panic");
    }

    // Many "workers" cross the save boundary while it runs — the overlap
    // scenario that previously spawned redundant concurrent saves. Every
    // one of these triggers must be coalesced away. Each call also
    // contends on the slot lock against the parked save's unfinished
    // handle, mirroring the concurrent worker overlap; the async-task
    // form of that race is covered by the SaveSlot tests.
    for _ in 0..16 {
        save_assembly_state(&ctx);
    }
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "only the in-flight save may run"
    );

    // Release the parked save; drain must then complete.
    {
        let (lock, cvar) = &*gate;
        lock.lock().unwrap().1 = true;
        cvar.notify_all();
    }
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(10), ctx.save_slot.drain())
            .await
            .is_ok(),
        "drain must finish once the parked save is released"
    );

    // Once idle, the next boundary saves again.
    save_assembly_state(&ctx);
    ctx.save_slot.drain().await;
    assert_eq!(
        save_calls.load(std::sync::atomic::Ordering::Relaxed),
        2,
        "an idle slot must accept the next trigger"
    );
}

