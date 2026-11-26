use std::path::Path;

use futures_util::future::try_join_all;
use reqwest::Client;

use super::api::{
    fetch_build, fetch_front_door, fetch_patch_build, fetch_patch_manifest, is_known_vo_locale,
    vo_lang_matches,
};
use super::error::{SophonError, SophonResult};
use super::preinstall_state::{PreinstallState, cached_preinstall};
use super::read_installed_tag;
use crate::api_scrape::PackageBranch;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
/// Availability and best-effort sizes for a preinstall package.
pub struct PreinstallInfo {
    pub preinstall_available: bool,
    pub preinstall_downloaded: bool,
    pub preinstall_tag: Option<String>,
    pub preinstall_compressed_size: u64,
    pub preinstall_decompressed_size: u64,
}

/// Query remote preinstall metadata and the local completed-package cache.
pub async fn check_preinstall(
    client: &Client,
    game_id: &str,
    vo_lang: &str,
    game_dir: &Path,
) -> SophonResult<PreinstallInfo> {
    let installed_tag = read_installed_tag(game_dir);
    let branches = fetch_front_door(client, game_id).await?;
    let (main_branch, pre_download_branch) = branches.take_branches();
    let main_branch = main_branch.ok_or(SophonError::NoGameManifest)?;

    if let Some(state) = cached_for_branches(
        game_dir,
        game_id,
        vo_lang,
        installed_tag.as_deref(),
        &main_branch,
        pre_download_branch.as_ref(),
    ) {
        let (compressed, decompressed) = state.cached_sizes();
        return Ok(PreinstallInfo {
            preinstall_available: true,
            preinstall_downloaded: true,
            preinstall_tag: Some(state.tag),
            preinstall_compressed_size: compressed,
            preinstall_decompressed_size: decompressed,
        });
    }

    let Some(pre_branch) = pre_download_branch else {
        return Ok(no_preinstall());
    };
    let preinstall_tag = pre_branch.tag.clone();
    let (preinstall_compressed_size, preinstall_decompressed_size) =
        match query_preinstall_sizes(client, vo_lang, &pre_branch).await {
            Ok(sizes) => sizes,
            Err(err) => {
                log::warn!("Failed to estimate preinstall sizes: {err}");
                (0, 0)
            }
        };

    Ok(PreinstallInfo {
        preinstall_available: true,
        preinstall_downloaded: false,
        preinstall_tag: Some(preinstall_tag),
        preinstall_compressed_size,
        preinstall_decompressed_size,
    })
}

async fn query_preinstall_sizes(
    client: &Client,
    vo_lang: &str,
    pre_branch: &PackageBranch,
) -> SophonResult<(u64, u64)> {
    let patch_build = fetch_patch_build(client, pre_branch).await?;
    if patch_build.tag != pre_branch.tag {
        return Err(SophonError::PreinstallStateInvalid(
            "Preinstall branch and patch target tags disagree".into(),
        ));
    }
    let target_build = fetch_build(client, pre_branch, Some(&patch_build.tag)).await?;
    if target_build.tag != patch_build.tag {
        return Err(SophonError::PreinstallStateInvalid(
            "Preinstall patch and normal build target tags disagree".into(),
        ));
    }

    let patch_manifests: Vec<_> = patch_build
        .manifests
        .iter()
        .filter(|meta| field_is_selected(&meta.matching_field, vo_lang))
        .cloned()
        .collect();
    if patch_manifests.is_empty() {
        return Err(SophonError::NoGameManifest);
    }
    let main_fields: rustc_hash::FxHashSet<&str> = target_build
        .manifests
        .iter()
        .filter(|meta| field_is_selected(&meta.matching_field, vo_lang))
        .map(|meta| meta.matching_field.as_str())
        .collect();
    if !main_fields.contains("game") {
        return Err(SophonError::NoGameManifest);
    }

    let patch_results = try_join_all(
        patch_manifests
            .iter()
            .map(|meta| fetch_patch_manifest(client, meta)),
    )
    .await?;

    let mut estimate = aggregate_patch_sizes(&patch_results, &patch_build.tag, &main_fields)?;

    let selected_target_fields: Vec<_> = target_build
        .manifests
        .iter()
        .filter(|meta| field_is_selected(&meta.matching_field, vo_lang))
        .cloned()
        .collect();
    let target_manifests = try_join_all(selected_target_fields.iter().map(|meta| {
        super::api::fetch_manifest_proto(client, &meta.manifest_download, &meta.manifest.id)
    }))
    .await?;
    for (meta, manifest) in selected_target_fields.iter().zip(target_manifests) {
        for asset in manifest.assets.iter().filter(|asset| !asset.is_directory()) {
            let key = (
                meta.matching_field.clone(),
                asset.asset_name_str().to_owned(),
            );
            if !estimate.covered_targets.contains(&key) {
                estimate.decompressed = estimate.decompressed.saturating_add(asset.asset_size);
            }
        }
    }

    Ok((estimate.compressed, estimate.decompressed))
}

