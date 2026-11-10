use std::io::{Cursor, Read, SeekFrom, Write};

use super::{
    BufferPool, CoverHeader, HeaderInfo, K_BYTE_RLE_TYPE, K_SIGN_TAG_BIT, MAX_ARRAY_POOL_LEN,
    MAX_ARRAY_POOL_SECOND_OFFSET, MAX_MEM_BUFFER_LEN, RleRefClip, SeekableRead,
};
use crate::game_installer::hdiffpatch::parser::{BinaryExtensions, read_long_7bit_from_slice};

static SHARED_BUFFER_POOL: BufferPool = BufferPool::new(1);
static CACHE_BUFFER_POOL: BufferPool = BufferPool::new(1);
const REGION_CHUNK_SIZE: usize = 4 << 20;
const PROGRESS_INTERVAL: u64 = REGION_CHUNK_SIZE as u64;

pub(super) fn trim_idle_buffers() {
    SHARED_BUFFER_POOL.clear();
    CACHE_BUFFER_POOL.clear();
}

pub(crate) fn write_cover_stream_to_output(
    clips: &mut [Box<dyn Read>],
    input_stream: &mut dyn SeekableRead,
    output_stream: &mut dyn Write,
    header_info: &HeaderInfo,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    write_cover_stream_inner(
        clips,
        input_stream,
        None,
        output_stream,
        header_info,
        on_progress,
    )
}

#[cfg(test)]
pub(crate) fn write_cover_stream_to_output_with_chunk_size(
    clips: &mut [Box<dyn Read>],
    input_stream: &mut dyn SeekableRead,
    output_stream: &mut dyn Write,
    header_info: &HeaderInfo,
    chunk_size: usize,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    write_cover_stream_inner_with_chunk_size(
        clips,
        input_stream,
        None,
        output_stream,
        header_info,
        on_progress,
        chunk_size,
    )
}

/// Variant with direct old-data slice access, bypassing seek+read_exact.
pub(crate) fn write_cover_stream_to_output_with_slice(
    clips: &mut [Box<dyn Read>],
    old_data: &[u8],
    output_stream: &mut dyn Write,
    header_info: &HeaderInfo,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    // dummy never read when old_data is Some
    let mut dummy = Cursor::new(&[][..]);
    write_cover_stream_inner(
        clips,
        &mut dummy,
        Some(old_data),
        output_stream,
        header_info,
        on_progress,
    )
}

