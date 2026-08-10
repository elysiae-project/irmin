use libc;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead as _, BufWriter, Read as _, Seek as _, SeekFrom, Write as _};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

static EPOCH: LazyLock<Instant> = LazyLock::new(Instant::now);

#[inline]
fn now_nanos() -> u64 {
    EPOCH.elapsed().as_nanos() as u64
}

use dashmap::DashMap;

use futures_util::StreamExt;
use futures_util::future::try_join_all;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::api::{
    fetch_build, fetch_front_door, fetch_manifest, fetch_patch_build, fetch_patch_manifest,
    is_known_vo_locale, vo_lang_matches,
};
use super::assembly_opt::{md5_hex_eq, md5_to_hex, posix_advise};
use super::error::{SophonError, SophonResult};
use super::handle::DownloadHandle;
use super::read_installed_tag;
use super::{MAX_RETRIES, cancelable_sleep, retry_delay};
use crate::api_scrape::{
    DownloadInfo, SophonBuildData, SophonManifestMeta, SophonPatchManifestMeta,
};
use crate::proto_parse::{
    SophonManifestAssetChunk, SophonManifestAssetProperty, SophonManifestProto,
    SophonPatchAssetChunk, SophonPatchAssetProperty,
};

const HDIFF_MAGIC: &[u8; 5] = b"HDIFF";
const BLANK_FILE_MD5: &str = "d41d8cd98f00b204e9800998ecf8427e";
const PREINSTALL_STATE_FILE_EXT: &str = ".json";
const HK4E_DATA_DIR_GLOBAL: &str =
    "\x47\x65\x6e\x73\x68\x69\x6e\x49\x6d\x70\x61\x63\x74\x5f\x44\x61\x74\x61";
const HK4E_DATA_DIR_CN: &str = "\x59\x75\x61\x6e\x53\x68\x65\x6e\x5f\x44\x61\x74\x61";
const HKRPG_DATA_DIR: &str = "\x53\x74\x61\x72\x52\x61\x69\x6c\x5f\x44\x61\x74\x61";
const NAP_GAME_DATA_DIR: &str =
    "\x5a\x65\x6e\x6c\x65\x73\x73\x5a\x6f\x6e\x65\x5a\x65\x72\x6f\x5f\x44\x61\x74\x61";
const NAP_GAME_NAME: &str = "\x5a\x65\x6e\x6c\x65\x73\x73\x5a\x6f\x6e\x65\x5a\x65\x72\x6f";
/// RAII guard that removes a blank diff_ref file on drop.
struct DiffRefGuard(Option<PathBuf>);

impl Drop for DiffRefGuard {
    fn drop(&mut self) {
        if let Some(ref path) = self.0 {
            let _ = fs::remove_file(path);
        }
    }
}

