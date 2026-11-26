//! Sophon game downloader. Manifest-based chunk downloads with zstd
//! compression.
//!
//! # Features
//!
//! All operations are enabled by default. Disable default features and
//! select only what you need to shrink the build:
//!
//! - `download`: fresh install via [`Sophon::download`].
//! - `update`: versioned update planning and installation (implies `download`).
//! - `discovery`: update and preinstall availability/size queries (enabled by
//!   default; does not enable download operations).
//! - `verify`: post-install integrity repair via `verify_integrity`.
//! - `preinstall`: preinstall download and apply (implies `download`, `patch`).
//! - `patch`: HDiff patch engine used by preinstall apply.
//! - `plugins`: plugin and channel SDK installation.
//!
//! The chunk download/assembly engine, per-chunk MD5 verification, and resume
//! state are always compiled because every operation shares them. The
//! `benchmark*` and `sophon-profiling` features are development-only.

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

#[cfg(all(feature = "benchmark", feature = "patch"))]
pub mod bench_support;

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

/// Global allocator used by jemalloc profiling and benchmark builds.
#[cfg(all(
    unix,
    any(feature = "sophon-profiling", feature = "benchmark-jemalloc")
))]
#[global_allocator]
static GLOBAL_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
