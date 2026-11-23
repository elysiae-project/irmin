//! End-to-end integration tests driving each public `Sophon` method through
//! the full happy path (manifest fetch → file check → chunk download →
//! assembly → commit) using wiremock for the front-door, build, manifest, and
//! chunk endpoints.
//!
//! These tests guard against integration errors in the public API surface
//! that unit tests of internal functions cannot reach. They require the
//! `bench-internals` feature to expose the `bench_test_endpoints` override
//! that redirects the hardcoded API URLs to the wiremock server.
//!
//! Run: cargo test --test sophon_e2e --features bench-internals -- --include-ignored

#![cfg(feature = "bench-internals")]

use bytes::Bytes;
use prost::Message as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use irmin_rs::game_installer::bench_test_endpoints::{set_build_base, set_front_door};
#[cfg(feature = "preinstall")]
use irmin_rs::game_installer::bench_test_endpoints::set_patch_build_base;
use irmin_rs::proto_parse::{SophonManifestAssetChunk, SophonManifestAssetProperty, SophonManifestProto};
use irmin_rs::{DownloadHandle, Sophon, SophonProgress, VerifyMode};

/// Fixture holding the wiremock server and the manifest/chunk data needed
/// to drive a one-file, one-chunk download end-to-end.
struct E2eFixture {
    server: MockServer,
    /// The decompressed payload the assembled output must match.
    payload: Vec<u8>,
    /// The file name the manifest declares for the asset.
    file_name: &'static str,
    /// The manifest id the build response references. (Kept for fixture
    /// clarity; the build body is built inline in setup.)
    #[allow(dead_code)]
    manifest_id: &'static str,
    /// The matching_field for the game manifest. (Kept for fixture clarity.)
    #[allow(dead_code)]
    matching_field: &'static str,
}

