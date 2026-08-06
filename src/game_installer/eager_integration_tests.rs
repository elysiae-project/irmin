//! Integration tests for eager decompression path.

use std::fs;

use crate::game_installer::chunk_bitmap::ChunkBitmap;
use crate::game_installer::eager_decompress::eager_decompress_chunk;
use crate::game_installer::output_allocator::OutputAllocator;

fn compress_data(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(std::io::Cursor::new(data), 3).unwrap()
}

fn md5_hex(data: &[u8]) -> String {
    hex::encode(fast_md5::digest(data))
}

/// Full eager install: 3 files, multiple chunks each, all download-only.
/// Verifies output files match expected content.
#[test]
fn eager_install_multi_file_multi_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    fs::create_dir_all(&game_dir).unwrap();

    let alloc = OutputAllocator::new(&game_dir);
    let bitmap_path = game_dir.join(".sophon_bitmap");

    // 3 files, each with 3 chunks of 4096 bytes
    let num_files = 3;
    let chunks_per_file = 3;
    let chunk_size = 4096usize;
    let total_chunks = num_files * chunks_per_file;

    let bitmap = ChunkBitmap::create(&bitmap_path, total_chunks).unwrap();

    // Generate test data for each chunk
    let mut all_chunks: Vec<Vec<u8>> = Vec::new();
    for i in 0..total_chunks {
        let data: Vec<u8> = (0..chunk_size)
            .map(|j| ((i * chunk_size + j) % 256) as u8)
            .collect();
        all_chunks.push(data);
    }

    // Pre-allocate output files
    for file_idx in 0..num_files {
        let file_size = (chunks_per_file * chunk_size) as u64;
        let file_name = format!("file_{file_idx}.bin");
        alloc
            .preallocate_file(file_idx, &file_name, file_size)
            .unwrap();
    }

    // Decompress each chunk eagerly
    for file_idx in 0..num_files {
        for chunk_idx in 0..chunks_per_file {
            let global_idx = file_idx * chunks_per_file + chunk_idx;
            let chunk_data = &all_chunks[global_idx];
            let compressed = compress_data(chunk_data);
            let comp_hash = md5_hex(&compressed);
            let decomp_hash = md5_hex(chunk_data);
            let offset = (chunk_idx * chunk_size) as u64;

            let handle = alloc.get_handle(file_idx).unwrap();
            let written = eager_decompress_chunk(
                &compressed,
                &handle,
                offset,
                chunk_size as u64,
                &comp_hash,
                &decomp_hash,
            )
            .unwrap();

            assert_eq!(written, chunk_size as u64);
            bitmap.mark_complete(global_idx);
        }
    }

    // Verify all chunks marked complete
    assert!(bitmap.all_complete_for_range(0, total_chunks));

    // Verify output file contents
    for file_idx in 0..num_files {
        let file_name = format!("file_{file_idx}.bin");
        let path = game_dir.join(&file_name);
        let content = fs::read(&path).unwrap();

        for chunk_idx in 0..chunks_per_file {
            let global_idx = file_idx * chunks_per_file + chunk_idx;
            let start = chunk_idx * chunk_size;
            let end = start + chunk_size;
            assert_eq!(
                &content[start..end],
                &all_chunks[global_idx][..],
                "Mismatch at file {file_idx} chunk {chunk_idx}"
            );
        }
    }

    // Sync bitmap and verify reload
    bitmap.sync().unwrap();
    let reloaded = ChunkBitmap::load(&bitmap_path).unwrap();
    assert!(reloaded.all_complete_for_range(0, total_chunks));
}

/// Resume after interruption: mark some chunks complete, verify only
/// incomplete chunks need processing.
#[test]
fn eager_resume_after_interruption() {
    let dir = tempfile::tempdir().unwrap();
    let bitmap_path = dir.path().join("resume.bitmap");

    let total_chunks = 20;
    let bitmap = ChunkBitmap::create(&bitmap_path, total_chunks).unwrap();

    // Simulate: first 12 chunks completed before interruption
    for i in 0..12 {
        bitmap.mark_complete(i);
    }
    bitmap.sync().unwrap();

    // Simulate restart: reload bitmap
    let reloaded = ChunkBitmap::load(&bitmap_path).unwrap();

    // Verify completed chunks are skipped
    let mut needs_download = Vec::new();
    for i in 0..total_chunks {
        if !reloaded.is_complete(i) {
            needs_download.push(i);
        }
    }

    assert_eq!(needs_download, vec![12, 13, 14, 15, 16, 17, 18, 19]);
    assert_eq!(needs_download.len(), 8);
}