#[cfg(test)]
pub(crate) fn write_cover_stream_to_output_with_slice_and_chunk_size(
    clips: &mut [Box<dyn Read>],
    old_data: &[u8],
    output_stream: &mut dyn Write,
    header_info: &HeaderInfo,
    chunk_size: usize,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    let mut dummy = Cursor::new(&[][..]);
    write_cover_stream_inner_with_chunk_size(
        clips,
        &mut dummy,
        Some(old_data),
        output_stream,
        header_info,
        on_progress,
        chunk_size,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_cover_stream_inner(
    clips: &mut [Box<dyn Read>],
    input_stream: &mut dyn SeekableRead,
    old_data: Option<&[u8]>,
    output_stream: &mut dyn Write,
    header_info: &HeaderInfo,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    write_cover_stream_inner_with_chunk_size(
        clips,
        input_stream,
        old_data,
        output_stream,
        header_info,
        on_progress,
        REGION_CHUNK_SIZE,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_cover_stream_inner_with_chunk_size(
    clips: &mut [Box<dyn Read>],
    input_stream: &mut dyn SeekableRead,
    old_data: Option<&[u8]>,
    output_stream: &mut dyn Write,
    header_info: &HeaderInfo,
    on_progress: Option<&dyn Fn(u64)>,
    chunk_size: usize,
) -> std::io::Result<()> {
    if chunk_size == 0 {
        return Err(std::io::Error::other("region chunk size is zero"));
    }
    if chunk_size > REGION_CHUNK_SIZE {
        return Err(std::io::Error::other(
            "region chunk size exceeds the production cache limit",
        ));
    }
    if header_info.chunk_info.cover_count < 0 {
        return Err(std::io::Error::other("cover_count is negative"));
    }
    let mut shared_buffer = SHARED_BUFFER_POOL.take(MAX_ARRAY_POOL_LEN);
    if shared_buffer.capacity() < MAX_ARRAY_POOL_LEN {
        shared_buffer = Vec::with_capacity(MAX_ARRAY_POOL_LEN);
    }
    shared_buffer.resize(MAX_ARRAY_POOL_LEN, 0);
    let cache_vec = prepare_region_cache(CACHE_BUFFER_POOL.take(chunk_size), chunk_size);
    let mut cache = Cursor::new(cache_vec);

    let mut new_pos_back = 0i64;
    let mut total_written: u64 = 0;
    let mut last_reported = 0u64;
    let mut rle_struct = RleRefClip::default();
    let (cover_slice, rest_slice) = clips.split_first_mut().unwrap();
    let cover_reader = &mut **cover_slice;
    let rest_slice: &mut [Box<dyn Read>] = rest_slice;
    let [rle_ctrl_box, rle_code_box, new_data_box] = rest_slice else {
        return Err(std::io::Error::other("expected 3 remaining clip streams"));
    };
    let rle_ctrl_reader = &mut **rle_ctrl_box;
    let rle_code_reader = &mut **rle_code_box;
    let new_data_reader = &mut **new_data_box;
    let headers = enumerate_cover_headers_checked(
        cover_reader,
        header_info.chunk_info.cover_buf_size,
        header_info.chunk_info.cover_count,
    )?;

    let mut headers = headers;
    for cover_result in headers.by_ref() {
        let cover = cover_result?;
        if cover.new_pos < 0 {
            return Err(std::io::Error::other("cover new position is negative"));
        }
        if cover.new_pos < new_pos_back {
            return Err(std::io::Error::other(
                "backward or overlapping covers in patch",
            ));
        }
        if cover.cover_length < 0 {
            return Err(std::io::Error::other("cover length is negative"));
        }
        if cover.old_pos < 0 {
            return Err(std::io::Error::other("cover old position is negative"));
        }
        let old_cover_end = cover
            .old_pos
            .checked_add(cover.cover_length)
            .ok_or_else(|| std::io::Error::other("old cover length overflow"))?;
        if old_cover_end > header_info.old_data_size {
            return Err(std::io::Error::other(
                "cover extends past expected source size",
            ));
        }
        let cover_end = cover
            .new_pos
            .checked_add(cover.cover_length)
            .ok_or_else(|| std::io::Error::other("cover length overflow"))?;
        if cover_end > header_info.new_data_size {
            return Err(std::io::Error::other(
                "cover extends past expected output size",
            ));
        }
        if new_pos_back < cover.new_pos {
            let copy_length = cover.new_pos - new_pos_back;
            write_reader_region(
                &mut cache,
                new_data_reader,
                copy_length,
                chunk_size,
                output_stream,
                &mut total_written,
                &mut last_reported,
                on_progress,
                &mut shared_buffer,
                &mut rle_struct,
                rle_ctrl_reader,
                rle_code_reader,
            )?;
        }

        if let Some(slice) = old_data {
            write_slice_region(
                &mut cache,
                slice,
                cover.old_pos,
                cover.cover_length,
                chunk_size,
                output_stream,
                &mut total_written,
                &mut last_reported,
                on_progress,
                &mut shared_buffer,
                &mut rle_struct,
                rle_ctrl_reader,
                rle_code_reader,
            )?;
        } else {
            input_stream.seek(SeekFrom::Start(cover.old_pos as u64))?;
            write_reader_region(
                &mut cache,
                input_stream,
                cover.cover_length,
                chunk_size,
                output_stream,
                &mut total_written,
                &mut last_reported,
                on_progress,
                &mut shared_buffer,
                &mut rle_struct,
                rle_ctrl_reader,
                rle_code_reader,
            )?;
        }
        new_pos_back = cover
            .new_pos
            .checked_add(cover.cover_length)
            .ok_or_else(|| std::io::Error::other("new_pos overflow in cover iteration"))?;
    }

    if new_pos_back < header_info.new_data_size {
        let copy_length = header_info.new_data_size - new_pos_back;
        if copy_length < 0 {
            return Err(std::io::Error::other(
                "tail copy length is negative; diff is malformed",
            ));
        }
        write_reader_region(
            &mut cache,
            new_data_reader,
            copy_length,
            chunk_size,
            output_stream,
            &mut total_written,
            &mut last_reported,
            on_progress,
            &mut shared_buffer,
            &mut rle_struct,
            rle_ctrl_reader,
            rle_code_reader,
        )?;
    }
    let buffered_total = total_written
        .checked_add(cache.get_ref().len() as u64)
        .ok_or_else(|| std::io::Error::other("output byte count overflow"))?;
    if buffered_total != header_info.new_data_size as u64 {
        return Err(std::io::Error::other(
            "decoded output size differs from declared size",
        ));
    }
    flush_region_cache(
        &mut cache,
        output_stream,
        &mut total_written,
        &mut last_reported,
        on_progress,
    )?;
    if total_written > last_reported
        && let Some(callback) = on_progress
    {
        callback(total_written);
    }
    SHARED_BUFFER_POOL.return_buf(shared_buffer);
    CACHE_BUFFER_POOL.return_buf(cache.into_inner());
    Ok(())
}

fn write_cache_to_output(
    cache: &mut Cursor<Vec<u8>>,
    output: &mut dyn Write,
) -> std::io::Result<()> {
    let data = cache.get_ref();
    if !data.is_empty() {
        output.write_all(data)?;
        cache.get_mut().clear();
        cache.set_position(0);
    }
    Ok(())
}

fn prepare_region_cache(mut cache: Vec<u8>, chunk_size: usize) -> Vec<u8> {
    cache.clear();
    if cache.capacity() < chunk_size {
        cache.reserve_exact(chunk_size);
    } else if cache.capacity() > chunk_size {
        cache.shrink_to(chunk_size);
    }
    cache
}

#[allow(clippy::too_many_arguments)]
fn write_reader_region(
    cache: &mut Cursor<Vec<u8>>,
    source: &mut dyn Read,
    length: i64,
    chunk_size: usize,
    output: &mut dyn Write,
    total_written: &mut u64,
    last_reported: &mut u64,
    on_progress: Option<&dyn Fn(u64)>,
    shared_buffer: &mut [u8],
    rle_loader: &mut RleRefClip,
    rle_ctrl_stream: &mut dyn Read,
    rle_code_stream: &mut dyn Read,
) -> std::io::Result<()> {
    if length < 0 {
        return Err(std::io::Error::other("region length is negative"));
    }
    let mut remaining = length as u64;
    while remaining > 0 {
        let chunk_length = usize::try_from(remaining)
            .unwrap_or(usize::MAX)
            .min(chunk_size);
        let chunk_length_i64 = i64::try_from(chunk_length)
            .map_err(|_| std::io::Error::other("region chunk exceeds i64"))?;
        ensure_region_cache_room(
            cache,
            chunk_length,
            chunk_size,
            output,
            total_written,
            last_reported,
            on_progress,
        )?;
        let region_start = cache.get_ref().len();
        debug_assert_eq!(cache.position(), region_start as u64);
        tbytes_copy_stream_inner(source, cache, shared_buffer, chunk_length)?;
        cache.set_position(region_start as u64);
        apply_region_rle(
            cache,
            region_start,
            chunk_length_i64,
            shared_buffer,
            rle_loader,
            rle_ctrl_stream,
            rle_code_stream,
        )
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("decode output region chunk: {error}"))
        })?;
        if cache.get_ref().len() == chunk_size {
            flush_region_cache(cache, output, total_written, last_reported, on_progress)?;
        }
        remaining -= chunk_length as u64;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn write_slice_region(
    cache: &mut Cursor<Vec<u8>>,
    source: &[u8],
    source_offset: i64,
    length: i64,
    chunk_size: usize,
    output: &mut dyn Write,
    total_written: &mut u64,
    last_reported: &mut u64,
    on_progress: Option<&dyn Fn(u64)>,
    shared_buffer: &mut [u8],
    rle_loader: &mut RleRefClip,
    rle_ctrl_stream: &mut dyn Read,
    rle_code_stream: &mut dyn Read,
) -> std::io::Result<()> {
    if source_offset < 0 || length < 0 {
        return Err(std::io::Error::other("slice region has negative range"));
    }
    let start = usize::try_from(source_offset)
        .map_err(|_| std::io::Error::other("slice region offset does not fit memory"))?;
    let length = usize::try_from(length)
        .map_err(|_| std::io::Error::other("slice region length does not fit memory"))?;
    let end = start
        .checked_add(length)
        .ok_or_else(|| std::io::Error::other("slice region end overflow"))?;
    let source = source
        .get(start..end)
        .ok_or_else(|| std::io::Error::other("slice region exceeds old data"))?;
    let mut remaining = source;
    while !remaining.is_empty() {
        let chunk_length = remaining.len().min(chunk_size);
        let chunk_length_i64 = i64::try_from(chunk_length)
            .map_err(|_| std::io::Error::other("region chunk exceeds i64"))?;
        ensure_region_cache_room(
            cache,
            chunk_length,
            chunk_size,
            output,
            total_written,
            last_reported,
            on_progress,
        )?;
        let region_start = cache.get_ref().len();
        debug_assert_eq!(cache.position(), region_start as u64);
        cache.write_all(&remaining[..chunk_length])?;
        cache.set_position(region_start as u64);
        apply_region_rle(
            cache,
            region_start,
            chunk_length_i64,
            shared_buffer,
            rle_loader,
            rle_ctrl_stream,
            rle_code_stream,
        )
        .map_err(|error| {
            std::io::Error::new(error.kind(), format!("decode slice region chunk: {error}"))
        })?;
        if cache.get_ref().len() == chunk_size {
            flush_region_cache(cache, output, total_written, last_reported, on_progress)?;
        }
        remaining = &remaining[chunk_length..];
    }
    Ok(())
}

fn apply_region_rle(
    cache: &mut Cursor<Vec<u8>>,
    region_start: usize,
    length: i64,
    shared_buffer: &mut [u8],
    rle_loader: &mut RleRefClip,
    rle_ctrl_stream: &mut dyn Read,
    rle_code_stream: &mut dyn Read,
) -> std::io::Result<()> {
    tbytes_determine_rle_type(
        rle_loader,
        cache,
        length,
        shared_buffer,
        rle_ctrl_stream,
        rle_code_stream,
    )?;
    let expected_position = region_start
        .checked_add(length as usize)
        .ok_or_else(|| std::io::Error::other("decoded region cursor overflow"))?;
    if cache.position() != expected_position as u64 {
        return Err(std::io::Error::other(
            "RLE decoder did not consume the complete output region",
        ));
    }
    Ok(())
}

fn ensure_region_cache_room(
    cache: &mut Cursor<Vec<u8>>,
    chunk_length: usize,
    cache_limit: usize,
    output: &mut dyn Write,
    total_written: &mut u64,
    last_reported: &mut u64,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    if chunk_length > cache_limit {
        return Err(std::io::Error::other(
            "output region chunk exceeds cache limit",
        ));
    }
    if cache
        .get_ref()
        .len()
        .checked_add(chunk_length)
        .is_none_or(|needed| needed > cache_limit)
    {
        flush_region_cache(cache, output, total_written, last_reported, on_progress)?;
    }
    Ok(())
}

fn flush_region_cache(
    cache: &mut Cursor<Vec<u8>>,
    output: &mut dyn Write,
    total_written: &mut u64,
    last_reported: &mut u64,
    on_progress: Option<&dyn Fn(u64)>,
) -> std::io::Result<()> {
    let cache_len = cache.get_ref().len();
    if cache_len == 0 {
        return Ok(());
    }
    write_cache_to_output(cache, output)?;
    *total_written = total_written
        .checked_add(cache_len as u64)
        .ok_or_else(|| std::io::Error::other("output byte count overflow"))?;
    if let Some(callback) = on_progress
        && total_written.saturating_sub(*last_reported) >= PROGRESS_INTERVAL
    {
        callback(*total_written);
        *last_reported = *total_written;
    }
    Ok(())
}

fn tbytes_copy_stream_inner(
    input: &mut dyn Read,
    output: &mut Cursor<Vec<u8>>,
    shared_buffer: &mut [u8],
    mut read_len: usize,
) -> std::io::Result<()> {
    while read_len > 0 {
        let to_read = shared_buffer.len().min(read_len);
        input.read_exact(&mut shared_buffer[..to_read])?;
        output.write_all(&shared_buffer[..to_read])?;
        read_len -= to_read;
    }
    Ok(())
}

/// Drain the currently pending set/copy RLE run before reading another ctrl
/// byte. RLE ctrl records encode run lengths that can span many buffer-sized
/// decode steps (and even multiple covers), so the loop must keep applying
/// the pending run until it is exhausted or `copy_length` is consumed.
fn drain_rle_pending(
    rle_loader: &mut RleRefClip,
    out_cache: &mut Cursor<Vec<u8>>,
    copy_length: &mut i64,
    shared_buffer: &mut [u8],
    rle_code_stream: &mut dyn Read,
) -> std::io::Result<()> {
    while *copy_length > 0 && (rle_loader.mem_set_length > 0 || rle_loader.mem_copy_length > 0) {
        tbytes_set_rle(
            rle_loader,
            out_cache,
            copy_length,
            shared_buffer,
            rle_code_stream,
        )?;
    }
    Ok(())
}

fn tbytes_determine_rle_type(
    rle_loader: &mut RleRefClip,
    out_cache: &mut Cursor<Vec<u8>>,
    mut copy_length: i64,
    shared_buffer: &mut [u8],
    mut rle_ctrl_stream: &mut dyn Read,
    rle_code_stream: &mut dyn Read,
) -> std::io::Result<()> {
    // Drain any run left pending from a previous region before consuming the
    // next ctrl record.
    drain_rle_pending(
        rle_loader,
        out_cache,
        &mut copy_length,
        shared_buffer,
        rle_code_stream,
    )?;

    while copy_length > 0 {
        let mut p_sign_buf = [0u8; 1];
        rle_ctrl_stream.read_exact(&mut p_sign_buf)?;
        let p_sign = p_sign_buf[0];

        let rle_type = p_sign >> (8 - K_BYTE_RLE_TYPE);
        let raw_length = rle_ctrl_stream.read_long_7bit_tagged(K_BYTE_RLE_TYPE, p_sign)?;
        let length = raw_length
            .checked_add(1)
            .ok_or_else(|| std::io::Error::other("RLE length overflow after +1"))?;

        if rle_type == 3 {
            rle_loader.mem_copy_length = length;
        } else {
            rle_loader.mem_set_length = length;
            if rle_type == 2 {
                let mut val = [0u8; 1];
                rle_code_stream.read_exact(&mut val)?;
                rle_loader.mem_set_value = val[0];
            } else {
                rle_loader.mem_set_value = (0u8).wrapping_sub(rle_type);
            }
        }
        drain_rle_pending(
            rle_loader,
            out_cache,
            &mut copy_length,
            shared_buffer,
            rle_code_stream,
        )?;
    }
    Ok(())
}

/// Element-wise wrapping add of `src` into `dst` (`dst[i] += src[i]`).
/// Uses AVX2 when available at runtime, falling back to a generic loop.
#[inline]
fn wrapping_add_slices(dst: &mut [u8], src: &[u8]) {
    // Hard check (not debug_assert): the AVX2 path relies on src.len() >= n.
    let n = dst.len();
    let src = &src[..n];
    if n == 0 {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if n >= 32 && is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 support checked above; src.len() >= dst.len().
        return unsafe { wrapping_add_slices_avx2(dst, src) };
    }
    wrapping_add_slices_generic(dst, src);
}

#[cfg(target_arch = "x86_64")]
/// # Safety
/// Requires AVX2 CPU support at runtime and `src.len() >= dst.len()`.
#[target_feature(enable = "avx2")]
unsafe fn wrapping_add_slices_avx2(dst: &mut [u8], src: &[u8]) {
    use std::arch::x86_64::*;
    let n = dst.len();
    let dst_ptr = dst.as_mut_ptr();
    let src_ptr = src.as_ptr();
    let mut i = 0;
    while i + 32 <= n {
        // SAFETY: i + 32 <= n so both loads and the store are in-bounds.
        unsafe {
            let d = _mm256_loadu_si256(dst_ptr.add(i) as *const __m256i);
            let s = _mm256_loadu_si256(src_ptr.add(i) as *const __m256i);
            let r = _mm256_add_epi8(d, s);
            _mm256_storeu_si256(dst_ptr.add(i) as *mut __m256i, r);
        }
        i += 32;
    }
    if i + 16 <= n {
        // SAFETY: i + 16 <= n.
        unsafe {
            let d = _mm_loadu_si128(dst_ptr.add(i) as *const __m128i);
            let s = _mm_loadu_si128(src_ptr.add(i) as *const __m128i);
            let r = _mm_add_epi8(d, s);
            _mm_storeu_si128(dst_ptr.add(i) as *mut __m128i, r);
        }
        i += 16;
    }
    while i < n {
        // SAFETY: i < n.
        unsafe {
            *dst_ptr.add(i) = (*dst_ptr.add(i)).wrapping_add(*src_ptr.add(i));
        }
        i += 1;
    }
}

#[inline(always)]
fn wrapping_add_slices_generic(dst: &mut [u8], src: &[u8]) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d = d.wrapping_add(*s);
    }
}

/// Add a constant `v` to every byte in `dst` (`dst[i] += v`).
/// Uses AVX2 when available at runtime, falling back to a generic loop.
#[inline]
fn wrapping_add_const(dst: &mut [u8], v: u8) {
    let n = dst.len();
    if n == 0 || v == 0 {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if n >= 32 && is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 support checked just above; slice is valid.
        return unsafe { wrapping_add_const_avx2(dst, v) };
    }
    wrapping_add_const_generic(dst, v);
}

#[cfg(target_arch = "x86_64")]
/// # Safety
/// Requires AVX2 CPU support at runtime.
#[target_feature(enable = "avx2")]
unsafe fn wrapping_add_const_avx2(dst: &mut [u8], v: u8) {
    use std::arch::x86_64::*;
    let n = dst.len();
    let dst_ptr = dst.as_mut_ptr();
    let vvec = _mm256_set1_epi8(v as i8);
    let mut i = 0;
    while i + 32 <= n {
        // SAFETY: i + 32 <= n.
        unsafe {
            let d = _mm256_loadu_si256(dst_ptr.add(i) as *const __m256i);
            let r = _mm256_add_epi8(d, vvec);
            _mm256_storeu_si256(dst_ptr.add(i) as *mut __m256i, r);
        }
        i += 32;
    }
    let vvec128 = _mm_set1_epi8(v as i8);
    if i + 16 <= n {
        // SAFETY: i + 16 <= n.
        unsafe {
            let d = _mm_loadu_si128(dst_ptr.add(i) as *const __m128i);
            let r = _mm_add_epi8(d, vvec128);
            _mm_storeu_si128(dst_ptr.add(i) as *mut __m128i, r);
        }
        i += 16;
    }
    while i < n {
        // SAFETY: i < n.
        unsafe {
            *dst_ptr.add(i) = (*dst_ptr.add(i)).wrapping_add(v);
        }
        i += 1;
    }
}

#[inline(always)]
fn wrapping_add_const_generic(dst: &mut [u8], v: u8) {
    for b in dst.iter_mut() {
        *b = b.wrapping_add(v);
    }
}

pub(crate) fn tbytes_set_rle(
    rle_loader: &mut RleRefClip,
    out_cache: &mut Cursor<Vec<u8>>,
    copy_length: &mut i64,
    shared_buffer: &mut [u8],
    rle_code_stream: &mut dyn Read,
) -> std::io::Result<()> {
    tbytes_set_rle_single(rle_loader, out_cache, copy_length, shared_buffer)?;
    if rle_loader.mem_copy_length == 0 {
        return Ok(());
    }

    let decode_step = rle_loader
        .mem_copy_length
        .min(*copy_length)
        .min(MAX_ARRAY_POOL_SECOND_OFFSET as i64) as usize;
    let last_pos = out_cache.position() as usize;
    rle_code_stream.read_exact(&mut shared_buffer[..decode_step])?;
    let cache_data = out_cache.get_mut();
    let end_pos = last_pos
        .checked_add(decode_step)
        .ok_or_else(|| std::io::Error::other("RLE vector cache range overflow"))?;
    if end_pos > cache_data.len() {
        return Err(std::io::Error::other("RLE vector: cache underflow"));
    }
    // Reslice once so the per-element bounds check happens up-front, then use
    // the SIMD-accelerated element-wise add.
    let dst = &mut cache_data[last_pos..end_pos];
    let src = &shared_buffer[..decode_step];
    wrapping_add_slices(dst, src);
    out_cache.set_position(end_pos as u64);
    rle_loader.mem_copy_length -= decode_step as i64;
    *copy_length -= decode_step as i64;
    Ok(())
}

pub(crate) fn tbytes_set_rle_single(
    rle_loader: &mut RleRefClip,
    out_cache: &mut Cursor<Vec<u8>>,
    copy_length: &mut i64,
    shared_buffer: &mut [u8],
) -> std::io::Result<()> {
    if rle_loader.mem_set_length == 0 {
        return Ok(());
    }
    let mem_set_step = rle_loader
        .mem_set_length
        .min(*copy_length)
        .min(shared_buffer.len() as i64);

    if rle_loader.mem_set_value != 0 {
        // Operate directly on the cache Vec — avoids two memcpys through shared_buffer.
        // The broadcast add is SIMD-accelerated when AVX2 is available.
        let pos = out_cache.position() as usize;
        let len = mem_set_step as usize;
        let cache_data = out_cache.get_mut();
        if pos + len > cache_data.len() {
            return Err(std::io::Error::other("RLE single: cache underflow"));
        }
        let v = rle_loader.mem_set_value;
        wrapping_add_const(&mut cache_data[pos..pos + len], v);
        out_cache.set_position((pos + len) as u64);
    } else {
        let cur = out_cache.position();
        out_cache.set_position(cur + mem_set_step as u64);
    }
    *copy_length -= mem_set_step;
    rle_loader.mem_set_length -= mem_set_step;
    Ok(())
}

/// Iterator over validated cover headers borrowed from the encoded patch data.
pub struct CoverHeaderIterator<'a> {
    reader: CoverReader<'a>,
    remaining: i64,
    last_old_pos_back: i64,
    last_new_pos_back: i64,
}

