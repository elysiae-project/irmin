//! Verify installed files and re-download any whose hash mismatches, then
//! report how many files were repaired.
//!
//! Usage: `cargo run --example verify -- <game_id> <vo_lang> <game_dir>`
//!
//! `verify_integrity` takes no `DownloadHandle` and is uncancellable: it runs
//! to completion ignoring signals. `Assembling` events count only successfully
//! verified repairs; this example reports that count after `Ok(())`. Generated
//! launcher metadata is rebuilt separately and is not included in the count.
//! Any scan, repair, or metadata-generation failure returns an error.

#[path = "common/mod.rs"]
mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use irmin::{Sophon, SophonProgress};

#[tokio::main]
async fn main() {
    let args = common::parse_common_args("verify");

    let sophon = Sophon::builder(args.game_id, args.game_dir)
        .vo_lang(args.vo_lang)
        .build();

    let repaired = Arc::new(AtomicU64::new(0));
    let counter = repaired.clone();
    let on_progress = move |p: SophonProgress| {
        if let SophonProgress::Assembling {
            assembled_files, ..
        } = &p
        {
            counter.fetch_max(*assembled_files, Ordering::Relaxed);
        }
        common::print_progress(p);
    };

    match sophon.verify_integrity(on_progress).await {
        Ok(()) => {
            let n = repaired.load(Ordering::Relaxed);
            println!("verified. repaired {n} file(s).");
        }
        Err(e) => {
            eprintln!("error: {e:?}");
            std::process::exit(1);
        }
    }
}
