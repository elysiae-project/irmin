use irmin_rs::Sophon;
use irmin_rs::game_installer::DownloadHandle;
use irmin_rs::SophonProgress;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("Usage: {} <game_id> <game_dir> [vo_lang] [duration_secs]", args[0]);
        std::process::exit(1);
    }
    let game_id = &args[1];
    let game_dir = &args[2];
    let vo_lang = args.get(3).map(|s| s.as_str()).unwrap_or("en-us");
    let duration = args.get(4)
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(90);

    let sophon = Sophon::builder(game_id, game_dir)
        .vo_lang(vo_lang)
        .build();

    let handle = DownloadHandle::new();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = Arc::clone(&stop);

    // Memory monitor thread
    let _pid = std::process::id();
    let monitor = std::thread::spawn(move || {
        let start = Instant::now();
        while !stop_clone.load(Ordering::Relaxed) {
            let elapsed = start.elapsed().as_secs();
            // Read VmRSS from /proc/self/status
            let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
            let rss: u64 = status
                .lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            // Read VmHWM
            let hwm: u64 = status
                .lines()
                .find(|l| l.starts_with("VmHWM:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            // Read Threads count
            let threads: u64 = status
                .lines()
                .find(|l| l.starts_with("Threads:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u64>().ok())
                .unwrap_or(0);
            println!("{elapsed:3}s  RSS={rss:>7} KiB ({:>5.1} MiB)  HWM={hwm:>7} KiB ({:>5.1} MiB)  Threads={threads}", rss as f64 / 1024.0, hwm as f64 / 1024.0);
            std::thread::sleep(Duration::from_secs(5));
        }
    });

    // Spawn download in a tokio runtime
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let handle_clone = handle.clone();
    let progress = move |p: SophonProgress| {
        if let SophonProgress::Downloading { downloaded_bytes, total_bytes, speed_bps, eta_seconds } = p {
            let pct = if total_bytes > 0 {
                (downloaded_bytes as f64 / total_bytes as f64) * 100.0
            } else {
                0.0
            };
            eprintln!("\r  {pct:>5.1}%  {downloaded_bytes:>12} / {total_bytes:>12}  {speed_bps:>10.0} B/s  ETA {eta_seconds:.0}s    ");
        }
    };

    // Run download in background
    let download_handle = runtime.spawn(async move {
        sophon.download(&handle_clone, progress).await
    });

    // Let it run for the specified duration
    std::thread::sleep(Duration::from_secs(duration));

    // Pause
    eprintln!("\nPausing download...");
    handle.pause();

    // Monitor memory after pause for 30s
    std::thread::sleep(Duration::from_secs(30));

    // Stop monitor
    stop.store(true, Ordering::Relaxed);
    monitor.join().unwrap();

    // Cancel and clean up
    handle.cancel();
    let _ = runtime.block_on(download_handle);

    // Clean up downloaded files
    eprintln!("Cleaning up...");
    let chunks_dir = std::path::Path::new(game_dir).join("chunks");
    if chunks_dir.exists() {
        let _ = std::fs::remove_dir_all(&chunks_dir);
    }
    let _ = std::fs::remove_dir_all(std::path::Path::new(game_dir).join(".sophon"));
    // Remove any .zstd chunk files
    if let Ok(entries) = std::fs::read_dir(game_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("zstd") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    eprintln!("Done.");
}