impl DiffRefGuard {
    fn new(path: PathBuf) -> Self {
        Self(Some(path))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchMethod {
    CopyOver,
    Patch,
    DownloadOver,
    Remove,
    Skip,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchAssetInfo {
    pub target_file_path: String,
    pub target_file_size: u64,
    pub target_file_hash: String,
    pub patch_method: PatchMethod,
    pub patch_name: String,
    pub patch_hash: String,
    pub patch_offset: u64,
    pub patch_size: u64,
    pub patch_chunk_length: u64,
    pub original_file_path: Option<String>,
    pub original_file_hash: Option<String>,
    pub original_file_size: Option<u64>,
    pub matching_field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchChunkInfo {
    pub patch_name: String,
    pub patch_size: u64,
    pub patch_md5: String,
    pub matching_field: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PreinstallState {
    pub tag: String,
    pub game_id: String,
    pub vo_lang: String,
    pub installed_tag: String,
    pub patch_assets: Vec<PatchAssetInfo>,
    pub deleted_files: Vec<String>,
    pub downloaded_chunks: rustc_hash::FxHashSet<String>,
    pub diff_downloads: rustc_hash::FxHashMap<String, Arc<DownloadInfo>>,
    pub main_chunk_download: DownloadInfo,
    pub main_manifest_ids: Vec<(String, String)>,
}

impl PreinstallState {
    pub fn state_file_path(game_dir: &Path, tag: &str) -> PathBuf {
        game_dir.join(format!(
            ".sophon_preinstall_{tag}{PREINSTALL_STATE_FILE_EXT}"
        ))
    }

    pub fn marker_file_path(game_dir: &Path, tag: &str) -> PathBuf {
        game_dir.join(format!(".sophon_preinstall_{tag}"))
    }
}

pub fn save_preinstall_state(game_dir: &Path, state: &PreinstallState) -> SophonResult<()> {
    let path = PreinstallState::state_file_path(game_dir, &state.tag);
    let tmp_path = path.with_extension("json.tmp");
    {
        let f = fs::File::create(&tmp_path)?;
        let mut writer = BufWriter::with_capacity(super::FILE_WRITE_BUFFER_SIZE, f);
        serde_json::to_writer(&mut writer, state)
            .map_err(|err| SophonError::PreinstallStateInvalid(err.to_string()))?;
        writer.flush()?;
        let inner = writer
            .into_inner()
            .map_err(|err| SophonError::Io(err.into_error()))?;
        inner.sync_data()?;
    }
    fs::rename(&tmp_path, &path)?;
    Ok(())
}

pub fn load_preinstall_state(game_dir: &Path, tag: &str) -> SophonResult<PreinstallState> {
    let path = PreinstallState::state_file_path(game_dir, tag);
    let content = fs::read_to_string(&path).map_err(|err| {
        SophonError::PreinstallStateInvalid(format!("Failed to read preinstall state: {err}"))
    })?;
    serde_json::from_str(&content).map_err(|err| {
        SophonError::PreinstallStateInvalid(format!("Failed to parse preinstall state: {err}"))
    })
}

pub fn delete_preinstall_state(game_dir: &Path, tag: &str) {
    let _ = fs::remove_file(PreinstallState::state_file_path(game_dir, tag));
    let _ = fs::remove_file(PreinstallState::marker_file_path(game_dir, tag));
}

pub struct PreinstallPlan {
    pub patch_assets: Vec<PatchAssetInfo>,
    pub deleted_files: Vec<String>,
    pub unique_chunks: Vec<PatchChunkInfo>,
    pub diff_downloads: rustc_hash::FxHashMap<String, Arc<DownloadInfo>>,
    pub main_chunk_download: DownloadInfo,
    pub tag: String,
    pub main_manifest_ids: Vec<(String, String)>,
}

/// Validates that all i64 fields from the patch protobuf are non-negative.
fn validate_patch_chunk_fields(chunk: &SophonPatchAssetChunk, asset_name: &str) -> bool {
    if chunk.patch_size < 0 {
        log::warn!("Skipping chunk with negative patch_size for asset {asset_name}");
        return false;
    }
    if chunk.patch_offset < 0 {
        log::warn!("Skipping chunk with negative patch_offset for asset {asset_name}");
        return false;
    }
    if chunk.patch_length < 0 {
        log::warn!("Skipping chunk with negative patch_length for asset {asset_name}");
        return false;
    }
    if chunk.original_file_length < 0 {
        log::warn!("Skipping chunk with negative original_file_length for asset {asset_name}");
        return false;
    }
    true
}

fn validate_patch_asset_fields(asset: &SophonPatchAssetProperty) -> bool {
    if asset.asset_size < 0 {
        log::warn!(
            "Skipping asset with negative asset_size: {name}",
            name = asset.asset_name
        );
        return false;
    }
    true
}

pub async fn build_preinstall_plan(
    client: &Client,
    game_id: &str,
    vo_lang: &str,
    game_dir: &Path,
) -> SophonResult<PreinstallPlan> {
    let (_, pre_branch) = fetch_front_door(client, game_id).await?;
    let pre_branch = pre_branch.ok_or(SophonError::NoPreinstallAvailable)?;

    let installed_tag = read_installed_tag(game_dir).ok_or(SophonError::NoInstalledVersion)?;

    let patch_build = fetch_patch_build(client, &pre_branch).await?;
    let tag = patch_build.tag.clone();

    let main_build = fetch_build(client, &pre_branch, None).await?;

    let qualifying_patch: Vec<&SophonPatchManifestMeta> = patch_build
        .manifests
        .iter()
        .filter(|m| {
            m.matching_field == "game"
                || vo_lang_matches(&m.matching_field, vo_lang)
                || !is_known_vo_locale(&m.matching_field)
        })
        .collect();

    if qualifying_patch.is_empty() {
        return Err(SophonError::NoGameManifest);
    }

    let main_by_field: rustc_hash::FxHashMap<&str, &SophonManifestMeta> = main_build
        .manifests
        .iter()
        .filter(|m| {
            m.matching_field == "game"
                || vo_lang_matches(&m.matching_field, vo_lang)
                || !is_known_vo_locale(&m.matching_field)
        })
        .map(|m| (m.matching_field.as_str(), m))
        .collect();

    let mut main_manifest_ids: Vec<(String, String)> = Vec::new();
    let mut main_chunk_download: Option<DownloadInfo> = None;

    for meta in main_by_field.values() {
        main_manifest_ids.push((meta.matching_field.clone(), meta.manifest.id.clone()));
        if main_chunk_download.is_none() && meta.matching_field == "game" {
            main_chunk_download = Some(meta.chunk_download.clone());
        }
    }
    let main_chunk_download = main_chunk_download
        .or_else(|| {
            main_by_field
                .values()
                .next()
                .map(|m| m.chunk_download.clone())
        })
        .ok_or(SophonError::NoGameManifest)?;

    let mut all_patch_assets: Vec<PatchAssetInfo> = Vec::new();
    let mut all_deleted_files: Vec<String> = Vec::new();
    let mut seen_chunk_names: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let mut seen_patch_targets: rustc_hash::FxHashSet<String> = rustc_hash::FxHashSet::default();
    let mut unique_chunks: Vec<PatchChunkInfo> = Vec::new();
    let mut diff_downloads: rustc_hash::FxHashMap<String, Arc<DownloadInfo>> =
        rustc_hash::FxHashMap::default();

    let fetch_futures: Vec<_> = qualifying_patch
        .iter()
        .map(|meta| fetch_patch_manifest(client, meta))
        .collect();
    let patch_results = try_join_all(fetch_futures).await?;

    let total_patch_assets: usize = patch_results
        .iter()
        .map(|r| r.patch_manifest.patch_assets.len())
        .fold(0usize, |acc, len| acc.saturating_add(len));
    const MAX_PREINSTALL_ASSETS: usize = 1_000_000;
    if total_patch_assets > MAX_PREINSTALL_ASSETS {
        log::warn!(
            "Preinstall manifest has {total_patch_assets} assets, exceeding safe limit of {MAX_PREINSTALL_ASSETS}",
        );
    }
    all_patch_assets.reserve(total_patch_assets.min(MAX_PREINSTALL_ASSETS));
    seen_chunk_names.reserve(total_patch_assets.min(MAX_PREINSTALL_ASSETS));
    seen_patch_targets.reserve(total_patch_assets.min(MAX_PREINSTALL_ASSETS));
    unique_chunks.reserve(total_patch_assets.min(MAX_PREINSTALL_ASSETS));

    for result in patch_results {
        let patch_manifest = result.patch_manifest;
        let matching_field = result.matching_field;

        diff_downloads.insert(
            matching_field.clone(),
            Arc::new(result.diff_download.clone()),
        );

        for asset_prop in &patch_manifest.patch_assets {
            let patch_info = asset_prop
                .asset_infos
                .iter()
                .find(|info| info.version_tag.eq_ignore_ascii_case(&installed_tag));

            let has_main_entry = main_by_field.contains_key(matching_field.as_str());

            match patch_info {
                Some(info) => {
                    seen_patch_targets.insert(asset_prop.asset_name.clone());
                    let chunk = match &info.chunk {
                        Some(c) => c,
                        None => {
                            if has_main_entry {
                                if !validate_patch_asset_fields(asset_prop) {
                                    continue;
                                }
                                all_patch_assets.push(PatchAssetInfo {
                                    target_file_path: asset_prop.asset_name.clone(),
                                    target_file_size: asset_prop.asset_size as u64,
                                    target_file_hash: asset_prop.asset_hash_md5.clone(),
                                    patch_method: PatchMethod::DownloadOver,
                                    patch_name: String::new(),
                                    patch_hash: String::new(),
                                    patch_offset: 0,
                                    patch_size: 0,
                                    patch_chunk_length: 0,
                                    original_file_path: None,
                                    original_file_hash: None,
                                    original_file_size: None,
                                    matching_field: matching_field.clone(),
                                });
                            } else {
                                log::warn!(
                                    "Patch info exists but chunk is None for asset {name} (matching_field={matching_field}), and no main manifest",
                                    name = asset_prop.asset_name,
                                );
                            }
                            continue;
                        }
                    };

                    if !validate_patch_asset_fields(asset_prop) {
                        continue;
                    }
                    if !validate_patch_chunk_fields(chunk, &asset_prop.asset_name) {
                        continue;
                    }

                    let (method, original_file_path, original_file_hash, original_file_size) =
                        if chunk.original_file_name.is_empty() {
                            (PatchMethod::CopyOver, None, None, None)
                        } else {
                            (
                                PatchMethod::Patch,
                                Some(chunk.original_file_name.clone()),
                                Some(chunk.original_file_md5.clone()),
                                Some(chunk.original_file_length as u64),
                            )
                        };

                    if seen_chunk_names.insert(chunk.patch_name.clone()) {
                        unique_chunks.push(PatchChunkInfo {
                            patch_name: chunk.patch_name.clone(),
                            patch_size: chunk.patch_size as u64,
                            patch_md5: chunk.patch_md5.clone(),
                            matching_field: matching_field.clone(),
                        });
                    }

                    all_patch_assets.push(PatchAssetInfo {
                        target_file_path: asset_prop.asset_name.clone(),
                        target_file_size: asset_prop.asset_size as u64,
                        target_file_hash: asset_prop.asset_hash_md5.clone(),
                        patch_method: method,
                        patch_name: chunk.patch_name.clone(),
                        patch_hash: chunk.patch_md5.clone(),
                        patch_offset: chunk.patch_offset as u64,
                        patch_size: chunk.patch_size as u64,
                        patch_chunk_length: chunk.patch_length as u64,
                        original_file_path,
                        original_file_hash,
                        original_file_size,
                        matching_field: matching_field.clone(),
                    });
                }
                None if has_main_entry => {
                    seen_patch_targets.insert(asset_prop.asset_name.clone());
                    if !validate_patch_asset_fields(asset_prop) {
                        continue;
                    }
                    all_patch_assets.push(PatchAssetInfo {
                        target_file_path: asset_prop.asset_name.clone(),
                        target_file_size: asset_prop.asset_size as u64,
                        target_file_hash: asset_prop.asset_hash_md5.clone(),
                        patch_method: PatchMethod::DownloadOver,
                        patch_name: String::new(),
                        patch_hash: String::new(),
                        patch_offset: 0,
                        patch_size: 0,
                        patch_chunk_length: 0,
                        original_file_path: None,
                        original_file_hash: None,
                        original_file_size: None,
                        matching_field: matching_field.clone(),
                    });
                }
                None => {
                    log::warn!(
                        "No patch info for asset {name} (matching_field={matching_field}) and no main manifest, skipping",
                        name = asset_prop.asset_name,
                    );
                }
            }
        }

        for unused_prop in &patch_manifest.unused_assets {
            if !unused_prop.version_tag.eq_ignore_ascii_case(&installed_tag) {
                continue;
            }
            for info in &unused_prop.asset_infos {
                for file in &info.assets {
                    if !seen_patch_targets.contains(file.file_name.as_str()) {
                        all_deleted_files.push(file.file_name.clone());
                    }
                }
            }
        }
    }

    if diff_downloads.is_empty() {
        return Err(SophonError::NoGameManifest);
    }

    // Emit DownloadOver for main-manifest assets not already covered by the
    // patch manifest. Without this, new files in the target version are
    // silently dropped from the preinstall plan.
    // Fetch all manifests in parallel (saves 200-400ms vs sequential).
    let manifest_futures: Vec<_> = main_by_field
        .iter()
        .map(|(matching_field, meta)| {
            let field = matching_field.to_string();
            async move {
                let result = fetch_manifest(client, &meta.manifest_download, &meta.manifest.id).await?;
                Ok::<_, SophonError>((field, result))
            }
        })
        .collect();
    let manifest_results = try_join_all(manifest_futures).await?;

    for (matching_field, manifest_result) in manifest_results {
        for asset in &manifest_result.manifest.assets {
            if asset.is_directory() {
                seen_patch_targets.insert(asset.asset_name.clone());
                continue;
            }
            if seen_patch_targets.contains(asset.asset_name.as_str()) {
                continue;
            }
            all_patch_assets.push(PatchAssetInfo {
                target_file_path: asset.asset_name.clone(),
                target_file_size: asset.asset_size,
                target_file_hash: asset.asset_hash_md5.clone(),
                patch_method: PatchMethod::DownloadOver,
                patch_name: String::new(),
                patch_hash: String::new(),
                patch_offset: 0,
                patch_size: 0,
                patch_chunk_length: 0,
                original_file_path: None,
                original_file_hash: None,
                original_file_size: None,
                matching_field: matching_field.clone(),
            });
            seen_patch_targets.insert(asset.asset_name.clone());
        }
    }

    Ok(PreinstallPlan {
        patch_assets: all_patch_assets,
        deleted_files: all_deleted_files,
        unique_chunks,
        diff_downloads,
        main_chunk_download,
        tag,
        main_manifest_ids,
    })
}

fn patching_chunk_dir(game_dir: &Path) -> PathBuf {
    game_dir.join("patching").join("chunk")
}

type ProgressUpdater = Arc<dyn Fn(SophonProgress) + Send + Sync>;

/// Shared state for preinstall download workers. Created once per worker,
/// passed by reference to each chunk — eliminates per-chunk Arc clones.
struct PreinstallWorkerCtx {
    client: Client,
    diff_downloads: rustc_hash::FxHashMap<String, Arc<DownloadInfo>>,
    chunks_dir: PathBuf,
    handle: DownloadHandle,
    updater: ProgressUpdater,
    downloaded_bytes: Arc<AtomicU64>,
    resume_offset: u64,
    total_bytes: u64,
    chunk_bytes_map: Arc<DashMap<String, u64>>,
    state_saver: super::installer::StateSaver,
    chunks_since_save: Arc<AtomicUsize>,
}

async fn process_preinstall_chunk(
    chunk_info: PatchChunkInfo,
    ctx: &PreinstallWorkerCtx,
    diff_download: &Arc<DownloadInfo>,
) -> SophonResult<()> {
    if ctx.handle.is_cancelled() {
        return Err(SophonError::Cancelled);
    }

    ctx.handle
        .wait_if_paused(
            &*ctx.updater,
            ctx.downloaded_bytes.load(Ordering::Relaxed) + ctx.resume_offset,
            ctx.total_bytes,
        )
        .await?;

    validate_patch_name(&chunk_info.patch_name)?;

    let chunk_path = ctx.chunks_dir.join(&chunk_info.patch_name);

    download_patch_chunk_with_retries(
        &ctx.client,
        diff_download,
        &chunk_info.patch_name,
        &chunk_path,
        &chunk_info.patch_md5,
        super::MAX_RETRIES,
        &ctx.handle,
        &ctx.downloaded_bytes,
    )
    .await?;

    ctx.chunk_bytes_map
        .insert(chunk_info.patch_name.clone(), chunk_info.patch_size);

    let save_count = ctx.chunks_since_save.fetch_add(1, Ordering::Relaxed) + 1;
    if save_count
        .is_multiple_of(crate::CHUNK_STATE_SAVE_INTERVAL as usize)
    {
        let map: HashMap<String, u64> = ctx.chunk_bytes_map
            .iter()
            .map(|r| (r.key().clone(), *r.value()))
            .collect();
        (ctx.state_saver)(&map);
    }

    Ok(())
}

#[allow(clippy::too_many_arguments, clippy::type_complexity)]
pub async fn preinstall_download(
    download_client: &Client,
    plan: PreinstallPlan,
    game_dir: &Path,
    game_id: &str,
    vo_lang: &str,
    handle: DownloadHandle,
    updater: ProgressUpdater,
    state_saver: super::installer::StateSaver,
    resume_chunks: HashMap<String, u64>,
) -> SophonResult<PreinstallState> {
    let chunks_dir = patching_chunk_dir(game_dir);
    {
        let cd = chunks_dir.clone();
        tokio::task::spawn_blocking(move || fs::create_dir_all(&cd)).await??;
    }

    let installed_tag = read_installed_tag(game_dir).ok_or(SophonError::NoInstalledVersion)?;

    let total_bytes: u64 = plan
        .unique_chunks
        .iter()
        .map(|c| c.patch_size)
        .fold(0u64, |acc, x| acc.saturating_add(x));
    let downloaded_bytes = Arc::new(AtomicU64::new(0));

    let chunk_bytes_map: Arc<DashMap<String, u64>> = Arc::new(DashMap::new());
    for (k, v) in resume_chunks {
        chunk_bytes_map.insert(k, v);
    }

    // Phase 1 — checking existing files. Files that already exist with a
    // matching hash are folded into the baseline offset, NOT into
    // `downloaded_bytes`, so the download phase only counts bytes actually
    // streamed over the network.
    let total_files = plan.unique_chunks.len() as u64;
    updater(SophonProgress::CalculatingDownloads {
        checked_files: 0,
        total_files,
    });
    let checked_arc = Arc::new(AtomicU64::new(0));
    let resume_offset_arc = Arc::new(AtomicU64::new(0));
    let last_check_emit = Arc::new(AtomicU64::new(now_nanos()));
    {
        let semaphore = Arc::new(tokio::sync::Semaphore::new(16));
        let map = Arc::clone(&chunk_bytes_map);
        let upd = Arc::clone(&updater);
        let futures = plan.unique_chunks.iter().cloned().map(|chunk| {
            let permit = Arc::clone(&semaphore);
            let checked = Arc::clone(&checked_arc);
            let offset = Arc::clone(&resume_offset_arc);
            let map = Arc::clone(&map);
            let upd = Arc::clone(&upd);
            let last_emit = Arc::clone(&last_check_emit);
            let chunks_dir = chunks_dir.clone();
            async move {
                let _permit = permit.acquire().await.ok()?;
                let path = chunks_dir.join(&chunk.patch_name);
                let ok = tokio::task::spawn_blocking(move || {
                    verify_file_hash(&path, &chunk.patch_md5)
                })
                .await
                .ok()?;
                if ok {
                    map.insert(chunk.patch_name.clone(), chunk.patch_size);
                    offset.fetch_add(chunk.patch_size, Ordering::Relaxed);
                }
                let checked = checked.fetch_add(1, Ordering::Relaxed) + 1;
                // Emit by elapsed time (not a fixed file multiple) so the bar
                // advances even for short preinstall manifests.
                let now = now_nanos();
                if now.saturating_sub(last_emit.load(Ordering::Relaxed))
                    >= super::PROGRESS_UPDATE_INTERVAL_MS * 1_000_000
                {
                    last_emit.store(now, Ordering::Relaxed);
                    upd(SophonProgress::CalculatingDownloads {
                        checked_files: checked,
                        total_files,
                    });
                }
                Some(())
            }
        });
        futures_util::stream::iter(futures)
            .buffer_unordered(16)
            .collect::<Vec<_>>()
            .await;
    }
    updater(SophonProgress::CalculatingDownloads {
        checked_files: total_files,
        total_files,
    });
    let resume_offset = resume_offset_arc.load(Ordering::Relaxed);

    super::download::check_available_space(&chunks_dir, total_bytes)?;

    updater(SophonProgress::Downloading {
        downloaded_bytes: resume_offset,
        total_bytes,
        speed_bps: 0.0,
        eta_seconds: 0.0,
    });

    let chunks_since_save = Arc::new(AtomicUsize::new(0usize));

    if handle.is_cancelled() {
        return Err(SophonError::Cancelled);
    }

    let total: usize = plan.unique_chunks.len();
    let queue: Arc<Mutex<VecDeque<PatchChunkInfo>>> = Arc::new(Mutex::new(
        plan.unique_chunks
            .into_iter()
            .filter(|c| !chunk_bytes_map.contains_key(&c.patch_name))
            .collect(),
    ));
    let remaining: Arc<AtomicUsize> = Arc::new(AtomicUsize::new(
        queue.lock().unwrap_or_else(|e| e.into_inner()).len(),
    ));
    let cancelled = Arc::new(AtomicU8::new(0));
    let first_error: Arc<Mutex<Option<SophonError>>> = Arc::new(Mutex::new(None));
    let mut workers = tokio::task::JoinSet::new();

    let diff_downloads = plan.diff_downloads.clone();

    // Background timer emits progress every second regardless of chunk completion
    // rate.
    let progress_done = Arc::new(AtomicBool::new(false));
    {
        let db_atomic = Arc::clone(&downloaded_bytes);
        let timer_updater = Arc::clone(&updater);
        let timer_handle = handle.clone();
        let done_flag = Arc::clone(&progress_done);
        let timer_last_bytes = Arc::new(AtomicU64::new(resume_offset));
        let timer_last_time = Arc::new(AtomicU64::new(now_nanos()));
        let timer_smooth_speed = Arc::new(AtomicU64::new(0));
        let timer_eta_history = Arc::new(Mutex::new(VecDeque::new()));

        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(
                super::PROGRESS_UPDATE_INTERVAL_MS,
            ));
            loop {
                interval.tick().await;
                if done_flag.load(Ordering::Relaxed) || timer_handle.is_cancelled() {
                    break;
                }
                let db = db_atomic.load(Ordering::Relaxed) + resume_offset;
                let now = now_nanos();
                let last_ns = timer_last_time.load(Ordering::Relaxed);
                let elapsed = if last_ns == 0 {
                    0.0
                } else {
                    now.saturating_sub(last_ns) as f64 / 1_000_000_000.0
                };
                let instant_speed = if elapsed >= 1.0 {
                    let last_db = timer_last_bytes.load(Ordering::Relaxed);
                    let window_bytes = db.saturating_sub(last_db);
                    let speed = window_bytes as f64 / elapsed;
                    timer_last_bytes.store(db, Ordering::Relaxed);
                    timer_last_time.store(now, Ordering::Relaxed);
                    speed
                } else {
                    0.0
                };

                let speed_alpha = 1.0
                    / (super::SPEED_SMOOTH_WINDOW_SECS * 1000.0
                        / super::PROGRESS_UPDATE_INTERVAL_MS as f64);
                let speed_bps = super::ewma_update(&timer_smooth_speed, instant_speed, speed_alpha);
                let eta_speed_bps = super::compute_eta_speed(&timer_eta_history, instant_speed);

                let remaining = total_bytes.saturating_sub(db);
                let eta = if eta_speed_bps > 0.0 {
                    remaining as f64 / eta_speed_bps
                } else {
                    0.0
                };

                timer_updater(SophonProgress::Downloading {
                    downloaded_bytes: db,
                    total_bytes,
                    speed_bps,
                    eta_seconds: eta,
                });
            }
        });
    }

    // Spawn a worker closure factory to avoid repeating the body.
    let spawn_worker = |workers: &mut tokio::task::JoinSet<()>| {
        let queue = Arc::clone(&queue);
        let cancelled = Arc::clone(&cancelled);
        let first_error = Arc::clone(&first_error);
        let remaining = Arc::clone(&remaining);
        let ctx = PreinstallWorkerCtx {
            client: download_client.clone(),
            diff_downloads: diff_downloads.clone(),
            chunks_dir: chunks_dir.clone(),
            handle: handle.clone(),
            updater: Arc::clone(&updater),
            downloaded_bytes: Arc::clone(&downloaded_bytes),
            resume_offset,
            total_bytes,
            chunk_bytes_map: Arc::clone(&chunk_bytes_map),
            state_saver: Arc::clone(&state_saver),
            chunks_since_save: Arc::clone(&chunks_since_save),
        };

        workers.spawn(async move {
            loop {
                if ctx.handle.is_cancelled() {
                    return;
                }
                let chunk_info = {
                    let mut q = queue.lock().unwrap_or_else(|e| e.into_inner());
                    match q.pop_front() {
                        Some(v) => v,
                        None => break,
                    }
                };
                let diff_download = ctx.diff_downloads
                    .get(&chunk_info.matching_field)
                    .cloned()
                    .unwrap_or_else(|| {
                        log::error!(
                            "No diff_download for matching_field={field}, using first available",
                            field = chunk_info.matching_field
                        );
                        ctx.diff_downloads
                            .values()
                            .next()
                            .cloned()
                            .expect("diff_downloads must not be empty")
                    });
                let result = process_preinstall_chunk(
                    chunk_info,
                    &ctx,
                    &diff_download,
                )
                .await;
                if let Err(err) = result {
                    if matches!(err, SophonError::Cancelled) {
                        cancelled.store(1, Ordering::Relaxed);
                    } else if let Ok(mut guard) = first_error.lock()
                        && guard.is_none()
                    {
                        *guard = Some(err);
                    }
                }
                if remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    break;
                }
            }
        });
    };

    // Spawn initial workers and scale up while throughput keeps growing,
    // mirroring the main installer's adaptive download scheduler.
    let adaptive = super::adaptive_download::AdaptiveDownload::new();
    let initial = adaptive.current_target().min(total);
    let mut spawned = 0;
    for _ in 0..initial {
        spawn_worker(&mut workers);
        spawned += 1;
    }

    loop {
        tokio::select! {
            biased;
            result = workers.join_next() => {
                if result.is_none() {
                    break;
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                let db = downloaded_bytes.load(Ordering::Relaxed);
                adaptive.measure(db);

                let target = adaptive.current_target();
                let queue_len = queue.lock().unwrap_or_else(|e| e.into_inner()).len();
                // Spawn up to target, but only if queue has work.
                while spawned < target && queue_len > spawned {
                    spawn_worker(&mut workers);
                    spawned += 1;
                }

                if remaining.load(Ordering::Relaxed) == 0 {
                    break;
                }
            }
        }
    }

    while workers.join_next().await.is_some() {}

    super::reclaim_memory();

    progress_done.store(true, Ordering::Relaxed);

    if cancelled.load(Ordering::Relaxed) != 0 {
        return Err(SophonError::Cancelled);
    }
    if let Some(err) = first_error.lock().unwrap_or_else(|e| e.into_inner()).take() {
        return Err(err);
    }

    // Persist all downloaded chunks.
    let final_map = Arc::clone(&chunk_bytes_map);
    let map: HashMap<String, u64> = final_map
        .iter()
        .map(|r| (r.key().clone(), *r.value()))
        .collect();
    state_saver(&map);

    updater(SophonProgress::Downloading {
        downloaded_bytes: total_bytes,
        total_bytes,
        speed_bps: 0.0,
        eta_seconds: 0.0,
    });

    let downloaded_chunks: rustc_hash::FxHashSet<String> = match Arc::try_unwrap(chunk_bytes_map) {
        Ok(map) => map.into_iter().map(|(k, _)| k).collect(),
        Err(arc) => {
            log::warn!("chunk_bytes_map DashMap still has references, using iter");
            arc.iter().map(|r| r.key().clone()).collect()
        }
    };
    let tag_str = plan.tag.clone();
    let state = PreinstallState {
        tag: plan.tag,
        game_id: game_id.to_string(),
        vo_lang: vo_lang.to_string(),
        installed_tag,
        patch_assets: plan.patch_assets,
        deleted_files: plan.deleted_files,
        downloaded_chunks,
        diff_downloads: plan.diff_downloads,
        main_chunk_download: plan.main_chunk_download,
        main_manifest_ids: plan.main_manifest_ids,
    };

    let marker_path = PreinstallState::marker_file_path(game_dir, &tag_str);
    let marker_path_clone = marker_path.clone();
    tokio::task::spawn_blocking(move || fs::write(&marker_path_clone, &tag_str)).await??;

    if let Err(err) = save_preinstall_state(game_dir, &state) {
        let marker_path_for_cleanup = marker_path.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = fs::remove_file(&marker_path_for_cleanup);
        })
        .await;
        return Err(err);
    }

