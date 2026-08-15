//! Canonical fresh-download example: build a `Sophon`, read on-disk resume
//! state, run a download, and demonstrate pause/resume/cancel across Ctrl-C.
//!
//! Usage: `cargo run --example download -- <game_id> <vo_lang> <game_dir>`
//!
//! Ctrl-C: 1st press pauses, 2nd resumes, 3rd cancels.
//!
//! Notes (commented, not wired, so an integrator can enable per need):
//! - `verify_mode`: `Full` (default) hashes every chunk and survives a crash
//!   via a chunk bitmap; `Deferred` skips per-chunk hashing and drops
//!   crash-resume; `None` is size-only.
//! - `.state_dir(dir)` overrides where the per-game resume state is stored
//!   (defaults to `game_dir`).
//! - `.client(reqwest::Client)` overrides the manifest-fetch client (chunk
//!   downloads always use an internal client).
//! - `SophonError::is_retryable()` marks transient `DownloadFailed`/`Http`
//!   errors worth retrying.

#[path = "common/mod.rs"]
mod common;

use irmin::game_installer::SophonError;
use irmin::{DownloadHandle, Sophon, VerifyMode};

#[tokio::main]
async fn main() {
    let args = common::parse_common_args("download");
    let handle = DownloadHandle::new();

    let sophon = Sophon::builder(args.game_id, args.game_dir)
        .vo_lang(args.vo_lang)
        .verify_mode(VerifyMode::Full)
        .build();

    println!("resume_info: {:?}", sophon.resume_info());
    println!("installed_tag: {:?}", sophon.installed_tag());

    install_pause_resume_cancel(&handle);

    match sophon.download(&handle, common::print_progress).await {
        Ok(()) => println!("download complete."),
        Err(SophonError::Cancelled) => {
            eprintln!("cancelled.");
            std::process::exit(130);
        }
        Err(e) => {
            eprintln!("error: {e:?}");
            std::process::exit(1);
        }
    }
}

/// 1st Ctrl-C pauses, 2nd resumes, 3rd cancels.
fn install_pause_resume_cancel(handle: &DownloadHandle) {
    let h = handle.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\npausing (Ctrl-C again to resume, again to cancel)...");
            h.pause();
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\nresuming...");
            h.resume();
        }
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling...");
            h.cancel();
        }
    });
}
