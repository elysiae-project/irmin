use crate::game_installer::hdiffpatch::{
    HDiffPublic as HDiff, HDiffSourceBackend, MAX_HDIFF20_STEP_SIZE,
};
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

pub const HDIFF20_STEP_STRESS_MIN_SIZE_MIB: usize = 4;
pub const HDIFF20_STEP_STRESS_MAX_SIZE_MIB: usize = 8;
const HDIFF20_STEP_STRESS_BYTES_PER_COVER: usize = 5;
const HDIFF20_STEP_STRESS_SMALL_STEP_COVERS: usize = 4096;

/// Per-step cover plus RLE section lengths for a synthetic one-byte-cover stress fixture with a changed leading literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hdiff20StepFixtureShape {
    pub output_bytes: usize,
    pub cover_count: usize,
    pub step_sizes_bytes: Vec<usize>,
}

pub fn hdiff20_step_stress_shape(size_mib: usize) -> Result<Hdiff20StepFixtureShape, String> {
    if !(HDIFF20_STEP_STRESS_MIN_SIZE_MIB..=HDIFF20_STEP_STRESS_MAX_SIZE_MIB).contains(&size_mib) {
        return Err(format!(
            "HDIFF20 step-stress fixture requires --size-mib between {HDIFF20_STEP_STRESS_MIN_SIZE_MIB} and {HDIFF20_STEP_STRESS_MAX_SIZE_MIB}"
        ));
    }

    let output_bytes = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "HDIFF20 step-stress fixture size is too large".to_owned())?;
    let cover_count = output_bytes - 1;
    let max_covers_per_step =
        MAX_HDIFF20_STEP_SIZE / HDIFF20_STEP_STRESS_BYTES_PER_COVER;
    let mut remaining_covers = cover_count;
    let mut step_cover_counts = Vec::new();

    step_cover_counts.push(remaining_covers.min(max_covers_per_step));
    remaining_covers -= step_cover_counts[0];
    if remaining_covers > max_covers_per_step {
        step_cover_counts.push(HDIFF20_STEP_STRESS_SMALL_STEP_COVERS);
        remaining_covers -= HDIFF20_STEP_STRESS_SMALL_STEP_COVERS;
    }
    while remaining_covers > 0 {
        let next = remaining_covers.min(max_covers_per_step);
        step_cover_counts.push(next);
        remaining_covers -= next;
    }

    let step_sizes_bytes = step_cover_counts
        .into_iter()
        .map(|cover_count| cover_count * HDIFF20_STEP_STRESS_BYTES_PER_COVER)
        .collect();
    Ok(Hdiff20StepFixtureShape {
        output_bytes,
        cover_count,
        step_sizes_bytes,
    })
}

