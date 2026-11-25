//! High-level Sophon entry point. Owns the HTTP client, game identity, paths,
//! verify mode, and resume state, hiding the internal installer plumbing.

#[cfg(feature = "preinstall")]
use std::collections::HashMap;
#[cfg(any(feature = "download", feature = "update"))]
use std::future::Future;
#[cfg(any(test, feature = "download", feature = "update", feature = "preinstall"))]
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(any(feature = "download", feature = "update", feature = "preinstall"))]
use std::sync::Arc;
#[cfg(any(feature = "download", feature = "update"))]
use std::sync::OnceLock;
#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(test, feature = "download", feature = "update"))]
use rustc_hash::FxHashMap;

use crate::SophonProgress;
use crate::client::DownloadClient;
#[cfg(any(feature = "download", feature = "update", feature = "preinstall"))]
use crate::game_installer::StateSaver;
#[cfg(feature = "discovery")]
use crate::game_installer::UpdateInfo;
use crate::game_installer::{self, VerifyMode};
#[cfg(any(test, feature = "download", feature = "update", feature = "preinstall"))]
use crate::game_installer::{CompletedFiles, DownloadHandle, InstallOptions, ResumeContext};
#[cfg(any(feature = "download", feature = "update"))]
use crate::game_installer::{InstallCallbacks, SophonError};
#[cfg(any(test, feature = "download", feature = "update", feature = "preinstall"))]
use crate::types::DownloadType;
use crate::types::{DownloadState, ResumeInfo};

/// Closure accepted by every progress-bearing method. Implementations must be
/// `'static` so the library can share the callback across worker tasks.
pub trait ProgressFn: Fn(SophonProgress) + Send + Sync + 'static {}
impl<T: Fn(SophonProgress) + Send + Sync + 'static> ProgressFn for T {}

