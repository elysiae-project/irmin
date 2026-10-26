//! Sophon game downloader. Manifest-based chunk downloads with zstd
//! compression.

/// Wire-format models for game branches, builds, and download metadata.
pub mod api_scrape;
/// HTTP client wrappers used for API and chunk requests.
pub mod client;
/// Installer, download, verification, and preinstall operations.
pub mod game_installer;
/// Manifest identity helpers.
pub mod manifest;
/// Progress events and IPC-friendly errors.
pub mod progress;
/// Protobuf manifest models and decoders.
pub mod proto_parse;
/// High-level builder and per-game download interface.
pub mod sophon;
/// Persisted download and resume-state models.
pub mod types;

/// Tuned HTTP client used for chunk transfers.
pub use client::DownloadClient;
/// Current pause/cancellation state of a [`DownloadHandle`].
pub use game_installer::ControlState;
/// Cancellation and pause handle for long-running operations.
pub use game_installer::DownloadHandle;
/// Integrity verification policy for installation and repair.
pub use game_installer::VerifyMode;
/// Compute the manifest identity used to validate resumable state.
pub use manifest::compute_content_manifest_hash;
/// Progress event emitted by high-level operations.
pub use progress::SophonProgress;
/// Progress callback bound accepted by high-level operations.
pub use sophon::ProgressFn;
/// Per-game downloader and its fluent configuration builder.
pub use sophon::Sophon;
/// Fluent builder for [`Sophon`].
pub use sophon::SophonBuilder;
/// Load persisted resume state for a game from a state directory.
pub use sophon::load_download_state;
/// Construct the path used for a game's persisted download state.
pub use sophon::state_file_path;
/// Number of completed chunks between periodic state saves.
pub use types::CHUNK_STATE_SAVE_INTERVAL;
/// Persisted download state used for crash recovery.
pub use types::DownloadState;
/// Kind of operation represented by persisted state.
pub use types::DownloadType;
/// Summary of a resumable operation.
pub use types::ResumeInfo;

/// Global allocator. Enabled under `sophon-profiling` for jemalloc stats.
#[cfg(all(unix, feature = "sophon-profiling"))]
#[global_allocator]
static GLOBAL_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
