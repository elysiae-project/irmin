use std::path::Path;

use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::api::{fetch_build, is_known_vo_locale, vo_lang_matches};
use super::error::{SophonError, SophonResult};
use super::read_installed_tag;

use crate::api_scrape::PackageBranch;
use crate::api_scrape::SophonManifestMeta;

#[derive(Debug, Clone, Serialize, Deserialize)]
/// Availability and byte-size summary for update and preinstall operations.
pub struct UpdateInfo {
    pub update_available: bool,
    /// A remote preinstall or a usable local package is available.
    pub preinstall_available: bool,
    /// A validated completed-package record exists (not merely a marker).
    /// Legacy records may still need exact-target metadata recovery on apply.
    pub preinstall_downloaded: bool,
    pub current_tag: Option<String>,
    pub remote_tag: String,
    pub preinstall_tag: Option<String>,
    /// Best-effort estimate: zero if unknown, or a conservative cached target total.
    pub update_compressed_size: u64,
    pub update_decompressed_size: u64,
    pub preinstall_compressed_size: u64,
    pub preinstall_decompressed_size: u64,
}

/// Query remote build metadata and compare it with the installed version.
pub async fn check_update(
    client: &Client,
    game_id: &str,
    vo_lang: &str,
    game_dir: &Path,
) -> SophonResult<UpdateInfo> {
    let (front_door, current_tag) =
        tokio::join!(super::api::fetch_front_door(client, game_id), async {
            read_installed_tag(game_dir)
        });
    let (branch, pre_download_branch) = front_door?;

    let main_branch = branch.main.ok_or(SophonError::NoGameManifest)?;
    check_update_with_branches(
        game_id,
        vo_lang,
        game_dir,
        current_tag,
        main_branch,
        pre_download_branch,
        |branch, from| async move {
            if let Some(from) = from {
                fetch_diff_sizes(client, &branch, &from, &branch.tag, vo_lang).await
            } else {
                fetch_build_sizes(client, &branch, vo_lang).await
            }
        },
    )
    .await
}

// Branch discovery is separate from optional size queries, so local completed
// packages remain usable when historical build/manifest endpoints disappear.
pub(super) async fn check_update_with_branches<F, Fut>(
    game_id: &str,
    vo_lang: &str,
    game_dir: &Path,
    current_tag: Option<String>,
    main_branch: PackageBranch,
    pre_download_branch: Option<PackageBranch>,
    mut query_sizes: F,
) -> SophonResult<UpdateInfo>
where
    F: FnMut(PackageBranch, Option<String>) -> Fut,
    Fut: std::future::Future<Output = SophonResult<(u64, u64)>>,
{
    let remote_tag = main_branch.tag.clone();
    let update_available = current_tag.as_deref().is_some_and(|tag| tag != remote_tag);
    let cached_live = update_available
        .then(|| {
            super::preinstall::cached_preinstall(
                game_dir,
                &remote_tag,
                game_id,
                vo_lang,
                current_tag.as_deref(),
            )
        })
        .flatten();
    let cached = cached_live.or_else(|| {
        pre_download_branch.as_ref().and_then(|pre| {
            super::preinstall::cached_preinstall(
                game_dir,
                &pre.tag,
                game_id,
                vo_lang,
                current_tag.as_deref(),
            )
        })
    });

    let (update_compressed_size, update_decompressed_size) = if update_available {
        match query_sizes(main_branch, current_tag.clone()).await {
            Ok(sizes) => sizes,
            Err(err) => {
                log::warn!("Failed to estimate update sizes (availability is unchanged): {err}");
                cached
                    .as_ref()
                    .filter(|state| state.tag == remote_tag)
                    .map(|state| state.cached_sizes())
                    .unwrap_or((0, 0))
            }
        }
    } else {
        (0, 0)
    };

    let preinstall_downloaded = cached.is_some();
    let (
        preinstall_available,
        preinstall_tag,
        preinstall_compressed_size,
        preinstall_decompressed_size,
    ) = if let Some(state) = cached {
        let (cs, ds) = state.cached_sizes();
        (true, Some(state.tag), cs, ds)
    } else if let Some(pre) = pre_download_branch {
        let tag = pre.tag.clone();
        let (cs, ds) = query_sizes(pre, None).await.unwrap_or_else(|err| {
            log::warn!("Failed to fetch preinstall sizes: {err}");
            (0, 0)
        });
        (true, Some(tag), cs, ds)
    } else {
        (false, None, 0, 0)
    };

    Ok(UpdateInfo {
        update_available,
        preinstall_available,
        preinstall_downloaded,
        current_tag,
        remote_tag,
        preinstall_tag,
        update_compressed_size,
        update_decompressed_size,
        preinstall_compressed_size,
        preinstall_decompressed_size,
    })
}

