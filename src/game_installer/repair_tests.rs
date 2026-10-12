use super::*;
use crate::api_scrape::Compression;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn installer(
    field: &str,
    assets: Vec<SophonManifestAssetProperty>,
    info: DownloadInfo,
) -> SophonInstaller {
    SophonInstaller {
        manifest: SophonManifestProto { assets },
        chunk_download: info,
        label: field.into(),
        matching_field: field.into(),
        manifest_hash: String::new(),
    }
}

async fn verify_assets(
    client: &Client,
    assets: Vec<(SophonManifestAssetProperty, Arc<DownloadInfo>)>,
    game_dir: &Path,
    emit: impl FnMut(SophonProgress) + Send,
) -> SophonResult<()> {
    let installers = assets
        .into_iter()
        .map(|(asset, info)| installer("game", vec![asset], (*info).clone()))
        .collect();
    verify_installers(client, installers, "fixture", "en-US", game_dir, emit).await
}

fn empty_asset(name: &str) -> SophonManifestAssetProperty {
    SophonManifestAssetProperty {
        asset_name: name.into(),
        asset_chunks: vec![],
        asset_type: 0,
        asset_size: 0,
        asset_hash_md5: hex::encode(fast_md5::digest(b"")),
    }
}

fn download_info() -> Arc<DownloadInfo> {
    Arc::new(DownloadInfo {
        encryption: 0,
        password: String::new(),
        compression: Compression::None,
        url_prefix: "http://127.0.0.1:1/".into(),
        url_suffix: String::new(),
    })
}

fn chunk_asset(name: &str, bytes: &[u8]) -> (SophonManifestAssetProperty, Vec<u8>) {
    let compressed = zstd::encode_all(bytes, 1).unwrap();
    let hash = hex::encode(fast_md5::digest(bytes));
    let chunk = SophonManifestAssetChunk {
        chunk_name: "shared_chunk".into(),
        chunk_decompressed_hash_md5: hash.clone(),
        chunk_on_file_offset: 0,
        chunk_size: compressed.len() as u64,
        chunk_size_decompressed: bytes.len() as u64,
        chunk_compressed_hash_xxh: 0,
        chunk_compressed_hash_md5: hex::encode(fast_md5::digest(&compressed)),
        // The protobuf runtime field defaults to zero, not the reuse sentinel.
        chunk_old_offset: 0,
    };
    (
        SophonManifestAssetProperty {
            asset_name: name.into(),
            asset_chunks: vec![chunk],
            asset_type: 0,
            asset_size: bytes.len() as u64,
            asset_hash_md5: hash,
        },
        compressed,
    )
}

fn cache_chunk(game_dir: &Path, compressed: &[u8]) {
    fs::create_dir_all(game_dir.join("chunks")).unwrap();
    fs::write(game_dir.join("chunks/shared_chunk.zstd"), compressed).unwrap();
}

async fn chunk_server(compressed: &[u8]) -> (MockServer, Arc<DownloadInfo>) {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared_chunk"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(compressed))
        .expect(1)
        .mount(&server)
        .await;
    let mut info = (*download_info()).clone();
    info.url_prefix = format!("{}/", server.uri());
    (server, Arc::new(info))
}

#[tokio::test]
async fn repair_assembly_failure_does_not_report_success() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("broken"), b"original bytes").unwrap();
    let mut malformed = empty_asset("broken");
    malformed.asset_size = 1; // No chunks can produce the declared byte.
    let mut events = vec![];
    let result = verify_assets(
        &Client::new(),
        vec![(malformed, download_info())],
        dir.path(),
        |event| events.push(event),
    )
    .await;
    assert!(matches!(result, Err(SophonError::SizeMismatch { .. })));
    assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
    assert!(!events.iter().any(|p| matches!(p,
        SophonProgress::Assembling { assembled_files, .. } if *assembled_files > 0
    )));
    assert_eq!(
        fs::read(dir.path().join("broken")).unwrap(),
        b"original bytes"
    );
}

#[tokio::test]
async fn repair_invalid_asset_is_an_error_not_a_skipped_success() {
    let dir = tempfile::tempdir().unwrap();
    let mut events = vec![];
    let result = verify_assets(
        &Client::new(),
        vec![(empty_asset("../escape"), download_info())],
        dir.path(),
        |event| events.push(event),
    )
    .await;
    assert!(result.is_err());
    assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
}

#[tokio::test]
async fn repair_missing_empty_file_is_created() {
    let dir = tempfile::tempdir().unwrap();
    let mut events = vec![];
    verify_assets(
        &Client::new(),
        vec![(empty_asset("empty"), download_info())],
        dir.path(),
        |event| events.push(event),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(dir.path().join("empty")).unwrap(), b"");
    assert!(matches!(events.last(), Some(SophonProgress::Finished)));
}