/// Per-game resume-state filename within `state_dir`.
const STATE_FILE_PREFIX: &str = ".sophon_download_state_";
const STATE_FILE_SUFFIX: &str = ".json";
#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
static STATE_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Primary entry point. Owns its manifest-fetch HTTP client, game identity,
/// install path, resume-state directory, and verify mode. Chunk downloads
/// build their own HTTP/1.1 client internally regardless of `client`.
pub struct Sophon {
    #[cfg_attr(
        not(any(
            feature = "download",
            feature = "update",
            feature = "verify",
            feature = "preinstall"
        )),
        allow(dead_code)
    )]
    client: reqwest::Client,
    game_id: String,
    vo_lang: String,
    game_dir: PathBuf,
    state_dir: PathBuf,
    #[cfg_attr(not(any(feature = "download", feature = "update")), allow(dead_code))]
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

    /// Consume the builder and create a downloader. Defaults the state
    /// directory to the game directory and constructs a tuned client unless one
    /// was set.
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

    /// Game identifier used for branch and build requests.
    pub fn game_id(&self) -> &str {
        &self.game_id
    }

    /// Installation directory read and modified by download operations.
    pub fn game_dir(&self) -> &Path {
        &self.game_dir
    }

    /// Voice-over locale used when selecting language-specific manifests.
    pub fn vo_lang(&self) -> &str {
        &self.vo_lang
    }

    /// Fresh download with automatic resume. Reuses on-disk chunks when the
    /// persisted manifest hash matches the remote; otherwise discards stale
    /// state and downloads fresh. Removes the resume state on success.
    #[cfg(feature = "download")]
    pub fn download(
        &self,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> impl Future<Output = Result<(), SophonError>> + Send + 'static {
        let progress = Arc::new(on_progress);
        progress(SophonProgress::FetchingManifest);

        let client = self.client.clone();
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let game_dir = self.game_dir.clone();
        let state_dir = self.state_dir.clone();
        let state_file = self.state_file();
        let verify_mode = self.verify_mode;
        let handle = handle.clone();

        async move {
            let (installers, tag, manifest_hash) =
                game_installer::build_installers(&client, &game_id, &vo_lang).await?;

            let (resume, options) = Self::resolve_resume_owned(
                &state_dir,
                &game_dir,
                &game_id,
                &vo_lang,
                verify_mode,
                &handle,
                DownloadType::Fresh,
                &manifest_hash,
            );
            let callbacks = Self::build_callbacks_owned(
                state_file.clone(),
                game_id.clone(),
                vo_lang.clone(),
                game_dir.clone(),
                progress.clone(),
                DownloadType::Fresh,
                None,
                manifest_hash,
            );
            let vo_langs = vec![vo_lang.clone()];

            let result = game_installer::install(
                installers,
                &game_dir,
                vec![],
                &tag,
                resume,
                options,
                callbacks,
                &game_id,
                &vo_langs,
            )
            .await;

            if result.is_ok() {
                let _ = std::fs::remove_file(&state_file);
            }
            progress(SophonProgress::Finished);
            result
        }
    }

    /// Update an existing installation to the latest remote tag. Reads the
    /// installed tag, builds an update plan, and installs. Returns
    /// [`SophonError::NoInstalledVersion`] when no installation is present.
    #[cfg(feature = "update")]
    pub fn update(
        &self,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> impl Future<Output = Result<(), SophonError>> + Send + 'static {
        let progress = Arc::new(on_progress);
        let client = self.client.clone();
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let game_dir = self.game_dir.clone();
        let state_dir = self.state_dir.clone();
        let state_file = self.state_file();
        let verify_mode = self.verify_mode;
        let handle = handle.clone();

        async move {
            let current_tag = game_installer::read_installed_tag(&game_dir)
                .ok_or(SophonError::NoInstalledVersion)?;

            progress(SophonProgress::FetchingManifest);

            let (installers, deleted_files, new_tag, manifest_hash) =
                game_installer::build_update_installers(
                    &client,
                    &game_id,
                    &vo_lang,
                    &current_tag,
                    &game_dir,
                )
                .await?;

            let (resume, options) = Self::resolve_resume_owned(
                &state_dir,
                &game_dir,
                &game_id,
                &vo_lang,
                verify_mode,
                &handle,
                DownloadType::Update,
                &manifest_hash,
            );
            let callbacks = Self::build_callbacks_owned(
                state_file.clone(),
                game_id.clone(),
                vo_lang.clone(),
                game_dir.clone(),
                progress.clone(),
                DownloadType::Update,
                Some(current_tag),
                manifest_hash,
            );
            let vo_langs = vec![vo_lang.clone()];

            let result = game_installer::install(
                installers,
                &game_dir,
                deleted_files,
                &new_tag,
                resume,
                options,
                callbacks,
                &game_id,
                &vo_langs,
            )
            .await;

            if result.is_ok() {
                let _ = std::fs::remove_file(&state_file);
            }
            progress(SophonProgress::Finished);
            result
        }
    }

    /// Pre-download an upcoming version's patch package into `game_dir`,
    /// persisting chunk progress for resume. Completed packages retain their
    /// exact target metadata for release-day application. Returns
    /// [`SophonError::NoPreinstallAvailable`] when no preinstall exists.
    #[cfg(feature = "preinstall")]
    pub fn preinstall(
        &self,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> impl Future<Output = Result<(), SophonError>> + Send + 'static {
        let progress = Arc::new(on_progress);
        progress(SophonProgress::FetchingManifest);

        let client = self.client.clone();
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let game_dir = self.game_dir.clone();
        let state_dir = self.state_dir.clone();
        let state_file = self.state_file();
        let handle = handle.clone();

        async move {
            let plan =
                game_installer::build_preinstall_plan(&client, &game_id, &vo_lang, &game_dir)
                    .await?;

            let prev_chunks: HashMap<String, u64> = Self::load_state_owned(&state_dir, &game_id)
                .filter(|s| s.game_id == game_id && s.download_type == DownloadType::Preinstall)
                .map(|s| s.downloaded_chunks)
                .unwrap_or_default();

            let state_saver = Self::build_preinstall_saver_owned(
                state_file.clone(),
                game_id.clone(),
                vo_lang.clone(),
                game_dir.clone(),
            );

            game_installer::preinstall_download(
                &client,
                plan,
                &game_dir,
                &game_id,
                &vo_lang,
                handle,
                progress.clone(),
                state_saver,
                prev_chunks,
            )
            .await?;

            let _ = std::fs::remove_file(&state_file);
            progress(SophonProgress::Finished);
            Ok(())
        }
    }

    /// Apply a previously downloaded preinstall package identified by
    /// `preinstall_tag` and verify the result. New packages use saved target
    /// metadata even after upstream pre-download metadata disappears. Payload
    /// chunks may still require the CDN. Legacy packages require exact-target
    /// metadata recovery; failure preserves their cached state and payloads.
    #[cfg(feature = "preinstall")]
    pub fn apply_preinstall(
        &self,
        preinstall_tag: &str,
        handle: &DownloadHandle,
        on_progress: impl ProgressFn,
    ) -> impl Future<Output = Result<(), SophonError>> + Send + 'static {
        let progress = Arc::new(on_progress);
        let client = self.client.clone();
        let game_dir = self.game_dir.clone();
        let preinstall_tag = preinstall_tag.to_string();
        let handle = handle.clone();

        async move {
            game_installer::apply_preinstall(&client, &game_dir, &preinstall_tag, progress, &handle)
                .await
        }
    }

    /// Query update and preinstall availability and sizes without mutating
    /// disk.
    ///
    /// The returned future is `Send` and owns its data, so it can be spawned on
    /// a multi-threaded runtime. The borrow of `&self` ends at the call site.
    #[cfg(feature = "discovery")]
    pub fn check_update(
        &self,
    ) -> impl Future<Output = Result<UpdateInfo, SophonError>> + Send + 'static {
        let client = self.client.clone();
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let game_dir = self.game_dir.clone();
        async move { game_installer::check_update(&client, &game_id, &vo_lang, &game_dir).await }
    }

    /// Hash installed files and re-download missing or damaged files.
    ///
    /// This explicit repair bypasses persisted verification-cache hits, honors
    /// the selected voice language and game resource exclusions, and
    /// regenerates launcher-owned metadata. `Ok(())` and `Finished` are emitted
    /// only after successful repair and regeneration. Plugins/SDKs are outside
    /// this scope.
    #[cfg(feature = "verify")]
    pub fn verify_integrity(
        &self,
        on_progress: impl ProgressFn,
    ) -> impl Future<Output = Result<(), SophonError>> + Send + 'static {
        let client = self.client.clone();
        let game_id = self.game_id.clone();
        let vo_lang = self.vo_lang.clone();
        let game_dir = self.game_dir.clone();
        async move {
            game_installer::verify_integrity(&client, &game_id, &vo_lang, &game_dir, on_progress)
                .await
        }
    }

    /// `true` iff a parseable resume state for this game exists in `state_dir`.
    pub fn has_resume_state(&self) -> bool {
        load_download_state(&self.state_dir, &self.game_id).is_some()
    }

    /// Resume state summary, if a parseable state exists.
    pub fn resume_info(&self) -> Option<ResumeInfo> {
        load_download_state(&self.state_dir, &self.game_id).map(|s| ResumeInfo {
            game_id: s.game_id,
            download_type: s.download_type,
        })
    }

    /// Installed game tag, if an installation is present in `game_dir`.
    pub fn installed_tag(&self) -> Option<String> {
        game_installer::read_installed_tag(&self.game_dir)
    }

    #[cfg(any(test, feature = "download", feature = "update", feature = "preinstall"))]
    fn state_file(&self) -> PathBuf {
        state_file_path(&self.state_dir, &self.game_id)
    }

    #[cfg(test)]
    fn load_state(&self) -> Option<DownloadState> {
        load_download_state(&self.state_dir, &self.game_id)
    }

    /// Static variant of [`load_state`](Self::load_state) taking the fields
    /// that drive it. Used by `Send`-returning public methods that have moved
    /// `&self` fields into owned locals before the first `await`.
    #[cfg(any(test, feature = "download", feature = "update", feature = "preinstall"))]
    fn load_state_owned(state_dir: &Path, game_id: &str) -> Option<DownloadState> {
        load_download_state(state_dir, game_id)
    }

    /// Decide whether the persisted state can resume the fresh manifest. Reuses
    /// chunks when game id, vo language, install path, download type, and
    /// manifest hash all match; otherwise discards the stale `chunks/` dir.
    #[cfg(test)]
    fn resolve_resume(
        &self,
        handle: &DownloadHandle,
        download_type: DownloadType,
        manifest_hash: &str,
    ) -> (ResumeContext, InstallOptions) {
        Self::resolve_resume_owned(
            &self.state_dir,
            &self.game_dir,
            &self.game_id,
            &self.vo_lang,
            self.verify_mode,
            handle,
            download_type,
            manifest_hash,
        )
    }

    /// Static variant of [`resolve_resume`](Self::resolve_resume) taking the
    /// `Sophon` fields by reference so the public `Send`-returning methods can
    /// call it from inside an `async move` block after cloning `&self` fields.
    #[cfg(any(test, feature = "download", feature = "update"))]
    #[allow(clippy::too_many_arguments)]
    fn resolve_resume_owned(
        state_dir: &Path,
        game_dir: &Path,
        game_id: &str,
        vo_lang: &str,
        verify_mode: VerifyMode,
        handle: &DownloadHandle,
        download_type: DownloadType,
        manifest_hash: &str,
    ) -> (ResumeContext, InstallOptions) {
        let prev = Self::load_state_owned(state_dir, game_id);
        let matches = prev.as_ref().is_some_and(|s| {
            s.game_id == game_id
                && s.vo_lang == vo_lang
                && s.output_path == game_dir.to_string_lossy()
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
                // ponytail: removes whole chunks dir on hash mismatch;
                // tolerates missing dir
                let _ = std::fs::remove_dir_all(game_dir.join("chunks"));
            }
            (empty_resume(), false)
        };

        let options = InstallOptions {
            is_preinstall: false,
            is_resume,
            handle: handle.clone(),
            verify_mode,
        };
        (resume, options)
    }

    /// Static variant of [`build_callbacks`](Self::build_callbacks) taking the
    /// `Sophon` fields by value/clone so the public `Send`-returning methods
    /// can call it from inside an `async move` block.
    #[cfg(any(feature = "download", feature = "update"))]
    #[allow(clippy::too_many_arguments)]
    fn build_callbacks_owned(
        state_file: PathBuf,
        game_id: String,
        vo_lang: String,
        game_dir: PathBuf,
        progress: Arc<dyn Fn(SophonProgress) + Send + Sync>,
        download_type: DownloadType,
        current_tag: Option<String>,
        manifest_hash: String,
    ) -> InstallCallbacks {
        let output_path = game_dir.to_string_lossy().into_owned();

        let state_saver: StateSaver = Arc::new(move |entries: &[(&str, u64)]| {
            let state = DownloadStateRef {
                game_id: &game_id,
                vo_lang: &vo_lang,
                output_path: &output_path,
                download_type: &download_type,
                current_tag: current_tag.as_deref(),
                manifest_hash: &manifest_hash,
                downloaded_chunks: entries,
                completed_files: &[],
            };
            atomic_write_serialized(&state_file, &state);
        });

        InstallCallbacks {
            updater: progress,
            state_saver,
            completion_state: Arc::new(OnceLock::new()),
        }
    }

    /// Static variant of [`build_preinstall_saver`](Self::build_preinstall_saver).
    #[cfg(feature = "preinstall")]
    fn build_preinstall_saver_owned(
        state_file: PathBuf,
        game_id: String,
        vo_lang: String,
        game_dir: PathBuf,
    ) -> StateSaver {
        let output_path = game_dir.to_string_lossy().into_owned();

        Arc::new(move |entries: &[(&str, u64)]| {
            let state = DownloadStateRef {
                game_id: &game_id,
                vo_lang: &vo_lang,
                output_path: &output_path,
                download_type: &DownloadType::Preinstall,
                current_tag: None,
                manifest_hash: "",
                downloaded_chunks: entries,
                completed_files: &[],
            };
            atomic_write_serialized(&state_file, &state);
        })
    }
}