/// Mixed mode: some files use eager (all download-only chunks),
/// others would use traditional assembly (has old-file refs).
/// This test verifies the eager path works independently.
#[test]
fn eager_mixed_mode_coexistence() {
    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    fs::create_dir_all(&game_dir).unwrap();

    let alloc = OutputAllocator::new(&game_dir);
    let bitmap_path = game_dir.join(".sophon_bitmap");

    // 10 total chunks: first 6 are eager (2 files × 3 chunks), last 4 are traditional
    let total_chunks = 10;
    let bitmap = ChunkBitmap::create(&bitmap_path, total_chunks).unwrap();

    // Eager file 0: chunks 0-2
    let chunk_size = 2048usize;
    alloc
        .preallocate_file(0, "eager_file_0.bin", (3 * chunk_size) as u64)
        .unwrap();

    for chunk_idx in 0..3 {
        let data: Vec<u8> = vec![(chunk_idx as u8 + 0xA0); chunk_size];
        let compressed = compress_data(&data);
        let handle = alloc.get_handle(0).unwrap();
        let written = eager_decompress_chunk(
            &compressed,
            &handle,
            (chunk_idx * chunk_size) as u64,
            chunk_size as u64,
            &md5_hex(&compressed),
            &md5_hex(&data),
        )
        .unwrap();
        assert_eq!(written, chunk_size as u64);
        bitmap.mark_complete(chunk_idx);
    }

    // Eager file 1: chunks 3-5
    alloc
        .preallocate_file(1, "eager_file_1.bin", (3 * chunk_size) as u64)
        .unwrap();

    for i in 0..3 {
        let chunk_idx = 3 + i;
        let data: Vec<u8> = vec![(chunk_idx as u8 + 0xB0); chunk_size];
        let compressed = compress_data(&data);
        let handle = alloc.get_handle(1).unwrap();
        let written = eager_decompress_chunk(
            &compressed,
            &handle,
            (i * chunk_size) as u64,
            chunk_size as u64,
            &md5_hex(&compressed),
            &md5_hex(&data),
        )
        .unwrap();
        assert_eq!(written, chunk_size as u64);
        bitmap.mark_complete(chunk_idx);
    }

    // Traditional chunks 6-9 are NOT processed here (they'd go through assembly)
    // Verify only eager chunks are marked
    assert!(bitmap.all_complete_for_range(0, 6));
    assert!(!bitmap.is_complete(6));
    assert!(!bitmap.is_complete(9));

    // Verify eager output files
    let f0 = fs::read(game_dir.join("eager_file_0.bin")).unwrap();
    assert_eq!(f0.len(), 3 * chunk_size);
    assert!(f0[..chunk_size].iter().all(|&b| b == 0xA0));
    assert!(f0[chunk_size..2 * chunk_size].iter().all(|&b| b == 0xA1));
    assert!(f0[2 * chunk_size..].iter().all(|&b| b == 0xA2));

    let f1 = fs::read(game_dir.join("eager_file_1.bin")).unwrap();
    assert_eq!(f1.len(), 3 * chunk_size);
    assert!(f1[..chunk_size].iter().all(|&b| b == 0xB3));
    assert!(f1[chunk_size..2 * chunk_size].iter().all(|&b| b == 0xB4));
    assert!(f1[2 * chunk_size..].iter().all(|&b| b == 0xB5));
}

/// Hash mismatch triggers error and chunk is NOT marked in bitmap.
#[test]
fn eager_hash_mismatch_no_bitmap_mark() {
    let dir = tempfile::tempdir().unwrap();
    let game_dir = dir.path().join("game");
    fs::create_dir_all(&game_dir).unwrap();

    let alloc = OutputAllocator::new(&game_dir);
    let bitmap_path = game_dir.join(".sophon_bitmap");
    let bitmap = ChunkBitmap::create(&bitmap_path, 5).unwrap();

    let data = vec![0xFFu8; 1024];
    let compressed = compress_data(&data);

    alloc.preallocate_file(0, "bad.bin", 1024).unwrap();
    let handle = alloc.get_handle(0).unwrap();

    // Wrong compressed hash
    let result = eager_decompress_chunk(
        &compressed,
        &handle,
        0,
        1024,
        "00000000000000000000000000000000",
        &md5_hex(&data),
    );

    assert!(result.is_err());
    assert!(!bitmap.is_complete(0));
}