    Ok(state)
}

async fn download_patch_chunk_with_retries(
    client: &Client,
    diff_download: &DownloadInfo,
    patch_name: &str,
    dest: &Path,
    expected_md5: &str,
    max_retries: u32,
    handle: &DownloadHandle,
    progress: &Arc<AtomicU64>,
) -> SophonResult<()> {
    validate_patch_name(patch_name)?;
    let url = diff_download.url_for(patch_name);
    let mut last_err = String::new();

    for attempt in 0..max_retries {
        if handle.is_cancelled() {
            return Err(SophonError::Cancelled);
        }
        match download_patch_chunk_inner(client, &url, dest, expected_md5, handle, progress).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = err.to_string();
                if !err.is_retryable() {
                    return Err(err);
                }
                if attempt < max_retries - 1 {
                    log::warn!(
                        "Patch chunk {patch_name} failed (attempt {attempt_num}/{max_retries}): {last_err}",
                        attempt_num = attempt + 1,
                    );
                    let delay = retry_delay(attempt);
                    if cancelable_sleep(handle, delay).await.is_err() {
                        return Err(SophonError::Cancelled);
                    }
                } else {
                    log::warn!(
                        "Patch chunk {patch_name} failed (final attempt {attempt_num}/{max_retries}): {last_err}",
                        attempt_num = attempt + 1,
                    );
                }
            }
        }
    }

    Err(SophonError::DownloadFailed {
        chunk: patch_name.to_string(),
        attempts: max_retries,
        error: last_err,
    })
}