#[cfg(any(test, feature = "download", feature = "update"))]
fn empty_resume() -> ResumeContext {
    ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: FxHashMap::default(),
        resume_seed: CompletedFiles::default(),
    }
}

/// Write `data` to `target` via a temp file then rename, so a crash mid-write
/// leaves the previous state intact. Silently ignores IO errors.
#[cfg(test)]
fn atomic_write(target: &Path, data: &[u8]) {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok((tmp, mut file)) = create_state_temp_file(target) else {
        return;
    };
    let temp_file = StateTempFile::new(tmp);
    if file.write_all(data).is_err() {
        return;
    }
    drop(file);
    let _ = temp_file.commit(target);
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
fn create_state_temp_file(target: &Path) -> std::io::Result<(PathBuf, std::fs::File)> {
    let name = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    for _ in 0..16 {
        let sequence = STATE_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temp_path =
            target.with_file_name(format!(".{name}.{}.{}.tmp", std::process::id(), sequence));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "unable to create a unique state temp file",
    ))
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
struct StateTempFile {
    path: PathBuf,
    committed: bool,
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
impl StateTempFile {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            committed: false,
        }
    }

    fn commit(mut self, target: &Path) -> std::io::Result<()> {
        std::fs::rename(&self.path, target)?;
        self.committed = true;
        Ok(())
    }
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
impl Drop for StateTempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Serializes `&[(&str, u64)]` as a JSON map.
#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
struct ChunkSliceMap<'a>(&'a [(&'a str, u64)]);

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
impl serde::Serialize for ChunkSliceMap<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (k, v) in self.0 {
            map.serialize_entry(k, v)?;
        }
        map.end()
    }
}

/// Write a downloaded-chunks snapshot through the production state-file
/// path (serialize + temp file + rename) so benchmarks cover the full save.
#[cfg(feature = "benchmark")]
#[doc(hidden)]
pub fn bench_write_state_file(target: &std::path::Path, entries: &[(&str, u64)]) {
    let state = DownloadStateRef {
        game_id: "bench",
        vo_lang: "en-us",
        output_path: "bench",
        download_type: &DownloadType::Fresh,
        current_tag: None,
        manifest_hash: "bench",
        downloaded_chunks: entries,
        completed_files: &[],
    };
    atomic_write_serialized(target, &state);
}

/// Serialize the same state as `bench_write_state_file` into memory,
/// isolating the CPU cost of JSON encoding from file I/O.
#[cfg(feature = "benchmark")]
#[doc(hidden)]
pub fn bench_serialize_state(entries: &[(&str, u64)]) -> usize {
    let state = DownloadStateRef {
        game_id: "bench",
        vo_lang: "en-us",
        output_path: "bench",
        download_type: &DownloadType::Fresh,
        current_tag: None,
        manifest_hash: "bench",
        downloaded_chunks: entries,
        completed_files: &[],
    };
    let mut buf = Vec::with_capacity(4 * 1024 * 1024);
    serde_json::to_writer(&mut buf, &state).expect("serialize");
    buf.len()
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DownloadStateRef<'a> {
    game_id: &'a str,
    vo_lang: &'a str,
    output_path: &'a str,
    download_type: &'a DownloadType,
    current_tag: Option<&'a str>,
    manifest_hash: &'a str,
    #[serde(serialize_with = "serialize_chunk_slice")]
    downloaded_chunks: &'a [(&'a str, u64)],
    completed_files: &'a [usize],
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
fn serialize_chunk_slice<S: serde::Serializer>(
    chunks: &[(&str, u64)],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize as _;
    ChunkSliceMap(chunks).serialize(serializer)
}

