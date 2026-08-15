//! High-level Sophon entry point. Owns the HTTP client, game identity, paths,
//! verify mode, and resume state, hiding the internal installer plumbing.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use rustc_hash::FxHashMap;

use crate::client::DownloadClient;
use crate::game_installer::{
    self, CompletedFiles, DownloadHandle, InstallCallbacks, InstallOptions, ResumeContext,
    SophonError, StateSaver, UpdateInfo, VerifyMode,
};
use crate::types::{DownloadState, DownloadType, ResumeInfo};
use crate::SophonProgress;

/// Closure accepted by every progress-bearing method. Implementations must be
/// `'static` so the library can share the callback across worker tasks.
pub trait ProgressFn: Fn(SophonProgress) + Send + Sync + 'static {}
impl<T: Fn(SophonProgress) + Send + Sync + 'static> ProgressFn for T {}

/// Per-game resume-state filename within `state_dir`.
const STATE_FILE_PREFIX: &str = ".sophon_download_state_";
const STATE_FILE_SUFFIX: &str = ".json";

/// Primary entry point. Owns its manifest-fetch HTTP client, game identity,
/// install path, resume-state directory, and verify mode. Chunk downloads
/// build their own HTTP/1.1 client internally regardless of `client`.
pub struct Sophon {
    client: reqwest::Client,
    game_id: String,
    vo_lang: String,
    game_dir: PathBuf,
    state_dir: PathBuf,
    verify_mode: VerifyMode,
}

/// Fluent builder for [`Sophon`].
pub struct SophonBuilder {
    client: Option<reqwest::Client>,
    game_id: String,
    vo_lang: String,
    game_dir: PathBuf,
    state_dir: Option<PathBuf>,
    verify_mode: VerifyMode,
}

impl SophonBuilder {
    /// Required: Sophon game id (e.g. `"osZYTRIqKLlt"`). Required: install dir.
    pub fn new(game_id: impl Into<String>, game_dir: impl Into<PathBuf>) -> Self {
        Self {
            client: None,
            game_id: game_id.into(),
            vo_lang: "en-US".to_string(),
            game_dir: game_dir.into(),
            state_dir: None,
            verify_mode: VerifyMode::Full,
        }
    }

    /// Voice-over language. Default `"en-US"`.
    pub fn vo_lang(mut self, lang: impl Into<String>) -> Self {
        self.vo_lang = lang.into();
        self
    }

    /// Directory for persisted resume state. Default: `game_dir`. Independent
    /// of the install dir so state can live in an app-data directory.
    pub fn state_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.state_dir = Some(dir.into());
        self
    }

    /// Integrity verification strictness. Default [`VerifyMode::Full`].
    pub fn verify_mode(mut self, mode: VerifyMode) -> Self {
        self.verify_mode = mode;
        self
    }

    /// Override the manifest-fetch HTTP client. Default: a tuned HTTP/1.1
    /// client. Chunk downloads always build their own client internally.
    pub fn client(mut self, client: reqwest::Client) -> Self {
        self.client = Some(client);
        self
    }

    pub fn build(self) -> Sophon {
        Sophon {
            client: self.client.unwrap_or_else(|| DownloadClient::new().0),
            game_id: self.game_id.clone(),
            vo_lang: self.vo_lang,
            game_dir: self.game_dir.clone(),
            state_dir: self.state_dir.unwrap_or_else(|| self.game_dir.clone()),
            verify_mode: self.verify_mode,
        }
    }
}

impl Sophon {
    /// Start a builder for the given game and install directory.
    pub fn builder(game_id: impl Into<String>, game_dir: impl Into<PathBuf>) -> SophonBuilder {
        SophonBuilder::new(game_id, game_dir)
    }

    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    pub fn game_dir(&self) -> &Path {
        &self.game_dir
    }

    pub fn vo_lang(&self) -> &str {
        &self.vo_lang
    }

