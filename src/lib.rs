//! Sophon game downloader. Manifest-based chunk downloads with zstd
//! compression.

pub mod api_scrape;
pub mod client;
pub mod game_installer;
pub mod manifest;
pub mod progress;
pub mod proto_parse;
pub mod sophon;
pub mod types;

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use game_installer::SophonError;
use game_installer::installer::{InstallCallbacks, InstallOptions, ResumeContext};
use game_installer::UpdateInfo;

pub use client::DownloadClient;
pub use game_installer::DownloadHandle;
pub use game_installer::VerifyMode;
pub use manifest::compute_content_manifest_hash;
pub use progress::SophonProgress;
pub use sophon::{ProgressFn, Sophon, SophonBuilder, load_download_state, state_file_path};
pub use types::CHUNK_STATE_SAVE_INTERVAL;
pub use types::{DownloadState, DownloadType, ResumeInfo};

/// Progress callback for download and assembly events.
pub type ProgressUpdater = Arc<dyn Fn(SophonProgress) + Send + Sync>;
/// State saver callback for persisting download progress.
pub type StateSaver = Arc<dyn Fn(&std::collections::HashMap<String, u64>) + Send + Sync>;

/// Global allocator. Enabled under `sophon-profiling` for jemalloc stats.
#[cfg(all(unix, feature = "sophon-profiling"))]
#[global_allocator]
static GLOBAL_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

/// Downloads a fresh game installation. The caller supplies the
/// [`DownloadHandle`] so it can pause/resume/cancel the in-flight
/// download from another task.
///
/// `verify_mode` controls the trade-off between integrity checking and
/// CPU/IO overhead. See [`VerifyMode`] for details.
pub async fn sophon_download(
    client: &reqwest::Client,
    game_id: &str,
    vo_lang: &str,
    output_path: &str,
    handle: &DownloadHandle,
    on_progress: ProgressUpdater,
    verify_mode: VerifyMode,
) -> Result<(), SophonError> {
    let game_dir = PathBuf::from(output_path);
    on_progress(SophonProgress::FetchingManifest);

    let (installers, tag, _manifest_hash) =
        game_installer::build_installers(client, game_id, vo_lang).await?;

    let options = InstallOptions {
        is_preinstall: false,
        is_resume: false,
        handle: handle.clone(),
        verify_mode,
    };
    let callbacks = InstallCallbacks {
        updater: on_progress.clone(),
        state_saver: Arc::new(|_| {}),
        completion_state: Arc::new(OnceLock::new()),
    };
    let resume = ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: Default::default(),
        resume_seed: Default::default(),
    };
    let vo_langs = vec![vo_lang.to_string()];

    game_installer::install(
        installers,
        &game_dir,
        vec![],
        &tag,
        resume,
        options,
        callbacks,
        game_id,
        &vo_langs,
    )
    .await?;

    on_progress(SophonProgress::Finished);
    Ok(())
}

/// Updates an existing game installation. See [`sophon_download`] for the
/// `handle` and `verify_mode` parameters.
pub async fn sophon_update(
    client: &reqwest::Client,
    game_id: &str,
    vo_lang: &str,
    output_path: &str,
    handle: &DownloadHandle,
    on_progress: ProgressUpdater,
    verify_mode: VerifyMode,
) -> Result<(), SophonError> {
    let game_dir = PathBuf::from(output_path);
    let current_tag = game_installer::read_installed_tag(&game_dir)
        .ok_or(SophonError::NoInstalledVersion)?;

    on_progress(SophonProgress::FetchingManifest);

    let (installers, deleted_files, new_tag, _manifest_hash) =
        game_installer::build_update_installers(client, game_id, vo_lang, &current_tag, &game_dir)
            .await?;

    let options = InstallOptions {
        is_preinstall: false,
        is_resume: false,
        handle: handle.clone(),
        verify_mode,
    };
    let callbacks = InstallCallbacks {
        updater: on_progress.clone(),
        state_saver: Arc::new(|_| {}),
        completion_state: Arc::new(OnceLock::new()),
    };
    let resume = ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: Default::default(),
        resume_seed: Default::default(),
    };
    let vo_langs = vec![vo_lang.to_string()];

    game_installer::install(
        installers,
        &game_dir,
        deleted_files,
        &new_tag,
        resume,
        options,
        callbacks,
        game_id,
        &vo_langs,
    )
    .await?;

    on_progress(SophonProgress::Finished);
    Ok(())
}

/// Pre-downloads an upcoming game version using patch-based preinstall. See
/// [`sophon_download`] for the `handle` parameter.
pub async fn sophon_preinstall(
    client: &reqwest::Client,
    game_id: &str,
    vo_lang: &str,
    output_path: &str,
    handle: &DownloadHandle,
    on_progress: ProgressUpdater,
) -> Result<(), SophonError> {
    let game_dir = PathBuf::from(output_path);
    on_progress(SophonProgress::FetchingManifest);

    let plan = game_installer::build_preinstall_plan(client, game_id, vo_lang, &game_dir).await?;

    game_installer::preinstall_download(
        client,
        plan,
        &game_dir,
        game_id,
        vo_lang,
        handle.clone(),
        on_progress.clone(),
        Arc::new(|_| {}),
        Default::default(),
    )
    .await?;

    on_progress(SophonProgress::Finished);
    Ok(())
}

/// Applies a previously downloaded preinstall package. See
/// [`sophon_download`] for the `handle` parameter.
pub async fn sophon_apply_preinstall(
    client: &reqwest::Client,
    preinstall_tag: &str,
    output_path: &str,
    handle: &DownloadHandle,
    on_progress: ProgressUpdater,
) -> Result<(), SophonError> {
    let game_dir = PathBuf::from(output_path);
    game_installer::apply_preinstall(client, &game_dir, preinstall_tag, on_progress, handle).await
}

/// Checks if an update is available for the game.
pub async fn sophon_check_update(
    client: &reqwest::Client,
    game_id: &str,
    vo_lang: &str,
    output_path: &str,
) -> Result<UpdateInfo, SophonError> {
    let game_dir = PathBuf::from(output_path);
    game_installer::check_update(client, game_id, vo_lang, &game_dir).await
}

/// Verifies the integrity of installed game files and re-downloads any
/// corrupted ones.
pub async fn sophon_verify_integrity(
    client: &reqwest::Client,
    game_id: &str,
    vo_lang: &str,
    output_path: &str,
    on_progress: ProgressUpdater,
) -> Result<(), SophonError> {
    let game_dir = PathBuf::from(output_path);
    let updater = on_progress;
    game_installer::verify_integrity(client, game_id, vo_lang, &game_dir, move |p| updater(p))
        .await
}