#[tokio::test]
async fn repair_normalizes_decoded_offsets_and_replaces_damaged_file() {
    use prost::Message;
    let dir = tempfile::tempdir().unwrap();
    let (asset, compressed) = chunk_asset("file", b"valid data");
    cache_chunk(dir.path(), &compressed);
    fs::write(dir.path().join("file"), b"broken!!!!").unwrap();
    let decoded = SophonManifestAssetProperty::decode(asset.encode_to_vec().as_slice()).unwrap();
    assert_eq!(decoded.asset_chunks[0].chunk_old_offset, 0);
    verify_assets(
        &Client::new(),
        vec![(decoded, download_info())],
        dir.path(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"valid data");
}

#[tokio::test]
async fn repair_shares_chunks_across_files_and_repeated_occurrences() {
    let dir = tempfile::tempdir().unwrap();
    let (mut first, compressed) = chunk_asset("a/b", b"data");
    let mut second = first.clone();
    second.asset_name = "a_b".into(); // Previously collided with a/b's temp directory.
    let mut repeated = first.asset_chunks[0].clone();
    repeated.chunk_on_file_offset = 4;
    first.asset_chunks.push(repeated);
    first.asset_size = 8;
    first.asset_hash_md5 = hex::encode(fast_md5::digest(b"datadata"));
    cache_chunk(dir.path(), &compressed);
    let mut events = vec![];
    verify_assets(
        &Client::new(),
        vec![(first, download_info()), (second, download_info())],
        dir.path(),
        |event| events.push(event),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(dir.path().join("a/b")).unwrap(), b"datadata");
    assert_eq!(fs::read(dir.path().join("a_b")).unwrap(), b"data");
    assert!(!dir.path().join("chunks/shared_chunk.zstd").exists());
    assert!(events.iter().any(|p| matches!(
        p,
        SophonProgress::Assembling {
            assembled_files: 2,
            total_files: 2
        }
    )));
    assert!(!fs::read_dir(dir.path()).unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with("tmp-verify-")
    }));
}

#[tokio::test]
async fn repair_redownloads_same_sized_corrupt_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let (asset, compressed) = chunk_asset("file", b"healthy payload");
    cache_chunk(dir.path(), &vec![0; compressed.len()]);
    let (server, info) = chunk_server(&compressed).await;
    verify_assets(&Client::new(), vec![(asset, info)], dir.path(), |_| {})
        .await
        .unwrap();
    assert_eq!(
        fs::read(dir.path().join("file")).unwrap(),
        b"healthy payload"
    );
    server.verify().await;
}

#[tokio::test]
async fn repair_retries_a_downloaded_chunk_with_wrong_hash() {
    let dir = tempfile::tempdir().unwrap();
    let (asset, compressed) = chunk_asset("file", b"verified payload");
    let server = MockServer::start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let attempts = Arc::clone(&requests);
    Mock::given(method("GET"))
        .and(path("/shared_chunk"))
        .respond_with(move |_: &wiremock::Request| {
            let bytes = if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                vec![0; compressed.len()]
            } else {
                compressed.clone()
            };
            ResponseTemplate::new(200).set_body_bytes(bytes)
        })
        .expect(2)
        .mount(&server)
        .await;
    let mut info = (*download_info()).clone();
    info.url_prefix = format!("{}/", server.uri());
    verify_assets(
        &Client::new(),
        vec![(asset, Arc::new(info))],
        dir.path(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(
        fs::read(dir.path().join("file")).unwrap(),
        b"verified payload"
    );
}

#[tokio::test]
async fn repair_creates_missing_chunk_directory() {
    let dir = tempfile::tempdir().unwrap();
    let (asset, compressed) = chunk_asset("file", b"payload");
    let (server, info) = chunk_server(&compressed).await;
    assert!(!dir.path().join("chunks").exists());
    verify_assets(&Client::new(), vec![(asset, info)], dir.path(), |_| {})
        .await
        .unwrap();
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"payload");
    server.verify().await;
}

#[tokio::test]
async fn repair_ignores_persistent_cache_with_preserved_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let (asset, compressed) = chunk_asset("file", b"good data");
    let path = dir.path().join("file");
    fs::write(&path, b"good data").unwrap();
    let modified = path.metadata().unwrap().modified().unwrap();
    let cache = DashMap::new();
    assert!(
        cache::check_file_md5_cached(
            &path,
            asset.asset_size,
            &asset.asset_hash_md5,
            dir.path(),
            &cache
        )
        .unwrap()
    );
    cache::save_verification_cache(dir.path(), &cache).unwrap();
    fs::write(&path, b"bad! data").unwrap();
    fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    // Establish that the old scan really would trust this stale cache.
    assert!(
        cache::check_file_md5_cached(
            &path,
            asset.asset_size,
            &asset.asset_hash_md5,
            dir.path(),
            &cache
        )
        .unwrap()
    );
    cache_chunk(dir.path(), &compressed);
    verify_assets(
        &Client::new(),
        vec![(asset, download_info())],
        dir.path(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"good data");
}