pub fn prepare_hdiff20_step_stress_fixture(
    dir: &Path,
    size_mib: usize,
) -> Result<Hdiff20StepFixtureShape, String> {
    let shape = hdiff20_step_stress_shape(size_mib)?;
    let old = deterministic_data(0x20_5EED, shape.output_bytes);
    let mut new = Vec::with_capacity(shape.output_bytes);
    new.push(old[0] ^ 0xff);
    new.extend_from_slice(&old[..shape.output_bytes - 1]);
    let mut diff_stream = Vec::new();
    let mut max_step_size = 0usize;
    let mut is_first_step = true;
    for step_size in &shape.step_sizes_bytes {
        let first_step = is_first_step;
        let cover_count = *step_size / HDIFF20_STEP_STRESS_BYTES_PER_COVER;
        let cover_size = cover_count
            .checked_mul(3)
            .ok_or_else(|| "HDIFF20 cover step size overflow".to_owned())?;
        let rle_size = cover_count
            .checked_mul(2)
            .ok_or_else(|| "HDIFF20 RLE step size overflow".to_owned())?;
        let combined_size = cover_size
            .checked_add(rle_size)
            .ok_or_else(|| "HDIFF20 step size overflow".to_owned())?;
        if combined_size > MAX_HDIFF20_STEP_SIZE {
            return Err("generated HDIFF20 step exceeds the production limit".to_owned());
        }
        max_step_size = max_step_size.max(combined_size);

        let mut cover_data = Vec::new();
        cover_data
            .try_reserve_exact(cover_size)
            .map_err(|error| format!("failed to reserve HDIFF20 cover fixture: {error}"))?;
        for cover_index in 0..cover_count {
            if first_step && cover_index == 0 {
                cover_data.extend_from_slice(&[0, 1, 1]);
            } else {
                cover_data.extend_from_slice(&[0, 0, 1]);
            }
        }

        let mut rle_data = Vec::new();
        rle_data
            .try_reserve_exact(rle_size)
            .map_err(|error| format!("failed to reserve HDIFF20 RLE fixture: {error}"))?;
        for _ in 0..cover_count {
            rle_data.extend_from_slice(&[1, 0]);
        }

        diff_stream.extend_from_slice(&encode_varint(cover_size as u64));
        diff_stream.extend_from_slice(&encode_varint(rle_size as u64));
        diff_stream.extend_from_slice(&cover_data);
        diff_stream.extend_from_slice(&rle_data);
        if first_step {
            diff_stream.push(new[0]);
            is_first_step = false;
        }
    }

    let mut patch = b"HDIFF20&nocomp\0".to_vec();
    for value in [
        shape.output_bytes as u64,
        shape.output_bytes as u64,
        shape.cover_count as u64,
        max_step_size as u64,
        diff_stream.len() as u64,
        0,
    ] {
        patch.extend_from_slice(&encode_varint(value));
    }
    patch.extend_from_slice(&diff_stream);

    fs::write(dir.join("old.bin"), &old).map_err(|error| error.to_string())?;
    fs::write(dir.join("new.bin"), &new).map_err(|error| error.to_string())?;
    fs::write(dir.join("patch.hdiff"), patch).map_err(|error| error.to_string())?;
    Ok(shape)
}

pub fn uncovered_prefix_fixture_sizes(size_mib: usize) -> Result<(usize, usize), String> {
    const BLOCK_SIZE: usize = 64 * 1024;
    let output_size = size_mib
        .checked_mul(1024 * 1024)
        .ok_or_else(|| "HDiff fixture size is too large".to_owned())?;
    let aligned_size = output_size / BLOCK_SIZE * BLOCK_SIZE;
    // The fixture needs at least one uncovered block and one copied block.
    if aligned_size < BLOCK_SIZE * 2 {
        return Err("uncovered-prefix fixture requires at least two 64 KiB blocks".to_owned());
    }
    // Keep a copied suffix while leaving most output in the uncovered-data path.
    let suffix_size = (aligned_size / 16).max(BLOCK_SIZE);
    let suffix_size = suffix_size / BLOCK_SIZE * BLOCK_SIZE;
    let prefix_size = aligned_size - suffix_size;
    Ok((prefix_size, suffix_size))
}

