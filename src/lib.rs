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

pub use client::DownloadClient;
pub use game_installer::DownloadHandle;
pub use game_installer::VerifyMode;
pub use manifest::compute_content_manifest_hash;
pub use progress::SophonProgress;
pub use sophon::{ProgressFn, Sophon, SophonBuilder, load_download_state, state_file_path};
pub use types::CHUNK_STATE_SAVE_INTERVAL;
pub use types::{DownloadState, DownloadType, ResumeInfo};

/// Global allocator. Enabled under `sophon-profiling` for jemalloc stats.
#[cfg(all(unix, feature = "sophon-profiling"))]
#[global_allocator]
static GLOBAL_ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
