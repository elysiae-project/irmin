use std::fs::{self, File};
use std::os::unix::fs::FileExt;
use std::time::Instant;

fn main() {
    let dir = tempfile::tempdir().unwrap();

    // Generate 8 MiB of compressible data
    let data_size = 8 * 1024 * 1024;
    let mut data = vec![0u8; data_size];
    let mut seed: u64 = 12345;
    for i in 0..data_size {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        data[i] = (seed >> 33) as u8;
    }
    // Add repetition for compressibility
    for i in (4096..data_size).step_by(4096) {
        if (i / 4096) % 4 == 0 {
            let prev: Vec<u8> = data[i - 4096..i].to_vec();
            data[i..i + 4096].copy_from_slice(&prev);
        }
    }

    let compressed = zstd::encode_all(std::io::Cursor::new(&data), 3).unwrap();
    println!(
        "Data: {} MiB, Compressed: {:.2} MiB (ratio {:.1}:1)",
        data_size / 1024 / 1024,
        compressed.len() as f64 / 1024.0 / 1024.0,
        data_size as f64 / compressed.len() as f64
    );

    let iterations = 100;

    // Traditional path: write .zstd to disk, read back, decompress, write output
    let traditional_time = {
        let zstd_path = dir.path().join("chunk.zstd");
        let out_path = dir.path().join("trad_output.bin");

        let start = Instant::now();
        for _ in 0..iterations {
            fs::write(&zstd_path, &compressed).unwrap();
            let out_file = File::create(&out_path).unwrap();
            out_file.set_len(data_size as u64).unwrap();
            let compressed_read = fs::read(&zstd_path).unwrap();
            let decompressed = zstd::decode_all(std::io::Cursor::new(&compressed_read)).unwrap();
            out_file.write_all_at(&decompressed, 0).unwrap();
        }
        start.elapsed()
    };

    // Eager path: decompress from memory, write output directly (no .zstd round-trip)
    let eager_time = {
        let out_path = dir.path().join("eager_output.bin");

        let start = Instant::now();
        for _ in 0..iterations {
            let out_file = File::create(&out_path).unwrap();
            out_file.set_len(data_size as u64).unwrap();
            let decompressed = zstd::decode_all(std::io::Cursor::new(&compressed)).unwrap();
            out_file.write_all_at(&decompressed, 0).unwrap();
        }
        start.elapsed()
    };

    println!(
        "\nTraditional ({iterations} iters): {:.1} ms total, {:.2} ms/chunk",
        traditional_time.as_millis(),
        traditional_time.as_secs_f64() * 1000.0 / iterations as f64
    );
    println!(
        "Eager ({iterations} iters): {:.1} ms total, {:.2} ms/chunk",
        eager_time.as_millis(),
        eager_time.as_secs_f64() * 1000.0 / iterations as f64
    );
    println!(
        "\nSpeedup: {:.2}x ({:.1}% faster)",
        traditional_time.as_secs_f64() / eager_time.as_secs_f64(),
        (1.0 - eager_time.as_secs_f64() / traditional_time.as_secs_f64()) * 100.0
    );
}