async fn download_chunk_with_retries(
    client: &Client,
    chunk_download: &DownloadInfo,
    chunk: &SophonManifestAssetChunk,
    dest: &Path,
    handle: &DownloadHandle,
) -> SophonResult<()> {
    let mut last_err = String::new();

    for attempt in 0..MAX_RETRIES {
        if handle.is_cancelled() {
            return Err(SophonError::Cancelled);
        }
        match super::download::download_chunk(
            client,
            chunk_download,
            chunk.into(),
            dest,
            None,
            Some(handle),
            false, // preinstall always uses full verification
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(err) => {
                last_err = err.to_string();
                if !err.is_retryable() {
                    return Err(err);
                }
                if attempt < MAX_RETRIES - 1 {
                    log::warn!(
                        "Chunk {chunk_name} failed (attempt {attempt_num}/{MAX_RETRIES}): {last_err}",
                        chunk_name = chunk.chunk_name,
                        attempt_num = attempt + 1,
                    );
                    let delay = retry_delay(attempt);
                    if cancelable_sleep(handle, delay).await.is_err() {
                        return Err(SophonError::Cancelled);
                    }
                } else {
                    log::warn!(
                        "Chunk {chunk_name} failed (final attempt {attempt_num}/{MAX_RETRIES}): {last_err}",
                        chunk_name = chunk.chunk_name,
                        attempt_num = attempt + 1,
                    );
                }
            }
        }
    }

    Err(SophonError::DownloadFailed {
        chunk: chunk.chunk_name.clone(),
        attempts: MAX_RETRIES,
        error: last_err,
    })
}

async fn download_patch_chunk_inner(
    client: &Client,
    url: &str,
    dest: &Path,
    expected_md5: &str,
    handle: &DownloadHandle,
    progress: &Arc<AtomicU64>,
) -> SophonResult<()> {
    let resp = client.get(url).send().await?;
    if resp.status().is_client_error() {
        let status = resp.status().as_u16() as i32;
        let url_clone = url.to_string();
        let _body = resp.text().await.unwrap_or_default();
        return Err(SophonError::ApiError(
            status,
            format!("{status} for {url_clone}"),
        ));
    }
    let resp = resp.error_for_status()?;
    let content_length: Option<u64> = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());
    let mut stream = resp.bytes_stream();
    let file = tokio::fs::File::create(dest).await?;
    let mut file = super::download::EvictingWriter::new(file);
    let needs_hash = !expected_md5.is_empty();
    let mut hasher = if needs_hash {
        Some(super::assembly_opt::take_md5()?)
    } else {
        None
    };
    let mut total_len = 0u64;

    loop {
        let next_chunk = stream.next();
        let result = tokio::select! {
            biased;
            _ = handle.cancelled_future() => {
                progress.fetch_sub(total_len, Ordering::Relaxed);
                let _ = tokio::fs::remove_file(dest).await;
                return Err(SophonError::Cancelled);
            }
            result = next_chunk => result,
        };

        match result {
            Some(Ok(bytes)) => {
                if bytes.is_empty() && content_length.is_none_or(|expected| total_len < expected) {
                    progress.fetch_sub(total_len, Ordering::Relaxed);
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(SophonError::Io(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "corrupted compressed data: empty chunk while data remaining",
                    )));
                }
                total_len += bytes.len() as u64;
                if let Some(expected_len) = content_length
                    && total_len > expected_len
                {
                    progress.fetch_sub(total_len, Ordering::Relaxed);
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(SophonError::SizeMismatch {
                        item: dest.display().to_string(),
                        expected: expected_len,
                        actual: total_len,
                    });
                }
                if let Some(ref mut h) = hasher {
                    if let Err(e) = h.update(&bytes) {
                        progress.fetch_sub(total_len, Ordering::Relaxed);
                        let _ = tokio::fs::remove_file(dest).await;
                        return Err(e.into());
                    }
                }
                if let Err(e) = file.write_all(&bytes).await {
                    progress.fetch_sub(total_len, Ordering::Relaxed);
                    let _ = tokio::fs::remove_file(dest).await;
                    return Err(e.into());
                }
                progress.fetch_add(bytes.len() as u64, Ordering::Relaxed);
            }
            Some(Err(e)) => {
                progress.fetch_sub(total_len, Ordering::Relaxed);
                let _ = tokio::fs::remove_file(dest).await;
                return Err(e.into());
            }
            None => break,
        }
    }
    file.flush().await?;

    if let Some(expected_len) = content_length
        && total_len != expected_len
    {
        progress.fetch_sub(total_len, Ordering::Relaxed);
        let _ = tokio::fs::remove_file(dest).await;
        return Err(SophonError::SizeMismatch {
            item: dest.display().to_string(),
            expected: expected_len,
            actual: total_len,
        });
    }

    if needs_hash {
        let digest: [u8; 16] = hasher.as_mut().unwrap().finish()?;
        if !md5_hex_eq(&digest, expected_md5) {
            progress.fetch_sub(total_len, Ordering::Relaxed);
            let _ = tokio::fs::remove_file(dest).await;
            return Err(SophonError::Md5Mismatch {
                item: dest.display().to_string(),
                expected: expected_md5.to_string(),
                actual: md5_to_hex(&digest),
            });
        }
    }

    file.flush_and_evict_all().await.ok();
    drop(file);
    if let Some(h) = hasher {
        super::assembly_opt::return_md5(h);
    }
    Ok(())
}

pub(super) fn verify_chunk_md5(path: &Path, expected_md5: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let fd = file.as_raw_fd();
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return false,
    };
    let mut hasher = match super::assembly_opt::take_md5() {
        Ok(h) => h,
        Err(_) => return false,
    };
    let hashed = if len == 0 {
        let _ = hasher.update(b"");
        true
    } else {
        posix_advise(fd, 0, 0, libc::POSIX_FADV_SEQUENTIAL);
        let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
        let mut ok = true;
        loop {
            let buf = match reader.fill_buf() {
                Ok(b) => b,
                Err(_) => {
                    ok = false;
                    break;
                }
            };
            if buf.is_empty() {
                break;
            }
            if hasher.update(buf).is_err() {
                ok = false;
                break;
            }
            let n = buf.len();
            reader.consume(n);
        }
        posix_advise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
        ok
    };
    let result = if hashed {
        match hasher.finish() {
            Ok(digest) => md5_hex_eq(&digest, expected_md5),
            Err(_) => false,
        }
    } else {
        false
    };
    super::assembly_opt::return_md5(hasher);
    result
}