enum CoverReader<'a> {
    Buffered {
        data: Vec<u8>,
        offset: usize,
    },
    Streaming {
        reader: std::io::BufReader<&'a mut dyn Read>,
    },
}

impl<'a> CoverHeaderIterator<'a> {
    pub(crate) fn new(
        cover_reader: &'a mut dyn Read,
        cover_size: i64,
        cover_count: i64,
    ) -> std::io::Result<Self> {
        if cover_count <= 0 {
            return Ok(Self {
                reader: CoverReader::Buffered {
                    data: Vec::new(),
                    offset: 0,
                },
                remaining: 0,
                last_old_pos_back: 0,
                last_new_pos_back: 0,
            });
        }
        let reader = if cover_size < MAX_MEM_BUFFER_LEN {
            let mut buffer = vec![0; cover_size as usize];
            cover_reader.read_exact(&mut buffer).map_err(|err| {
                std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    format!("failed to read cover data: {err}"),
                )
            })?;
            CoverReader::Buffered {
                data: buffer,
                offset: 0,
            }
        } else {
            CoverReader::Streaming {
                reader: std::io::BufReader::with_capacity(256 * 1024, cover_reader),
            }
        };
        Ok(Self {
            reader,
            remaining: cover_count,
            last_old_pos_back: 0,
            last_new_pos_back: 0,
        })
    }
}