#[cfg(any(
    test,
    feature = "download",
    feature = "update",
    feature = "preinstall",
    feature = "benchmark"
))]
fn atomic_write_serialized<T: serde::Serialize>(target: &Path, value: &T) {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok((tmp, file)) = create_state_temp_file(target) else {
        return;
    };
    let temp_file = StateTempFile::new(tmp);
    let write_result = (|| -> std::io::Result<()> {
        let mut writer = std::io::BufWriter::with_capacity(256 * 1024, file);
        // Resume state is machine-readable; compact JSON reduces checkpoint
        // writes.
        serde_json::to_writer(&mut writer, value).map_err(std::io::Error::other)?;
        writer.flush()?;
        Ok(())
    })();
    if write_result.is_err() {
        return;
    }
    let _ = temp_file.commit(target);
}

#[cfg(all(test, feature = "bench-diagnostics"))]
fn atomic_write_pretty_serialized<T: serde::Serialize>(target: &Path, value: &T) {
    if let Some(parent) = target.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok((tmp, file)) = create_state_temp_file(target) else {
        return;
    };
    let temp_file = StateTempFile::new(tmp);
    let write_result = (|| -> std::io::Result<()> {
        let mut writer = std::io::BufWriter::with_capacity(256 * 1024, file);
        serde_json::to_writer_pretty(&mut writer, value).map_err(std::io::Error::other)?;
        writer.flush()?;
        Ok(())
    })();
    if write_result.is_err() {
        return;
    }
    let _ = temp_file.commit(target);
}