pub(super) fn verify_chunk_xxh64(path: &Path, expected_xxh64: &str) -> bool {
    let Ok(file) = fs::File::open(path) else {
        log::warn!(
            "Failed to open file for XXH64 verification: {path_display}",
            path_display = path.display()
        );
        return false;
    };
    let fd = file.as_raw_fd();
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return false,
    };
    let mut hasher = super::assembly_opt::take_xxh64();
    let result = if len == 0 {
        hasher.update(b"");
        super::assembly_opt::xxh64_hex_eq(hasher.digest(), expected_xxh64)
    } else {
        posix_advise(fd, 0, 0, libc::POSIX_FADV_SEQUENTIAL);
        let mut reader = std::io::BufReader::with_capacity(256 * 1024, file);
        let mut ok = true;
        loop {
            let buf = match reader.fill_buf() {
                Ok(b) => b,
                Err(_) => {
                    ok = false;
                    break;
                }
            };
            if buf.is_empty() {
                break;
            }
            hasher.update(buf);
            let n = buf.len();
            reader.consume(n);
        }
        posix_advise(fd, 0, 0, libc::POSIX_FADV_DONTNEED);
        if ok {
            super::assembly_opt::xxh64_hex_eq(hasher.digest(), expected_xxh64)
        } else {
            false
        }
    };
    super::assembly_opt::return_xxh64(hasher);
    result
}

pub(super) fn verify_file_hash(path: &Path, expected_hash: &str) -> bool {
    if expected_hash.is_empty() {
        return true;
    }
    match expected_hash.len() {
        32 => verify_chunk_md5(path, expected_hash),
        16 => verify_chunk_xxh64(path, expected_hash),
        _ => {
            log::warn!(
                "Unknown hash format (length={len}): {hash}",
                len = expected_hash.len(),
                hash = expected_hash
            );
            false
        }
    }
}

pub async fn apply_preinstall(
    client: &Client,
    game_dir: &Path,
    preinstall_tag: &str,
    updater: ProgressUpdater,
    handle: &DownloadHandle,
) -> SophonResult<()> {
    let mut state = load_preinstall_state(game_dir, preinstall_tag)?;

    let current_tag = read_installed_tag(game_dir).ok_or(SophonError::NoInstalledVersion)?;

    if current_tag != state.installed_tag {
        return Err(SophonError::PreinstallStateInvalid(format!(
            "Installed version changed since preinstall (expected {expected}, got {actual})",
            expected = state.installed_tag,
            actual = current_tag
        )));
    }

    let chunks_dir = patching_chunk_dir(game_dir);
    let total_files = state.patch_assets.len() as u64;
    let applied_files = Arc::new(AtomicU64::new(0u64));
    let last_apply_update = AtomicU64::new(0);

    let game_dir_owned = game_dir.to_path_buf();
    let filter_cache = tokio::task::spawn_blocking(move || FilterCache::new(&game_dir_owned))
        .await
        .unwrap_or_else(|_| FilterCache::new(game_dir));
    filter_patch_assets_for_removed_features(&filter_cache, &mut state.patch_assets);

    let download_over_context =
        fetch_download_over_context(client, &state.game_id, &state.vo_lang).await?;

    let regenerate_pkg_version = if state.game_id == "hk4e" {
        Some(download_over_context.clone())
    } else {
        None
    };

    for asset in &state.patch_assets {
        if asset.patch_method == PatchMethod::Skip {
            applied_files.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "Skipping patch for removed feature asset: {path}",
                path = asset.target_file_path
            );
            continue;
        }

        if regenerate_pkg_version.is_some() && is_hk4e_pkg_version(&asset.target_file_path) {
            applied_files.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "Regenerating hk4e pkg_version (skipping CDN patch): {path}",
                path = asset.target_file_path,
            );
            continue;
        }

        if regenerate_pkg_version.is_some()
            && asset
                .target_file_path
                .to_lowercase()
                .ends_with("ctable_streaming.dat")
        {
            applied_files.fetch_add(1, Ordering::Relaxed);
            log::warn!(
                "Skipping hk4e Linux-filtered asset: {path}",
                path = asset.target_file_path,
            );
            continue;
        }

        let target_path = validate_asset_path(game_dir, &asset.target_file_path)?;
        let already_patched = {
            let tp = target_path.clone();
            let sz = asset.target_file_size;
            let md5 = asset.target_file_hash.clone();
            tokio::task::spawn_blocking(move || is_file_already_patched(&tp, sz, &md5)).await?
        };
        if already_patched {
            applied_files.fetch_add(1, Ordering::Relaxed);
            continue;
        }

        let is_filtered = is_filtered_asset(&filter_cache, asset);
        let asset = Arc::new(asset.clone());

        match asset.patch_method {
            PatchMethod::CopyOver => {
                const COPY_MAX_RETRIES: usize = 4;
                let mut fallback_to_download = false;
                let mut skip_progress = false;
                let asset = Arc::new(asset.clone());

                for attempt in 0..=COPY_MAX_RETRIES {
                    let gd = game_dir.to_path_buf();
                    let cd = chunks_dir.to_path_buf();
                    let a = Arc::clone(&asset);
                    let result =
                        tokio::task::spawn_blocking(move || apply_copy_over(&gd, &cd, a.as_ref()))
                            .await?;

                    match result {
                        Ok(()) => break,
                        Err(err) => {
                            if is_filtered {
                                log::warn!(
                                    "CopyOver failed for filtered asset, skipping: {path} ({err})",
                                    path = asset.target_file_path,
                                );
                                applied_files.fetch_add(1, Ordering::Relaxed);
                                skip_progress = true;
                                break;
                            }
                            if attempt == COPY_MAX_RETRIES {
                                log::warn!(
                                    "CopyOver failed for {path} after {attempts} attempts: {err}, falling back to DownloadOver",
                                    path = asset.target_file_path,
                                    attempts = COPY_MAX_RETRIES + 1,
                                );
                                fallback_to_download = true;
                            } else {
                                if handle.is_cancelled() {
                                    return Err(SophonError::Cancelled);
                                }
                                let delay = retry_delay(attempt as u32);
                                log::warn!(
                                    "CopyOver failed for {path} (attempt {attempt_num}/{max_attempts}): {err}, retrying in {delay_ms}ms...",
                                    path = asset.target_file_path,
                                    attempt_num = attempt + 1,
                                    max_attempts = COPY_MAX_RETRIES + 1,
                                    delay_ms = delay.as_millis()
                                );
                                if cancelable_sleep(handle, delay).await.is_err() {
                                    return Err(SophonError::Cancelled);
                                }
                            }
                        }
                    }
                }

                if skip_progress {
                    continue;
                }
                if fallback_to_download {
                    apply_download_over_with_retry(
                        client,
                        game_dir,
                        &state,
                        asset.as_ref(),
                        &download_over_context,
                        handle,
                    )
                    .await?;
                }
            }
            PatchMethod::Patch => {
                const PATCH_MAX_RETRIES: usize = 4;
                let mut fallback_to_download = false;
                let mut skip_progress = false;

                for attempt in 0..=PATCH_MAX_RETRIES {
                    let gd = game_dir.to_path_buf();
                    let cd = chunks_dir.to_path_buf();
                    let a = Arc::clone(&asset);
                    let fc = filter_cache.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        apply_hdiff_patch(&gd, &cd, a.as_ref(), &fc)
                    })
                    .await?;

                    match result {
                        Ok(()) => break,
                        Err(err) => {
                            if is_filtered {
                                log::warn!(
                                    "HDiff patch failed for filtered asset, skipping: {path} ({err})",
                                    path = asset.target_file_path,
                                );
                                applied_files.fetch_add(1, Ordering::Relaxed);
                                skip_progress = true;
                                break;
                            }
                            if attempt == PATCH_MAX_RETRIES {
                                log::warn!(
                                    "HDiff patch failed for {path} after {attempts} attempts: {err}, falling back to DownloadOver",
                                    path = asset.target_file_path,
                                    attempts = PATCH_MAX_RETRIES + 1,
                                );
                                fallback_to_download = true;
                            } else {
                                if handle.is_cancelled() {
                                    return Err(SophonError::Cancelled);
                                }
                                let delay = retry_delay(attempt as u32);
                                log::warn!(
                                    "HDiff patch failed for {path} (attempt {attempt_num}/{max_attempts}): {err}, retrying in {delay_ms}ms...",
                                    path = asset.target_file_path,
                                    attempt_num = attempt + 1,
                                    max_attempts = PATCH_MAX_RETRIES + 1,
                                    delay_ms = delay.as_millis()
                                );
                                if cancelable_sleep(handle, delay).await.is_err() {
                                    return Err(SophonError::Cancelled);
                                }
                            }
                        }
                    }
                }

                if skip_progress {
                    continue;
                }
                if fallback_to_download {
                    apply_download_over_with_retry(
                        client,
                        game_dir,
                        &state,
                        asset.as_ref(),
                        &download_over_context,
                        handle,
                    )
                    .await?;
                }
            }
            PatchMethod::DownloadOver => {
                apply_download_over_with_retry(
                    client,
                    game_dir,
                    &state,
                    asset.as_ref(),
                    &download_over_context,
                    handle,
                )
                .await?;
            }
            PatchMethod::Remove | PatchMethod::Skip => {}
        }

        let count = applied_files.fetch_add(1, Ordering::Relaxed) + 1;
        let now = now_nanos();
        if now.saturating_sub(last_apply_update.load(Ordering::Relaxed))
            >= super::PROGRESS_UPDATE_INTERVAL_MS * 1_000_000
        {
            updater(SophonProgress::ApplyingPreinstall {
                applied_files: count,
                total_files,
            });
            last_apply_update.store(now, Ordering::Relaxed);
        }
    }

    if let Some(context) = regenerate_pkg_version {
        let gd = game_dir.to_path_buf();
        let vo = state.vo_lang.clone();
        tokio::task::spawn_blocking(move || {
            regenerate_hk4e_pkg_version(&gd, &context, &[vo])
        })
        .await??;
    }

    {
        let gd = game_dir.to_path_buf();
        let df = state.deleted_files.clone();
        tokio::task::spawn_blocking(move || {
            for rel in &df {
                if let Err(err) = super::assembly::validate_asset_name(rel) {
                    log::warn!("Skipping deleted file with invalid path: {err}");
                    continue;
                }
                let path = gd.join(rel);
                if let Err(err) = fs::remove_file(&path) {
                    let path_display = path.display();
                    log::warn!("Failed to delete file {path_display}: {err}");
                }
            }
        })
        .await?;
    }

    {
        let gd = game_dir.to_path_buf();
        let tag = state.tag.clone();
        tokio::task::spawn_blocking(move || {
            super::write_installed_tag(&gd, &tag)?;
            let patching_dir = gd.join("patching");
            if patching_dir.exists() {
                let _ = fs::remove_dir_all(&patching_dir);
            }
            delete_preinstall_state(&gd, &tag);
            Ok::<(), SophonError>(())
        })
        .await??;
    }

    Ok(())
}

