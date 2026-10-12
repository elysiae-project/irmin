use super::*;
use crate::api_scrape::Compression;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
