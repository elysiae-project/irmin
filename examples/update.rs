//! Update an existing install to the latest remote tag.
//!
//! Usage: `cargo run --example update -- <game_id> <vo_lang> <game_dir>`
//!
//! Ctrl-C cancels.

#[path = "common/mod.rs"]
mod common;

use irmin::game_installer::SophonError;
use irmin::{DownloadHandle, Sophon, VerifyMode};

#[tokio::main]
async fn main() {
    let args = common::parse_common_args("update");
    let handle = DownloadHandle::new();

    let sophon = Sophon::builder(args.game_id, args.game_dir)
        .vo_lang(args.vo_lang)
        .verify_mode(VerifyMode::Full)
        .build();

    println!("installed_tag: {:?}", sophon.installed_tag());

    common::install_cancel_handler(&handle);

    match sophon.update(&handle, common::print_progress).await {
        Ok(()) => println!("update complete."),
        Err(SophonError::NoInstalledVersion) => {
            eprintln!("no install to update; run download first.");
            std::process::exit(1);
        }
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