const HK4E_PKG_VERSION_FILES: &[&str] = &["pkg_version", "beyond_pkg_version"];

fn is_hk4e_pkg_version(target_file_path: &str) -> bool {
    let name = target_file_path.rsplit('/').next().unwrap_or(target_file_path);
    HK4E_PKG_VERSION_FILES
        .iter()
        .any(|suffix| name == *suffix || name.starts_with("Audio_") && name.ends_with(suffix))
}

fn regenerate_hk4e_pkg_version(
    game_dir: &Path,
    context: &DownloadOverContext,
    vo_lang: &[String],
) -> SophonResult<()> {
    let mut assets: Vec<SophonManifestAssetProperty> = Vec::new();
    for (_, (_, proto)) in &context.manifests {
        for asset in &proto.assets {
            if !asset.is_directory() {
                assets.push(asset.clone());
            }
        }
    }

    super::game_filters::filter_hk4e_asset_list(
        game_dir,
        &mut assets,
        vo_lang,
    );

    let manifest = Arc::new(super::compact_manifest::CompactManifest::from(assets));
    super::game_filters::write_pkg_version_from_manifest(game_dir, &manifest, vo_lang)?;
    Ok(())
}

fn validate_asset_path(game_dir: &Path, asset_path: &str) -> SophonResult<PathBuf> {
    super::assembly::validate_asset_name(asset_path)?;
    Ok(game_dir.join(asset_path))
}

fn validate_patch_name(patch_name: &str) -> SophonResult<()> {
    if patch_name.is_empty() {
        return Err(SophonError::InvalidAssetName(
            "patch_name cannot be empty".into(),
        ));
    }
    if patch_name.contains('\0') {
        return Err(SophonError::InvalidAssetName(
            "patch_name cannot contain null bytes".into(),
        ));
    }
    let mut chars = patch_name.chars();
    if let (Some(first), Some(':')) = (chars.next(), chars.next())
        && first.is_ascii_alphabetic()
    {
        return Err(SophonError::PathTraversal(patch_name.into()));
    }
    if patch_name.starts_with('/')
        || patch_name.starts_with('\\')
        || patch_name.starts_with("./")
        || patch_name.starts_with(".\\")
        || patch_name.split(&['/', '\\']).any(|c| c == "..")
    {
        return Err(SophonError::PathTraversal(patch_name.into()));
    }
    Ok(())
}

fn is_file_already_patched(path: &Path, expected_size: u64, expected_md5: &str) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if metadata.len() != expected_size {
        return false;
    }
    verify_file_hash(path, expected_md5)
}

#[derive(Clone)]
pub(super) struct FilterCache {
    pub(super) kdel_tokens: Option<Vec<String>>,
    pub(super) blacklist_entries: Option<Vec<String>>,
    pub(super) ignored_lang_patterns: Option<Vec<String>>,
}

impl FilterCache {
    fn new(game_dir: &Path) -> Self {
        let game_dir_str = game_dir.to_string_lossy();

        let kdel_tokens = if game_dir_str.contains(NAP_GAME_NAME)
            || game_dir.join(NAP_GAME_DATA_DIR).exists()
        {
            let kdel_path = game_dir.join(format!("{NAP_GAME_DATA_DIR}/Persistent/KDelResource"));
            fs::read_to_string(&kdel_path).ok().and_then(|content| {
                let first_line = content.lines().next()?;
                let tokens: Vec<String> = first_line
                    .split(|c: char| {
                        c == '|'
                            || c == ';'
                            || c == ','
                            || c == '$'
                            || c == '#'
                            || c == '@'
                            || c == '+'
                            || c == ' '
                    })
                    .map(|token| {
                        token
                            .trim_matches(|c: char| {
                                c == '|'
                                    || c == ';'
                                    || c == ','
                                    || c == '$'
                                    || c == '#'
                                    || c == '@'
                                    || c == '+'
                                    || c == ' '
                            })
                            .to_string()
                    })
                    .filter(|t| !t.is_empty())
                    .collect();
                if tokens.is_empty() {
                    None
                } else {
                    Some(tokens)
                }
            })
        } else {
            None
        };

        let blacklist_entries = if game_dir.join(HKRPG_DATA_DIR).exists() {
            let blacklist_path = game_dir.join(format!(
                "{HKRPG_DATA_DIR}/Persistent/DownloadBlacklist.json"
            ));
            fs::read_to_string(&blacklist_path)
                .ok()
                .map(|content| {
                    let mut entries: Vec<String> = content
                        .lines()
                        .filter_map(extract_blacklist_filename)
                        .map(|name| name.to_lowercase())
                        .collect();
                    // Generate cross-path variants.
                    let variants: Vec<String> = entries
                        .iter()
                        .flat_map(|entry| {
                            if entry.contains("persistent/") {
                                vec![entry.replace("persistent/", "streamingassets/")]
                            } else if entry.contains("streamingassets/") {
                                vec![entry.replace("streamingassets/", "persistent/")]
                            } else {
                                vec![]
                            }
                        })
                        .collect();
                    entries.extend(variants);
                    entries
                })
                .and_then(|entries| Some(entries).filter(|e| !e.is_empty()))
        } else {
            None
        };

        let ignored_lang_patterns = if game_dir.join(HK4E_DATA_DIR_GLOBAL).exists()
            || game_dir.join(HK4E_DATA_DIR_CN).exists()
        {
            let persistent_dir = super::game_filters::find_hk4e_persistent_dir(game_dir);
            let installed = read_hk4e_installed_langs(&persistent_dir);
            let all_langs: &[&str] = &["Chinese", "English(US)", "Japanese", "Korean"];
            let ignored: Vec<String> = all_langs
                .iter()
                .filter(|lang| !installed.iter().any(|inst| inst == **lang))
                .map(|lang| format!("/{lang}/").to_lowercase())
                .collect();
            Some(ignored)
        } else {
            None
        };

        FilterCache {
            kdel_tokens,
            blacklist_entries,
            ignored_lang_patterns,
        }
    }
}

/// Case-insensitive substring check without allocating a lowercased copy.
/// Assumes `pattern` is already lowercase.
fn contains_ignore_ascii_case(haystack: &str, pattern: &str) -> bool {
    if pattern.len() > haystack.len() {
        return false;
    }
    let h = haystack.as_bytes();
    let p = pattern.as_bytes();
    for i in 0..=(h.len() - p.len()) {
        if h[i..i + p.len()]
            .iter()
            .zip(p.iter())
            .all(|(a, b)| a.to_ascii_lowercase() == *b)
        {
            return true;
        }
    }
    false
}

fn is_filtered_asset(cache: &FilterCache, asset: &PatchAssetInfo) -> bool {
    if let Some(ref tokens) = cache.kdel_tokens {
        for token in tokens {
            if asset.matching_field.eq_ignore_ascii_case(token) {
                return true;
            }
        }
    }

    if cache.blacklist_entries.is_some() || cache.ignored_lang_patterns.is_some() {
        let path = &asset.target_file_path;

        if let Some(ref entries) = cache.blacklist_entries {
            for entry in entries {
                if contains_ignore_ascii_case(path, entry) {
                    return true;
                }
            }
        }

        if let Some(ref patterns) = cache.ignored_lang_patterns {
            for pattern in patterns {
                if contains_ignore_ascii_case(path, pattern) {
                    return true;
                }
            }
        }
    }

    false
}

fn extract_blacklist_filename(line: &str) -> Option<String> {
    let marker = "\"fileName\":\"";
    let start = line.find(marker)? + marker.len();
    let rest = &line[start..];
    let end = rest.find('"')?;
    Some(rest[..end].replace('\\', "/"))
}

fn read_hk4e_installed_langs(persistent_dir: &Path) -> Vec<String> {
    if let Ok(entries) = fs::read_dir(persistent_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with("audio_lang_")
                && let Ok(content) = fs::read_to_string(entry.path())
            {
                let langs: Vec<String> = content
                    .lines()
                    .map(|l| l.trim())
                    .filter(|l| !l.is_empty())
                    .map(|l| l.to_string())
                    .collect();
                if !langs.is_empty() {
                    return langs;
                }
            }
        }
    }
    vec!["English(US)".to_string()]
}

pub(super) fn filter_patch_assets_for_removed_features(
    cache: &FilterCache,
    assets: &mut [PatchAssetInfo],
) {
    for asset in assets.iter_mut() {
        if matches!(
            asset.patch_method,
            PatchMethod::CopyOver | PatchMethod::DownloadOver | PatchMethod::Patch
        ) && is_filtered_asset(cache, asset)
        {
            asset.patch_method = PatchMethod::Skip;
        }
    }
}

