//! Sophon game downloader. Manifest-based chunk downloads with zstd
//! compression.

pub mod api_scrape;
pub mod client;
pub mod game_installer;
pub mod manifest;
pub mod progress;
pub mod proto_parse;
pub mod types;

use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use napi::bindgen_prelude::BigInt;
use napi::threadsafe_function::{ThreadsafeFunction, ThreadsafeFunctionCallMode};
use napi_derive::napi;

use game_installer::DownloadHandle;
use game_installer::installer::{InstallCallbacks, InstallOptions, ResumeContext};
use progress::SophonProgress;
use types::DownloadState;

pub use manifest::compute_content_manifest_hash;
pub use types::CHUNK_STATE_SAVE_INTERVAL;
pub use client::DownloadClient;

static HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
static ACTIVE_DOWNLOAD: OnceLock<Mutex<Option<DownloadHandle>>> = OnceLock::new();

/// Filename of the persisted download state within the state directory.
const DOWNLOAD_STATE_FILE: &str = ".sophon_download_state";

pub(crate) fn client() -> &'static reqwest::Client {
    HTTP_CLIENT
        .get()
        .expect("HTTP client not initialized; call initClient() first")
}

pub(crate) fn active_download() -> &'static Mutex<Option<DownloadHandle>> {
    ACTIVE_DOWNLOAD.get_or_init(|| Mutex::new(None))
}

/// Load persisted download state from `state_dir`. Returns `None` if the
/// file is missing or unparseable.
fn load_download_state(state_dir: &str) -> Option<DownloadState> {
    let path = PathBuf::from(state_dir).join(DOWNLOAD_STATE_FILE);
    let content = std::fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

/// Initialize the HTTP client. Must be called once before any download.
#[napi]
pub fn init_client() -> napi::Result<()> {
    HTTP_CLIENT
        .set(reqwest::Client::new())
        .map_err(|_| napi::Error::from_reason("init_client already called"))?;
    Ok(())
}

/// Builds a closure `Fn(SophonProgress) + Send + Sync + Clone + 'static` that
/// serializes each progress event to a JSON string and forwards it to the
/// provided ThreadsafeFunction.
fn make_progress_emitter(
    tsfn: ThreadsafeFunction<String>,
) -> Arc<dyn Fn(SophonProgress) + Send + Sync> {
    let inner = Arc::new(tsfn);
    Arc::new(move |progress: SophonProgress| {
        if let Ok(json) = serde_json::to_string(&progress) {
            let _ = inner.call(Ok(json), ThreadsafeFunctionCallMode::NonBlocking);
        }
    })
}

/// Downloads a fresh game installation.
#[napi]
pub async fn sophon_download(
    game_id: String,
    vo_lang: String,
    output_path: String,
    on_progress: ThreadsafeFunction<String>,
) -> napi::Result<()> {
    let game_dir = PathBuf::from(&output_path);
    let updater = make_progress_emitter(on_progress);

    updater(SophonProgress::FetchingManifest);

    let (installers, tag, game_code) =
        game_installer::build_installers(client(), &game_id, &vo_lang)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    let handle = DownloadHandle::new();
    *active_download().lock().unwrap() = Some(handle.clone());

    let options = InstallOptions {
        is_preinstall: false,
        is_resume: false,
        handle,
    };
    let callbacks = InstallCallbacks {
        updater: updater.clone(),
        state_saver: Arc::new(|_| {}),
        completion_state: Arc::new(OnceLock::new()),
    };
    let resume = ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: Default::default(),
        resume_seed: Default::default(),
    };
    let vo_langs = vec![vo_lang.clone()];

    game_installer::install(
        installers,
        &game_dir,
        vec![],
        &tag,
        resume,
        options,
        callbacks,
        &game_code,
        &vo_langs,
    )
    .await
    .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    *active_download().lock().unwrap() = None;
    updater(SophonProgress::Finished);
    Ok(())
}

/// Updates an existing game installation.
#[napi]
pub async fn sophon_update(
    game_id: String,
    vo_lang: String,
    output_path: String,
    on_progress: ThreadsafeFunction<String>,
) -> napi::Result<()> {
    let game_dir = PathBuf::from(&output_path);
    let updater = make_progress_emitter(on_progress);

    let current_tag = game_installer::read_installed_tag(&game_dir)
        .ok_or_else(|| napi::Error::from_reason("No installed version found — cannot update"))?;

    updater(SophonProgress::FetchingManifest);

    let (installers, deleted_files, new_tag, game_code) =
        game_installer::build_update_installers(client(), &game_id, &vo_lang, &current_tag, &game_dir)
            .await
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    let handle = DownloadHandle::new();
    *active_download().lock().unwrap() = Some(handle.clone());

    let options = InstallOptions {
        is_preinstall: false,
        is_resume: false,
        handle,
    };
    let callbacks = InstallCallbacks {
        updater: updater.clone(),
        state_saver: Arc::new(|_| {}),
        completion_state: Arc::new(OnceLock::new()),
    };
    let resume = ResumeContext {
        prev_manifest_hash: String::new(),
        prev_downloaded_chunks: Default::default(),
        resume_seed: Default::default(),
    };
    let vo_langs = vec![vo_lang.clone()];

    game_installer::install(
        installers,
        &game_dir,
        deleted_files,
        &new_tag,
        resume,
        options,
        callbacks,
        &game_code,
        &vo_langs,
    )
    .await
    .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    *active_download().lock().unwrap() = None;
    updater(SophonProgress::Finished);
    Ok(())
}