#[tokio::test]
async fn repair_checks_whole_file_hash_before_replacing_original() {
    let dir = tempfile::tempdir().unwrap();
    let (mut asset, compressed) = chunk_asset("file", b"valid chunk hashes");
    asset.asset_hash_md5 = "00000000000000000000000000000000".into();
    cache_chunk(dir.path(), &compressed);
    fs::write(dir.path().join("file"), b"original").unwrap();
    let mut events = vec![];
    let result = verify_assets(
        &Client::new(),
        vec![(asset, download_info())],
        dir.path(),
        |p| events.push(p),
    )
    .await;
    assert!(matches!(result, Err(SophonError::Md5Mismatch { .. })));
    assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"original");
    assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
}

#[tokio::test]
async fn repair_nonempty_file_becomes_empty_but_directory_is_not_a_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("empty"), b"not empty").unwrap();
    let mut directory = empty_asset("directory");
    directory.asset_type = 64;
    directory.asset_hash_md5.clear();
    verify_assets(
        &Client::new(),
        vec![
            (empty_asset("empty"), download_info()),
            (directory, download_info()),
        ],
        dir.path(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(fs::read(dir.path().join("empty")).unwrap(), b"");
    assert!(!dir.path().join("directory").is_file());
}

#[tokio::test]
async fn repair_default_english_without_record_repairs_voice_and_regenerates_indexes() {
    let dir = tempfile::tempdir().unwrap();
    let voice_path = "data/English(US)/voice.pck";
    let (voice, compressed) = chunk_asset(voice_path, b"english audio");
    cache_chunk(dir.path(), &compressed);
    fs::create_dir_all(dir.path().join("data/English(US)")).unwrap();
    fs::write(dir.path().join(voice_path), b"damaged audio").unwrap();
    let mut generated = empty_asset("pkg_version");
    generated.asset_size = 123; // This CDN hash/size must not be used locally.
    let assets = vec![
        voice,
        empty_asset("data/Chinese/missing.pck"),
        empty_asset("data/ctable_streaming.dat"),
        generated,
        empty_asset("beyond_pkg_version"),
        empty_asset("Audio_Japanese_pkg_version"),
    ];
    let mut events = vec![];
    verify_installers(
        &Client::new(),
        vec![installer("game", assets, (*download_info()).clone())],
        "hk4e",
        "en-US",
        dir.path(),
        |p| events.push(p),
    )
    .await
    .unwrap();
    assert_eq!(
        fs::read(dir.path().join(voice_path)).unwrap(),
        b"english audio"
    );
    assert!(!dir.path().join("data/Chinese/missing.pck").exists());
    assert!(!dir.path().join("data/ctable_streaming.dat").exists());
    assert!(dir.path().join("beyond_pkg_version").is_file());
    assert_eq!(
        fs::read(dir.path().join("Audio_Japanese_pkg_version")).unwrap(),
        b""
    );
    assert!(
        fs::read_to_string(dir.path().join("pkg_version"))
            .unwrap()
            .contains(voice_path)
    );
    assert!(
        fs::read_to_string(dir.path().join("Audio_English(US)_pkg_version"))
            .unwrap()
            .contains(voice_path)
    );
    assert_eq!(
        fs::read_to_string(
            dir.path()
                .join("GenshinImpact_Data/Persistent/audio_lang_14")
        )
        .unwrap(),
        "English(US)\n"
    );
    assert!(matches!(events.last(), Some(SophonProgress::Finished)));
}

#[tokio::test]
async fn repair_rejects_conflicting_destination_aliases_before_mutation() {
    for second_name in ["a/file", "a/./file", "a//file"] {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("a")).unwrap();
        fs::write(dir.path().join("a/file"), b"first requirement").unwrap();
        let (first, _) = chunk_asset("a/file", b"first requirement");
        let (second, compressed) = chunk_asset(second_name, b"second requirement");
        cache_chunk(dir.path(), &compressed);
        let mut events = vec![];
        let result = verify_assets(
            &Client::new(),
            vec![(first, download_info()), (second, download_info())],
            dir.path(),
            |p| events.push(p),
        )
        .await;
        assert!(
            result.is_err(),
            "conflicting destination {second_name} was accepted"
        );
        assert_eq!(
            fs::read(dir.path().join("a/file")).unwrap(),
            b"first requirement"
        );
        assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
    }
}