/// Apply copyover patch to target file. Caller performs path validation.
fn apply_copy_over(game_dir: &Path, chunks_dir: &Path, asset: &PatchAssetInfo) -> SophonResult<()> {
    validate_patch_name(&asset.patch_name)?;
    let chunk_path = chunks_dir.join(&asset.patch_name);
    if asset.patch_hash.is_empty() {
        if !chunk_path.exists() {
            return Err(SophonError::PatchChunkNotFound(asset.patch_name.clone()));
        }
    } else if !verify_file_hash(&chunk_path, &asset.patch_hash) {
        return Err(SophonError::Md5Mismatch {
            item: asset.patch_name.clone(),
            expected: asset.patch_hash.clone(),
            actual: "(computed)".to_string(),
        });
    }

    let target_path = validate_asset_path(game_dir, &asset.target_file_path)?;
    let mut chunk_file = fs::File::open(&chunk_path)?;
    let chunk_fd = chunk_file.as_raw_fd();
    posix_advise(chunk_fd, 0, 0, libc::POSIX_FADV_SEQUENTIAL);
    chunk_file.seek(SeekFrom::Start(asset.patch_offset))?;

    // Check if this is an HDiff patch by reading just the magic bytes
    let mut magic_buf = [0u8; HDIFF_MAGIC.len()];
    if asset.patch_chunk_length >= HDIFF_MAGIC.len() as u64 {
        chunk_file.read_exact(&mut magic_buf)?;
    }
    if magic_buf == HDIFF_MAGIC.as_ref() {
        let patch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            apply_hdiff_patch_from_files(game_dir, &chunk_path, asset.patch_offset, asset)
        }));
        posix_advise(chunk_fd, 0, 0, libc::POSIX_FADV_DONTNEED);
        match patch_result {
            Ok(result) => return result,
            Err(_) => {
                return Err(SophonError::HDiffPatchFailed {
                    file: asset.target_file_path.clone(),
                    error: "HDiff patch panicked".to_string(),
                });
            }
        }
    }

    // Seek past magic bytes and copy all chunk data.
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let safe_hash = asset.target_file_hash.replace(['/', '\\', '\0'], "_");
    let safe_path = asset.target_file_path.replace(['/', '\\', '\0', ':'], "_");
    // Use both hash and path to avoid collisions when target_file_hash is empty.
    let temp_path = game_dir.join(format!("patching/copyover_{safe_path}_{safe_hash}.tmp"));
    {
        let file = fs::File::create(&temp_path)?;
        file.set_len(asset.patch_chunk_length)?;
        let copied = super::sysio::copy_file_range(
            chunk_fd,
            asset.patch_offset,
            file.as_raw_fd(),
            0,
            asset.patch_chunk_length,
        )?;
        if copied != asset.patch_chunk_length {
            let _ = fs::remove_file(&temp_path);
            return Err(SophonError::SizeMismatch {
                item: asset.target_file_path.clone(),
                expected: asset.patch_chunk_length,
                actual: copied,
            });
        }
    }
    let actual_size = fs::metadata(&temp_path)?.len();
    if actual_size != asset.target_file_size {
        let _ = fs::remove_file(&temp_path);
        return Err(SophonError::SizeMismatch {
            item: asset.target_file_path.clone(),
            expected: asset.target_file_size,
            actual: actual_size,
        });
    }
    if !asset.target_file_hash.is_empty() && !verify_file_hash(&temp_path, &asset.target_file_hash)
    {
        let _ = fs::remove_file(&temp_path);
        return Err(SophonError::Md5Mismatch {
            item: asset.target_file_path.clone(),
            expected: asset.target_file_hash.clone(),
            actual: "(computed)".to_string(),
        });
    }
    fs::rename(&temp_path, &target_path)?;

    posix_advise(chunk_fd, 0, 0, libc::POSIX_FADV_DONTNEED);
    Ok(())
}

fn apply_hdiff_patch(
    game_dir: &Path,
    chunks_dir: &Path,
    asset: &PatchAssetInfo,
    cache: &FilterCache,
) -> SophonResult<()> {
    let original_path = match &asset.original_file_path {
        Some(p) => validate_asset_path(game_dir, p)?,
        None => validate_asset_path(game_dir, &asset.target_file_path)?,
    };
    let mut diff_ref_guard: DiffRefGuard = DiffRefGuard(None);
    if !original_path.exists() {
        let is_blank = asset.original_file_hash.as_deref() == Some(BLANK_FILE_MD5)
            || asset.original_file_size == Some(0);
        if is_blank {
            let safe_patch_name = asset.patch_name.replace(['/', '\\', '\0'], "_");
            let diff_ref = game_dir.join(format!("patching/{safe_patch_name}.diff_ref"));
            if let Some(parent) = diff_ref.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::File::create(&diff_ref)?;
            if !verify_file_hash(&diff_ref, BLANK_FILE_MD5) {
                return Err(SophonError::HDiffPatchFailed {
                    file: asset.target_file_path.clone(),
                    error: format!(
                        "Blank diff_ref file hash mismatch for created empty file at {diff_ref:?}"
                    ),
                });
            }
            diff_ref_guard = DiffRefGuard::new(diff_ref);
        } else if is_filtered_asset(cache, asset) {
            log::warn!(
                "Original file missing for filtered asset, skipping: {path}",
                path = asset.target_file_path
            );
            return Ok(());
        } else {
            return Err(SophonError::OriginalFileMissing(
                original_path.to_string_lossy().to_string(),
            ));
        }
    }

    if diff_ref_guard.0.is_none()
        && let Some(ref expected_size) = asset.original_file_size
    {
        let actual_size = match fs::metadata(&original_path) {
            Ok(m) => m.len(),
            Err(err) => {
                if is_filtered_asset(cache, asset) {
                    log::warn!(
                        "Failed to read metadata for filtered asset {path}: {err}, skipping",
                        path = asset.target_file_path
                    );
                    return Ok(());
                }
                log::warn!(
                    "Failed to read metadata for original file {path}: {err}, falling back to DownloadOver",
                    path = original_path.display()
                );
                return Err(SophonError::OriginalFileMissing(format!(
                    "{path} (Metadata unreadable: {err})",
                    path = original_path.display()
                )));
            }
        };
        if actual_size != *expected_size {
            if is_filtered_asset(cache, asset) {
                log::warn!(
                    "Original file size mismatch for filtered asset, skipping: {path}",
                    path = asset.target_file_path
                );
                return Ok(());
            }
            log::warn!(
                "Original file size mismatch for {path}: expected {expected_size}, got {actual_size}, falling back to DownloadOver (keeping file for old-chunk reuse)",
                path = original_path.display(),
            );
            return Err(SophonError::OriginalFileMissing(format!(
                "{path} (Size mismatch: expected {expected_size}, got {actual_size})",
                path = original_path.display(),
            )));
        }
    }
    if let Some(ref expected_md5) = asset.original_file_hash
        && diff_ref_guard.0.is_none()
        && original_path.exists()
        && !expected_md5.is_empty()
        && !verify_file_hash(&original_path, expected_md5)
        && is_filtered_asset(cache, asset)
    {
        log::warn!(
            "Original file MD5 mismatch for filtered asset, skipping: {path}",
            path = asset.target_file_path
        );
        return Ok(());
    }
    if let Some(ref expected_md5) = asset.original_file_hash
        && diff_ref_guard.0.is_none()
        && original_path.exists()
        && !expected_md5.is_empty()
        && !verify_file_hash(&original_path, expected_md5)
        && !is_filtered_asset(cache, asset)
    {
        log::warn!(
            "Original file MD5 mismatch for {path}, falling back to DownloadOver (keeping file for old-chunk reuse)",
            path = original_path.display()
        );
        return Err(SophonError::OriginalFileMissing(
            original_path.to_string_lossy().to_string(),
        ));
    }

    validate_patch_name(&asset.patch_name)?;
    let chunk_path = chunks_dir.join(&asset.patch_name);
    if !chunk_path.exists() {
        return Err(SophonError::PatchChunkNotFound(asset.patch_name.clone()));
    }

    let target_path = validate_asset_path(game_dir, &asset.target_file_path)?;
    let safe_hash = asset.target_file_hash.replace(['/', '\\', '\0'], "_");
    let safe_path = asset.target_file_path.replace(['/', '\\', '\0', ':'], "_");
    let temp_output = game_dir.join(format!("patching/{safe_path}_{safe_hash}.tmp.out"));
    let _ = fs::remove_file(&temp_output);

    if let Some(parent) = temp_output.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let effective_original = diff_ref_guard.0.as_deref().unwrap_or(&original_path);
    let op = effective_original.to_string_lossy().into_owned();
    let dp = chunk_path.to_string_lossy().into_owned();
    let tp = temp_output.to_string_lossy().into_owned();
    let diff_offset = asset.patch_offset;

    let patch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut hdiff = super::hdiffpatch::HDiff::with_offset(op, dp, tp, diff_offset);
        hdiff.apply(None)
    }));

    {
        if let Ok(f) = fs::File::open(&chunk_path) {
            posix_advise(f.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }

    match patch_result {
        Ok(true) => {
            let actual_size = fs::metadata(&temp_output)?.len();
            if actual_size != asset.target_file_size {
                let _ = fs::remove_file(&temp_output);
                return Err(SophonError::SizeMismatch {
                    item: asset.target_file_path.clone(),
                    expected: asset.target_file_size,
                    actual: actual_size,
                });
            }
            if !asset.target_file_hash.is_empty()
                && !verify_file_hash(&temp_output, &asset.target_file_hash)
            {
                let _ = fs::remove_file(&temp_output);
                return Err(SophonError::Md5Mismatch {
                    item: asset.target_file_path.clone(),
                    expected: asset.target_file_hash.clone(),
                    actual: "(computed)".to_string(),
                });
            }
            fs::rename(&temp_output, &target_path)?;
            Ok(())
        }
        Ok(false) => {
            let _ = fs::remove_file(&temp_output);
            Err(SophonError::HDiffPatchFailed {
                file: asset.target_file_path.clone(),
                error: "HDiff apply returned false".to_string(),
            })
        }
        Err(join_err) => {
            let _ = fs::remove_file(&temp_output);
            let error_msg = if let Some(s) = join_err.downcast_ref::<&'static str>() {
                s.to_string()
            } else if let Some(s) = join_err.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown thread panic".to_string()
            };
            Err(SophonError::HDiffPatchFailed {
                file: asset.target_file_path.clone(),
                error: format!("HDiff thread panicked: {error_msg}"),
            })
        }
    }
}