/// Per-game resume-state file path under `state_dir`.
pub fn state_file_path(state_dir: &Path, game_id: &str) -> PathBuf {
    state_dir.join(format!(
        "{}{}{}",
        STATE_FILE_PREFIX, game_id, STATE_FILE_SUFFIX
    ))
}

/// Load the persisted resume state for `game_id` from `state_dir`. Returns
/// `None` when the file is missing or unparseable. Pure file read; no HTTP.
pub fn load_download_state(state_dir: &Path, game_id: &str) -> Option<DownloadState> {
    let content = std::fs::read_to_string(state_file_path(state_dir, game_id)).ok()?;
    serde_json::from_str(&content).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tempfile::tempdir;

    fn make_sophon(game_id: &str, game_dir: &Path, state_dir: &Path) -> Sophon {
        Sophon::builder(game_id.to_string(), game_dir.to_path_buf())
            .state_dir(state_dir.to_path_buf())
            .build()
    }

    fn write_state(s: &Sophon, state: &DownloadState) {
        let json = serde_json::to_vec_pretty(state).unwrap();
        atomic_write(&s.state_file(), &json);
    }

    #[test]
    fn borrowed_download_state_serialization_matches_owned_state() {
        for download_type in [
            DownloadType::Fresh,
            DownloadType::Update,
            DownloadType::Preinstall,
        ] {
            for current_tag in [None, Some("release-2.0")] {
                for downloaded_chunks in [
                    Vec::<(&str, u64)>::new(),
                    vec![("unicode-雪.bin", 123), ("escaped\n\"chunk", 456)],
                ] {
                    let state = DownloadState {
                        game_id: "nap".into(),
                        vo_lang: "en-US".into(),
                        output_path: "/games/test".into(),
                        download_type: download_type.clone(),
                        current_tag: current_tag.map(str::to_owned),
                        manifest_hash: "a1b2c3".into(),
                        downloaded_chunks: downloaded_chunks
                            .iter()
                            .map(|(k, v)| (k.to_string(), *v))
                            .collect(),
                        completed_files: CompletedFiles::default(),
                    };
                    let view = DownloadStateRef {
                        game_id: &state.game_id,
                        vo_lang: &state.vo_lang,
                        output_path: &state.output_path,
                        download_type: &state.download_type,
                        current_tag: state.current_tag.as_deref(),
                        manifest_hash: &state.manifest_hash,
                        downloaded_chunks: &downloaded_chunks,
                        completed_files: &[],
                    };
                    let owned = serde_json::to_vec_pretty(&state).unwrap();
                    let borrowed = serde_json::to_vec_pretty(&view).unwrap();
                    // HashMap and Vec may produce different key orderings.
                    // Compare as parsed values (order-independent) for the
                    // map fields, and verify byte equality only for empty maps.
                    if downloaded_chunks.is_empty() {
                        assert_eq!(owned, borrowed);
                    } else {
                        let owned_val: serde_json::Value = serde_json::from_slice(&owned).unwrap();
                        let borrowed_val: serde_json::Value =
                            serde_json::from_slice(&borrowed).unwrap();
                        assert_eq!(owned_val, borrowed_val);
                    }
                    let compact = serde_json::to_vec(&view).unwrap();
                    assert!(!compact.contains(&b'\n'));
                    let decoded: DownloadState = serde_json::from_slice(&compact).unwrap();
                    assert_eq!(decoded.game_id, state.game_id);
                    assert_eq!(decoded.vo_lang, state.vo_lang);
                    assert_eq!(decoded.output_path, state.output_path);
                    assert_eq!(decoded.download_type, state.download_type);
                    assert_eq!(decoded.current_tag, state.current_tag);
                    assert_eq!(decoded.manifest_hash, state.manifest_hash);
                    assert_eq!(decoded.downloaded_chunks, state.downloaded_chunks);
                    assert_eq!(
                        serde_json::to_value(decoded.completed_files).unwrap(),
                        serde_json::json!([])
                    );
                }
            }
        }
    }

    #[test]
    fn streamed_state_write_keeps_previous_file_on_serialize_error() {
        struct FailingState;

        impl serde::Serialize for FailingState {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                use serde::ser::SerializeMap;

                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("partial", &true)?;
                Err(serde::ser::Error::custom("forced serialization failure"))
            }
        }

        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, b"previous state").unwrap();

        atomic_write_serialized(&path, &FailingState);

        assert_eq!(std::fs::read(&path).unwrap(), b"previous state");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn concurrent_atomic_state_writes_use_distinct_temp_files() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("state.json");
        let writers = (0..8)
            .map(|index| {
                let path = path.clone();
                std::thread::spawn(move || {
                    let payload = format!("state-{index}-{}", "x".repeat(128 * 1024));
                    let expected = serde_json::to_string(&payload).unwrap();
                    atomic_write_serialized(&path, &payload);
                    expected
                })
            })
            .collect::<Vec<_>>();
        let expected = writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .collect::<Vec<_>>();

        let final_state = std::fs::read_to_string(&path).unwrap();
        assert!(expected.contains(&final_state));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[cfg(feature = "bench-diagnostics")]
    #[test]
    #[ignore = "measures resume-state map clone and serialization cost"]
    fn benchmark_download_state_snapshot_serialization() {
        use std::time::Instant;

        fn vm_hwm_kib() -> Option<u64> {
            let status = std::fs::read_to_string("/proc/self/status").ok()?;
            status.lines().find_map(|line| {
                line.strip_prefix("VmHWM:")
                    .and_then(|value| value.split_whitespace().next())
                    .and_then(|value| value.parse().ok())
            })
        }

        for entry_count in [10_000usize, 100_000, 1_000_000] {
            let downloaded_chunks: Vec<(String, u64)> = (0..entry_count)
                .map(|index| (format!("patch-chunk-{index:08}.bin"), 256 * 1024))
                .collect();
            let chunk_refs: Vec<(&str, u64)> = downloaded_chunks
                .iter()
                .map(|(k, v)| (k.as_str(), *v))
                .collect();
            let owned_chunks: std::collections::HashMap<String, u64> = chunk_refs
                .iter()
                .map(|(k, v)| ((*k).to_string(), *v))
                .collect();
            // chunk_refs is a 1:1 ordered view of downloaded_chunks, so the
            // map must keep every entry; a collapse here would silently skew
            // the serialization-size diagnostic below.
            debug_assert_eq!(owned_chunks.len(), chunk_refs.len());
            let iterations = if entry_count == 1_000_000 { 5 } else { 20 };
            let dir = tempdir().unwrap();
            let path = dir.path().join("state.json");
            let state = DownloadState {
                game_id: "nap".into(),
                vo_lang: "en-US".into(),
                output_path: "/games/test".into(),
                download_type: DownloadType::Preinstall,
                current_tag: None,
                manifest_hash: String::new(),
                downloaded_chunks: owned_chunks,
                completed_files: CompletedFiles::default(),
            };
            let state_view = DownloadStateRef {
                game_id: &state.game_id,
                vo_lang: &state.vo_lang,
                output_path: &state.output_path,
                download_type: &state.download_type,
                current_tag: state.current_tag.as_deref(),
                manifest_hash: &state.manifest_hash,
                downloaded_chunks: &chunk_refs,
                completed_files: &[],
            };
            let expected_json = serde_json::to_vec_pretty(&state).unwrap();
            let view_json = serde_json::to_vec_pretty(&state_view).unwrap();
            assert_eq!(expected_json, view_json);
            let compact_json = serde_json::to_vec(&state_view).unwrap();
            let compact_state: DownloadState = serde_json::from_slice(&compact_json).unwrap();
            assert_eq!(compact_state.downloaded_chunks, state.downloaded_chunks);
            assert_eq!(compact_state.download_type, state.download_type);
            assert_eq!(compact_state.current_tag, state.current_tag);

            if let Ok(mode) = std::env::var("IRMIN_STATE_BENCH_MODE") {
                assert!(matches!(mode.as_str(), "baseline" | "pretty" | "compact"));
                let before = vm_hwm_kib();
                let mode_json_bytes = if mode == "compact" {
                    compact_json.len()
                } else {
                    expected_json.len()
                };
                if mode == "baseline" {
                    let owned_state = DownloadState {
                        game_id: state.game_id.clone(),
                        vo_lang: state.vo_lang.clone(),
                        output_path: state.output_path.clone(),
                        download_type: state.download_type.clone(),
                        current_tag: state.current_tag.clone(),
                        manifest_hash: state.manifest_hash.clone(),
                        downloaded_chunks: state.downloaded_chunks.clone(),
                        completed_files: CompletedFiles::default(),
                    };
                    let json = serde_json::to_vec_pretty(&owned_state).unwrap();
                    atomic_write(&path, &json);
                } else if mode == "pretty" {
                    atomic_write_pretty_serialized(&path, &state_view);
                } else {
                    atomic_write_serialized(&path, &state_view);
                }
                let after = vm_hwm_kib();
                println!(
                    "download_state_peak mode={mode} entries={entry_count} json_bytes={} hwm_before_kib={before:?} hwm_after_kib={after:?}",
                    mode_json_bytes,
                );
                std::hint::black_box(std::fs::metadata(&path).unwrap().len());
                continue;
            }

            let mut baseline_elapsed = Vec::with_capacity(iterations);
            let mut streaming_elapsed = Vec::with_capacity(iterations);
            let mut compact_elapsed = Vec::with_capacity(iterations);
            for round in 0..iterations {
                let order = if round % 2 == 0 {
                    [0usize, 1, 2]
                } else {
                    [2usize, 1, 0]
                };
                for variant in order {
                    let started = Instant::now();
                    match variant {
                        0 => {
                            let owned_state = DownloadState {
                                game_id: state.game_id.clone(),
                                vo_lang: state.vo_lang.clone(),
                                output_path: state.output_path.clone(),
                                download_type: state.download_type.clone(),
                                current_tag: state.current_tag.clone(),
                                manifest_hash: state.manifest_hash.clone(),
                                downloaded_chunks: state.downloaded_chunks.clone(),
                                completed_files: CompletedFiles::default(),
                            };
                            let json = serde_json::to_vec_pretty(&owned_state).unwrap();
                            atomic_write(&path, &json);
                            baseline_elapsed.push(started.elapsed());
                        }
                        1 => {
                            atomic_write_pretty_serialized(&path, &state_view);
                            streaming_elapsed.push(started.elapsed());
                        }
                        _ => {
                            atomic_write_serialized(&path, &state_view);
                            compact_elapsed.push(started.elapsed());
                        }
                    }
                }
            }
            let output: DownloadState =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            assert_eq!(output.game_id, state.game_id);
            assert_eq!(output.vo_lang, state.vo_lang);
            assert_eq!(output.output_path, state.output_path);
            assert_eq!(output.downloaded_chunks, state.downloaded_chunks);
            assert_eq!(output.download_type, state.download_type);
            assert_eq!(output.current_tag, state.current_tag);
            assert_eq!(output.manifest_hash, state.manifest_hash);
            assert_eq!(
                serde_json::to_value(output.completed_files).unwrap(),
                serde_json::json!([])
            );
            let median = |samples: &mut [std::time::Duration]| {
                samples.sort_unstable();
                samples[samples.len() / 2]
            };
            let p95 = |samples: &mut [std::time::Duration]| {
                samples.sort_unstable();
                samples[samples.len() * 95 / 100]
            };
            println!(
                "download_state_snapshot entries={entry_count} iterations={iterations} pretty_bytes={} compact_bytes={} baseline_median_ms={} streaming_median_ms={} compact_median_ms={} baseline_p95_ms={} stream_p95_ms={} compact_p95_ms={}",
                expected_json.len(),
                compact_json.len(),
                median(&mut baseline_elapsed).as_micros() as f64 / 1000.0,
                median(&mut streaming_elapsed).as_micros() as f64 / 1000.0,
                median(&mut compact_elapsed).as_micros() as f64 / 1000.0,
                p95(&mut baseline_elapsed).as_micros() as f64 / 1000.0,
                p95(&mut streaming_elapsed).as_micros() as f64 / 1000.0,
                p95(&mut compact_elapsed).as_micros() as f64 / 1000.0,
            );
        }
    }

    fn sample_state(
        game_id: &str,
        vo_lang: &str,
        output_path: &str,
        hash: &str,
        chunks: HashMap<String, u64>,
    ) -> DownloadState {
        DownloadState {
            game_id: game_id.to_string(),
            vo_lang: vo_lang.to_string(),
            output_path: output_path.to_string(),
            download_type: DownloadType::Fresh,
            current_tag: None,
            manifest_hash: hash.to_string(),
            downloaded_chunks: chunks,
            completed_files: CompletedFiles::default(),
        }
    }

    #[test]
    fn state_file_isolates_per_game() {
        let tmp = tempdir().unwrap();
        let state = tmp.path().join("state");
        std::fs::create_dir_all(&state).unwrap();
        let a = make_sophon("gameA", &tmp.path().join("a"), &state);
        let b = make_sophon("gameB", &tmp.path().join("b"), &state);
        assert_ne!(a.state_file(), b.state_file());
    }

    #[test]
    fn load_state_round_trips() {
        let tmp = tempdir().unwrap();
        let game_dir = tmp.path().join("game");
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let s = make_sophon("gameA", &game_dir, &state_dir);

        let mut chunks = HashMap::new();
        chunks.insert("c0".to_string(), 10);
        write_state(
            &s,
            &sample_state("gameA", "en-US", &game_dir.to_string_lossy(), "h", chunks),
        );

        let loaded = s.load_state().expect("state loads");
        assert_eq!(loaded.game_id, "gameA");
        assert_eq!(loaded.manifest_hash, "h");
        assert_eq!(loaded.downloaded_chunks.get("c0"), Some(&10));
    }

    #[test]
    fn resolve_resume_reuses_when_hash_matches() {
        let tmp = tempdir().unwrap();
        let game_dir = tmp.path().join("game");
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        std::fs::create_dir_all(game_dir.join("chunks")).unwrap();
        std::fs::write(game_dir.join("chunks").join("c0.zstd"), b"x").unwrap();

        let s = make_sophon("gameA", &game_dir, &state_dir);
        let mut chunks = HashMap::new();
        chunks.insert("c0".to_string(), 10);
        write_state(
            &s,
            &sample_state("gameA", "en-US", &game_dir.to_string_lossy(), "h", chunks),
        );

        let (resume, options) = s.resolve_resume(&DownloadHandle::new(), DownloadType::Fresh, "h");
        assert!(options.is_resume);
        assert_eq!(resume.prev_manifest_hash, "h");
        assert_eq!(resume.prev_downloaded_chunks.get("c0"), Some(&10));
        assert!(
            game_dir.join("chunks").exists(),
            "chunks dir untouched on reuse"
        );
    }

    #[test]
    fn resolve_resume_discards_chunks_when_hash_differs() {
        let tmp = tempdir().unwrap();
        let game_dir = tmp.path().join("game");
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let chunks_dir = game_dir.join("chunks");
        std::fs::create_dir_all(&chunks_dir).unwrap();
        std::fs::write(chunks_dir.join("old.zstd"), b"x").unwrap();

        let s = make_sophon("gameA", &game_dir, &state_dir);
        write_state(
            &s,
            &sample_state(
                "gameA",
                "en-US",
                &game_dir.to_string_lossy(),
                "old",
                HashMap::new(),
            ),
        );

        let (resume, options) =
            s.resolve_resume(&DownloadHandle::new(), DownloadType::Fresh, "new");
        assert!(!options.is_resume);
        assert!(resume.prev_manifest_hash.is_empty());
        assert!(resume.prev_downloaded_chunks.is_empty());
        assert!(!chunks_dir.exists(), "stale chunks dir removed on mismatch");
    }

    #[test]
    fn resolve_resume_ignores_mismatched_vo_lang() {
        let tmp = tempdir().unwrap();
        let game_dir = tmp.path().join("game");
        let state_dir = tmp.path().join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let s = make_sophon("gameA", &game_dir, &state_dir);
        write_state(
            &s,
            &sample_state(
                "gameA",
                "zh-CN",
                &game_dir.to_string_lossy(),
                "h",
                HashMap::new(),
            ),
        );

        let (resume, options) = s.resolve_resume(&DownloadHandle::new(), DownloadType::Fresh, "h");
        assert!(!options.is_resume);
        assert!(resume.prev_manifest_hash.is_empty());
    }
}