/// Fetch build statistics and return compressed/uncompressed sizes for matching manifests.
pub async fn fetch_build_sizes(
    client: &Client,
    branch: &PackageBranch,
    vo_lang: &str,
) -> SophonResult<(u64, u64)> {
    let build = fetch_build(client, branch, Some(&branch.tag)).await?;
    if build.tag != branch.tag {
        return Err(SophonError::PreinstallStateInvalid(
            "Size metadata returned a different target tag".into(),
        ));
    }

    // Match the same filter as build_installers_from_data:
    // game + VO language + non-VO manifests (cutscenes, etc.)
    let qualifying: Vec<&SophonManifestMeta> = build
        .manifests
        .iter()
        .filter(|m| {
            m.matching_field == "game"
                || vo_lang_matches(&m.matching_field, vo_lang)
                || !is_known_vo_locale(&m.matching_field)
        })
        .collect();

    if qualifying.is_empty() {
        return Err(SophonError::NoGameManifest);
    }

    let mut cs = 0u64;
    let mut ds = 0u64;
    for meta in qualifying {
        cs += super::api::parse_size(&meta.stats.compressed_size)?;
        ds += super::api::parse_size(&meta.stats.uncompressed_size)?;
    }
    Ok((cs, ds))
}

/// Fetch patch metadata and sum the selected diff payload sizes.
pub async fn fetch_diff_sizes(
    client: &Client,
    branch: &PackageBranch,
    from_tag: &str,
    to_tag: &str,
    vo_lang: &str,
) -> SophonResult<(u64, u64)> {
    let (old_build, new_build) = tokio::try_join!(
        fetch_build(client, branch, Some(from_tag)),
        fetch_build(client, branch, Some(to_tag)),
    )?;

    if old_build.tag != from_tag || new_build.tag != to_tag {
        return Err(SophonError::PreinstallStateInvalid(
            "Update size metadata returned a different version".into(),
        ));
    }
    let mut cs = 0u64;
    let mut ds = 0u64;
    let mut seen_chunks: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();

    let old_map: rustc_hash::FxHashMap<String, &SophonManifestMeta> = old_build
        .manifests
        .iter()
        .map(|m| (m.matching_field.clone(), m))
        .collect();

    for new_meta in &new_build.manifests {
        if new_meta.matching_field != "game"
            && !vo_lang_matches(&new_meta.matching_field, vo_lang)
            && is_known_vo_locale(&new_meta.matching_field)
        {
            continue;
        }

        let matching_field = new_meta.matching_field.clone();

        let (new_manifest, old_manifest) = tokio::try_join!(
            super::api::fetch_manifest_proto(
                client,
                &new_meta.manifest_download,
                &new_meta.manifest.id,
            ),
            async {
                match old_map.get(&matching_field) {
                    Some(old_meta) => super::api::fetch_manifest_proto(
                        client,
                        &old_meta.manifest_download,
                        &old_meta.manifest.id,
                    )
                    .await
                    .map(Some),
                    None => {
                        Ok::<Option<crate::proto_parse::SophonManifestProto>, SophonError>(None)
                    }
                }
            }
        )?;
        let old_files_md5: rustc_hash::FxHashMap<&str, &str> = old_manifest
            .as_ref()
            .into_iter()
            .flat_map(|manifest| manifest.assets.iter())
            .filter(|file| !file.is_directory())
            .map(|file| (file.asset_name.as_str(), file.asset_hash_md5.as_str()))
            .collect();
        let old_chunks: rustc_hash::FxHashSet<&str> = old_manifest
            .as_ref()
            .into_iter()
            .flat_map(|manifest| manifest.assets.iter())
            .filter(|file| !file.is_directory())
            .flat_map(|file| file.asset_chunks.iter())
            .map(|chunk| chunk.chunk_decompressed_hash_md5.as_str())
            .collect();

        for file in &new_manifest.assets {
            if file.is_directory() {
                continue;
            }
            let needs_download = match old_files_md5.get(file.asset_name.as_str()) {
                Some(old_md5) => *old_md5 != file.asset_hash_md5,
                None => true,
            };
            if needs_download {
                for chunk in &file.asset_chunks {
                    // Skip chunks whose decompressed content already exists in an old file
                    if old_chunks.contains(chunk.chunk_decompressed_hash_md5.as_str()) {
                        continue;
                    }
                    if seen_chunks.insert(chunk.chunk_name.clone()) {
                        cs += chunk.chunk_size;
                        ds += chunk.chunk_size_decompressed;
                    }
                }
            }
        }
    }

    Ok((cs, ds))
}

