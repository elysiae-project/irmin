use std::hash::{Hash, Hasher};
use std::path::Path;

use reqwest::Client;
use rustc_hash::FxHasher;
use serde::{Deserialize, Serialize};

use super::api::{fetch_build, is_known_vo_locale, vo_lang_matches};
use super::error::{SophonError, SophonResult};
use super::read_installed_tag;

use crate::api_scrape::PackageBranch;
use crate::api_scrape::SophonManifestMeta;

struct CompactChunkNameSet {
    bytes: Vec<u8>,
    entries: Vec<CompactChunkNameEntry>,
    table: hashbrown::HashTable<usize>,
}

struct CompactChunkNameEntry {
    hash: u64,
    range: std::ops::Range<usize>,
}

impl CompactChunkNameSet {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            entries: Vec::new(),
            table: hashbrown::HashTable::new(),
        }
    }

    fn insert(&mut self, name: &str) -> std::io::Result<bool> {
        let mut hasher = FxHasher::default();
        name.hash(&mut hasher);
        self.insert_hashed(name, hasher.finish())
    }

    #[cfg(test)]
    fn insert_with_hash(&mut self, name: &str, hash: u64) -> std::io::Result<bool> {
        self.insert_hashed(name, hash)
    }

    fn insert_hashed(&mut self, name: &str, hash: u64) -> std::io::Result<bool> {
        if self
            .table
            .find(hash, |&index| {
                let entry = &self.entries[index];
                self.bytes[entry.range.clone()] == *name.as_bytes()
            })
            .is_some()
        {
            return Ok(false);
        }

        self.table
            .try_reserve(1, |&index| self.entries[index].hash)
            .map_err(|error| {
                std::io::Error::other(format!(
                    "failed to reserve compact chunk-name hash table: {error}"
                ))
            })?;
        self.bytes.try_reserve(name.len()).map_err(|error| {
            std::io::Error::other(format!(
                "failed to reserve compact chunk-name arena: {error}"
            ))
        })?;
        self.entries.try_reserve(1).map_err(|error| {
            std::io::Error::other(format!(
                "failed to reserve compact chunk-name index: {error}"
            ))
        })?;
        let start = self.bytes.len();
        let end = start
            .checked_add(name.len())
            .ok_or_else(|| std::io::Error::other("chunk-name arena length overflow"))?;
        self.bytes.extend_from_slice(name.as_bytes());
        self.entries.push(CompactChunkNameEntry {
            hash,
            range: start..end,
        });
        let index = self.entries.len() - 1;
        // Rehashing uses stored hashes, while `find` resolves collisions by comparing bytes.
        self.table
            .insert_unique(hash, index, |&entry_index| self.entries[entry_index].hash);
        Ok(true)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

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
    let mut seen_chunks = CompactChunkNameSet::new();

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
            .map(|file| (file.asset_name_str(), file.asset_hash_md5_str()))
            .collect();
        let old_chunks: rustc_hash::FxHashSet<&str> = old_manifest
            .as_ref()
            .into_iter()
            .flat_map(|manifest| manifest.assets.iter())
            .filter(|file| !file.is_directory())
            .flat_map(|file| file.asset_chunks.iter())
            .map(|chunk| chunk.chunk_decompressed_hash_md5_str())
            .collect();

        accumulate_diff_manifest_sizes(
            &old_files_md5,
            &old_chunks,
            &new_manifest,
            &mut seen_chunks,
            &mut cs,
            &mut ds,
        )?;
    }

    Ok((cs, ds))
}