fn no_preinstall() -> PreinstallInfo {
    PreinstallInfo {
        preinstall_available: false,
        preinstall_downloaded: false,
        preinstall_tag: None,
        preinstall_compressed_size: 0,
        preinstall_decompressed_size: 0,
    }
}

fn cached_for_branches(
    game_dir: &Path,
    game_id: &str,
    vo_lang: &str,
    installed_tag: Option<&str>,
    main_branch: &PackageBranch,
    pre_branch: Option<&PackageBranch>,
) -> Option<PreinstallState> {
    let update_available = installed_tag.is_some_and(|tag| tag != main_branch.tag);
    update_available
        .then(|| cached_preinstall(game_dir, &main_branch.tag, game_id, vo_lang, installed_tag))
        .flatten()
        .or_else(|| {
            pre_branch.and_then(|branch| {
                cached_preinstall(game_dir, &branch.tag, game_id, vo_lang, installed_tag)
            })
        })
}

fn field_is_selected(matching_field: &str, vo_lang: &str) -> bool {
    matching_field == "game"
        || vo_lang_matches(matching_field, vo_lang)
        || !is_known_vo_locale(matching_field)
}

struct PatchSizeEstimate {
    compressed: u64,
    decompressed: u64,
    covered_targets: rustc_hash::FxHashSet<(String, String)>,
}

