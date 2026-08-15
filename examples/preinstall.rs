//! Pre-download an upcoming version's patch, or apply a preinstalled patch.
//!
//! Preinstall (run days before release):
//!   `cargo run --example preinstall -- <game_id> <vo_lang> <game_dir>`
//! Apply (run on release day):
//!   `cargo run --example preinstall -- <game_id> <vo_lang> <game_dir> --apply <tag>`
//!
//! `preinstall` downloads the patch package in the background; `apply_preinstall`
//! writes it into the install on release. Ctrl-C cancels either.

#[path = "common/mod.rs"]
mod common;

use irmin::game_installer::SophonError;
use irmin::{DownloadHandle, Sophon, VerifyMode};

#[tokio::main]
async fn main() {
    let args = common::parse_common_args("preinstall");
    let handle = DownloadHandle::new();

    let sophon = Sophon::builder(args.game_id, args.game_dir)
        .vo_lang(args.vo_lang)
        .verify_mode(VerifyMode::Full)
        .build();

    let apply_tag = find_apply_tag();
    common::install_cancel_handler(&handle);

    let result = match apply_tag {
        Some(tag) => {
            println!("applying preinstall {tag}...");
            sophon.apply_preinstall(&tag, &handle, common::print_progress).await
        }
        None => {
            println!("checking for a preinstall...");
            let info = match sophon.check_update().await {
                Ok(i) => i,
                Err(e) => {
                    eprintln!("error: {e:?}");
                    std::process::exit(1);
                }
            };
            println!(
                "preinstall_available: {} (already downloaded: {})",
                info.preinstall_available, info.preinstall_downloaded
            );
            if let Some(tag) = info.preinstall_tag.as_deref() {
                println!("preinstall_tag: {tag}");
            }
            sophon.preinstall(&handle, common::print_progress).await
        }
    };

    match result {
        Ok(()) => println!("preinstall complete."),
        Err(SophonError::NoPreinstallAvailable) => {
            eprintln!("no preinstall available.");
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

/// Find `--apply <tag>` after the three shared positional args.
fn find_apply_tag() -> Option<String> {
    let mut args = std::env::args().skip(1);
    // Skip the three positional shared args consumed by parse_common_args.
    args.next();
    args.next();
    args.next();
    while let Some(a) = args.next() {
        if a == "--apply" {
            return Some(args.next().unwrap_or_else(|| {
                eprintln!("--apply requires a <tag>");
                std::process::exit(2);
            }));
        }
    }
    None
}
