use std::fs;
#[cfg(feature = "preinstall")]
use std::io::{BufWriter, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::error::{SophonError, SophonResult};
use crate::api_scrape::DownloadInfo;
use crate::proto_parse::SophonManifestProto;
#[cfg(feature = "preinstall")]
use crate::proto_parse::normalize_full_download_manifest;

const PREINSTALL_STATE_FILE_EXT: &str = ".json";

/// Adapter that streams serialized state into a SHA-256 hasher.
struct Sha256Writer<'a>(&'a mut Sha256);

impl std::io::Write for Sha256Writer<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.update(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Strategy used to produce or remove a target file during preinstall
/// application.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum PatchMethod {
    CopyOver,
    Patch,
    DownloadOver,
    Remove,
    Skip,
}

/// Target-file metadata and the patch/download action selected for it.
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

/// Identity and checksum metadata for a shared patch payload chunk.
#[cfg(feature = "preinstall")]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PatchChunkInfo {
    pub patch_name: String,
    pub patch_size: u64,
    pub patch_md5: String,
    pub matching_field: String,
}

/// Persisted preinstall package state, including payload references and target
/// metadata.
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_snapshot: Option<TargetSnapshot>,
}

/// Sealed snapshot of exact target manifests retained for later package
/// application.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetSnapshot {
    pub(super) version: u32,
    pub(super) game_id: String,
    pub(super) vo_lang: String,
    pub(super) installed_tag: String,
    pub(super) tag: String,
    pub(super) build_id: String,
    pub(super) legacy_coverage: bool,
    pub(super) manifests: Vec<TargetManifest>,
    pub(super) digest: String,
}

/// One manifest and its chunk endpoint retained in a target snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetManifest {
    pub(super) matching_field: String,
    pub(super) manifest_id: String,
    pub(super) chunk_download: DownloadInfo,
    pub(super) manifest: SophonManifestProto,
}

pub(super) fn invalid_state(message: impl Into<String>) -> SophonError {
    SophonError::PreinstallStateInvalid(message.into())
}

pub(super) fn validate_tag(tag: &str) -> SophonResult<()> {
    if tag.is_empty() || tag.contains(['/', '\\', '\0']) || tag == "." || tag == ".." {
        return Err(invalid_state("Invalid preinstall version tag"));
    }
    Ok(())
}