impl<'a> Iterator for CoverHeaderIterator<'a> {
    type Item = std::io::Result<CoverHeader>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining <= 0 {
            return None;
        }
        self.remaining -= 1;

        let old_pos_back = self.last_old_pos_back;
        let new_pos_back = self.last_new_pos_back;

        let result = match &mut self.reader {
            CoverReader::Buffered { data, offset } => {
                if *offset >= data.len() {
                    return Some(Err(std::io::Error::other("cover header data truncated")));
                }
                let p_sign = data[*offset];
                *offset += 1;

                let inc_old_pos_sign = p_sign >> (8 - K_SIGN_TAG_BIT);
                let inc_old_pos =
                    match read_long_7bit_from_slice(data, offset, K_SIGN_TAG_BIT, p_sign) {
                        Ok(v) => v,
                        Err(err) => return Some(Err(err)),
                    };
                let old_pos = match if inc_old_pos_sign == 0 {
                    old_pos_back.checked_add(inc_old_pos)
                } else {
                    old_pos_back.checked_sub(inc_old_pos)
                } {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "old_pos overflow in cover header",
                        )));
                    }
                };
                if old_pos < 0 {
                    return Some(Err(std::io::Error::other(
                        "invalid negative old_pos in cover header",
                    )));
                }

                let copy_length = match read_long_7bit_from_slice(data, offset, 0, 0) {
                    Ok(v) => v,
                    Err(err) => return Some(Err(err)),
                };
                let cover_length = match read_long_7bit_from_slice(data, offset, 0, 0) {
                    Ok(v) => v,
                    Err(err) => return Some(Err(err)),
                };
                if copy_length < 0 || cover_length < 0 {
                    return Some(Err(std::io::Error::other(
                        "invalid negative copy_length or cover_length in cover header",
                    )));
                }
                let new_pos_back = match new_pos_back.checked_add(copy_length) {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "new_pos overflow in cover header",
                        )));
                    }
                };
                self.last_old_pos_back = match old_pos.checked_add(cover_length) {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "old_pos overflow in cover header",
                        )));
                    }
                };
                self.last_new_pos_back = match new_pos_back.checked_add(cover_length) {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "last_new_pos overflow in cover header",
                        )));
                    }
                };
                Ok(CoverHeader::new(old_pos, new_pos_back, cover_length))
            }
            CoverReader::Streaming { reader } => {
                let mut p_sign_buf = [0u8; 1];
                if reader.read_exact(&mut p_sign_buf).is_err() {
                    return Some(Err(std::io::Error::other("cover header data truncated")));
                }
                let p_sign = p_sign_buf[0];

                let inc_old_pos_sign = p_sign >> (8 - K_SIGN_TAG_BIT);
                let inc_old_pos = match reader.read_long_7bit_tagged(K_SIGN_TAG_BIT, p_sign) {
                    Ok(v) => v,
                    Err(err) => return Some(Err(err)),
                };
                let old_pos = match if inc_old_pos_sign == 0 {
                    old_pos_back.checked_add(inc_old_pos)
                } else {
                    old_pos_back.checked_sub(inc_old_pos)
                } {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "old_pos overflow in cover header (stream)",
                        )));
                    }
                };
                if old_pos < 0 {
                    return Some(Err(std::io::Error::other(
                        "invalid negative old_pos in cover header",
                    )));
                }

                let copy_length = match reader.read_long_7bit() {
                    Ok(v) => v,
                    Err(err) => return Some(Err(err)),
                };
                let cover_length = match reader.read_long_7bit() {
                    Ok(v) => v,
                    Err(err) => return Some(Err(err)),
                };
                if copy_length < 0 || cover_length < 0 {
                    return Some(Err(std::io::Error::other(
                        "invalid negative copy_length or cover_length in cover header",
                    )));
                }
                let new_pos_back = match new_pos_back.checked_add(copy_length) {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "new_pos overflow in cover header (stream)",
                        )));
                    }
                };
                self.last_old_pos_back = match old_pos.checked_add(cover_length) {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "old_pos overflow in cover header (stream)",
                        )));
                    }
                };
                self.last_new_pos_back = match new_pos_back.checked_add(cover_length) {
                    Some(v) => v,
                    None => {
                        return Some(Err(std::io::Error::other(
                            "last_new_pos overflow in cover header (stream)",
                        )));
                    }
                };
                Ok(CoverHeader::new(old_pos, new_pos_back, cover_length))
            }
        };

        Some(result)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining.max(0) as usize;
        (remaining, Some(remaining))
    }
}

