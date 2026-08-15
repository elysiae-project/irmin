//! Query the live API for update and preinstall availability without writing
//! disk. The one cheap live operation.
//!
//! Usage: `cargo run --example check_update -- <game_id> <vo_lang> <game_dir>`

#[path = "common/mod.rs"]
mod common;

use irmin::game_installer::UpdateInfo;
use irmin::Sophon;

#[tokio::main]
async fn main() {
    let args = common::parse_common_args("check_update");

    let sophon = Sophon::builder(args.game_id, args.game_dir)
        .vo_lang(args.vo_lang)
        .build();

    match sophon.check_update().await {
        Ok(info) => print_update_info(&info),
        Err(e) => {
            eprintln!("error: {e:?}");
            std::process::exit(1);
        }
    }
}

fn print_update_info(i: &UpdateInfo) {
    println!("update_available: {}", i.update_available);
    println!("current_tag: {:?}", i.current_tag);
    println!("remote_tag: {}", i.remote_tag);
    println!("update_compressed_size: {} bytes", i.update_compressed_size);
    println!("update_decompressed_size: {} bytes", i.update_decompressed_size);
    println!("preinstall_available: {}", i.preinstall_available);
    println!("preinstall_downloaded: {}", i.preinstall_downloaded);
    println!("preinstall_tag: {:?}", i.preinstall_tag);
    println!("preinstall_compressed_size: {} bytes", i.preinstall_compressed_size);
    println!("preinstall_decompressed_size: {} bytes", i.preinstall_decompressed_size);
}