fn aggregate_patch_sizes(
    patch_results: &[super::api::PatchManifestWithMeta],
    target_tag: &str,
    main_fields: &rustc_hash::FxHashSet<&str>,
) -> SophonResult<PatchSizeEstimate> {
    let mut seen_chunks = rustc_hash::FxHashSet::default();
    let mut seen_targets = rustc_hash::FxHashSet::default();
    let mut compressed = 0u64;
    let mut decompressed = 0u64;

    for result in patch_results {
        for asset in &result.patch_manifest.patch_assets {
            let info = asset
                .asset_infos
                .iter()
                .find(|info| info.version_tag.eq_ignore_ascii_case(target_tag));
            let Some(info) = info else {
                continue;
            };
            if asset.asset_size < 0 {
                return Err(SophonError::PreinstallStateInvalid(format!(
                    "Negative target asset size for {}",
                    asset.asset_name
                )));
            }
            if !main_fields.contains(result.matching_field.as_str()) {
                continue;
            }
            seen_targets.insert((result.matching_field.clone(), asset.asset_name.clone()));
            decompressed = decompressed.saturating_add(asset.asset_size as u64);
            if let Some(chunk) = &info.chunk {
                if chunk.patch_size < 0 {
                    return Err(SophonError::PreinstallStateInvalid(format!(
                        "Negative patch size for {}",
                        asset.asset_name
                    )));
                }
                if seen_chunks.insert(chunk.patch_name.clone()) {
                    compressed = compressed.saturating_add(chunk.patch_size as u64);
                }
            }
        }
    }

    Ok(PatchSizeEstimate {
        compressed,
        decompressed,
        covered_targets: seen_targets,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "preinstall")]
    use crate::api_scrape::{Compression, DownloadInfo};
    use crate::proto_parse::{
        SophonPatchAssetChunk, SophonPatchAssetInfo, SophonPatchAssetProperty, SophonPatchProto,
    };

    fn patch_result(
        matching_field: &str,
        asset_name: &str,
        asset_size: i64,
        patch_name: Option<&str>,
        patch_size: i64,
    ) -> super::super::api::PatchManifestWithMeta {
        super::super::api::PatchManifestWithMeta {
            patch_manifest: SophonPatchProto {
                patch_assets: vec![SophonPatchAssetProperty {
                    asset_name: asset_name.to_owned(),
                    asset_size,
                    asset_hash_md5: "d41d8cd98f00b204e9800998ecf8427e".to_owned(),
                    asset_infos: vec![SophonPatchAssetInfo {
                        version_tag: "2.0".to_owned(),
                        chunk: patch_name.map(|name| SophonPatchAssetChunk {
                            patch_name: name.to_owned(),
                            version_tag: "2.0".to_owned(),
                            build_id: "build".to_owned(),
                            patch_size,
                            patch_md5: "d41d8cd98f00b204e9800998ecf8427e".to_owned(),
                            patch_offset: 0,
                            patch_length: patch_size,
                            original_file_name: String::new(),
                            original_file_length: 0,
                            original_file_md5: String::new(),
                        }),
                    }],
                }],
                unused_assets: Vec::new(),
            },
            #[cfg(feature = "preinstall")]
            diff_download: DownloadInfo {
                encryption: 0,
                password: String::new(),
                compression: Compression::None,
                url_prefix: String::new(),
                url_suffix: String::new(),
            },
            matching_field: matching_field.to_owned(),
        }
    }

    #[test]
    fn selects_game_vo_and_non_vo_fields() {
        assert!(field_is_selected("game", "en-us"));
        assert!(field_is_selected("en-us", "en-us"));
        assert!(field_is_selected("cutscenes", "en-us"));
        assert!(!field_is_selected("ja-jp", "en-us"));
    }

    #[test]
    fn aggregate_patch_sizes_deduplicates_chunks_and_ignores_unselected_fields() {
        let patch_results = vec![
            patch_result("game", "a.bin", 100, Some("shared"), 50),
            patch_result("en-us", "voice.bin", 200, Some("shared"), 50),
            patch_result("ja-jp", "japanese.bin", 300, Some("ja"), 30),
        ];
        let main_fields = ["game", "en-us"].into_iter().collect();

        let estimate = aggregate_patch_sizes(&patch_results, "2.0", &main_fields).unwrap();

        assert_eq!(estimate.compressed, 50);
        assert_eq!(estimate.decompressed, 300);
        assert_eq!(estimate.covered_targets.len(), 2);
    }

    #[test]
    fn aggregate_patch_sizes_counts_download_over_targets() {
        let patch_results = [patch_result("game", "new.bin", 75, None, 0)];
        let main_fields = ["game"].into_iter().collect();

        let estimate = aggregate_patch_sizes(&patch_results, "2.0", &main_fields).unwrap();

        assert_eq!(estimate.compressed, 0);
        assert_eq!(estimate.decompressed, 75);
        assert!(
            estimate
                .covered_targets
                .contains(&("game".to_owned(), "new.bin".to_owned()))
        );
    }

    #[test]
    fn aggregate_patch_sizes_rejects_negative_payload_sizes() {
        let patch_results = [patch_result("game", "bad.bin", 75, Some("bad"), -1)];
        let main_fields = ["game"].into_iter().collect();

        assert!(aggregate_patch_sizes(&patch_results, "2.0", &main_fields).is_err());
    }
}