impl E2eFixture {
    /// Build a fixture with a small incompressible payload, mount the
    /// front-door, build, manifest, and chunk endpoints on a fresh
    /// wiremock server, and point the test endpoint overrides at it.
    async fn setup() -> Self {
        let server = MockServer::start().await;

        // Small incompressible payload (32 KiB) so zstd frames are realistic
        // but the test stays fast.
        let payload: Vec<u8> = (0..32 * 1024)
            .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
            .collect();
        let compressed = {
            use std::io::Write as _;
            let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
            encoder.window_log(20).unwrap();
            encoder.write_all(&payload).unwrap();
            encoder.finish().unwrap()
        };
        let compressed_hash = hex::encode(fast_md5::digest(&compressed));
        let decompressed_hash = hex::encode(fast_md5::digest(&payload));
        let file_md5 = decompressed_hash.clone();

        let file_name = "output.bin";
        let manifest_id = "manifest-game";
        let matching_field = "game";

        // Build the manifest protobuf. The chunk is eager (old_offset = -1)
        // so install() uses the eager decompression path.
        let chunk = SophonManifestAssetChunk::from_parts(
            Bytes::from("chunk0"),
            Bytes::from(decompressed_hash),
            0,
            compressed.len() as u64,
            payload.len() as u64,
            0,
            Bytes::from(compressed_hash),
            -1,
        );
        let asset = SophonManifestAssetProperty::from_parts(
            Bytes::from(file_name),
            vec![chunk],
            0,
            payload.len() as u64,
            Bytes::from(file_md5),
        );
        let manifest = SophonManifestProto { assets: vec![asset] };
        let manifest_bytes = manifest.encode_to_vec();

        // Mount the chunk endpoint: GET /{url_suffix}/{chunk_name}.
        // The DownloadInfo in the build response points url_prefix at the
        // server and url_suffix at "chunks", so the chunk URL is
        // {server}/chunks/chunk0.
        Mock::given(method("GET"))
            .and(path("chunks/chunk0"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(compressed.clone())
                    .insert_header("content-length", compressed.len().to_string()),
            )
            .mount(&server)
            .await;

        // Mount the manifest endpoint: GET /{url_suffix}/{manifest_id}.
        // The manifest_download DownloadInfo points url_suffix at "manifests".
        Mock::given(method("GET"))
            .and(path(format!("manifests/{manifest_id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_bytes(manifest_bytes.clone())
                    .insert_header(
                        "content-length",
                        manifest_bytes.len().to_string(),
                    ),
            )
            .mount(&server)
            .await;

        // Mount the front-door endpoint. The response must contain a
        // game_branch whose biz starts with "{game_id}_" (we use "nap" so
        // biz "nap_e2e" matches).
        let front_door_body = serde_json::json!({
            "retcode": 0,
            "message": "OK",
            "data": {
                "game_branches": [{
                    "game": { "id": "nap_e2e", "biz": "nap_e2e" },
                    "main": {
                        "package_id": "pkg",
                        "branch": "main",
                        "password": "pw",
                        "tag": "1.0.0",
                        "diff_tags": [],
                    },
                    "pre_download": null,
                }]
            }
        });
        Mock::given(method("GET"))
            .and(path("api/getGameBranches"))
            .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
            .mount(&server)
            .await;

        // Mount the build endpoint. The manifest's chunk_download and
        // manifest_download DownloadInfo point at the server so the chunk
        // and manifest fetches hit the mocks above.
        let build_body = serde_json::json!({
            "retcode": 0,
            "message": "OK",
            "data": {
                "build_id": "build-1",
                "tag": "1.0.0",
                "manifests": [{
                    "category_id": "1",
                    "category_name": "game",
                    "matching_field": matching_field,
                    "manifest": {
                        "id": manifest_id,
                        "checksum": "x",
                        "compressed_size": "0",
                        "uncompressed_size": "0",
                    },
                    "chunk_download": {
                        "encryption": 0,
                        "password": "",
                        "compression": 0,
                        "url_prefix": format!("{}/", server.uri()),
                        "url_suffix": "chunks",
                    },
                    "manifest_download": {
                        "encryption": 0,
                        "password": "",
                        "compression": 0,
                        "url_prefix": format!("{}/", server.uri()),
                        "url_suffix": "manifests",
                    },
                    "stats": {
                        "compressed_size": "0",
                        "uncompressed_size": "0",
                        "file_count": "1",
                        "chunk_count": "1",
                    },
                }],
            }
        });
        Mock::given(method("GET"))
            .and(path("getBuild"))
            .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
            .mount(&server)
            .await;

        // Point the test endpoint overrides at the mock server.
        set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
        set_build_base(Some(format!("{}/getBuild", server.uri())));
        #[cfg(feature = "preinstall")]
        set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

        Self {
            server,
            payload,
            file_name,
            manifest_id,
            matching_field,
        }
    }

    /// Clear the test endpoint overrides (call in drop / test teardown).
    fn teardown(&self) {
        set_front_door(None);
        set_build_base(None);
        #[cfg(feature = "preinstall")]
        set_patch_build_base(None);
    }
}

impl Drop for E2eFixture {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// `Sophon::download` end-to-end: fetches the real (mocked) manifest via the
/// public `Sophon::download` method, downloads the chunk, assembles the
/// file, commits, and verifies the output. This is the closest a test can
/// get to the user's "download a game" flow without a live network.
///
/// DO NOT weaken or delete. This guards the full
/// `Sophon::download` → `build_installers` → `install()` → download →
/// assembly → commit pipeline, which is exactly the path the OnceLock crash
/// bug broke.
#[tokio::test]
async fn sophon_download_e2e_mocked_http_full_pipeline() {
    let fixture = E2eFixture::setup().await;

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir.clone())
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let on_progress = |_p: SophonProgress| {};

    let result = sophon.download(&handle, on_progress).await;
    assert!(
        result.is_ok(),
        "Sophon::download e2e failed: {:?}",
        result.err()
    );

    // The assembled output file must exist and match the payload.
    let out_path = game_dir.join(fixture.file_name);
    assert!(out_path.exists(), "output file must exist after download");
    let written = std::fs::read(&out_path).unwrap();
    assert_eq!(written, fixture.payload, "output must match the payload");

    // Verify request counts: front-door, build, manifest, chunk (4 requests).
    let received: Vec<_> = fixture.server.received_requests().await.unwrap();
    assert!(
        received.len() >= 4,
        "expected at least 4 HTTP requests (front-door, build, manifest, chunk), got {}",
        received.len()
    );
}

/// `Sophon::check_update` end-to-end: fetches the mocked front-door and
/// build, then queries update availability. Guards the check_update public
/// API path which `elysiae` calls to detect available updates.
#[tokio::test]
async fn sophon_check_update_e2e_mocked_http() {
    // Fixture's Drop clears the test endpoint overrides.
    let _fixture = E2eFixture::setup().await;

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();
    // No installed tag → check_update reports no update_available.

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::None)
        .build();

    let result = sophon.check_update().await;
    assert!(
        result.is_ok(),
        "Sophon::check_update e2e failed: {:?}",
        result.err()
    );
    let info = result.unwrap();
    // check_update returned a valid UpdateInfo without panicking. The point
    // of this test is reaching this point through the real public API path,
    // not the specific polarity (which depends on the installed tag).
    let _ = info.update_available;
}

/// `Sophon::update` end-to-end: writes an installed tag, fetches the
/// mocked front-door + old build + new build + manifests, computes the diff,
/// downloads the new chunk, assembles, and commits. Guards the update
/// public API path which `elysiae` calls to update an existing install.
///
/// The old manifest is empty (no files) and the new manifest has one file,
/// so the diff produces one installer with one eager chunk. This exercises
/// `build_update_installers` → `build_diff_installers` → `install()` with
/// the `DownloadType::Update` resume key and a non-empty deleted_files list
/// (none here since the old manifest is empty).
#[tokio::test]
async fn sophon_update_e2e_mocked_http_full_pipeline() {
    use wiremock::matchers::query_param;

    let server = MockServer::start().await;

    // Payload for the new file's chunk.
    let payload: Vec<u8> = (0..16 * 1024)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
        .collect();
    let compressed = {
        use std::io::Write as _;
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        encoder.window_log(20).unwrap();
        encoder.write_all(&payload).unwrap();
        encoder.finish().unwrap()
    };
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&payload));
    let file_md5 = decompressed_hash.clone();