#[cfg(test)]
fn calculate_diff_sizes_indexed<K, V, H>(
    old_files_md5: &rustc_hash::FxHashMap<K, V>,
    old_chunks: &rustc_hash::FxHashSet<H>,
    new_manifest: &crate::proto_parse::SophonManifestProto,
) -> (u64, u64)
where
    K: std::borrow::Borrow<str> + Eq + std::hash::Hash,
    V: AsRef<str>,
    H: std::borrow::Borrow<str> + Eq + std::hash::Hash,
{
    let mut seen_chunks = rustc_hash::FxHashSet::<String>::default();
    let mut compressed = 0u64;
    let mut decompressed = 0u64;

    for file in &new_manifest.assets {
        if file.is_directory() {
            continue;
        }
        let needs_download = match old_files_md5.get(file.asset_name.as_str()) {
            Some(old_md5) => old_md5.as_ref() != file.asset_hash_md5,
            None => true,
        };
        if needs_download {
            for chunk in &file.asset_chunks {
                if !old_chunks.contains(chunk.chunk_decompressed_hash_md5.as_str())
                    && seen_chunks.insert(chunk.chunk_name.clone())
                {
                    compressed += chunk.chunk_size;
                    decompressed += chunk.chunk_size_decompressed;
                }
            }
        }
    }
    (compressed, decompressed)
}

#[cfg(test)]
fn borrowed_diff_indexes_collect<'a>(
    old_manifest: &'a crate::proto_parse::SophonManifestProto,
) -> (
    rustc_hash::FxHashMap<&'a str, &'a str>,
    rustc_hash::FxHashSet<&'a str>,
) {
    let old_files_md5 = old_manifest
        .assets
        .iter()
        .filter(|file| !file.is_directory())
        .map(|file| (file.asset_name.as_str(), file.asset_hash_md5.as_str()))
        .collect();
    let old_chunks = old_manifest
        .assets
        .iter()
        .filter(|file| !file.is_directory())
        .flat_map(|file| file.asset_chunks.iter())
        .map(|chunk| chunk.chunk_decompressed_hash_md5.as_str())
        .collect();
    (old_files_md5, old_chunks)
}

#[cfg(test)]
fn owned_diff_indexes(
    old_manifest: &crate::proto_parse::SophonManifestProto,
) -> (
    rustc_hash::FxHashMap<String, String>,
    rustc_hash::FxHashSet<String>,
) {
    let old_files_md5 = old_manifest
        .assets
        .iter()
        .filter(|file| !file.is_directory())
        .map(|file| (file.asset_name.clone(), file.asset_hash_md5.clone()))
        .collect::<rustc_hash::FxHashMap<String, String>>();
    let old_chunks = old_manifest
        .assets
        .iter()
        .filter(|file| !file.is_directory())
        .flat_map(|file| file.asset_chunks.iter())
        .map(|chunk| chunk.chunk_decompressed_hash_md5.clone())
        .collect::<rustc_hash::FxHashSet<String>>();
    (old_files_md5, old_chunks)
}