#[tokio::test]
async fn repair_deduplicates_compatible_destination_aliases() {
    let dir = tempfile::tempdir().unwrap();
    let (first, compressed) = chunk_asset("a/file", b"same bytes");
    let mut second = first.clone();
    second.asset_name = "a/./file".into();
    second.asset_hash_md5.make_ascii_uppercase();
    cache_chunk(dir.path(), &compressed);
    let mut events = vec![];
    verify_assets(
        &Client::new(),
        vec![(first, download_info()), (second, download_info())],
        dir.path(),
        |p| events.push(p),
    )
    .await
    .unwrap();
    assert_eq!(fs::read(dir.path().join("a/file")).unwrap(), b"same bytes");
    assert!(events.iter().any(|p| matches!(
        p,
        SophonProgress::Assembling {
            assembled_files: 1,
            total_files: 1
        }
    )));
}

#[tokio::test]
async fn repair_preserves_star_rail_blacklist_and_rebuilds_app_info() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("StarRail_Data/Persistent")).unwrap();
    fs::write(
        dir.path()
            .join("StarRail_Data/Persistent/DownloadBlacklist.json"),
        "{\"fileName\":\"StarRail_Data/StreamingAssets/removed.pak\"}\n",
    )
    .unwrap();
    let mut app = empty_asset("StarRail_Data/app.info");
    app.asset_size = 100;
    let assets = vec![
        app,
        empty_asset("StarRail_Data/StreamingAssets/removed.pak"),
    ];
    verify_installers(
        &Client::new(),
        vec![installer("game", assets, (*download_info()).clone())],
        "hkrpg",
        "en-US",
        dir.path(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(
        !dir.path()
            .join("StarRail_Data/StreamingAssets/removed.pak")
            .exists()
    );
    assert_eq!(
        fs::read_to_string(dir.path().join("StarRail_Data/app.info")).unwrap(),
        "Cognosphere\nhkrpg_global\n"
    );
}

#[tokio::test]
async fn repair_does_not_reinstall_removed_nap_manifest() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("ZenlessZoneZero_Data/Persistent")).unwrap();
    fs::write(
        dir.path()
            .join("ZenlessZoneZero_Data/Persistent/KDelResource"),
        "old-scenes",
    )
    .unwrap();
    let installers = vec![
        installer(
            "game",
            vec![empty_asset("active")],
            (*download_info()).clone(),
        ),
        installer(
            "old-scenes",
            vec![empty_asset("removed")],
            (*download_info()).clone(),
        ),
    ];
    verify_installers(
        &Client::new(),
        installers,
        "nap",
        "en-US",
        dir.path(),
        |_| {},
    )
    .await
    .unwrap();
    assert!(dir.path().join("active").is_file());
    assert!(!dir.path().join("removed").exists());
}

#[tokio::test]
async fn repair_generation_failure_does_not_emit_finished() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("pkg_version")).unwrap();
    let mut events = vec![];
    let result = verify_installers(&Client::new(), vec![], "hk4e", "en-US", dir.path(), |p| {
        events.push(p)
    })
    .await;
    assert!(matches!(result, Err(SophonError::Io(_))));
    assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn repair_generation_flush_failure_does_not_emit_finished() {
    let dir = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink("/dev/full", dir.path().join("pkg_version")).unwrap();
    let mut events = vec![];
    let result = verify_installers(
        &Client::new(),
        vec![installer(
            "game",
            vec![empty_asset("empty")],
            (*download_info()).clone(),
        )],
        "hk4e",
        "en-US",
        dir.path(),
        |p| events.push(p),
    )
    .await;
    assert!(matches!(result, Err(SophonError::Io(_))));
    assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
}

#[tokio::test]
async fn repair_worker_panic_propagates_and_started_jobs_are_drained() {
    let started = Arc::new(std::sync::Barrier::new(4));
    let finished = Arc::new(AtomicUsize::new(0));
    let count = Arc::clone(&finished);
    let mut events = vec![];
    let result = run_repair_jobs(
        4,
        move |idx| {
            started.wait();
            if idx == 0 {
                panic!("injected repair worker failure");
            }
            count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
        |p| events.push(p),
    )
    .await;
    assert!(matches!(result, Err(SophonError::JoinError(_))));
    assert_eq!(finished.load(Ordering::SeqCst), 3);
    assert!(matches!(
        events.last(),
        Some(SophonProgress::Assembling {
            assembled_files: 3,
            total_files: 4
        })
    ));
    assert!(!events.iter().any(|p| matches!(p, SophonProgress::Finished)));
}