    // New manifest with one file + one eager chunk.
    let chunk = SophonManifestAssetChunk::from_parts(
        Bytes::from("chunk0"),
        Bytes::from(decompressed_hash),
        0,
        compressed.len() as u64,
        payload.len() as u64,
        0,
        Bytes::from(compressed_hash),
        -1,
    );
    let asset = SophonManifestAssetProperty::from_parts(
        Bytes::from("updated.bin"),
        vec![chunk],
        0,
        payload.len() as u64,
        Bytes::from(file_md5),
    );
    let new_manifest = SophonManifestProto { assets: vec![asset] };
    let new_manifest_bytes = new_manifest.encode_to_vec();

    // Old manifest (empty — no files to diff against).
    let old_manifest = SophonManifestProto { assets: vec![] };
    let old_manifest_bytes = old_manifest.encode_to_vec();

    // Chunk endpoint.
    Mock::given(method("GET"))
        .and(path("chunks/chunk0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(compressed.clone())
                .insert_header("content-length", compressed.len().to_string()),
        )
        .mount(&server)
        .await;

    // New manifest endpoint (manifest id "new-manifest").
    Mock::given(method("GET"))
        .and(path("manifests/new-manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(new_manifest_bytes.clone())
                .insert_header("content-length", new_manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;

    // Old manifest endpoint (manifest id "old-manifest").
    Mock::given(method("GET"))
        .and(path("manifests/old-manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(old_manifest_bytes.clone())
                .insert_header("content-length", old_manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;

    // Front-door.
    let front_door_body = serde_json::json!({
        "retcode": 0,
        "message": "OK",
        "data": {
            "game_branches": [{
                "game": { "id": "nap_e2e", "biz": "nap_e2e" },
                "main": {
                    "package_id": "pkg",
                    "branch": "main",
                    "password": "pw",
                    "tag": "2.0.0",
                },
                "pre_download": null,
            }]
        }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;

    // Build endpoint: match on the tag query param to differentiate old vs new.
    // Old build (tag=1.0.0) → empty manifest; New build (no tag or tag=2.0.0)
    // → file manifest.
    let old_build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "build_id": "old-build", "tag": "1.0.0",
            "manifests": [{
                "category_id": "1", "category_name": "game", "matching_field": "game",
                "manifest": { "id": "old-manifest", "checksum": "x",
                    "compressed_size": "0", "uncompressed_size": "0" },
                "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                    "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
                "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                    "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
                "stats": { "compressed_size": "0", "uncompressed_size": "0",
                    "file_count": "0", "chunk_count": "0" },
            }],
        }
    });
    let new_build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "build_id": "new-build", "tag": "2.0.0",
            "manifests": [{
                "category_id": "1", "category_name": "game", "matching_field": "game",
                "manifest": { "id": "new-manifest", "checksum": "x",
                    "compressed_size": "0", "uncompressed_size": "0" },
                "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                    "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
                "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                    "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
                "stats": { "compressed_size": "0", "uncompressed_size": "0",
                    "file_count": "1", "chunk_count": "1" },
            }],
        }
    });
    // Old build: tag=1.0.0 query param.
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .and(query_param("tag", "1.0.0"))
        .respond_with(ResponseTemplate::new(200).set_body_json(old_build_body))
        .mount(&server)
        .await;
    // New build: no tag query param (fetch_build with tag=None).
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .and(wiremock::matchers::query_param_is_missing("tag"))
        .respond_with(ResponseTemplate::new(200).set_body_json(new_build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();
    // Write an installed tag so update() can read it (from_tag = "1.0.0").
    irmin_rs::game_installer::write_installed_tag(&game_dir, "1.0.0").unwrap();

    let sophon = Sophon::builder("nap", game_dir.clone())
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let on_progress = |_p: SophonProgress| {};
    let result = sophon.update(&handle, on_progress).await;
    assert!(
        result.is_ok(),
        "Sophon::update e2e failed: {:?}",
        result.err()
    );

    // The updated output file must exist and match the payload.
    let out_path = game_dir.join("updated.bin");
    assert!(out_path.exists(), "updated output file must exist");
    let written = std::fs::read(&out_path).unwrap();
    assert_eq!(written, payload, "updated output must match the payload");

    // Clear overrides.
    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);
}

/// `Sophon::preinstall` end-to-end: fetches the mocked front-door (with a
/// pre_download branch), patch build (POST), main build, patch manifest
/// (empty), and main manifest, then runs `preinstall_download` with zero
/// chunks (Phase 1 no-op → immediate completion + state save). Guards the
/// preinstall public API path which `elysiae` calls to pre-download
/// upcoming patches.
///
/// DO NOT weaken or delete. This guards the full `Sophon::preinstall` →
/// `build_preinstall_plan` → `preinstall_download` pipeline, including the
/// Phase 1 checking-files loop that was the user's crash site (the same
/// loop that panicked in `install()`).
#[tokio::test]
async fn sophon_preinstall_e2e_mocked_http_empty_patch() {
    use std::collections::HashMap;

    let server = MockServer::start().await;

    // Main manifest (empty — the preinstall plan references it but has no
    // patch assets to apply).
    let main_manifest = SophonManifestProto { assets: vec![] };
    let main_manifest_bytes = main_manifest.encode_to_vec();

    // Patch manifest (empty — no patch assets).
    let patch_manifest = irmin_rs::proto_parse::SophonPatchProto::default();
    let patch_manifest_bytes = patch_manifest.encode_to_vec();

    // Main manifest endpoint.
    Mock::given(method("GET"))
        .and(path("manifests/main-manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(main_manifest_bytes.clone())
                .insert_header("content-length", main_manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;

    // Patch manifest endpoint.
    Mock::given(method("GET"))
        .and(path("patch-manifests/patch-manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(patch_manifest_bytes.clone())
                .insert_header("content-length", patch_manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;

    // Front-door with a pre_download branch.
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "game_branches": [{
                "game": { "id": "nap_e2e", "biz": "nap_e2e" },
                "main": {
                    "package_id": "pkg", "branch": "main", "password": "pw", "tag": "2.0.0",
                },
                "pre_download": {
                    "package_id": "pkg2", "branch": "main", "password": "pw", "tag": "2.0.0",
                },
            }]
        }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;

    let dl = serde_json::json!({
        "encryption": 0, "password": "", "compression": 0,
        "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests",
    });
    let patch_dl = serde_json::json!({
        "encryption": 0, "password": "", "compression": 0,
        "url_prefix": format!("{}/", server.uri()), "url_suffix": "patch-manifests",
    });
    let chunk_dl = serde_json::json!({
        "encryption": 0, "password": "", "compression": 0,
        "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks",
    });

    // Patch build endpoint (POST).
    let patch_build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "build_id": "patch-build", "patch_id": "patch-1", "tag": "2.0.0",
            "manifests": [{
                "category_id": "1", "category_name": "game", "matching_field": "game",
                "manifest": { "id": "patch-manifest", "checksum": "x",
                    "compressed_size": "0", "uncompressed_size": "0" },
                "diff_download": chunk_dl,
                "manifest_download": patch_dl,
                "stats": {},
            }],
        }
    });
    Mock::given(method("POST"))
        .and(path("getPatchBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(patch_build_body))
        .mount(&server)
        .await;

    // Main build endpoint (tag=2.0.0 — the preinstall target tag).
    let main_build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "build_id": "main-build", "tag": "2.0.0",
            "manifests": [{
                "category_id": "1", "category_name": "game", "matching_field": "game",
                "manifest": { "id": "main-manifest", "checksum": "x",
                    "compressed_size": "0", "uncompressed_size": "0" },
                "chunk_download": chunk_dl,
                "manifest_download": dl,
                "stats": { "compressed_size": "0", "uncompressed_size": "0",
                    "file_count": "0", "chunk_count": "0" },
            }],
        }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(main_build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();
    // Write an installed tag (preinstall requires it).
    irmin_rs::game_installer::write_installed_tag(&game_dir, "1.0.0").unwrap();

    let sophon = Sophon::builder("nap", game_dir.clone())
        .vo_lang("en-US")
        .verify_mode(VerifyMode::None)
        .build();

    let handle = DownloadHandle::new();
    let on_progress = |_p: SophonProgress| {};
    let result = sophon.preinstall(&handle, on_progress).await;
    assert!(
        result.is_ok(),
        "Sophon::preinstall e2e failed: {:?}",
        result.err()
    );

    // A preinstall state file should be written (the marker file).
    let entries: Vec<_> = std::fs::read_dir(&game_dir).unwrap().collect();
    let has_preinstall_marker = entries.iter().any(|e| {
        e.as_ref()
            .unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".sophon_preinstall")
    });
    assert!(
        has_preinstall_marker,
        "preinstall state file must be written after successful preinstall"
    );

    // Clear overrides.
    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);
    let _ = HashMap::<u8, u8>::new(); // suppress unused import in some cfgs
}

/// `Sophon::verify_integrity` end-to-end: fetches the mocked front-door and
/// build, then scans the (empty) game dir. Guards the verify_integrity
/// public API path which `elysiae` calls to repair/verify installs.
#[tokio::test]
async fn sophon_verify_integrity_e2e_mocked_http() {
    // Fixture's Drop clears the test endpoint overrides.
    let _fixture = E2eFixture::setup().await;

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();
    // Write an installed tag so verify_integrity can proceed past the
    // NoInstalledVersion check.
    irmin_rs::game_installer::write_installed_tag(&game_dir, "1.0.0").unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::None)
        .build();

    let on_progress = |_p: SophonProgress| {};
    let result = sophon.verify_integrity(on_progress).await;
    // verify_integrity with an empty game dir (only the tag file) should
    // complete — it scans zero files and reports them as missing/damaged,
    // then attempts repair. The exact outcome depends on the manifest, but
    // it must not panic.
    assert!(
        result.is_ok(),
        "Sophon::verify_integrity e2e should complete without panicking: {:?}",
        result.err()
    );
}

/// `Sophon::apply_preinstall` end-to-end: runs `preinstall` first to create
/// a valid preinstall state, then calls `apply_preinstall` to apply it.
/// Guards the apply_preinstall public API path which `elysiae` calls on
/// release day to apply a pre-downloaded patch.
///
/// With an empty patch (zero patch assets), apply_preinstall loads the
/// state, validates the installed tag, and completes without applying any
/// patches. This exercises the full `apply_preinstall` → state load →
/// validation → apply path.
#[tokio::test]
async fn sophon_apply_preinstall_e2e_mocked_http_empty_patch() {
    use std::collections::HashMap;

    let server = MockServer::start().await;

    let main_manifest = SophonManifestProto { assets: vec![] };
    let main_manifest_bytes = main_manifest.encode_to_vec();
    let patch_manifest = irmin_rs::proto_parse::SophonPatchProto::default();
    let patch_manifest_bytes = patch_manifest.encode_to_vec();

    Mock::given(method("GET"))
        .and(path("manifests/main-manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(main_manifest_bytes.clone())
                .insert_header("content-length", main_manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("patch-manifests/patch-manifest"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(patch_manifest_bytes.clone())
                .insert_header("content-length", patch_manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;

    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "game_branches": [{
                "game": { "id": "nap_e2e", "biz": "nap_e2e" },
                "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "2.0.0" },
                "pre_download": { "package_id": "pkg2", "branch": "main", "password": "pw", "tag": "2.0.0" },
            }]
        }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;

    let dl = serde_json::json!({
        "encryption": 0, "password": "", "compression": 0,
        "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests",
    });
    let patch_dl = serde_json::json!({
        "encryption": 0, "password": "", "compression": 0,
        "url_prefix": format!("{}/", server.uri()), "url_suffix": "patch-manifests",
    });
    let chunk_dl = serde_json::json!({
        "encryption": 0, "password": "", "compression": 0,
        "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks",
    });
    let patch_build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "build_id": "patch-build", "patch_id": "patch-1", "tag": "2.0.0",
            "manifests": [{
                "category_id": "1", "category_name": "game", "matching_field": "game",
                "manifest": { "id": "patch-manifest", "checksum": "x",
                    "compressed_size": "0", "uncompressed_size": "0" },
                "diff_download": chunk_dl, "manifest_download": patch_dl, "stats": {},
            }],
        }
    });
    Mock::given(method("POST"))
        .and(path("getPatchBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(patch_build_body))
        .mount(&server)
        .await;
    let main_build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": {
            "build_id": "main-build", "tag": "2.0.0",
            "manifests": [{
                "category_id": "1", "category_name": "game", "matching_field": "game",
                "manifest": { "id": "main-manifest", "checksum": "x",
                    "compressed_size": "0", "uncompressed_size": "0" },
                "chunk_download": chunk_dl, "manifest_download": dl,
                "stats": { "compressed_size": "0", "uncompressed_size": "0",
                    "file_count": "0", "chunk_count": "0" },
            }],
        }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(main_build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();
    irmin_rs::game_installer::write_installed_tag(&game_dir, "1.0.0").unwrap();

    let sophon = Sophon::builder("nap", game_dir.clone())
        .vo_lang("en-US")
        .verify_mode(VerifyMode::None)
        .build();

    // Step 1: preinstall to create the preinstall state.
    let handle = DownloadHandle::new();
    let on_progress = |_p: SophonProgress| {};
    let preinstall_result = sophon.preinstall(&handle, on_progress).await;
    assert!(
        preinstall_result.is_ok(),
        "preinstall step failed: {:?}",
        preinstall_result.err()
    );

    // Step 2: apply the preinstall (tag "2.0.0").
    let apply_result = sophon
        .apply_preinstall("2.0.0", &handle, |_p: SophonProgress| {})
        .await;
    assert!(
        apply_result.is_ok(),
        "Sophon::apply_preinstall e2e failed: {:?}",
        apply_result.err()
    );

    // Clear overrides.
    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);
    let _ = HashMap::<u8, u8>::new();
}

/// `Sophon::download` with a chunk that returns WRONG content (hash
/// mismatch). The download must detect the MD5 mismatch, remove the
/// corrupt chunk, and return an error — NOT crash or silently accept it.
#[tokio::test]
async fn sophon_download_e2e_chunk_hash_mismatch_returns_error_not_panic() {
    let server = MockServer::start().await;

    let corrupt_compressed = b"corrupt chunk bytes that don't match any hash";
    let corrupt_hash = hex::encode(fast_md5::digest(corrupt_compressed.as_ref()));
    // Manifest declares a hash that does NOT match the corrupt payload.
    let wrong_hash = hex::encode(fast_md5::digest(b"completely different data"));
    let decompressed_hash = hex::encode(fast_md5::digest(b"decompressed placeholder"));

    let chunk = SophonManifestAssetChunk::from_parts(
        Bytes::from("chunk0"),
        Bytes::from(decompressed_hash.clone()),
        0,
        corrupt_compressed.len() as u64,
        4096,
        0,
        Bytes::from(wrong_hash),
        -1,
    );
    let asset = SophonManifestAssetProperty::from_parts(
        Bytes::from("output.bin"),
        vec![chunk],
        0,
        4096,
        Bytes::from(decompressed_hash),
    );
    let manifest = SophonManifestProto { assets: vec![asset] };
    let manifest_bytes = manifest.encode_to_vec();

    Mock::given(method("GET"))
        .and(path("chunks/chunk0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(corrupt_compressed.as_ref())
                .insert_header("content-length", corrupt_compressed.len().to_string()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("manifests/manifest-game"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "game_branches": [{
            "game": { "id": "nap_e2e", "biz": "nap_e2e" },
            "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "1.0.0" },
            "pre_download": null,
        }] }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;
    let build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "build_id": "b", "tag": "1.0.0", "manifests": [{
            "category_id": "1", "category_name": "game", "matching_field": "game",
            "manifest": { "id": "manifest-game", "checksum": "x",
                "compressed_size": "0", "uncompressed_size": "0" },
            "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
            "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
            "stats": { "compressed_size": "0", "uncompressed_size": "0",
                "file_count": "1", "chunk_count": "1" },
        }] }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let result = sophon
        .download(&handle, |_p: SophonProgress| {})
        .await;

    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);
    let _ = corrupt_hash; // suppress unused in some paths

    assert!(
        result.is_err(),
        "download with corrupt chunk must return an error, got Ok"
    );
}

/// `Sophon::download` with a chunk that returns TRUNCATED data (fewer
/// bytes than promised). Must return an error — NOT crash or hang.
#[tokio::test]
async fn sophon_download_e2e_truncated_chunk_returns_error_not_panic() {
    let server = MockServer::start().await;

    let payload: Vec<u8> = (0..16 * 1024)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
        .collect();
    let compressed = {
        use std::io::Write as _;
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        encoder.window_log(20).unwrap();
        encoder.write_all(&payload).unwrap();
        encoder.finish().unwrap()
    };
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&payload));
    let file_md5 = decompressed_hash.clone();

    let chunk = SophonManifestAssetChunk::from_parts(
        Bytes::from("chunk0"),
        Bytes::from(decompressed_hash),
        0,
        compressed.len() as u64,
        payload.len() as u64,
        0,
        Bytes::from(compressed_hash),
        -1,
    );
    let asset = SophonManifestAssetProperty::from_parts(
        Bytes::from("output.bin"),
        vec![chunk],
        0,
        payload.len() as u64,
        Bytes::from(file_md5),
    );
    let manifest = SophonManifestProto { assets: vec![asset] };
    let manifest_bytes = manifest.encode_to_vec();

    Mock::given(method("GET"))
        .and(path("chunks/chunk0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(&compressed[..4])
                .insert_header("content-length", compressed.len().to_string()),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("manifests/manifest-game"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "game_branches": [{
            "game": { "id": "nap_e2e", "biz": "nap_e2e" },
            "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "1.0.0" },
            "pre_download": null,
        }] }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;
    let build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "build_id": "b", "tag": "1.0.0", "manifests": [{
            "category_id": "1", "category_name": "game", "matching_field": "game",
            "manifest": { "id": "manifest-game", "checksum": "x",
                "compressed_size": "0", "uncompressed_size": "0" },
            "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
            "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
            "stats": { "compressed_size": "0", "uncompressed_size": "0",
                "file_count": "1", "chunk_count": "1" },
        }] }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let result = sophon
        .download(&handle, |_p: SophonProgress| {})
        .await;

    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);

    assert!(
        result.is_err(),
        "download with truncated chunk must return an error, got Ok"
    );
}

/// `Sophon::download` with a manifest that returns CORRUPT protobuf.
/// Must return an error — NOT crash.
#[tokio::test]
async fn sophon_download_e2e_corrupt_manifest_returns_error_not_panic() {
    let server = MockServer::start().await;

    let corrupt_manifest = b"\xff\xff\xff\xff\xff\xff\xff\xff not a protobuf";

    Mock::given(method("GET"))
        .and(path("manifests/manifest-game"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(corrupt_manifest.as_slice())
                .insert_header("content-length", corrupt_manifest.len().to_string()),
        )
        .mount(&server)
        .await;
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "game_branches": [{
            "game": { "id": "nap_e2e", "biz": "nap_e2e" },
            "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "1.0.0" },
            "pre_download": null,
        }] }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;
    let build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "build_id": "b", "tag": "1.0.0", "manifests": [{
            "category_id": "1", "category_name": "game", "matching_field": "game",
            "manifest": { "id": "manifest-game", "checksum": "x",
                "compressed_size": "0", "uncompressed_size": "0" },
            "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
            "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
            "stats": { "compressed_size": "0", "uncompressed_size": "0",
                "file_count": "1", "chunk_count": "1" },
        }] }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let result = sophon
        .download(&handle, |_p: SophonProgress| {})
        .await;

    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);

    assert!(
        result.is_err(),
        "download with corrupt manifest must return an error, got Ok"
    );
}

/// `Sophon::download` with a chunk endpoint that returns HTTP 500. The
/// download must exhaust retries and return an error — NOT crash or hang.
/// Guards against partial network failures where the CDN errors mid-stream.
#[tokio::test]
async fn sophon_download_e2e_chunk_server_error_returns_error_not_panic() {
    let server = MockServer::start().await;

    let payload: Vec<u8> = (0..16 * 1024)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(7))
        .collect();
    let compressed = {
        use std::io::Write as _;
        let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
        encoder.window_log(20).unwrap();
        encoder.write_all(&payload).unwrap();
        encoder.finish().unwrap()
    };
    let compressed_hash = hex::encode(fast_md5::digest(&compressed));
    let decompressed_hash = hex::encode(fast_md5::digest(&payload));
    let file_md5 = decompressed_hash.clone();

    let chunk = SophonManifestAssetChunk::from_parts(
        Bytes::from("chunk0"),
        Bytes::from(decompressed_hash),
        0,
        compressed.len() as u64,
        payload.len() as u64,
        0,
        Bytes::from(compressed_hash),
        -1,
    );
    let asset = SophonManifestAssetProperty::from_parts(
        Bytes::from("output.bin"),
        vec![chunk],
        0,
        payload.len() as u64,
        Bytes::from(file_md5),
    );
    let manifest = SophonManifestProto { assets: vec![asset] };
    let manifest_bytes = manifest.encode_to_vec();

    Mock::given(method("GET"))
        .and(path("manifests/manifest-game"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;
    // Chunk returns 500 (persistent server error).
    Mock::given(method("GET"))
        .and(path("chunks/chunk0"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
        .mount(&server)
        .await;
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "game_branches": [{
            "game": { "id": "nap_e2e", "biz": "nap_e2e" },
            "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "1.0.0" },
            "pre_download": null,
        }] }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;
    let build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "build_id": "b", "tag": "1.0.0", "manifests": [{
            "category_id": "1", "category_name": "game", "matching_field": "game",
            "manifest": { "id": "manifest-game", "checksum": "x",
                "compressed_size": "0", "uncompressed_size": "0" },
            "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
            "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
            "stats": { "compressed_size": "0", "uncompressed_size": "0",
                "file_count": "1", "chunk_count": "1" },
        }] }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let result = sophon
        .download(&handle, |_p: SophonProgress| {})
        .await;

    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);

    assert!(
        result.is_err(),
        "download with persistent 500 chunk error must return an error, got Ok"
    );
}

/// `Sophon::download` with a manifest endpoint that returns HTTP 404. The
/// manifest fetch must fail and the download must return an error — NOT crash.
#[tokio::test]
async fn sophon_download_e2e_manifest_not_found_returns_error_not_panic() {
    let server = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("manifests/manifest-game"))
        .respond_with(ResponseTemplate::new(404).set_body_string("Not Found"))
        .mount(&server)
        .await;
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "game_branches": [{
            "game": { "id": "nap_e2e", "biz": "nap_e2e" },
            "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "1.0.0" },
            "pre_download": null,
        }] }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;
    let build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "build_id": "b", "tag": "1.0.0", "manifests": [{
            "category_id": "1", "category_name": "game", "matching_field": "game",
            "manifest": { "id": "manifest-game", "checksum": "x",
                "compressed_size": "0", "uncompressed_size": "0" },
            "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "chunks" },
            "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
            "stats": { "compressed_size": "0", "uncompressed_size": "0",
                "file_count": "1", "chunk_count": "1" },
        }] }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let result = sophon
        .download(&handle, |_p: SophonProgress| {})
        .await;

    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);