pub fn prepare_hdiff_uncovered_prefix_fixture(
    dir: &Path,
    size_mib: usize,
) -> Result<(usize, usize), String> {
    let (prefix_size, suffix_size) = uncovered_prefix_fixture_sizes(size_mib)?;
    let old = deterministic_data(0xBAD5, suffix_size);
    let prefix = deterministic_data(0xBAD6, prefix_size);
    let output_size = prefix_size + suffix_size;
    let mut new = Vec::with_capacity(output_size);
    new.extend_from_slice(&prefix);
    new.extend_from_slice(&old);
    let covers = [(0, prefix_size, suffix_size)];
    let patch = make_v13_zstd_patch_from_covers(&old, &new, &covers);

    fs::write(dir.join("old.bin"), &old).map_err(|error| error.to_string())?;
    fs::write(dir.join("new.bin"), &new).map_err(|error| error.to_string())?;
    fs::write(dir.join("patch.hdiff"), patch).map_err(|error| error.to_string())?;
    Ok((prefix_size, suffix_size))
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
    fn generated_hdiff20_step_stress_fixture_applies_exactly() {
        let dir = tempfile::tempdir().unwrap();
        let shape = prepare_hdiff20_step_stress_fixture(dir.path(), 4).unwrap();
        assert_eq!(shape.output_bytes, 4 * 1024 * 1024);
        assert_eq!(shape.cover_count, shape.output_bytes - 1);
        assert_eq!(shape.step_sizes_bytes.len(), 2);
        assert_eq!(shape.step_sizes_bytes[0], MAX_HDIFF20_STEP_SIZE / 5 * 5);
        assert!(
            shape
                .step_sizes_bytes
                .iter()
                .all(|size| *size <= MAX_HDIFF20_STEP_SIZE)
        );
        verify_hdiff_fixture(dir.path()).unwrap();
    }

    #[test]
    fn hdiff20_step_stress_shape_grows_after_a_small_step() {
        let shape = hdiff20_step_stress_shape(8).unwrap();
        assert_eq!(shape.step_sizes_bytes.len(), 4);
        assert_eq!(shape.step_sizes_bytes[0], shape.step_sizes_bytes[2]);
        assert!(shape.step_sizes_bytes[1] < shape.step_sizes_bytes[0]);
        assert_eq!(shape.step_sizes_bytes[0], MAX_HDIFF20_STEP_SIZE / 5 * 5);
        assert_eq!(shape.cover_count, shape.output_bytes - 1);

        let seven_mib = hdiff20_step_stress_shape(7).unwrap();
        assert_eq!(seven_mib.step_sizes_bytes.len(), 4);
        assert_eq!(seven_mib.step_sizes_bytes[0], seven_mib.step_sizes_bytes[2]);
        assert!(seven_mib.step_sizes_bytes[3] > 0);
        assert_eq!(seven_mib.cover_count, seven_mib.output_bytes - 1);
    }

    #[test]
    fn hdiff20_step_stress_fixture_rejects_unbounded_sizes() {
        assert!(hdiff20_step_stress_shape(3).is_err());
        assert!(hdiff20_step_stress_shape(9).is_err());
        assert!(hdiff20_step_stress_shape(usize::MAX).is_err());
    }

    #[test]
    fn uncovered_prefix_fixture_applies_and_reports_component_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let (prefix_size, suffix_size) =
            prepare_hdiff_uncovered_prefix_fixture(dir.path(), 1).unwrap();
        assert_eq!(prefix_size + suffix_size, 1024 * 1024);
        assert_eq!(suffix_size, 64 * 1024);

        let old = fs::read(dir.path().join("old.bin")).unwrap();
        let new = fs::read(dir.path().join("new.bin")).unwrap();
        assert_eq!(old.len(), suffix_size);
        assert_eq!(new.len(), prefix_size + suffix_size);
        assert_eq!(&new[prefix_size..], old);

        verify_hdiff_fixture(dir.path()).unwrap();
    }

    #[test]
    fn uncovered_prefix_sizes_are_aligned_and_bounded() {
        const BLOCK_SIZE: usize = 64 * 1024;
        assert!(uncovered_prefix_fixture_sizes(0).is_err());
        assert_eq!(
            uncovered_prefix_fixture_sizes(1).unwrap(),
            (15 * BLOCK_SIZE, BLOCK_SIZE)
        );
        for size_mib in [2, 3, 16, 64] {
            let (prefix, suffix) = uncovered_prefix_fixture_sizes(size_mib).unwrap();
            assert!(prefix >= BLOCK_SIZE);
            assert!(suffix >= BLOCK_SIZE);
            assert_eq!(prefix % BLOCK_SIZE, 0);
            assert_eq!(suffix % BLOCK_SIZE, 0);
            assert_eq!(prefix + suffix, size_mib * 1024 * 1024);
        }
        assert!(uncovered_prefix_fixture_sizes(usize::MAX).is_err());
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