#[cfg(test)]
fn make_diff_size_benchmark_manifests(
    file_count: usize,
    chunks_per_file: usize,
    changed_percent: usize,
    reused_hash_percent: usize,
) -> (
    crate::proto_parse::SophonManifestProto,
    crate::proto_parse::SophonManifestProto,
) {
    use crate::proto_parse::{SophonManifestAssetChunk, SophonManifestAssetProperty};

    let make_file = |file_idx: usize, changed: bool, is_old: bool| {
        let asset_chunks = (0..chunks_per_file)
            .map(|chunk_idx| {
                let hash = if is_old {
                    format!("old-hash-{:04}", chunk_idx % 32)
                } else if !changed && chunk_idx % 100 < reused_hash_percent {
                    format!("old-hash-{:04}", chunk_idx % 32)
                } else {
                    format!("new-hash-{file_idx:06}-{chunk_idx:04}")
                };
                SophonManifestAssetChunk {
                    chunk_name: format!("chunk-{:06}-{chunk_idx:04}", file_idx % 100),
                    chunk_decompressed_hash_md5: hash,
                    chunk_on_file_offset: (chunk_idx * 1024 * 1024) as u64,
                    chunk_size: 256 * 1024,
                    chunk_size_decompressed: 1024 * 1024,
                    chunk_compressed_hash_xxh: 0,
                    chunk_compressed_hash_md5: String::new(),
                    chunk_old_offset: -1,
                }
            })
            .collect();
        SophonManifestAssetProperty {
            asset_name: format!("asset-{file_idx:06}.pak"),
            asset_chunks,
            asset_type: 0,
            asset_size: (chunks_per_file * 1024 * 1024) as u64,
            asset_hash_md5: if changed {
                format!("new-file-hash-{file_idx:06}")
            } else {
                format!("old-file-hash-{file_idx:06}")
            },
        }
    };
    let old = (0..file_count)
        .map(|index| make_file(index, false, true))
        .collect::<Vec<_>>();
    let new = (0..file_count)
        .map(|index| make_file(index, index % 100 < changed_percent, false))
        .collect::<Vec<_>>();
    (
        crate::proto_parse::SophonManifestProto { assets: old },
        crate::proto_parse::SophonManifestProto { assets: new },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::game_installer::api;

    #[test]
    fn parse_size_returns_correct_values() {
        assert_eq!(api::parse_size("0").unwrap(), 0);
        assert_eq!(api::parse_size("1024").unwrap(), 1024);
        assert_eq!(api::parse_size("2048").unwrap(), 2048);
        assert_eq!(api::parse_size("999999").unwrap(), 999999);
    }

    #[test]
    fn is_known_vo_locale_identifies_known_and_unknown() {
        assert!(is_known_vo_locale("en-us"));
        assert!(is_known_vo_locale("ja-jp"));
        assert!(is_known_vo_locale("zh-cn"));
        assert!(!is_known_vo_locale("game"));
        assert!(!is_known_vo_locale("cutscenes"));
        assert!(!is_known_vo_locale(""));
    }
    #[test]
    fn vo_lang_matches_cases() {
        // CN matches zh-cn
        assert!(vo_lang_matches("zh-cn", "cn"));
        assert!(vo_lang_matches("zh-tw", "cn"));
        // EN matches en-us
        assert!(vo_lang_matches("en-us", "en"));
        // JP matches ja-jp
        assert!(vo_lang_matches("ja-jp", "jp"));
        // KR matches ko-kr
        assert!(vo_lang_matches("ko-kr", "kr"));
        // Wrong language doesn't match
        assert!(!vo_lang_matches("en-us", "jp"));
        assert!(!vo_lang_matches("ja-jp", "en"));
        // Case insensitive
        assert!(vo_lang_matches("EN-US", "en"));
        assert!(vo_lang_matches("zh-cn", "CN"));
        // Empty doesn't match
        assert!(!vo_lang_matches("", ""));
        assert!(!vo_lang_matches("game", "en"));
    }

    #[test]
    fn borrowed_diff_indexes_match_owned_results() {
        let (old, new) = make_diff_size_benchmark_manifests(20, 5, 50, 40);
        let (borrowed_files, borrowed_chunks) = borrowed_diff_indexes_collect(&old);
        let (owned_files, owned_chunks) = owned_diff_indexes(&old);
        assert_eq!(
            calculate_diff_sizes_indexed(&borrowed_files, &borrowed_chunks, &new),
            calculate_diff_sizes_indexed(&owned_files, &owned_chunks, &new)
        );
        let (no_old, _) = make_diff_size_benchmark_manifests(20, 5, 100, 0);
        let empty_files = rustc_hash::FxHashMap::<&str, &str>::default();
        let empty_chunks = rustc_hash::FxHashSet::<&str>::default();
        assert_eq!(
            calculate_diff_sizes_indexed(&empty_files, &empty_chunks, &no_old),
            (20 * 5 * 256 * 1024, 20 * 5 * 1024 * 1024)
        );

        let duplicate_asset = old.assets[0].clone();
        let duplicate_old = crate::proto_parse::SophonManifestProto {
            assets: vec![duplicate_asset.clone(), duplicate_asset],
        };
        let duplicate_new = crate::proto_parse::SophonManifestProto {
            assets: vec![new.assets[0].clone()],
        };
        let (borrowed_files, borrowed_chunks) = borrowed_diff_indexes_collect(&duplicate_old);
        let (owned_files, owned_chunks) = owned_diff_indexes(&duplicate_old);
        assert_eq!(
            calculate_diff_sizes_indexed(&borrowed_files, &borrowed_chunks, &duplicate_new),
            calculate_diff_sizes_indexed(&owned_files, &owned_chunks, &duplicate_new)
        );
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "measures update diff-size planning time"]
    fn benchmark_diff_size_indexes() {
        use std::time::Instant;

        for (changed_percent, reused_hash_percent) in [(10usize, 0usize), (50, 50), (100, 90)] {
            let (old, new) =
                make_diff_size_benchmark_manifests(5_000, 50, changed_percent, reused_hash_percent);
            let samples_per_policy = 30;
            let mut owned_elapsed = std::time::Duration::ZERO;
            let mut borrowed_elapsed = std::time::Duration::ZERO;
            let mut owned_result = None;
            let mut borrowed_result = None;
            for round in 0..samples_per_policy * 2 {
                for variant in (0..2).cycle().skip(round % 2).take(2) {
                    let started = Instant::now();
                    let result = match variant {
                        0 => {
                            let (files, chunks) = owned_diff_indexes(&old);
                            std::hint::black_box(calculate_diff_sizes_indexed(
                                &files, &chunks, &new,
                            ))
                        }
                        1 => {
                            let (files, chunks) = borrowed_diff_indexes_collect(&old);
                            std::hint::black_box(calculate_diff_sizes_indexed(
                                &files, &chunks, &new,
                            ))
                        }
                        _ => unreachable!(),
                    };
                    let elapsed = started.elapsed();
                    match variant {
                        0 => {
                            owned_elapsed += elapsed;
                            if let Some(expected) = owned_result {
                                assert_eq!(result, expected);
                            } else {
                                owned_result = Some(result);
                            }
                        }
                        1 => {
                            borrowed_elapsed += elapsed;
                            if let Some(expected) = borrowed_result {
                                assert_eq!(result, expected);
                            } else {
                                borrowed_result = Some(result);
                            }
                        }
                        _ => unreachable!(),
                    }
                }
            }
            assert_eq!(owned_result, borrowed_result);
            println!(
                "diff_size_indexes files=5000 chunks_per_file=50 changed_percent={changed_percent} reused_hash_percent={reused_hash_percent} samples_per_policy={samples_per_policy} owned_us={} borrowed_us={} result={:?}",
                owned_elapsed.as_micros(),
                borrowed_elapsed.as_micros(),
                owned_result.unwrap(),
            );
        }
    }

    #[test]
    fn update_info_serde_roundtrip_basic() {
        let info = UpdateInfo {
            update_available: true,
            preinstall_available: false,
            preinstall_downloaded: false,
            current_tag: Some("1.0.0".into()),
            remote_tag: "2.0.0".into(),
            preinstall_tag: None,
            update_compressed_size: 1_000_000,
            update_decompressed_size: 5_000_000,
            preinstall_compressed_size: 0,
            preinstall_decompressed_size: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: UpdateInfo = serde_json::from_str(&json).unwrap();
        assert!(decoded.update_available);
        assert!(!decoded.preinstall_available);
        assert_eq!(decoded.current_tag, Some("1.0.0".to_string()));
        assert_eq!(decoded.remote_tag, "2.0.0");
        assert_eq!(decoded.update_compressed_size, 1_000_000);
    }

    #[test]
    fn update_info_serde_roundtrip_all_fields() {
        let info = UpdateInfo {
            update_available: false,
            preinstall_available: true,
            preinstall_downloaded: true,
            current_tag: None,
            remote_tag: "3.0.0-pre".into(),
            preinstall_tag: Some("3.0.0-pre".into()),
            update_compressed_size: u64::MAX,
            update_decompressed_size: u64::MAX,
            preinstall_compressed_size: 0,
            preinstall_decompressed_size: 0,
        };
        let json = serde_json::to_string(&info).unwrap();
        let decoded: UpdateInfo = serde_json::from_str(&json).unwrap();
        assert!(decoded.preinstall_available);
        assert_eq!(decoded.current_tag, None);
        assert_eq!(decoded.preinstall_tag, Some("3.0.0-pre".to_string()));
        assert_eq!(decoded.update_compressed_size, u64::MAX);
    }

    #[test]
    fn update_info_deserialize_missing_field_fails() {
        let json = r#"{"update_available":true}"#;
        let result: Result<UpdateInfo, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }
    #[test]
    fn update_info_no_update_when_tags_match() {
        let info = UpdateInfo {
            update_available: false,
            preinstall_available: false,
            preinstall_downloaded: false,
            current_tag: Some("1.0.0".into()),
            remote_tag: "1.0.0".into(),
            preinstall_tag: None,
            update_compressed_size: 0,
            update_decompressed_size: 0,
            preinstall_compressed_size: 0,
            preinstall_decompressed_size: 0,
        };
        assert!(!info.update_available);
        assert_eq!(info.current_tag.as_deref(), Some("1.0.0"));
        assert_eq!(info.current_tag.as_deref(), Some(info.remote_tag.as_str()));
    }
}