pub(super) fn valid_hash(hash: &str) -> bool {
    matches!(hash.len(), 16 | 32) && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

pub(super) fn validate_patch_range(asset: &PatchAssetInfo) -> SophonResult<()> {
    validate_patch_name(&asset.patch_name)?;
    if asset
        .patch_offset
        .checked_add(asset.patch_chunk_length)
        .is_none_or(|end| end > asset.patch_size)
    {
        return Err(invalid_state(format!(
            "Invalid patch range: {}",
            asset.target_file_path
        )));
    }
    Ok(())
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

impl TargetSnapshot {
    #[cfg(feature = "preinstall")]
    pub(super) fn seal(
        &mut self,
        assets: &[PatchAssetInfo],
        deleted: &[String],
    ) -> SophonResult<()> {
        self.manifests
            .sort_unstable_by(|a, b| a.matching_field.cmp(&b.matching_field));
        for entry in &mut self.manifests {
            normalize_full_download_manifest(&mut entry.manifest);
        }
        self.digest = self.compute_digest(assets, deleted)?;
        Ok(())
    }

    fn compute_digest(
        &self,
        assets: &[PatchAssetInfo],
        deleted: &[String],
    ) -> SophonResult<String> {
        let mut hasher = Sha256::new();
        let mut writer = Sha256Writer(&mut hasher);
        serde_json::to_writer(
            &mut writer,
            &(
                self.version,
                &self.game_id,
                &self.vo_lang,
                &self.installed_tag,
                &self.tag,
                &self.build_id,
                self.legacy_coverage,
                &self.manifests,
                assets,
                deleted,
            ),
        )
        .map_err(|err| invalid_state(err.to_string()))?;
        Ok(hex::encode(hasher.finalize()))
    }

    pub(super) fn validate(&self, state: &PreinstallState) -> SophonResult<()> {
        if self.version != 1
            || self.game_id != state.game_id
            || self.vo_lang != state.vo_lang
            || self.installed_tag != state.installed_tag
            || self.tag != state.tag
        {
            return Err(invalid_state(
                "Target snapshot identity/version does not match this package",
            ));
        }
        if self.digest != self.compute_digest(&state.patch_assets, &state.deleted_files)? {
            return Err(invalid_state(
                "Target snapshot integrity check failed; retain the package and retry preinstall",
            ));
        }
        let mut ids: Vec<_> = self
            .manifests
            .iter()
            .map(|m| (m.matching_field.clone(), m.manifest_id.clone()))
            .collect();
        let mut expected = state.main_manifest_ids.clone();
        ids.sort();
        expected.sort();
        if ids != expected
            || !ids.iter().any(|(field, _)| field == "game")
            || ids.windows(2).any(|w| w[0].0 == w[1].0)
            || ids
                .iter()
                .any(|(field, id)| field.is_empty() || id.is_empty())
        {
            return Err(invalid_state(
                "Target snapshot is missing required matching fields or manifest IDs",
            ));
        }
        let mut files = rustc_hash::FxHashMap::default();
        for entry in &self.manifests {
            let url = reqwest::Url::parse(&entry.chunk_download.url_for("chunk"))
                .map_err(|_| invalid_state("Invalid target chunk URL"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(invalid_state("Unsupported target chunk URL scheme"));
            }
            for file in &entry.manifest.assets {
                super::assembly::validate_asset_name(file.asset_name_str())?;
                if files
                    .insert((entry.matching_field.as_str(), file.asset_name_str()), file)
                    .is_some()
                {
                    return Err(invalid_state(format!(
                        "Duplicate target asset: {}",
                        file.asset_name_str()
                    )));
                }
                if file.is_directory() {
                    if file.asset_size != 0 || !file.asset_chunks.is_empty() {
                        return Err(invalid_state(format!(
                            "Invalid target directory: {}",
                            file.asset_name_str()
                        )));
                    }
                    continue;
                }
                if !valid_hash(file.asset_hash_md5_str()) || file.asset_size > i64::MAX as u64 {
                    return Err(invalid_state(format!(
                        "Invalid target size/hash: {}",
                        file.asset_name_str()
                    )));
                }
                let mut chunks: Vec<_> = file.asset_chunks.iter().collect();
                chunks.sort_unstable_by_key(|c| c.chunk_on_file_offset);
                let mut end = 0u64;
                for chunk in chunks {
                    if !super::assembly::validate_chunk_name(chunk.chunk_name_str())
                        || chunk.chunk_on_file_offset != end
                        || chunk.chunk_size == 0
                        || chunk.chunk_size > i64::MAX as u64
                        || (chunk.chunk_size_decompressed == 0 && file.asset_size != 0)
                        || (!chunk.chunk_decompressed_hash_md5.is_empty()
                            && !valid_hash(chunk.chunk_decompressed_hash_md5_str()))
                        || (!chunk.chunk_compressed_hash_md5.is_empty()
                            && !valid_hash(chunk.chunk_compressed_hash_md5_str()))
                    {
                        return Err(invalid_state(format!(
                            "Invalid target chunk/range: {}",
                            file.asset_name_str()
                        )));
                    }
                    end = end
                        .checked_add(chunk.chunk_size_decompressed)
                        .ok_or_else(|| invalid_state("Target chunk range overflow"))?;
                }
                if end != file.asset_size {
                    return Err(invalid_state(format!(
                        "Incomplete target chunk coverage: {}",
                        file.asset_name_str()
                    )));
                }
            }
        }
        for asset in &state.patch_assets {
            super::assembly::validate_asset_name(&asset.target_file_path)?;
            if matches!(
                asset.patch_method,
                PatchMethod::CopyOver | PatchMethod::Patch | PatchMethod::DownloadOver
            ) && (asset.target_file_size > i64::MAX as u64
                || !valid_hash(&asset.target_file_hash))
            {
                return Err(invalid_state(format!(
                    "Invalid patch target size/hash: {}",
                    asset.target_file_path
                )));
            }
            if let Some(original) = &asset.original_file_path {
                super::assembly::validate_asset_name(original)?;
            }
            if matches!(
                asset.patch_method,
                PatchMethod::CopyOver | PatchMethod::Patch
            ) {
                validate_patch_range(asset)?;
            }
            let file = files.get(&(
                asset.matching_field.as_str(),
                asset.target_file_path.as_str(),
            ));
            if let Some(file) = file {
                if file.is_directory()
                    || file.asset_size != asset.target_file_size
                    || !file
                        .asset_hash_md5_str()
                        .eq_ignore_ascii_case(&asset.target_file_hash)
                {
                    return Err(invalid_state(format!(
                        "Patch and target manifest disagree: {}",
                        asset.target_file_path
                    )));
                }
            } else if asset.patch_method == PatchMethod::DownloadOver {
                return Err(invalid_state(format!(
                    "DownloadOver target missing: {}",
                    asset.target_file_path
                )));
            }
            if !self
                .manifests
                .iter()
                .any(|m| m.matching_field == asset.matching_field)
                && !state.diff_downloads.contains_key(&asset.matching_field)
            {
                return Err(invalid_state(format!(
                    "Unrecorded patch matching field: {}",
                    asset.matching_field
                )));
            }
        }
        for path in &state.deleted_files {
            super::assembly::validate_asset_name(path)?;
        }
        Ok(())
    }
}

impl PreinstallState {
    #[cfg(any(feature = "discovery", feature = "preinstall"))]
    pub fn state_file_path(game_dir: &Path, tag: &str) -> PathBuf {
        game_dir.join(format!(
            ".sophon_preinstall_{tag}{PREINSTALL_STATE_FILE_EXT}"
        ))
    }

    #[cfg(feature = "preinstall")]
    pub fn marker_file_path(game_dir: &Path, tag: &str) -> PathBuf {
        game_dir.join(format!(".sophon_preinstall_{tag}"))
    }

    #[cfg_attr(not(feature = "discovery"), allow(dead_code))]
    pub(super) fn cached_sizes(&self) -> (u64, u64) {
        let mut compressed = 0u64;
        let mut decompressed = 0u64;
        let mut seen = rustc_hash::FxHashSet::default();
        if let Some(snapshot) = &self.target_snapshot {
            for entry in &snapshot.manifests {
                for file in &entry.manifest.assets {
                    decompressed = decompressed.saturating_add(file.asset_size);
                    for chunk in &file.asset_chunks {
                        if seen.insert((entry.matching_field.as_str(), chunk.chunk_name_str())) {
                            compressed = compressed.saturating_add(chunk.chunk_size);
                        }
                    }
                }
            }
        } else {
            for asset in &self.patch_assets {
                decompressed = decompressed.saturating_add(asset.target_file_size);
                if !asset.patch_name.is_empty()
                    && seen.insert((asset.matching_field.as_str(), asset.patch_name.as_str()))
                {
                    compressed = compressed.saturating_add(asset.patch_size);
                }
            }
        }
        (compressed, decompressed)
    }
}

#[cfg_attr(not(feature = "discovery"), allow(dead_code))]
pub(super) fn cached_preinstall(
    game_dir: &Path,
    tag: &str,
    game_id: &str,
    vo_lang: &str,
    installed_tag: Option<&str>,
) -> Option<PreinstallState> {
    let state = load_preinstall_state(game_dir, tag).ok()?;
    let same_locale = match (
        super::api::normalize_vo_locale(&state.vo_lang),
        super::api::normalize_vo_locale(vo_lang),
    ) {
        (Some(saved), Some(requested)) => saved == requested,
        _ => state.vo_lang.eq_ignore_ascii_case(vo_lang),
    };
    if state.game_id != game_id
        || !same_locale
        || Some(state.installed_tag.as_str()) != installed_tag
    {
        return None;
    }
    if state.target_snapshot.is_none() {
        let mut fields = rustc_hash::FxHashSet::default();
        if !state
            .main_manifest_ids
            .iter()
            .any(|(field, _)| field == "game")
            || state
                .main_manifest_ids
                .iter()
                .any(|(field, id)| id.is_empty() || !fields.insert(field))
        {
            return None;
        }
        for asset in &state.patch_assets {
            super::assembly::validate_asset_name(&asset.target_file_path).ok()?;
            if let Some(path) = &asset.original_file_path {
                super::assembly::validate_asset_name(path).ok()?;
            }
            if !valid_hash(&asset.target_file_hash) {
                return None;
            }
            if asset.patch_method == PatchMethod::DownloadOver
                && !fields.contains(&asset.matching_field)
            {
                return None;
            }
            if matches!(
                asset.patch_method,
                PatchMethod::CopyOver | PatchMethod::Patch
            ) {
                validate_patch_range(asset).ok()?;
                if !state.downloaded_chunks.contains(&asset.patch_name) {
                    return None;
                }
            }
        }
        for path in &state.deleted_files {
            super::assembly::validate_asset_name(path).ok()?;
        }
    }
    Some(state)
}

#[cfg(feature = "preinstall")]
pub fn save_preinstall_state(game_dir: &Path, state: &PreinstallState) -> SophonResult<()> {
    validate_tag(&state.tag)?;
    if let Some(snapshot) = &state.target_snapshot {
        snapshot.validate(state)?;
    }
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
    fs::File::open(game_dir)?.sync_all()?;
    Ok(())
}

pub fn load_preinstall_state(game_dir: &Path, tag: &str) -> SophonResult<PreinstallState> {
    validate_tag(tag)?;
    let path = PreinstallState::state_file_path(game_dir, tag);
    let content = fs::read_to_string(&path).map_err(|err| {
        SophonError::PreinstallStateInvalid(format!("Failed to read preinstall state: {err}"))
    })?;
    let state: PreinstallState = serde_json::from_str(&content).map_err(|err| {
        SophonError::PreinstallStateInvalid(format!("Failed to parse preinstall state: {err}"))
    })?;
    if state.tag != tag {
        return Err(invalid_state(
            "Requested target does not match the saved package",
        ));
    }
    if let Some(snapshot) = &state.target_snapshot {
        snapshot.validate(&state)?;
    }
    Ok(state)
}

#[cfg(feature = "preinstall")]
pub fn delete_preinstall_state(game_dir: &Path, tag: &str) {
    let _ = fs::remove_file(PreinstallState::state_file_path(game_dir, tag));
    let _ = fs::remove_file(PreinstallState::marker_file_path(game_dir, tag));
}