impl<'a> ExactSizeIterator for CoverHeaderIterator<'a> {}

#[cfg(test)]
pub(crate) fn enumerate_cover_headers(
    cover_reader: &mut dyn Read,
    cover_size: i64,
    cover_count: i64,
) -> std::io::Result<Vec<CoverHeader>> {
    if cover_count < 0 {
        return Err(std::io::Error::other("cover_count is negative"));
    }
    if cover_size < 0 {
        return Err(std::io::Error::other("cover_size is negative"));
    }
    const MAX_COVER_COUNT: i64 = 50_000_000;
    const MAX_COVER_HEADERS_MEMORY: usize = 1 << 30;
    const COVER_HEADER_SIZE: usize = 32;
    let max_headers_by_memory = (MAX_COVER_HEADERS_MEMORY / COVER_HEADER_SIZE) as i64;
    if cover_count > MAX_COVER_COUNT || cover_count > max_headers_by_memory {
        return Err(std::io::Error::other(
            "cover_count exceeds safe maximum or memory limit",
        ));
    }
    if cover_count > 0 && cover_size == 0 {
        return Err(std::io::Error::other("cover_count > 0 but cover_size is 0"));
    }
    CoverHeaderIterator::new(cover_reader, cover_size, cover_count)?.collect()
}