/// Pre-downloads an upcoming game version using patch-based preinstall.
#[napi]
pub async fn sophon_preinstall(
    game_id: String,
    vo_lang: String,
    output_path: String,
    on_progress: ThreadsafeFunction<String>,
) -> napi::Result<()> {
    let game_dir = PathBuf::from(&output_path);
    let updater = make_progress_emitter(on_progress);

    updater(SophonProgress::FetchingManifest);

    let plan = game_installer::build_preinstall_plan(client(), &game_id, &vo_lang, &game_dir)
        .await
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    let handle = DownloadHandle::new();
    *active_download().lock().unwrap() = Some(handle.clone());

    game_installer::preinstall_download(
        client(),
        &plan,
        &game_dir,
        &game_id,
        &vo_lang,
        handle,
        updater.clone(),
        Arc::new(|_| {}),
        Default::default(),
    )
    .await
    .map_err(|e| napi::Error::from_reason(e.to_string()))?;

    *active_download().lock().unwrap() = None;
    updater(SophonProgress::Finished);
    Ok(())
}

/// Applies a previously downloaded preinstall package.
#[napi]
pub async fn sophon_apply_preinstall(
    preinstall_tag: String,
    output_path: String,
) -> napi::Result<()> {
    let game_dir = PathBuf::from(&output_path);
    let handle = DownloadHandle::new();
    game_installer::apply_preinstall(
        client(),
        &game_dir,
        &preinstall_tag,
        Arc::new(|_| {}),
        &handle,
    )
    .await
    .map_err(|e| napi::Error::from_reason(e.to_string()))
}

/// Pauses the active download.
#[napi]
pub fn sophon_pause() -> napi::Result<()> {
    if let Some(h) = active_download().lock().unwrap().as_ref() {
        h.pause();
        Ok(())
    } else {
        Err(napi::Error::from_reason("No active download"))
    }
}

/// Resumes a paused download.
#[napi]
pub fn sophon_resume() -> napi::Result<()> {
    if let Some(h) = active_download().lock().unwrap().as_ref() {
        h.resume();
        Ok(())
    } else {
        Err(napi::Error::from_reason("No active download"))
    }
}

/// Cancels the active download.
#[napi]
pub fn sophon_cancel() -> napi::Result<()> {
    if let Some(h) = active_download().lock().unwrap().as_ref() {
        h.cancel();
        Ok(())
    } else {
        Err(napi::Error::from_reason("No active download"))
    }
}

/// Checks if an update is available for the game.
#[napi]
pub async fn sophon_check_update(
    game_id: String,
    vo_lang: String,
    output_path: String,
) -> napi::Result<UpdateInfoNapi> {
    let game_dir = PathBuf::from(&output_path);
    let info = game_installer::check_update(client(), &game_id, &vo_lang, &game_dir)
        .await
        .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    Ok(UpdateInfoNapi {
        update_available: info.update_available,
        preinstall_available: info.preinstall_available,
        preinstall_downloaded: info.preinstall_downloaded,
        current_tag: info.current_tag,
        remote_tag: info.remote_tag,
        preinstall_tag: info.preinstall_tag,
        update_compressed_size: BigInt::from(info.update_compressed_size),
        update_decompressed_size: BigInt::from(info.update_decompressed_size),
        preinstall_compressed_size: BigInt::from(info.preinstall_compressed_size),
        preinstall_decompressed_size: BigInt::from(info.preinstall_decompressed_size),
    })
}

/// Checks if there is a downloadable state to resume.
#[napi]
pub fn sophon_has_resume_state(state_dir: String) -> bool {
    load_download_state(&state_dir).is_some()
}

/// napi-facing wrapper for `game_installer::UpdateInfo`. Uses `BigInt` for
/// `u64` fields because napi-rs 3.10+ does not implement `ToNapiValue` for
/// `u64` directly (to avoid silent precision loss).
#[napi(object)]
pub struct UpdateInfoNapi {
    pub update_available: bool,
    pub preinstall_available: bool,
    pub preinstall_downloaded: bool,
    pub current_tag: Option<String>,
    pub remote_tag: String,
    pub preinstall_tag: Option<String>,
    pub update_compressed_size: BigInt,
    pub update_decompressed_size: BigInt,
    pub preinstall_compressed_size: BigInt,
    pub preinstall_decompressed_size: BigInt,
}

/// napi-facing mirror of `types::DownloadType`. Serialized as camelCase
/// strings to match the serde representation persisted in state files.
#[napi(string_enum = "camelCase")]
pub enum DownloadTypeNapi {
    Fresh,
    Update,
    Preinstall,
}

/// napi-facing wrapper for `types::ResumeInfo`.
#[napi(object)]
pub struct ResumeInfoNapi {
    pub game_id: String,
    pub download_type: DownloadTypeNapi,
}

/// Returns details about the resumable download state, if any.
#[napi]
pub fn sophon_get_resume_info(state_dir: String) -> Option<ResumeInfoNapi> {
    load_download_state(&state_dir).map(|s| ResumeInfoNapi {
        game_id: s.game_id,
        download_type: match s.download_type {
            types::DownloadType::Fresh => DownloadTypeNapi::Fresh,
            types::DownloadType::Update => DownloadTypeNapi::Update,
            types::DownloadType::Preinstall => DownloadTypeNapi::Preinstall,
        },
    })
}

/// Verifies the integrity of installed game files and re-downloads any
/// corrupted ones.
#[napi]
pub async fn sophon_verify_integrity(
    game_id: String,
    vo_lang: String,
    output_path: String,
    on_progress: ThreadsafeFunction<String>,
) -> napi::Result<()> {
    let game_dir = PathBuf::from(&output_path);
    let updater = make_progress_emitter(on_progress);

    game_installer::verify_integrity(client(), &game_id, &vo_lang, &game_dir, move |p| {
        updater(p)
    })
    .await
    .map_err(|e| napi::Error::from_reason(e.to_string()))?;
    Ok(())
}