//! Query the live API for update availability without writing disk.
//!
//! Usage: `cargo run --example check_update -- <game_id> <vo_lang> <game_dir>`

#[path = "common/mod.rs"]
mod common;

use irmin_rs::Sophon;
use irmin_rs::game_installer::UpdateInfo;

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
    println!(
        "update_decompressed_size: {} bytes",
        i.update_decompressed_size
    );
}
