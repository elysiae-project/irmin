use crate::game_installer::hdiffpatch::{HDiffPublic as HDiff, HDiffSourceBackend};
use std::fs;
use std::path::Path;

pub fn deterministic_data(seed: u64, len: usize) -> Vec<u8> {
    let mut state = [
        seed,
        seed.wrapping_mul(6364136223846793005).wrapping_add(1),
        seed.wrapping_mul(1442695040888963407).wrapping_add(3),
        seed ^ 0xdeadbeefcafe1234,
    ];
    let mut output = Vec::with_capacity(len);
    for _ in 0..len {
        let result = state[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = state[1] << 17;
        state[2] ^= state[0];
        state[3] ^= state[1];
        state[1] ^= state[2];
        state[0] ^= state[3];
        state[2] ^= t;
        state[3] = state[3].rotate_left(45);
        output.push(result as u8);
    }
    output
}

pub fn make_v13_zstd_patch(old: &[u8], new: &[u8]) -> Vec<u8> {
    assert_eq!(old.len(), new.len());
    let block_size = 256usize;
    let blocks = old.len() / block_size;
    let mut covers = Vec::new();
    let mut run_start = None;
    for block in 0..blocks {
        let offset = block * block_size;
        if old[offset..offset + block_size] == new[offset..offset + block_size] {
            run_start.get_or_insert(block);
        } else if let Some(start) = run_start.take() {
            covers.push((
                start * block_size,
                start * block_size,
                (block - start) * block_size,
            ));
        }
    }
    if let Some(start) = run_start {
        covers.push((
            start * block_size,
            start * block_size,
            (blocks - start) * block_size,
        ));
    }

    make_v13_zstd_patch_from_covers(old, new, &covers)
}

pub fn make_v13_zstd_reordered_patch(old: &[u8], permutation: &[usize]) -> (Vec<u8>, Vec<u8>) {
    const BLOCK_SIZE: usize = 64 * 1024;
    assert_eq!(old.len() % BLOCK_SIZE, 0);
    assert!(
        permutation
            .iter()
            .all(|&index| index < old.len() / BLOCK_SIZE)
    );
    let new = permutation
        .iter()
        .flat_map(|&index| {
            let start = index * BLOCK_SIZE;
            old[start..start + BLOCK_SIZE].iter().copied()
        })
        .collect::<Vec<_>>();
    let covers = permutation
        .iter()
        .enumerate()
        .map(|(new_index, &old_index)| (old_index * BLOCK_SIZE, new_index * BLOCK_SIZE, BLOCK_SIZE))
        .collect::<Vec<_>>();
    (make_v13_zstd_patch_from_covers(old, &new, &covers), new)
}

fn make_v13_zstd_patch_from_covers(
    old: &[u8],
    new: &[u8],
    covers: &[(usize, usize, usize)],
) -> Vec<u8> {
    let mut checked_new_end = 0usize;
    for &(old_pos, new_pos, len) in covers {
        assert!(old_pos.checked_add(len).is_some_and(|end| end <= old.len()));
        assert!(new_pos >= checked_new_end);
        assert!(new_pos.checked_add(len).is_some_and(|end| end <= new.len()));
        checked_new_end = new_pos + len;
    }

    let mut cover_data = Vec::new();
    let mut previous_old_end = 0usize;
    let mut previous_new_end = 0usize;
    for &(old_pos, new_pos, len) in covers {
        let delta = old_pos.abs_diff(previous_old_end);
        let sign = usize::from(old_pos < previous_old_end) as u8;
        cover_data.extend_from_slice(&encode_tagged_1bit(sign, delta as u64));
        cover_data.extend_from_slice(&encode_varint((new_pos - previous_new_end) as u64));
        cover_data.extend_from_slice(&encode_varint(len as u64));
        previous_old_end = old_pos + len;
        previous_new_end = new_pos + len;
    }

    let mut rle_data = Vec::new();
    let mut previous_new_end = 0usize;
    for &(_, new_pos, len) in covers {
        let gap = new_pos - previous_new_end;
        if gap > 0 {
            rle_data.extend_from_slice(&encode_tagged_2bit(0, (gap - 1) as u64));
        }
        rle_data.extend_from_slice(&encode_tagged_2bit(0, (len - 1) as u64));
        previous_new_end = new_pos + len;
    }
    let tail = new.len() - previous_new_end;
    if tail > 0 {
        rle_data.extend_from_slice(&encode_tagged_2bit(0, (tail - 1) as u64));
    }

    let mut new_data = Vec::new();
    let mut previous_new_end = 0usize;
    for &(_, new_pos, len) in covers {
        if new_pos > previous_new_end {
            new_data.extend_from_slice(&new[previous_new_end..new_pos]);
        }
        previous_new_end = new_pos + len;
    }
    if previous_new_end < new.len() {
        new_data.extend_from_slice(&new[previous_new_end..]);
    }

    let cover_compressed = zstd::encode_all(cover_data.as_slice(), 3).expect("compress covers");
    let rle_compressed = zstd::encode_all(rle_data.as_slice(), 3).expect("compress rle");
    let new_data_compressed = zstd::encode_all(new_data.as_slice(), 3).expect("compress new data");
    debug_assert_eq!(
        zstd::decode_all(cover_compressed.as_slice()).unwrap(),
        cover_data
    );
    debug_assert_eq!(
        zstd::decode_all(rle_compressed.as_slice()).unwrap(),
        rle_data
    );
    debug_assert_eq!(
        zstd::decode_all(new_data_compressed.as_slice()).unwrap(),
        new_data
    );

    let mut patch = b"HDIFF13&zstd\0".to_vec();
    for value in [
        new.len() as u64,
        old.len() as u64,
        covers.len() as u64,
        cover_data.len() as u64,
        cover_compressed.len() as u64,
        rle_data.len() as u64,
        rle_compressed.len() as u64,
        0,
        0,
        new_data.len() as u64,
        new_data_compressed.len() as u64,
    ] {
        patch.extend_from_slice(&encode_varint(value));
    }
    patch.extend_from_slice(&cover_compressed);
    patch.extend_from_slice(&rle_compressed);
    patch.extend_from_slice(&new_data_compressed);
    patch
}

pub fn prepare_hdiff_fixture(dir: &Path, size_mib: usize) -> Result<(), String> {
    let old = deterministic_data(0xDEAD, size_mib * 1024 * 1024);
    let mut new = old.clone();
    for block in (0..new.len() / 256).step_by(6) {
        new[block * 256] ^= 0xff;
    }
    fs::write(dir.join("old.bin"), &old).map_err(|error| error.to_string())?;
    fs::write(dir.join("new.bin"), &new).map_err(|error| error.to_string())?;
    fs::write(dir.join("patch.hdiff"), make_v13_zstd_patch(&old, &new))
        .map_err(|error| error.to_string())
}

pub fn verify_hdiff_fixture(dir: &Path) -> Result<(), String> {
    apply_hdiff_fixture(dir)?;
    compare_hdiff_fixture(dir)
}

pub fn compare_hdiff_fixture(dir: &Path) -> Result<(), String> {
    let expected_path = dir.join("new.bin");
    let actual_path = dir.join("out.bin");
    let expected = fs::read(expected_path).map_err(|error| error.to_string())?;
    let actual = fs::read(actual_path).map_err(|error| error.to_string())?;
    if actual != expected {
        return Err("HDiff output mismatch".to_owned());
    }
    Ok(())
}

pub fn apply_hdiff_fixture(dir: &Path) -> Result<(), String> {
    apply_hdiff_fixture_with_backend(dir, HDiffSourceBackend::Mmap)
}

pub fn apply_hdiff_fixture_with_backend(
    dir: &Path,
    source_backend: HDiffSourceBackend,
) -> Result<(), String> {
    let mut patch = HDiff::new(
        dir.join("old.bin").to_string_lossy().into_owned(),
        dir.join("patch.hdiff").to_string_lossy().into_owned(),
        dir.join("out.bin").to_string_lossy().into_owned(),
    );
    if !patch.apply_with_source_backend(None, source_backend) {
        return Err("HDiff apply failed".to_owned());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_v13_patch_applies_and_matches_output() {
        let dir = tempfile::tempdir().unwrap();
        prepare_hdiff_fixture(dir.path(), 1).unwrap();
        verify_hdiff_fixture(dir.path()).unwrap();
    }

    #[test]
    fn varint_encoder_uses_big_endian_base_128_groups() {
        assert_eq!(encode_varint(300), [0x82, 0x2c]);
    }

    #[test]
    fn tagged_encoders_match_hdiff_test_format_boundaries() {
        assert_eq!(encode_tagged_1bit(0, 63), [0x3f]);
        assert_eq!(encode_tagged_1bit(0, 64), [0x40, 0x40]);
        assert_eq!(encode_tagged_2bit(0, 31), [0x1f]);
        assert_eq!(encode_tagged_2bit(0, 32), [0x20, 0x20]);
    }

    #[test]
    fn tiny_sparse_patch_has_one_cover_and_expected_gap_bytes() {
        let old = vec![0x11; 4 * 256];
        let mut new = old.clone();
        new[256] = 0x22;
        let patch = make_v13_zstd_patch(&old, &new);
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("old.bin"), &old).unwrap();
        fs::write(dir.path().join("new.bin"), &new).unwrap();
        fs::write(dir.path().join("patch.hdiff"), patch).unwrap();
        verify_hdiff_fixture(dir.path()).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn hdiff_backends_produce_identical_output() {
        use crate::game_installer::hdiffpatch::HDiffSourceBackend;

        for backend in [
            HDiffSourceBackend::Mmap,
            HDiffSourceBackend::File,
            HDiffSourceBackend::Windowed {
                window_size: 4096,
                stats: std::sync::Arc::new(
                    crate::game_installer::hdiffpatch::WindowStats::default(),
                ),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            prepare_hdiff_fixture(dir.path(), 1).unwrap();
            apply_hdiff_fixture_with_backend(dir.path(), backend).unwrap();
            compare_hdiff_fixture(dir.path()).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn reordered_old_covers_apply_identically_with_backward_reads() {
        use crate::game_installer::hdiffpatch::HDiffSourceBackend;

        const BLOCKS: usize = 64;
        const BLOCK_SIZE: usize = 64 * 1024;
        let old = deterministic_data(0xA11CE, BLOCKS * BLOCK_SIZE);
        let permutation = (0..BLOCKS).rev().collect::<Vec<_>>();
        let (patch, expected) = make_v13_zstd_reordered_patch(&old, &permutation);

        for backend in [
            HDiffSourceBackend::Mmap,
            HDiffSourceBackend::File,
            HDiffSourceBackend::Windowed {
                window_size: 256 * 1024,
                stats: std::sync::Arc::new(
                    crate::game_installer::hdiffpatch::WindowStats::default(),
                ),
            },
        ] {
            let dir = tempfile::tempdir().unwrap();
            fs::write(dir.path().join("old.bin"), &old).unwrap();
            fs::write(dir.path().join("new.bin"), &expected).unwrap();
            fs::write(dir.path().join("patch.hdiff"), &patch).unwrap();
            apply_hdiff_fixture_with_backend(dir.path(), backend).unwrap();
            compare_hdiff_fixture(dir.path()).unwrap();
        }
    }
}

fn encode_varint(value: u64) -> Vec<u8> {
    if value < 0x80 {
        return vec![value as u8];
    }
    let groups = (64 - value.leading_zeros() as usize).div_ceil(7);
    let mut bytes = Vec::with_capacity(groups);
    for group in (0..groups).rev() {
        let part = ((value >> (group * 7)) & 0x7f) as u8;
        bytes.push(if group > 0 { part | 0x80 } else { part });
    }
    bytes
}

fn encode_tagged_1bit(sign: u8, value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    if value < 64 {
        bytes.push((sign << 7) | value as u8);
        return bytes;
    }
    let bits = 64 - value.leading_zeros() as usize;
    let extra_groups = (bits - 6).div_ceil(7);
    let first_shift = extra_groups * 7;
    bytes.push((sign << 7) | 0x40 | ((value >> first_shift) as u8 & 0x3f));
    for group in (0..extra_groups).rev() {
        let chunk = ((value >> (group * 7)) & 0x7f) as u8;
        bytes.push(if group > 0 { chunk | 0x80 } else { chunk });
    }
    bytes
}

fn encode_tagged_2bit(rle_type: u8, value: u64) -> Vec<u8> {
    let mut bytes = Vec::new();
    if value < 32 {
        bytes.push((rle_type << 6) | value as u8);
        return bytes;
    }
    let bits = 64 - value.leading_zeros() as usize;
    let extra_groups = (bits - 5).div_ceil(7);
    let first_shift = extra_groups * 7;
    bytes.push((rle_type << 6) | 0x20 | ((value >> first_shift) as u8 & 0x1f));
    for group in (0..extra_groups).rev() {
        let chunk = ((value >> (group * 7)) & 0x7f) as u8;
        bytes.push(if group > 0 { chunk | 0x80 } else { chunk });
    }
    bytes
}
