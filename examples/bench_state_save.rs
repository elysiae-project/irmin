//! Microbenchmark: state save serialization (old arena-lookup vs pre-built vec).
//! Run: cargo run --release --features benchmark --example bench_state_save

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use irmin::game_installer::compact_manifest::StringArena;
use irmin::game_installer::installer::ChunkNameLookup;

const NUM_CHUNKS: usize = 100_000;
const NONZERO_FRACTION: f64 = 0.7;
const ITERATIONS: usize = 20;

fn rss_kib() -> u64 {
    let buf = std::fs::read_to_string("/proc/self/statm").unwrap();
    let pages: u64 = buf.split_whitespace().nth(1).unwrap().parse().unwrap();
    pages * 4
}

fn main() {
    // Build fixture data
    let names: Vec<String> = (0..NUM_CHUNKS)
        .map(|i| format!("chunk_{i:08}_abcdefghijklmnop"))
        .collect();

    let name_refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
    let arena = StringArena::from(name_refs.as_slice());
    let lookup = ChunkNameLookup::from_arena(arena);

    let downloaded: Vec<AtomicU32> = (0..NUM_CHUNKS)
        .map(|i| {
            if (i as f64) < NUM_CHUNKS as f64 * NONZERO_FRACTION {
                AtomicU32::new(((i % 8000) + 1000) as u32)
            } else {
                AtomicU32::new(0)
            }
        })
        .collect();

    // Pre-built names vec (the new approach)
    let prebuilt_names: Vec<String> = (0..NUM_CHUNKS).map(|i| lookup.get(i).to_owned()).collect();

    println!("=== State Save Benchmark ===");
    println!(
        "Chunks: {NUM_CHUNKS}, Non-zero: {:.0}%",
        NONZERO_FRACTION * 100.0
    );
    println!();

    // OLD approach: arena lookup + to_string per chunk
    let mut old_times = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let map: HashMap<String, u64> = downloaded
            .iter()
            .enumerate()
            .filter_map(|(i, v)| {
                let val = v.load(Ordering::Relaxed);
                if val > 0 {
                    Some((lookup.get(i).to_string(), val as u64))
                } else {
                    None
                }
            })
            .collect();
        std::hint::black_box(&map);
        old_times.push(start.elapsed());
    }

    let rss_after_old = rss_kib();

    // NEW approach: clone from pre-built vec
    let mut new_times = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let start = Instant::now();
        let mut map: HashMap<String, u64> = HashMap::with_capacity(downloaded.len() / 2);
        for (i, v) in downloaded.iter().enumerate() {
            let val = v.load(Ordering::Relaxed);
            if val > 0 {
                map.insert(prebuilt_names[i].clone(), val as u64);
            }
        }
        std::hint::black_box(&map);
        new_times.push(start.elapsed());
    }

    let rss_after_new = rss_kib();

    // Report
    old_times.sort();
    new_times.sort();
    let old_median = old_times[ITERATIONS / 2].as_micros();
    let new_median = new_times[ITERATIONS / 2].as_micros();
    let speedup = old_median as f64 / new_median as f64;

    println!("OLD (arena lookup + to_string):");
    println!("  median: {old_median} us");
    println!("  min:    {} us", old_times[0].as_micros());
    println!("  max:    {} us", old_times.last().unwrap().as_micros());
    println!();
    println!("NEW (clone from pre-built vec):");
    println!("  median: {new_median} us");
    println!("  min:    {} us", new_times[0].as_micros());
    println!("  max:    {} us", new_times.last().unwrap().as_micros());
    println!();
    println!("Speedup: {speedup:.2}x");
    println!("RSS after old: {rss_after_old} KiB, after new: {rss_after_new} KiB");
    println!(
        "Pre-built names vec size: ~{} KiB",
        prebuilt_names.iter().map(|s| s.len() + 24).sum::<usize>() / 1024
    );
}