    /// Fresh download with automatic resume. Reuses on-disk chunks when the
    /// persisted manifest hash matches the remote; otherwise discards stale
    /// state and downloads fresh. Removes the resume state on success.
    pub async fn download(
        &self,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> Result<(), SophonError> {
        let progress = Arc::new(on_progress);
        progress(SophonProgress::FetchingManifest);

        let (installers, tag, manifest_hash) =
            game_installer::build_installers(&self.client, &self.game_id, &self.vo_lang).await?;

        let (resume, options) = self.resolve_resume(handle, DownloadType::Fresh, &manifest_hash);
        let callbacks =
            self.build_callbacks(progress.clone(), DownloadType::Fresh, None, manifest_hash);
        let vo_langs = vec![self.vo_lang.clone()];

        let result = game_installer::install(
            installers,
            &self.game_dir,
            vec![],
            &tag,
            resume,
            options,
            callbacks,
            &self.game_id,
            &vo_langs,
        )
        .await;

        if result.is_ok() {
            let _ = std::fs::remove_file(self.state_file());
        }
        progress(SophonProgress::Finished);
        result
    }

    /// Update an existing installation to the latest remote tag. Reads the
    /// installed tag, builds an update plan, and installs. Returns
    /// [`SophonError::NoInstalledVersion`] when no installation is present.
    pub async fn update(
        &self,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> Result<(), SophonError> {
        let progress = Arc::new(on_progress);
        let current_tag = game_installer::read_installed_tag(&self.game_dir)
            .ok_or(SophonError::NoInstalledVersion)?;

        progress(SophonProgress::FetchingManifest);

        let (installers, deleted_files, new_tag, manifest_hash) = game_installer::build_update_installers(
            &self.client,
            &self.game_id,
            &self.vo_lang,
            &current_tag,
            &self.game_dir,
        )
        .await?;

        let (resume, options) = self.resolve_resume(handle, DownloadType::Update, &manifest_hash);
        let callbacks = self.build_callbacks(
            progress.clone(),
            DownloadType::Update,
            Some(current_tag),
            manifest_hash,
        );
        let vo_langs = vec![self.vo_lang.clone()];

        let result = game_installer::install(
            installers,
            &self.game_dir,
            deleted_files,
            &new_tag,
            resume,
            options,
            callbacks,
            &self.game_id,
            &vo_langs,
        )
        .await;

        if result.is_ok() {
            let _ = std::fs::remove_file(self.state_file());
        }
        progress(SophonProgress::Finished);
        result
    }

    /// Pre-download an upcoming version's patch package into `game_dir`,
    /// persisting chunk progress for resume. Returns
    /// [`SophonError::NoPreinstallAvailable`] when no preinstall exists.
    pub async fn preinstall(
        &self,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> Result<(), SophonError> {
        let progress = Arc::new(on_progress);
        progress(SophonProgress::FetchingManifest);

        let plan =
            game_installer::build_preinstall_plan(&self.client, &self.game_id, &self.vo_lang, &self.game_dir)
                .await?;

        let state_file = self.state_file();
        let prev_chunks: HashMap<String, u64> = self
            .load_state()
            .filter(|s| s.game_id == self.game_id && s.download_type == DownloadType::Preinstall)
            .map(|s| s.downloaded_chunks)
            .unwrap_or_default();

        let state_saver = self.build_preinstall_saver(state_file.clone());

        game_installer::preinstall_download(
            &self.client,
            plan,
            &self.game_dir,
            &self.game_id,
            &self.vo_lang,
            handle.clone(),
            progress.clone(),
            state_saver,
            prev_chunks.clone(),
        )
        .await?;

        let _ = std::fs::remove_file(&state_file);
        progress(SophonProgress::Finished);
        Ok(())
    }

    /// Apply a previously downloaded preinstall package identified by
    /// `preinstall_tag` and verify the result.
    pub async fn apply_preinstall(
        &self,
        preinstall_tag: &str,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> Result<(), SophonError> {
        let progress = Arc::new(on_progress);
        game_installer::apply_preinstall(&self.client, &self.game_dir, preinstall_tag, progress, handle)
            .await
    }

    /// Query update and preinstall availability and sizes without mutating disk.
    pub async fn check_update(&self) -> Result<UpdateInfo, SophonError> {
        game_installer::check_update(&self.client, &self.game_id, &self.vo_lang, &self.game_dir).await
    }

    /// Verify installed files and re-download any whose hash mismatches.
    pub async fn verify_integrity(&self, on_progress: impl ProgressFn) -> Result<(), SophonError> {
        game_installer::verify_integrity(&self.client, &self.game_id, &self.vo_lang, &self.game_dir, on_progress)
            .await
    }

    /// `true` iff a parseable resume state for this game exists in `state_dir`.
    pub fn has_resume_state(&self) -> bool {
        self.load_state().is_some()
    }

    /// Resume state summary, if a parseable state exists.
    pub fn resume_info(&self) -> Option<ResumeInfo> {
        self.load_state()
            .map(|s| ResumeInfo { game_id: s.game_id, download_type: s.download_type })
    }

    /// Installed game tag, if an installation is present in `game_dir`.
    pub fn installed_tag(&self) -> Option<String> {
        game_installer::read_installed_tag(&self.game_dir)
    }

    fn state_file(&self) -> PathBuf {
        self.state_dir
            .join(format!("{}{}{}", STATE_FILE_PREFIX, self.game_id, STATE_FILE_SUFFIX))
    }

    fn load_state(&self) -> Option<DownloadState> {
        let content = std::fs::read_to_string(self.state_file()).ok()?;
        serde_json::from_str(&content).ok()
    }

    /// Decide whether the persisted state can resume the fresh manifest. Reuses
    /// chunks when game id, vo language, install path, download type, and
    /// manifest hash all match; otherwise discards the stale `chunks/` dir.
    fn resolve_resume(
        &self,
        handle: &DownloadHandle,
        download_type: DownloadType,
        manifest_hash: &str,
    ) -> (ResumeContext, InstallOptions) {
        let prev = self.load_state();
        let matches = prev.as_ref().is_some_and(|s| {
            s.game_id == self.game_id
                && s.vo_lang == self.vo_lang
                && s.output_path == self.game_dir.to_string_lossy()
                && s.download_type == download_type
                && s.manifest_hash == manifest_hash
        });

        let (resume, is_resume) = if matches {
            let s = prev.unwrap();
            let chunks: FxHashMap<String, u64> = s.downloaded_chunks.into_iter().collect();
            let any = !chunks.is_empty();
            (
                ResumeContext {
                    prev_manifest_hash: s.manifest_hash,
                    prev_downloaded_chunks: chunks,
                    resume_seed: CompletedFiles::default(),
                },
                any,
            )
        } else {
            if prev.is_some() {
                // ponytail: removes whole chunks dir on hash mismatch; tolerates missing dir
                let _ = std::fs::remove_dir_all(self.game_dir.join("chunks"));
            }
            (empty_resume(), false)
        };

        let options = InstallOptions {
            is_preinstall: false,
            is_resume,
            handle: handle.clone(),
            verify_mode: self.verify_mode,
        };
        (resume, options)
    }

    fn build_callbacks(
        &self,
        progress: Arc<dyn Fn(SophonProgress) + Send + Sync>,
        download_type: DownloadType,
        current_tag: Option<String>,
        manifest_hash: String,
    ) -> InstallCallbacks {
        let state_file = self.state_file();
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let output_path = self.game_dir.to_string_lossy().into_owned();

        let state_saver: StateSaver = Arc::new(move |map: &HashMap<String, u64>| {
            let state = DownloadState {
                game_id: game_id.clone(),
                vo_lang: vo_lang.clone(),
                output_path: output_path.clone(),
                download_type: download_type.clone(),
                current_tag: current_tag.clone(),
                manifest_hash: manifest_hash.clone(),
                downloaded_chunks: map.clone(),
                completed_files: CompletedFiles::default(),
            };
            if let Ok(json) = serde_json::to_vec_pretty(&state) {
                atomic_write(&state_file, &json);
            }
        });

        InstallCallbacks {
            updater: progress,
            state_saver,
            completion_state: Arc::new(OnceLock::new()),
        }
    }

    fn build_preinstall_saver(&self, state_file: PathBuf) -> StateSaver {
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let output_path = self.game_dir.to_string_lossy().into_owned();

        Arc::new(move |map: &HashMap<String, u64>| {
            let state = DownloadState {
                game_id: game_id.clone(),
                vo_lang: vo_lang.clone(),
                output_path: output_path.clone(),
                download_type: DownloadType::Preinstall,
                current_tag: None,
                manifest_hash: String::new(),
                downloaded_chunks: map.clone(),
                completed_files: CompletedFiles::default(),
            };
            if let Ok(json) = serde_json::to_vec_pretty(&state) {
                atomic_write(&state_file, &json);
            }
        })
    }
}

fn empty_resume() -> ResumeContext {
    ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: FxHashMap::default(),
        resume_seed: CompletedFiles::default(),
    }
}

/// Write `data` to `target` via a temp file then rename, so a crash mid-write
/// leaves the previous state intact. Silently ignores IO errors.
fn atomic_write(target: &Path, data: &[u8]) {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let pid = std::process::id();
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = target.with_file_name(format!(".{name}.{pid}.tmp"));
    if std::fs::write(&tmp, data).is_err() {
        return;
    }
    let _ = std::fs::rename(&tmp, target);
}