fn accumulate_diff_manifest_sizes(
    old_files_md5: &rustc_hash::FxHashMap<&str, &str>,
    old_chunks: &rustc_hash::FxHashSet<&str>,
    new_manifest: &crate::proto_parse::SophonManifestProto,
    seen_chunks: &mut CompactChunkNameSet,
    compressed: &mut u64,
    decompressed: &mut u64,
) -> SophonResult<()> {
    for file in &new_manifest.assets {
        if file.is_directory() {
            continue;
        }
        let needs_download = match old_files_md5.get(file.asset_name_str()) {
            Some(old_md5) => *old_md5 != file.asset_hash_md5_str(),
            None => true,
        };
        if needs_download {
            for chunk in &file.asset_chunks {
                if old_chunks.contains(chunk.chunk_decompressed_hash_md5_str()) {
                    continue;
                }
                if seen_chunks
                    .insert(chunk.chunk_name_str())
                    .map_err(SophonError::Io)?
                {
                    *compressed += chunk.chunk_size;
                    *decompressed += chunk.chunk_size_decompressed;
                }
            }
        }
    }
    Ok(())
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
        let needs_download = match old_files_md5.get(file.asset_name_str()) {
            Some(old_md5) => old_md5.as_ref() != file.asset_hash_md5_str(),
            None => true,
        };
        if needs_download {
            for chunk in &file.asset_chunks {
                if !old_chunks.contains(chunk.chunk_decompressed_hash_md5_str())
                    && seen_chunks.insert(chunk.chunk_name_str().to_string())
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
        .map(|file| (file.asset_name_str(), file.asset_hash_md5_str()))
        .collect();
    let old_chunks = old_manifest
        .assets
        .iter()
        .filter(|file| !file.is_directory())
        .flat_map(|file| file.asset_chunks.iter())
        .map(|chunk| chunk.chunk_decompressed_hash_md5_str())
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
        .map(|file| {
            (
                file.asset_name_str().to_string(),
                file.asset_hash_md5_str().to_string(),
            )
        })
        .collect::<rustc_hash::FxHashMap<String, String>>();
    let old_chunks = old_manifest
        .assets
        .iter()
        .filter(|file| !file.is_directory())
        .flat_map(|file| file.asset_chunks.iter())
        .map(|chunk| chunk.chunk_decompressed_hash_md5_str().to_string())
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
                    chunk_name: format!("chunk-{:06}-{chunk_idx:04}", file_idx % 100).into(),
                    chunk_decompressed_hash_md5: hash.into(),
                    chunk_on_file_offset: (chunk_idx * 1024 * 1024) as u64,
                    chunk_size: 256 * 1024,
                    chunk_size_decompressed: 1024 * 1024,
                    chunk_compressed_hash_xxh: 0,
                    chunk_compressed_hash_md5: bytes::Bytes::new(),
                    chunk_old_offset: -1,
                }
            })
            .collect();
        SophonManifestAssetProperty {
            asset_name: format!("asset-{file_idx:06}.pak").into(),
            asset_chunks,
            asset_type: 0,
            asset_size: (chunks_per_file * 1024 * 1024) as u64,
            asset_hash_md5: if changed {
                format!("new-file-hash-{file_idx:06}").into()
            } else {
                format!("old-file-hash-{file_idx:06}").into()
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

    #[test]
    fn compact_chunk_name_set_is_exact_under_hash_collisions() {
        let mut names = CompactChunkNameSet::new();
        assert!(names.insert_with_hash("", 7).unwrap());
        assert!(names.insert_with_hash("alpha", 7).unwrap());
        assert!(names.insert_with_hash("βeta", 7).unwrap());
        assert!(!names.insert_with_hash("alpha", 7).unwrap());
        assert!(!names.insert_with_hash("", 7).unwrap());
        for index in 0..128 {
            assert!(
                names
                    .insert_with_hash(&format!("collision-{index}"), 7)
                    .unwrap()
            );
        }
        for index in 0..128 {
            assert!(
                !names
                    .insert_with_hash(&format!("collision-{index}"), 7)
                    .unwrap()
            );
        }
        assert_eq!(names.len(), 131);
    }

    #[test]
    fn compact_chunk_name_set_matches_owned_set_across_growth() {
        let mut compact = CompactChunkNameSet::new();
        let mut owned = rustc_hash::FxHashSet::default();
        for index in 0..10_000usize {
            let name = match index % 5 {
                0 => format!("chunk-{index:06}"),
                1 => format!("shared-{:04}", index % 97),
                2 => String::new(),
                3 => format!("音声/{:04}", index % 211),
                _ => format!("GameData/Persistent/{:08x}.chunk", index / 3),
            };
            assert_eq!(
                compact.insert(&name).unwrap(),
                owned.insert(name),
                "insert result differs at {index}"
            );
        }
        assert_eq!(compact.len(), owned.len());
    }

    #[test]
    fn diff_size_accumulation_deduplicates_names_across_manifests() {
        use crate::proto_parse::{SophonManifestAssetChunk, SophonManifestAssetProperty};

        let chunk = |name: &str, hash: &str, size: u64| SophonManifestAssetChunk {
            chunk_name: name.to_string().into(),
            chunk_decompressed_hash_md5: hash.to_owned().into(),
            chunk_on_file_offset: 0,
            chunk_size: size,
            chunk_size_decompressed: size * 2,
            chunk_compressed_hash_xxh: 0,
            chunk_compressed_hash_md5: bytes::Bytes::new(),
            chunk_old_offset: -1,
        };
        let file = |name: &str, hash: &str, chunks| SophonManifestAssetProperty {
            asset_name: name.to_string().into(),
            asset_chunks: chunks,
            asset_type: 0,
            asset_size: 0,
            asset_hash_md5: hash.to_owned().into(),
        };
        let first = crate::proto_parse::SophonManifestProto {
            assets: vec![file(
                "changed-a",
                "new-a",
                vec![
                    chunk("shared-name", "new-content-a", 10),
                    chunk("new-name", "new-content-b", 20),
                ],
            )],
        };
        let second = crate::proto_parse::SophonManifestProto {
            assets: vec![
                file(
                    "changed-b",
                    "new-b",
                    vec![chunk("shared-name", "new-content-c", 99)],
                ),
                file(
                    "unchanged",
                    "old-unchanged",
                    vec![chunk("ignored", "new-content-d", 77)],
                ),
                file(
                    "changed-c",
                    "new-c",
                    vec![chunk("reused", "old-content", 88)],
                ),
            ],
        };
        let mut old_files = rustc_hash::FxHashMap::default();
        old_files.insert("unchanged", "old-unchanged");
        old_files.insert("changed-a", "old-a");
        let mut old_chunks = rustc_hash::FxHashSet::default();
        old_chunks.insert("old-content");
        let mut seen = CompactChunkNameSet::new();
        let mut compressed = 0;
        let mut decompressed = 0;
        accumulate_diff_manifest_sizes(
            &old_files,
            &old_chunks,
            &first,
            &mut seen,
            &mut compressed,
            &mut decompressed,
        )
        .unwrap();
        accumulate_diff_manifest_sizes(
            &old_files,
            &old_chunks,
            &second,
            &mut seen,
            &mut compressed,
            &mut decompressed,
        )
        .unwrap();
        assert_eq!((compressed, decompressed), (30, 60));
        assert_eq!(seen.len(), 2);
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

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "compares owned and compact cross-manifest name dedup in update planning"]
    fn benchmark_diff_size_seen_chunk_set() {
        use std::time::{Duration, Instant};

        fn calculate_owned(
            old_files: &rustc_hash::FxHashMap<&str, &str>,
            old_chunks: &rustc_hash::FxHashSet<&str>,
            manifests: &[crate::proto_parse::SophonManifestProto],
        ) -> ((u64, u64), usize) {
            let mut owned = rustc_hash::FxHashSet::<bytes::Bytes>::default();
            let mut compressed = 0u64;
            let mut decompressed = 0u64;
            for new in manifests {
                for file in &new.assets {
                    if file.is_directory() {
                        continue;
                    }
                    let changed = match old_files.get(file.asset_name_str()) {
                        Some(old_hash) => *old_hash != file.asset_hash_md5_str(),
                        None => true,
                    };
                    if changed {
                        for chunk in &file.asset_chunks {
                            if !old_chunks.contains(chunk.chunk_decompressed_hash_md5_str())
                                && owned.insert((*chunk.chunk_name()).clone())
                            {
                                compressed += chunk.chunk_size;
                                decompressed += chunk.chunk_size_decompressed;
                            }
                        }
                    }
                }
            }
            ((compressed, decompressed), owned.len())
        }

        fn calculate_compact(
            old_files: &rustc_hash::FxHashMap<&str, &str>,
            old_chunks: &rustc_hash::FxHashSet<&str>,
            manifests: &[crate::proto_parse::SophonManifestProto],
        ) -> ((u64, u64), usize) {
            let mut seen = CompactChunkNameSet::new();
            let mut compressed = 0u64;
            let mut decompressed = 0u64;
            for new in manifests {
                accumulate_diff_manifest_sizes(
                    old_files,
                    old_chunks,
                    new,
                    &mut seen,
                    &mut compressed,
                    &mut decompressed,
                )
                .unwrap();
            }
            ((compressed, decompressed), seen.len())
        }

        for (changed_percent, reused_hash_percent) in [(10usize, 0usize), (50, 50), (100, 90)] {
            let (old, new) =
                make_diff_size_benchmark_manifests(5_000, 50, changed_percent, reused_hash_percent);
            let (old_files, old_chunks) = borrowed_diff_indexes_collect(&old);
            let mut first_assets = Vec::new();
            let mut second_assets = Vec::new();
            for (index, asset) in new.assets.into_iter().enumerate() {
                if index % 2 == 0 {
                    first_assets.push(asset);
                } else {
                    second_assets.push(asset);
                }
            }
            let manifests = [
                crate::proto_parse::SophonManifestProto {
                    assets: first_assets,
                },
                crate::proto_parse::SophonManifestProto {
                    assets: second_assets,
                },
            ];
            let expected = calculate_owned(&old_files, &old_chunks, &manifests);
            assert_eq!(
                expected,
                calculate_compact(&old_files, &old_chunks, &manifests)
            );
            let mut elapsed = [Duration::ZERO; 2];
            let samples_per_policy = 20;
            for round in 0..samples_per_policy * 2 {
                for variant in (0..2).cycle().skip(round % 2).take(2) {
                    let started = Instant::now();
                    let result = if variant == 0 {
                        calculate_owned(&old_files, &old_chunks, &manifests)
                    } else {
                        calculate_compact(&old_files, &old_chunks, &manifests)
                    };
                    elapsed[variant] += started.elapsed();
                    assert_eq!(result, expected);
                    std::hint::black_box(result);
                }
            }
            println!(
                "diff_size_seen_set files=5000 chunks_per_file=50 changed_percent={changed_percent} reused_hash_percent={reused_hash_percent} samples_per_policy={samples_per_policy} owned_ms={:.3} compact_ms={:.3} unique_names={}",
                elapsed[0].as_secs_f64() * 1000.0,
                elapsed[1].as_secs_f64() * 1000.0,
                expected.1,
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

#[cfg(feature = "bench-internals")]
pub mod benchmark {
    use super::*;

    pub struct DiffSizeResult {
        pub compressed: u64,
        pub decompressed: u64,
    }

    pub fn run_diff_sizes(
        old_files_md5: &rustc_hash::FxHashMap<&str, &str>,
        old_chunks: &rustc_hash::FxHashSet<&str>,
        new_manifest: &crate::proto_parse::SophonManifestProto,
    ) -> DiffSizeResult {
        let mut seen_chunks = CompactChunkNameSet::new();
        let mut compressed = 0u64;
        let mut decompressed = 0u64;
        accumulate_diff_manifest_sizes(
            old_files_md5,
            old_chunks,
            new_manifest,
            &mut seen_chunks,
            &mut compressed,
            &mut decompressed,
        )
        .expect("diff sizes calculation failed");
        DiffSizeResult {
            compressed,
            decompressed,
        }
    }
}