    assert!(
        result.is_err(),
        "download with 404 manifest must return an error, got Ok"
    );
}

/// `Sophon::download` with a chunk endpoint that drops the connection
/// mid-stream (sends a partial body then closes). The download must detect
/// the premature EOF and return an error — NOT crash or hang forever.
/// Guards against partial network failures (connection drop mid-download).
#[tokio::test]
async fn sophon_download_e2e_connection_drop_mid_stream_returns_error_not_panic() {
    use tokio::io::AsyncWriteExt as _;

    // Raw TCP server: sends HTTP headers + partial body, then closes.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let server_uri = format!("http://{addr}/");

    let _server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Read the request line + headers (drain until empty line).
        let mut buf = [0u8; 1024];
        let _ = tokio::io::AsyncReadExt::read(&mut socket, &mut buf).await;
        // Send a partial HTTP response: headers claiming 64 KiB, but only 4 bytes of body.
        let partial_response =
            b"HTTP/1.1 200 OK\r\nContent-Length: 65536\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n1234";
        let _ = socket.write_all(partial_response).await;
        // Close immediately — connection drop mid-stream.
        let _ = socket.shutdown().await;
    });

    // Build the fixture pointing the chunk endpoint at the raw server.
    let server = MockServer::start().await;
    // We need the front-door + build + manifest to point at the wiremock
    // server, but the chunk to point at the raw TCP server. Use a custom
    // build body with chunk_download.url_prefix = the raw server.
    let payload: Vec<u8> = vec![0xABu8; 64 * 1024]; // claimed but never fully sent
    let compressed_hash = hex::encode(fast_md5::digest(b"does not matter"));
    let decompressed_hash = hex::encode(fast_md5::digest(&payload));
    let file_md5 = decompressed_hash.clone();

    let chunk = SophonManifestAssetChunk::from_parts(
        Bytes::from("chunk0"),
        Bytes::from(decompressed_hash),
        0,
        65536, // claimed compressed size
        payload.len() as u64,
        0,
        Bytes::from(compressed_hash),
        -1,
    );
    let asset = SophonManifestAssetProperty::from_parts(
        Bytes::from("output.bin"),
        vec![chunk],
        0,
        payload.len() as u64,
        Bytes::from(file_md5),
    );
    let manifest = SophonManifestProto { assets: vec![asset] };
    let manifest_bytes = manifest.encode_to_vec();

    Mock::given(method("GET"))
        .and(path("manifests/manifest-game"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_bytes(manifest_bytes.clone())
                .insert_header("content-length", manifest_bytes.len().to_string()),
        )
        .mount(&server)
        .await;
    let front_door_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "game_branches": [{
            "game": { "id": "nap_e2e", "biz": "nap_e2e" },
            "main": { "package_id": "pkg", "branch": "main", "password": "pw", "tag": "1.0.0" },
            "pre_download": null,
        }] }
    });
    Mock::given(method("GET"))
        .and(path("api/getGameBranches"))
        .respond_with(ResponseTemplate::new(200).set_body_json(front_door_body))
        .mount(&server)
        .await;
    let build_body = serde_json::json!({
        "retcode": 0, "message": "OK",
        "data": { "build_id": "b", "tag": "1.0.0", "manifests": [{
            "category_id": "1", "category_name": "game", "matching_field": "game",
            "manifest": { "id": "manifest-game", "checksum": "x",
                "compressed_size": "0", "uncompressed_size": "0" },
            "chunk_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{server_uri}"), "url_suffix": "chunks" },
            "manifest_download": { "encryption": 0, "password": "", "compression": 0,
                "url_prefix": format!("{}/", server.uri()), "url_suffix": "manifests" },
            "stats": { "compressed_size": "0", "uncompressed_size": "0",
                "file_count": "1", "chunk_count": "1" },
        }] }
    });
    Mock::given(method("GET"))
        .and(path("getBuild"))
        .respond_with(ResponseTemplate::new(200).set_body_json(build_body))
        .mount(&server)
        .await;

    set_front_door(Some(format!("{}/api/getGameBranches", server.uri())));
    set_build_base(Some(format!("{}/getBuild", server.uri())));
    #[cfg(feature = "preinstall")]
    set_patch_build_base(Some(format!("{}/getPatchBuild", server.uri())));

    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    std::fs::create_dir_all(&game_dir).unwrap();

    let sophon = Sophon::builder("nap", game_dir)
        .vo_lang("en-US")
        .verify_mode(VerifyMode::Full)
        .build();

    let handle = DownloadHandle::new();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        sophon.download(&handle, |_p: SophonProgress| {}),
    )
    .await;

    set_front_door(None);
    set_build_base(None);
    #[cfg(feature = "preinstall")]
    set_patch_build_base(None);

    // Must not hang (timeout) and must return an error (not Ok or panic).
    assert!(
        result.is_ok(),
        "download with connection drop must not hang (timed out after 30s)"
    );
    let inner = result.unwrap();
    assert!(
        inner.is_err(),
        "download with connection drop mid-stream must return an error, got Ok"
    );
}