fn apply_hdiff_patch_from_files(
    game_dir: &Path,
    diff_path: &Path,
    diff_offset: u64,
    asset: &PatchAssetInfo,
) -> SophonResult<()> {
    let target_path = validate_asset_path(game_dir, &asset.target_file_path)?;

    let safe_patch_name = asset.patch_name.replace(['/', '\\', '\0'], "_");
    let empty_original_path = game_dir.join(format!("patching/{safe_patch_name}.diff_ref"));
    {
        if let Some(parent) = empty_original_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::File::create(&empty_original_path)?;
        // Verify the freshly-created diff_ref is empty before proceeding.
        if !verify_file_hash(&empty_original_path, BLANK_FILE_MD5) {
            let _ = fs::remove_file(&empty_original_path);
            return Err(SophonError::HDiffPatchFailed {
                file: asset.target_file_path.clone(),
                error: format!(
                    "Blank diff_ref file hash mismatch for created empty file at {empty_original_path:?}"
                ),
            });
        }
    }

    let safe_hash = asset.target_file_hash.replace(['/', '\\', '\0'], "_");
    let safe_path = asset.target_file_path.replace(['/', '\\', '\0', ':'], "_");
    let temp_output = game_dir.join(format!("patching/{safe_path}_{safe_hash}.tmp.out"));
    let _ = fs::remove_file(&temp_output);
    if let Some(parent) = temp_output.parent() {
        fs::create_dir_all(parent)?;
    }
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let op = empty_original_path.to_string_lossy().to_string();
    let dp = diff_path.to_string_lossy().to_string();
    let tp = temp_output.to_string_lossy().to_string();

    let patch_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
        let mut hdiff = super::hdiffpatch::HDiff::with_offset(op, dp, tp, diff_offset);
        hdiff.apply(None)
    }));

    let _ = fs::remove_file(&empty_original_path);

    match patch_result {
        Ok(true) => {
            let actual_size = fs::metadata(&temp_output)?.len();
            if actual_size != asset.target_file_size {
                let _ = fs::remove_file(&temp_output);
                return Err(SophonError::SizeMismatch {
                    item: asset.target_file_path.clone(),
                    expected: asset.target_file_size,
                    actual: actual_size,
                });
            }
            if !asset.target_file_hash.is_empty()
                && !verify_file_hash(&temp_output, &asset.target_file_hash)
            {
                let _ = fs::remove_file(&temp_output);
                return Err(SophonError::Md5Mismatch {
                    item: asset.target_file_path.clone(),
                    expected: asset.target_file_hash.clone(),
                    actual: "(computed)".to_string(),
                });
            }
            fs::rename(&temp_output, &target_path)?;
            Ok(())
        }
        Ok(false) => {
            let _ = fs::remove_file(&temp_output);
            Err(SophonError::HDiffPatchFailed {
                file: asset.target_file_path.clone(),
                error: "HDiff apply returned false (from files)".to_string(),
            })
        }
        Err(join_err) => {
            let _ = fs::remove_file(&temp_output);
            let error_msg = if let Some(s) = join_err.downcast_ref::<&'static str>() {
                s.to_string()
            } else if let Some(s) = join_err.downcast_ref::<String>() {
                s.clone()
            } else {
                "Unknown thread panic".to_string()
            };
            Err(SophonError::HDiffPatchFailed {
                file: asset.target_file_path.clone(),
                error: format!("HDiff thread panicked (from files): {error_msg}"),
            })
        }
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct DownloadOverContext {
    pub build: SophonBuildData,
    pub manifests: rustc_hash::FxHashMap<String, (DownloadInfo, SophonManifestProto)>,
}

pub async fn fetch_download_over_context(
    client: &Client,
    game_id: &str,
    vo_lang: &str,
) -> SophonResult<DownloadOverContext> {
    let (_, pre_branch) = fetch_front_door(client, game_id).await?;
    let pre_branch = pre_branch.ok_or(SophonError::NoPreinstallAvailable)?;
    let build = fetch_build(client, &pre_branch, None).await?;

    let mut manifests = rustc_hash::FxHashMap::default();
    for meta in &build.manifests {
        let should_fetch = meta.matching_field == "game"
            || vo_lang_matches(&meta.matching_field, vo_lang)
            || !is_known_vo_locale(&meta.matching_field);

        if !should_fetch {
            continue;
        }

        let manifest_result =
            fetch_manifest(client, &meta.manifest_download, &meta.manifest.id).await?;
        manifests.insert(
            meta.matching_field.clone(),
            (meta.chunk_download.clone(), manifest_result.manifest),
        );
    }

    Ok(DownloadOverContext { build, manifests })
}

async fn apply_download_over_with_retry(
    client: &Client,
    game_dir: &Path,
    state: &PreinstallState,
    asset: &PatchAssetInfo,
    context: &DownloadOverContext,
    handle: &DownloadHandle,
) -> SophonResult<()> {
    let mut last_err = None;
    for attempt in 0..MAX_RETRIES {
        if handle.is_cancelled() {
            return Err(SophonError::Cancelled);
        }
        match apply_download_over(client, game_dir, state, asset, context, handle).await {
            Ok(()) => return Ok(()),
            Err(err) => {
                if !err.is_retryable() {
                    return Err(err);
                }
                let err_msg = err.to_string();
                last_err = Some(err);
                let is_last = attempt == MAX_RETRIES - 1;
                if is_last {
                    break;
                }
                let delay = retry_delay(attempt);
                log::warn!(
                    "DownloadOver failed for {path} (attempt {attempt_num}/{MAX_RETRIES}), retrying in {delay_ms}ms: {err}",
                    path = asset.target_file_path,
                    attempt_num = attempt + 1,
                    delay_ms = delay.as_millis(),
                    err = err_msg
                );
                if cancelable_sleep(handle, delay).await.is_err() {
                    return Err(SophonError::Cancelled);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| SophonError::DownloadFailed {
        chunk: asset.target_file_path.clone(),
        attempts: MAX_RETRIES,
        error: "All retry attempts exhausted".to_string(),
    }))
}

async fn apply_download_over(
    client: &Client,
    game_dir: &Path,
    _state: &PreinstallState,
    asset: &PatchAssetInfo,
    context: &DownloadOverContext,
    handle: &DownloadHandle,
) -> SophonResult<()> {
    let target_path = validate_asset_path(game_dir, &asset.target_file_path)?;

    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }

    let (chunk_download, manifest) = context
        .manifests
        .get(&asset.matching_field)
        .ok_or_else(|| SophonError::NoVoiceManifest(asset.matching_field.clone()))?;

    let file_entry = manifest
        .assets
        .iter()
        .find(|a| a.asset_name == asset.target_file_path)
        .ok_or_else(|| SophonError::AssemblyFailed {
            file: asset.target_file_path.clone(),
            error: "File not found in main manifest for DownloadOver".to_string(),
        })?;

    let chunks_dir = game_dir.join("chunks");
    {
        let cd = chunks_dir.clone();
        tokio::task::spawn_blocking(move || fs::create_dir_all(&cd)).await??;
    }

    let download_semaphore = Arc::new(tokio::sync::Semaphore::new(super::DOWNLOAD_CONCURRENCY));
    let chunk_futures = file_entry.asset_chunks.iter().map(|chunk| {
        let mut chunk_path = chunks_dir.join(&chunk.chunk_name);
        chunk_path.set_extension("zstd");
        let client = client.clone();
        let chunk_download = chunk_download.clone();
        let chunk = chunk.clone();
        let handle = handle.clone();
        let permit = Arc::clone(&download_semaphore);
        async move {
            let _permit = permit.acquire().await.expect("semaphore not closed");
            download_chunk_with_retries(&client, &chunk_download, &chunk, &chunk_path, &handle)
                .await
        }
    });
    try_join_all(chunk_futures).await?;

    {
        let gd = game_dir.to_path_buf();
        let file_entry = file_entry.clone();
        let cd = chunks_dir.clone();
        let vc = Arc::new(dashmap::DashMap::new());
        tokio::task::spawn_blocking(move || {
            let tmp_dir = gd.join("tmp-patch-downloadover");
            fs::create_dir_all(&tmp_dir)?;
            let manifest = super::compact_manifest::CompactManifest::from(vec![file_entry]);
            let chunk_count =
                (manifest.file_chunk_range(0).end - manifest.file_chunk_range(0).start) as usize;
            let total_bytes: usize = (0..chunk_count)
                .map(|i| manifest.chunk(i).chunk_name.len())
                .sum();
            let mut chunk_arena =
                super::compact_manifest::StringArena::with_capacity(chunk_count, total_bytes);
            for ci in 0..chunk_count {
                chunk_arena.intern(manifest.chunk(ci).chunk_name);
            }
            let chunk_lookup = super::installer::ChunkNameLookup::from_arena(chunk_arena);
            let chunk_refcounts: Vec<AtomicU32> =
                (0..chunk_count).map(|_| AtomicU32::new(1)).collect();
            let result = super::assembly::assemble_file(
                &manifest,
                0,
                &gd,
                &cd,
                &tmp_dir,
                &chunk_lookup,
                &chunk_refcounts,
                &vc,
                false,
                super::VerifyMode::Full, // preinstall always uses full verification
            );
            if let Err(err) = fs::remove_dir_all(&tmp_dir) {
                log::warn!(
                    "Failed to clean up temp directory {path}: {err}",
                    path = tmp_dir.display(),
                );
            }
            result
        })
        .await??;
    }

    Ok(())
}

use crate::SophonProgress;

#[cfg(test)]
#[path = "preinstall_tests.rs"]
mod tests;