#[allow(clippy::needless_lifetimes)]
/// Parse cover headers while validating ranges and returning structured errors.
pub fn enumerate_cover_headers_checked<'a>(
    cover_reader: &'a mut dyn Read,
    cover_size: i64,
    cover_count: i64,
) -> std::io::Result<CoverHeaderIterator<'a>> {
    if cover_count < 0 {
        return Err(std::io::Error::other("cover_count is negative"));
    }
    if cover_size < 0 {
        return Err(std::io::Error::other("cover_size is negative"));
    }
    const MAX_COVER_COUNT: i64 = 50_000_000;
    const MAX_COVER_HEADERS_MEMORY: usize = 1 << 30;
    const COVER_HEADER_SIZE: usize = 32;
    let max_headers_by_memory = (MAX_COVER_HEADERS_MEMORY / COVER_HEADER_SIZE) as i64;
    if cover_count > MAX_COVER_COUNT || cover_count > max_headers_by_memory {
        return Err(std::io::Error::other(
            "cover_count exceeds safe maximum or memory limit",
        ));
    }
    if cover_count > 0 && cover_size == 0 {
        return Err(std::io::Error::other("cover_count > 0 but cover_size is 0"));
    }
    CoverHeaderIterator::new(cover_reader, cover_size, cover_count)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::RleRefClip;
    use super::{tbytes_set_rle, tbytes_set_rle_single};

    #[test]
    fn region_cache_clears_contents_and_keeps_capacity_sufficient() {
        let exact = super::prepare_region_cache(vec![0x5a; 16], 16);
        assert!(exact.is_empty());
        assert!(exact.capacity() >= 16);

        let oversized = super::prepare_region_cache(vec![0x5a; 64], 16);
        assert!(oversized.is_empty());
        assert!(oversized.capacity() >= 16);

        let undersized = super::prepare_region_cache(Vec::with_capacity(4), 16);
        assert!(undersized.is_empty());
        assert!(undersized.capacity() >= 16);
    }

    #[test]
    fn reader_region_copies_exact_chunks_from_sequential_source() {
        fn encode_rle_identity_run(length: usize) -> Vec<u8> {
            let mut value = i64::try_from(length - 1).unwrap();
            let mut chunks = Vec::new();
            loop {
                chunks.push((value & 0x7f) as u8);
                value >>= 7;
                if value == 0 {
                    break;
                }
            }
            chunks.reverse();
            let mut first = (chunks[0] & 0x1f) | 0x20;
            if chunks.len() == 1 {
                first &= !0x20;
            }
            let mut encoded = vec![first];
            for (index, chunk) in chunks.iter().enumerate().skip(1) {
                encoded.push(*chunk | if index + 1 < chunks.len() { 0x80 } else { 0 });
            }
            encoded
        }

        let source = (0..3 * 1024 * 1024)
            .map(|index| index as u8)
            .collect::<Vec<_>>();
        let mut input = Cursor::new(source.clone());
        let mut cache = Cursor::new(Vec::with_capacity(512 * 1024));
        let mut output = Vec::new();
        let mut total_written = 0;
        let mut last_reported = 0;
        let mut shared_buffer = vec![0u8; 1024 * 1024];
        let mut rle_loader = RleRefClip::default();
        let mut rle_ctrl = Cursor::new(encode_rle_identity_run(source.len()));
        let mut rle_code = Cursor::new([]);

        super::write_reader_region(
            &mut cache,
            &mut input,
            source.len() as i64,
            512 * 1024,
            &mut output,
            &mut total_written,
            &mut last_reported,
            None,
            &mut shared_buffer,
            &mut rle_loader,
            &mut rle_ctrl,
            &mut rle_code,
        )
        .unwrap();

        assert_eq!(output, source);
        assert_eq!(input.position(), 3 * 1024 * 1024);
        assert_eq!(total_written, 3 * 1024 * 1024);
    }

    #[test]
    fn rle_ref_clip_default_initialization() {
        let rle = RleRefClip::default();
        assert_eq!(rle.mem_copy_length, 0, "mem_copy_length should be 0");
        assert_eq!(rle.mem_set_length, 0, "mem_set_length should be 0");
        assert_eq!(rle.mem_set_value, 0, "mem_set_value should be 0");
    }

    #[test]
    fn rle_ref_clip_copy_is_independent() {
        let rle1 = RleRefClip {
            mem_set_value: 0x42,
            mem_set_length: 10,
            ..Default::default()
        };

        let _rle2 = rle1; // Copy (not reference) - verified to be independent

        assert_eq!(
            rle1.mem_set_value, 0x42,
            "original should be unchanged after copy modification"
        );
    }

    /// Test mem_set_value == 0 behavior: should skip bytes without modification
    #[test]
    fn tbytes_set_rle_single_skip_when_mem_set_value_zero() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 5,
            mem_set_value: 0,
        };

        let mut cache = Cursor::new(vec![0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF]);
        let mut copy_length: i64 = 5;
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok(), "tbytes_set_rle_single should succeed");
        assert_eq!(copy_length, 0, "copy_length should be fully consumed");
        assert_eq!(
            rle_loader.mem_set_length, 0,
            "mem_set_length should be exhausted"
        );
        assert_eq!(cache.position(), 5, "cache position should advance by 5");
    }

    /// Test mem_set_value == 0xFF (byte flip via wrapping_neg): 0 - 1 = 0xFF
    #[test]
    fn tbytes_set_rle_single_byte_flip_with_ff() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 3,
            mem_set_value: 0xFF, // wrapping_neg(1) = 0xFF
        };

        let mut cache = Cursor::new(vec![0x00, 0x7F, 0x80]);
        let mut copy_length: i64 = 3;
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok(), "tbytes_set_rle_single should succeed");
        assert_eq!(copy_length, 0, "copy_length should be fully consumed");

        // Verify wrapping_add with 0xFF (which is wrapping_neg of 1):
        // 0x00 + 0xFF = 0xFF (255)
        // 0x7F (127) + 0xFF (255) = 382 mod 256 = 126 = 0x7E
        // 0x80 (128) + 0xFF (255) = 383 mod 256 = 127 = 0x7F
        let data = cache.get_ref();
        assert_eq!(data[0], 0xFF, "byte 0 should be 0xFF");
        assert_eq!(data[1], 0x7E, "byte 1 should be 0x7E");
        assert_eq!(data[2], 0x7F, "byte 2 should be 0x7F");
    }

    /// Test wrapping_add behavior at boundary (0xFF + 0x01 = 0x00)
    #[test]
    fn tbytes_set_rle_single_wrapping_at_boundary() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 4,
            mem_set_value: 0x01, // wrapping_add will wrap at 0xFF
        };

        // Bytes at boundary: 0xFF should wrap to 0x00
        let mut cache = Cursor::new(vec![0xFF, 0xFF, 0x00, 0x7F]);
        let mut copy_length: i64 = 4;
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok(), "tbytes_set_rle_single should succeed");
        // 0xFF + 0x01 = 0x00 (wrapped), 0xFF + 0x01 = 0x00, 0x00 + 0x01 = 0x01, 0x7F +
        // 0x01 = 0x80
        let data = cache.get_ref();
        assert_eq!(data[0], 0x00, "0xFF + 0x01 should wrap to 0x00");
        assert_eq!(data[1], 0x00, "0xFF + 0x01 should wrap to 0x00");
        assert_eq!(data[2], 0x01, "0x00 + 0x01 should be 0x01");
        assert_eq!(data[3], 0x80, "0x7F + 0x01 should be 0x80");
    }

    /// Test mem_set_length > copy_length: should only process copy_length bytes
    #[test]
    fn tbytes_set_rle_single_truncates_to_copy_length() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 10, // More than copy_length
            mem_set_value: 0x0F,
        };

        let mut cache = Cursor::new(vec![0x00, 0x11, 0x22, 0x33]);
        let mut copy_length: i64 = 4; // Only 4 bytes to process
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok(), "tbytes_set_rle_single should succeed");
        assert_eq!(copy_length, 0, "copy_length should be exhausted");
        // mem_set_length should be reduced by 4 (copy_length), not to 0
        assert_eq!(
            rle_loader.mem_set_length, 6,
            "mem_set_length should have 6 remaining"
        );
    }

    /// Test mem_set_length == 0: should return early without modification
    #[test]
    fn tbytes_set_rle_single_early_return_when_no_length() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 0, // No work to do
            mem_set_value: 0xFF,
        };

        let mut cache = Cursor::new(vec![0xAA, 0xBB]);
        let mut copy_length: i64 = 2;
        let mut shared_buffer = [0u8; 32];
        let initial_pos = cache.position();

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok(), "tbytes_set_rle_single should succeed");
        assert_eq!(copy_length, 2, "copy_length should be unchanged");
        assert_eq!(
            cache.position(),
            initial_pos,
            "cache position should not advance"
        );
    }

    /// When mem_set_length exceeds shared_buffer.len(), the step should be
    /// capped to the buffer size. Only shared_buffer.len() bytes are processed.
    #[test]
    fn tbytes_set_rle_single_caps_by_small_shared_buffer() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 100, // much larger than buffer
            mem_set_value: 0x01,
        };

        let mut cache = Cursor::new(vec![0x00; 100]);
        let mut copy_length: i64 = 100;
        let mut shared_buffer = [0u8; 4]; // tiny buffer

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok(), "should succeed even with tiny buffer");
        // mem_set_step = min(100, 100, 4) = 4 bytes processed
        assert_eq!(copy_length, 96, "copy_length should be reduced by 4");
        assert_eq!(
            rle_loader.mem_set_length, 96,
            "mem_set_length should have 96 remaining"
        );
    }

    /// When both mem_set_length and copy_length are larger than the buffer,
    /// but copy_length is the smallest, step is capped to copy_length.
    #[test]
    fn tbytes_set_rle_single_caps_by_copy_length_when_smallest() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 100,
            mem_set_value: 0x01,
        };

        let mut cache = Cursor::new(vec![0x00; 100]);
        let mut copy_length: i64 = 3; // smaller than buffer
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok());
        // mem_set_step = min(100, 3, 32) = 3
        assert_eq!(copy_length, 0, "copy_length should be exhausted");
        assert_eq!(
            rle_loader.mem_set_length, 97,
            "mem_set_length should have 97 remaining"
        );
    }

    /// When mem_set_value is 0 and step is capped by small buffer, position
    /// advances by the buffer size without modifying data.
    #[test]
    fn tbytes_set_rle_single_zero_value_capped_by_buffer() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 50,
            mem_set_value: 0, // skip mode
        };

        let mut cache = Cursor::new(vec![0xAA; 50]);
        let mut copy_length: i64 = 50;
        let mut shared_buffer = [0u8; 8]; // tiny buffer

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok());
        // mem_set_step = min(50, 50, 8) = 8 bytes skipped
        assert_eq!(cache.position(), 8, "should advance 8 bytes");
        assert_eq!(copy_length, 42, "copy_length should be 50 - 8 = 42");
        assert_eq!(
            rle_loader.mem_set_length, 42,
            "mem_set_length should have 42 remaining"
        );
    }

    /// When both mem_set_length and mem_copy_length are 0, tbytes_set_rle
    /// should return immediately without doing anything.
    #[test]
    fn tbytes_set_rle_empty_stream_no_modification() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 0,
            mem_set_value: 0,
        };

        let mut cache = Cursor::new(vec![0xAA, 0xBB, 0xCC]);
        let mut copy_length: i64 = 3;
        let mut shared_buffer = [0u8; 32];
        let initial_pos = cache.position();

        // Create an empty rle_code_stream (no data to read)
        let mut rle_code: &[u8] = &[];
        let result = tbytes_set_rle(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
            &mut rle_code,
        );

        assert!(result.is_ok(), "empty RLE stream should succeed");
        assert_eq!(copy_length, 3, "copy_length should be unchanged");
        assert_eq!(
            cache.position(),
            initial_pos,
            "cache position should not advance"
        );
    }

    #[test]
    fn tbytes_set_rle_literal_adds_in_place_and_updates_counters() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 4,
            mem_set_length: 0,
            mem_set_value: 0,
        };
        let mut cache = Cursor::new(vec![0xff, 0x80, 0x10, 0x7f]);
        let mut copy_length = 4;
        let mut shared_buffer = [0; 8];
        let mut rle_code: &[u8] = &[0x01, 0x80, 0xfe, 0x02];

        tbytes_set_rle(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
            &mut rle_code,
        )
        .unwrap();

        assert_eq!(cache.get_ref(), &[0x00, 0x00, 0x0e, 0x81]);
        assert_eq!(cache.position(), 4);
        assert_eq!(rle_loader.mem_copy_length, 0);
        assert_eq!(copy_length, 0);
        assert!(rle_code.is_empty());
    }

    #[test]
    fn tbytes_set_rle_literal_processes_maximum_chunks() {
        use super::MAX_ARRAY_POOL_SECOND_OFFSET;

        let len = MAX_ARRAY_POOL_SECOND_OFFSET + 3;
        let original = vec![0x80; len];
        let deltas = vec![0x80; len];
        let mut cache = Cursor::new(original);
        let mut rle_loader = RleRefClip {
            mem_copy_length: len as i64,
            ..Default::default()
        };
        let mut copy_length = len as i64;
        let mut shared_buffer = vec![0; MAX_ARRAY_POOL_SECOND_OFFSET * 2];
        let mut rle_code = deltas.as_slice();

        while copy_length > 0 {
            tbytes_set_rle(
                &mut rle_loader,
                &mut cache,
                &mut copy_length,
                &mut shared_buffer,
                &mut rle_code,
            )
            .unwrap();
        }

        assert!(cache.get_ref().iter().all(|&byte| byte == 0));
        assert_eq!(rle_loader.mem_copy_length, 0);
        assert!(rle_code.is_empty());
    }

    #[test]
    fn tbytes_set_rle_literal_rejects_cache_underflow() {
        let mut cache = Cursor::new(vec![0xaa, 0xbb]);
        cache.set_position(1);
        let mut rle_loader = RleRefClip {
            mem_copy_length: 2,
            ..Default::default()
        };
        let mut copy_length = 2;
        let mut shared_buffer = [0; 4];
        let mut rle_code: &[u8] = &[1, 2];

        let error = tbytes_set_rle(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
            &mut rle_code,
        )
        .unwrap_err();

        assert_eq!(error.to_string(), "RLE vector: cache underflow");
        assert_eq!(cache.get_ref(), &[0xaa, 0xbb]);
        assert_eq!(cache.position(), 1);
        assert_eq!(copy_length, 2);
        assert_eq!(rle_loader.mem_copy_length, 2);
    }

    /// When mem_copy_length exceeds MAX_ARRAY_POOL_SECOND_OFFSET, the decode
    /// step is capped to MAX_ARRAY_POOL_SECOND_OFFSET in tbytes_set_rle.
    #[test]
    fn tbytes_set_rle_caps_decode_step_to_second_offset() {
        use super::MAX_ARRAY_POOL_SECOND_OFFSET;

        let len = MAX_ARRAY_POOL_SECOND_OFFSET + 100;
        let mut cache = Cursor::new(vec![0; len]);
        let mut rle_loader = RleRefClip {
            mem_copy_length: len as i64,
            ..Default::default()
        };
        let mut copy_length = len as i64;
        let mut shared_buffer = vec![0; MAX_ARRAY_POOL_SECOND_OFFSET * 2];
        let deltas = vec![1; len];
        let mut rle_code = deltas.as_slice();

        tbytes_set_rle(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
            &mut rle_code,
        )
        .unwrap();

        assert_eq!(copy_length, 100);
        assert_eq!(rle_loader.mem_copy_length, 100);
    }

    /// When mem_copy_length is zero, tbytes_set_rle skips the vector step
    /// entirely (returns early after calling tbytes_set_rle_single).
    #[test]
    fn tbytes_set_rle_zero_copy_length_returns_early() {
        use super::tbytes_set_rle;

        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 0,
            mem_set_value: 0,
        };

        let mut cache = Cursor::new(vec![0x00; 8]);
        let mut copy_length: i64 = 8;
        let mut shared_buffer = [0u8; 32];
        let mut rle_code: &[u8] = &[];

        let result = tbytes_set_rle(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
            &mut rle_code,
        );

        assert!(result.is_ok());
        // Since both mem_set_length and mem_copy_length are 0, nothing happens
        assert_eq!(copy_length, 8, "nothing consumed");
    }

    #[test]
    fn reader_region_rejects_negative_length_before_reading() {
        let mut cache = Cursor::new(Vec::new());
        let mut reader: &[u8] = &[];
        let mut shared_buffer = [0u8; 32];
        let mut output = Vec::new();
        let mut total_written = 0;
        let mut last_reported = 0;
        let mut rle_loader = RleRefClip::default();
        let mut rle_ctrl = Cursor::new([]);
        let mut rle_code = Cursor::new([]);
        let result = super::write_reader_region(
            &mut cache,
            &mut reader,
            -1,
            16,
            &mut output,
            &mut total_written,
            &mut last_reported,
            None,
            &mut shared_buffer,
            &mut rle_loader,
            &mut rle_ctrl,
            &mut rle_code,
        );
        assert!(result.is_err());
        assert!(output.is_empty());
        assert_eq!(total_written, 0);
    }

    /// tbytes_set_rle_single with basic non-zero addition (add 0x10).
    #[test]
    fn tbytes_set_rle_single_basic_addition() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 3,
            mem_set_value: 0x10, // add 16 to each byte
        };
        let mut cache = Cursor::new(vec![0x10, 0x20, 0x30]);
        let mut copy_length: i64 = 3;
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok());
        // 0x10 + 0x10 = 0x20, 0x20 + 0x10 = 0x30, 0x30 + 0x10 = 0x40
        let data = cache.get_ref();
        assert_eq!(data[0], 0x20);
        assert_eq!(data[1], 0x30);
        assert_eq!(data[2], 0x40);
    }

    /// tbytes_set_rle_single with single byte addition (minimum non-trivial
    /// case).
    #[test]
    fn tbytes_set_rle_single_single_byte_addition() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 1,
            mem_set_value: 0x01,
        };
        let mut cache = Cursor::new(vec![0x00]);
        let mut copy_length: i64 = 1;
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok());
        assert_eq!(copy_length, 0, "copy_length should be consumed");
        assert_eq!(
            rle_loader.mem_set_length, 0,
            "mem_set_length should be exhausted"
        );
        assert_eq!(cache.get_ref()[0], 0x01, "0x00 + 0x01 = 0x01");
    }

    /// tbytes_set_rle_single with non-zero mem_set_value capped by a small
    /// shared buffer.
    #[test]
    fn tbytes_set_rle_single_non_zero_capped_by_small_buffer() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 100,
            mem_set_value: 0x10, // non-zero
        };
        let mut cache = Cursor::new(vec![0x00; 100]);
        let mut copy_length: i64 = 100;
        let mut shared_buffer = [0u8; 4]; // tiny buffer

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok());
        // mem_set_step = min(100, 100, 4) = 4 bytes processed
        assert_eq!(copy_length, 96, "copy_length should be reduced by 4");
        assert_eq!(
            rle_loader.mem_set_length, 96,
            "mem_set_length should have 96 remaining"
        );
        // Verify the cache bytes were modified (non-zero code path)
        let data = cache.get_ref();
        assert_eq!(data[0], 0x10, "0x00 + 0x10 = 0x10");
        assert_eq!(data[3], 0x10, "0x00 + 0x10 = 0x10");
    }

    /// tbytes_set_rle_single with mem_set_value = 0x80 and data = 0x80 tests
    /// wrapping at 0x80 + 0x80 = 0x00.
    #[test]
    fn tbytes_set_rle_single_wrapping_0x80_plus_0x80() {
        let mut rle_loader = RleRefClip {
            mem_copy_length: 0,
            mem_set_length: 1,
            mem_set_value: 0x80,
        };
        let mut cache = Cursor::new(vec![0x80]);
        let mut copy_length: i64 = 1;
        let mut shared_buffer = [0u8; 32];

        let result = tbytes_set_rle_single(
            &mut rle_loader,
            &mut cache,
            &mut copy_length,
            &mut shared_buffer,
        );

        assert!(result.is_ok());
        assert_eq!(cache.get_ref()[0], 0x00, "0x80 + 0x80 should wrap to 0x00");
    }

    fn wrapping_add_slices_ref(dst: &mut [u8], src: &[u8]) {
        for (d, s) in dst.iter_mut().zip(src.iter()) {
            *d = d.wrapping_add(*s);
        }
    }

    fn wrapping_add_const_ref(dst: &mut [u8], v: u8) {
        for b in dst.iter_mut() {
            *b = b.wrapping_add(v);
        }
    }

    #[test]
    fn wrapping_add_slices_matches_generic_across_lengths() {
        let mut rng_state = 0xDEADBEEFu64;
        let mut next_byte = || {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng_state >> 32) as u8
        };
        for &len in &[
            0usize, 1, 15, 16, 17, 31, 32, 33, 47, 48, 63, 64, 65, 127, 128, 1024, 4096,
        ] {
            let mut dst_simd: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            let mut dst_ref = dst_simd.clone();
            let src: Vec<u8> = (0..len).map(|_| next_byte()).collect();
            super::wrapping_add_slices(&mut dst_simd, &src);
            wrapping_add_slices_ref(&mut dst_ref, &src);
            assert_eq!(
                dst_simd, dst_ref,
                "wrapping_add_slices mismatch at len {len}"
            );
        }
    }

    #[test]
    fn wrapping_add_slices_handles_src_longer_than_dst() {
        let mut dst = vec![0xAAu8; 64];
        let src = vec![0x55u8; 128];
        super::wrapping_add_slices(&mut dst, &src);
        assert!(dst.iter().all(|&b| b == 0xFF));
    }

    #[test]
    fn wrapping_add_slices_handles_unaligned_offsets() {
        let mut rng_state = 0x1234u64;
        let mut next_byte = || {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng_state >> 32) as u8
        };
        let total = 256usize;
        let backing: Vec<u8> = (0..total).map(|_| next_byte()).collect();
        for &start in &[0usize, 1, 7, 15, 16, 17, 31] {
            for &len in &[0usize, 1, 16, 31, 32, 33, 64, 100] {
                if start + len > backing.len() {
                    continue;
                }
                let mut dst_simd = backing[start..start + len].to_vec();
                let mut dst_ref = dst_simd.clone();
                let src: Vec<u8> = (0..len).map(|_| next_byte()).collect();
                super::wrapping_add_slices(&mut dst_simd, &src);
                wrapping_add_slices_ref(&mut dst_ref, &src);
                assert_eq!(
                    dst_simd, dst_ref,
                    "unaligned mismatch at start {start}, len {len}"
                );
            }
        }
    }

    #[test]
    fn wrapping_add_const_matches_generic_across_lengths() {
        let mut rng_state = 0xC0FFEEu64;
        let mut next_byte = || {
            rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
            (rng_state >> 32) as u8
        };
        for &len in &[
            0usize, 1, 15, 16, 17, 31, 32, 33, 47, 48, 63, 64, 65, 127, 128, 1024, 4096,
        ] {
            for &v in &[0u8, 1, 0x37, 0x80, 0xFF] {
                let mut dst_simd: Vec<u8> = (0..len).map(|_| next_byte()).collect();
                let mut dst_ref = dst_simd.clone();
                super::wrapping_add_const(&mut dst_simd, v);
                wrapping_add_const_ref(&mut dst_ref, v);
                assert_eq!(
                    dst_simd, dst_ref,
                    "wrapping_add_const mismatch at len {len}, v {v:#x}"
                );
            }
        }
    }

    #[test]
    #[should_panic(expected = "out of range for slice")]
    fn wrapping_add_slices_panics_on_short_src() {
        let mut dst = vec![0u8; 64];
        let src = vec![0u8; 32];
        super::wrapping_add_slices(&mut dst, &src);
    }
}
