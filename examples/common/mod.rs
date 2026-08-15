//! Shared helpers for the irmin examples:
//! a SophonProgress reporter, a Ctrl-C cancel handler, and common arg parsing.
//!
//! Not every example uses every item here, so the module allows dead code.

#![allow(dead_code)]

use std::path::PathBuf;

use irmin::{DownloadHandle, SophonProgress};

/// The three positional shared args every operation takes.
pub struct CommonArgs {
    pub game_id: String,
    pub vo_lang: String,
    pub game_dir: PathBuf,
}

/// Parse `<game_id> <vo_lang> <game_dir>` from the command line.
pub fn parse_common_args(name: &str) -> CommonArgs {
    let mut args = std::env::args().skip(1);
    let game_id = args.next().unwrap_or_else(|| usage(name, "missing <game_id>"));
    let vo_lang = args.next().unwrap_or_else(|| usage(name, "missing <vo_lang>"));
    let game_dir = args.next().unwrap_or_else(|| usage(name, "missing <game_dir>"));
    CommonArgs { game_id, vo_lang, game_dir: PathBuf::from(game_dir) }
}

fn usage(name: &str, msg: &str) -> ! {
    eprintln!("error: {msg}");
    eprintln!("usage: cargo run --example {name} -- <game_id> <vo_lang> <game_dir>");
    std::process::exit(2);
}

/// Print one human-readable line per `SophonProgress` variant.
///
/// `Downloading` refreshes the same line with `\r`; the rest print full lines.
pub fn print_progress(p: SophonProgress) {
    use std::io::Write as _;
    match p {
        SophonProgress::FetchingManifest => eprintln!("fetching manifest..."),
        SophonProgress::CalculatingDownloads { checked_files, total_files } => {
            eprintln!("calculating downloads: {checked_files}/{total_files} files");
        }
        SophonProgress::Downloading {
            downloaded_bytes,
            total_bytes,
            speed_bps,
            eta_seconds,
        } => {
            eprint!(
                "\rdownloading {}/{} MiB  {:.2} MiB/s  eta {:.0}s   ",
                mib(downloaded_bytes),
                mib(total_bytes),
                speed_bps / 1024.0 / 1024.0,
                eta_seconds
            );
            let _ = std::io::stderr().flush();
        }
        SophonProgress::Paused { downloaded_bytes, total_bytes } => {
            eprintln!(
                "\npaused at {}/{} MiB (Ctrl-C again to resume, again to cancel)",
                mib(downloaded_bytes),
                mib(total_bytes)
            );
        }
        SophonProgress::Assembling { assembled_files, total_files } => {
            eprintln!("assembling {assembled_files}/{total_files} files");
        }
        SophonProgress::CheckingFiles { checked_files, total_files } => {
            eprintln!("checking {checked_files}/{total_files} files");
        }
        SophonProgress::Verifying { scanned_files, total_files, error_count } => {
            eprintln!("verifying {scanned_files}/{total_files} files  {error_count} mismatches");
        }
        SophonProgress::Warning { message } => eprintln!("warning: {message}"),
        SophonProgress::Error { message } => eprintln!("\npipeline error: {message}"),
        SophonProgress::InstallingPlugins { current_plugin, total_plugins } => {
            eprintln!("installing plugin {current_plugin}/{total_plugins}");
        }
        SophonProgress::InstallingSdks { current_sdk, total_sdks } => {
            eprintln!("installing SDK {current_sdk}/{total_sdks}");
        }
        SophonProgress::DownloadingPlugin { name, downloaded_bytes, total_bytes } => {
            eprint!(
                "\rdownloading plugin {name}: {}/{} MiB   ",
                mib(downloaded_bytes),
                mib(total_bytes)
            );
            let _ = std::io::stderr().flush();
        }
        SophonProgress::ApplyingPreinstall { applied_files, total_files } => {
            eprintln!("applying preinstall {applied_files}/{total_files} files");
        }
        SophonProgress::Finished => eprintln!("\ndone."),
    }
}

fn mib(bytes: u64) -> f64 {
    bytes as f64 / 1024.0 / 1024.0
}

/// Install a Ctrl-C handler that cancels the operation on the first signal.
///
/// Used by operations that take a `DownloadHandle` (`update`, `preinstall`).
/// `download.rs` wires its own pause/resume/cancel handler; `verify` is
/// uncancellable and uses no handle.
pub fn install_cancel_handler(handle: &DownloadHandle) {
    let h = handle.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            eprintln!("\ncancelling...");
            h.cancel();
        }
    });
}
