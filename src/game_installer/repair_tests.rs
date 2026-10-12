use super::*;
use crate::api_scrape::Compression;

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

#[tokio::test]
async fn repair_assembly_failure_does_not_report_success() {
    let dir = tempfile::tempdir().unwrap();
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
    assert!(!events.iter().any(|p| matches!(
        p,
        SophonProgress::Assembling { assembled_files, .. } if *assembled_files > 0
    )));
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
