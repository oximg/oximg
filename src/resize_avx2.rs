//! AVX2+FMA f32 row stages for the u16 separable-convolution resize on
//! x86-64, an op-for-op port of the aarch64 NEON kernel
//! ([`crate::resize_neon`]). The shared driver, window math, and
//! schedule invariants live in [`crate::resize_kernel`].
//!
//! Stage design mirrors the NEON kernel, widened from 128-bit to
//! 256-bit vectors where the data layout allows:
//! - each source row is converted from interleaved u16 to f32 exactly
//!   once (planar for 3 channels, interleaved for 4) instead of being
//!   re-loaded and re-widened by every overlapping window;
//! - x86 has no structure loads (NEON's LD3/TBL), so deinterleaving
//!   widens eight RGB pixels to three f32x8 vectors and splits them
//!   into planes with cross-lane permutes plus blends;
//! - the vertical pass keeps its accumulators in registers across all
//!   taps of a 32-column tile rather than round-tripping an
//!   accumulator row through memory once per tap.
//!
//! Correctness contract: identical to the NEON kernel's — the f32
//! operation sequence per output value is independent of scheduling,
//! so the strip-mined ring and streamed emission are bit-identical to
//! the full-intermediate reference schedule (asserted by tests). The
//! horizontal accumulation/reduction trees differ from NEON's (8-lane
//! blocks in `horiz_row_x3`, tap pairs in `horiz_row_x4`), which only
//! the cross-arch accuracy comparison sees; the f64 ground-truth tests
//! hold both to the same ≤2 LSB envelope.

use crate::resize_kernel::{RowKernel, Windows, clamp_u16, resize_u16};
use anyhow::Result;

/// Marker type implementing [`RowKernel`] with AVX2+FMA intrinsics.
pub(crate) struct Avx2;

impl RowKernel for Avx2 {
    fn detect() -> bool {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    }
    unsafe fn stage_x3(row: &[u16], stage: &mut [f32], w: usize) {
        unsafe { stage_row_x3(row, stage, w) }
    }
    unsafe fn stage_x4(row: &[u16], stage: &mut [f32]) {
        unsafe { stage_row_x4(row, stage) }
    }
    unsafe fn horiz_x3(
        stage: &[f32],
        src_w: usize,
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slot: usize,
        dst_w: usize,
    ) {
        unsafe { horiz_row_x3(stage, src_w, w, ring, plane, slot, dst_w) }
    }
    unsafe fn horiz_x4(
        stage: &[f32],
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slot: usize,
        dst_w: usize,
    ) {
        unsafe { horiz_row_x4(stage, w, ring, plane, slot, dst_w) }
    }
    unsafe fn vert(plane: &[f32], coeffs: &[f32], offs: &[usize], dst_w: usize, acc: &mut [f32]) {
        unsafe { vert_accumulate(plane, coeffs, offs, dst_w, acc) }
    }
    unsafe fn store_x3(acc: &[f32], dst_w: usize, out: &mut [u16]) {
        unsafe { store_row_x3(acc, dst_w, out) }
    }
    unsafe fn store_x4(acc: &[f32], dst_w: usize, out: &mut [u16]) {
        unsafe { store_row_x4(acc, dst_w, out) }
    }
}

/// Resize interleaved u16 pixels (3 or 4 channels) with Lanczos3.
/// `src_bytes`/`dst_bytes` are the raw little-endian u16 buffers.
pub fn resize_u16_avx2(
    src_bytes: &[u8],
    src_w: usize,
    src_h: usize,
    dst_bytes: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    channels: usize,
) -> Result<()> {
    resize_u16::<Avx2>(src_bytes, src_w, src_h, dst_bytes, dst_w, dst_h, channels)
}

/// Widen eight contiguous u16 samples to f32 (exact: u16 < 2^24).
#[inline]
#[target_feature(enable = "avx2,fma")]
unsafe fn widen8(p: *const u16) -> std::arch::x86_64::__m256 {
    unsafe {
        use std::arch::x86_64::*;
        _mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(_mm_loadu_si128(p.cast())))
    }
}

/// Deinterleave one u16 RGB row into three planar f32 rows
/// (stage[0..w], stage[w..2w], stage[2w..3w]). Exact conversion, so the
/// convolution below sees the same operand values as widening on the
/// fly. Per eight-pixel (48-byte) group, three 128-bit loads widen to
/// three f32x8 vectors holding the interleaved samples; each plane is
/// then assembled by gathering that plane's samples from each vector to
/// their output lanes with a cross-lane permute and merging the three
/// with blends (the classic AVX2 float RGB deinterleave).
#[target_feature(enable = "avx2,fma")]
unsafe fn stage_row_x3(row: &[u16], stage: &mut [f32], w: usize) {
    unsafe {
        use std::arch::x86_64::*;
        // Widened vectors of an eight-pixel group:
        //   f0 = [r0 g0 b0 r1 g1 b1 r2 g2]
        //   f1 = [b2 r3 g3 b3 r4 g4 b4 r5]
        //   f2 = [g5 b5 r6 g6 b6 r7 g7 b7]
        // Index vectors map output lane j to the source lane holding
        // that plane's sample (lanes discarded by the blends are 0).
        let idx_r0 = _mm256_setr_epi32(0, 3, 6, 0, 0, 0, 0, 0);
        let idx_r1 = _mm256_setr_epi32(0, 0, 0, 1, 4, 7, 0, 0);
        let idx_r2 = _mm256_setr_epi32(0, 0, 0, 0, 0, 0, 2, 5);
        let idx_g0 = _mm256_setr_epi32(1, 4, 7, 0, 0, 0, 0, 0);
        let idx_g1 = _mm256_setr_epi32(0, 0, 0, 2, 5, 0, 0, 0);
        let idx_g2 = _mm256_setr_epi32(0, 0, 0, 0, 0, 0, 3, 6);
        let idx_b0 = _mm256_setr_epi32(2, 5, 0, 0, 0, 0, 0, 0);
        let idx_b1 = _mm256_setr_epi32(0, 0, 0, 3, 6, 0, 0, 0);
        let idx_b2 = _mm256_setr_epi32(0, 0, 0, 0, 0, 1, 4, 7);

        let (r_out, rest) = stage.split_at_mut(w);
        let (g_out, b_out) = rest.split_at_mut(w);
        let mut x = 0usize;
        while x + 8 <= w {
            let p = row.as_ptr().add(x * 3);
            let f0 = widen8(p);
            let f1 = widen8(p.add(8));
            let f2 = widen8(p.add(16));
            // R at lanes 0-2 / 3-5 / 6-7 of the three permutes; G at
            // 0-2 / 3-4 / 5-7; B at 0-1 / 2-4 / 5-7.
            let r = _mm256_blend_ps::<0b1100_0000>(
                _mm256_blend_ps::<0b0011_1000>(
                    _mm256_permutevar8x32_ps(f0, idx_r0),
                    _mm256_permutevar8x32_ps(f1, idx_r1),
                ),
                _mm256_permutevar8x32_ps(f2, idx_r2),
            );
            let g = _mm256_blend_ps::<0b1110_0000>(
                _mm256_blend_ps::<0b0001_1000>(
                    _mm256_permutevar8x32_ps(f0, idx_g0),
                    _mm256_permutevar8x32_ps(f1, idx_g1),
                ),
                _mm256_permutevar8x32_ps(f2, idx_g2),
            );
            let b = _mm256_blend_ps::<0b1110_0000>(
                _mm256_blend_ps::<0b0001_1100>(
                    _mm256_permutevar8x32_ps(f0, idx_b0),
                    _mm256_permutevar8x32_ps(f1, idx_b1),
                ),
                _mm256_permutevar8x32_ps(f2, idx_b2),
            );
            _mm256_storeu_ps(r_out.as_mut_ptr().add(x), r);
            _mm256_storeu_ps(g_out.as_mut_ptr().add(x), g);
            _mm256_storeu_ps(b_out.as_mut_ptr().add(x), b);
            x += 8;
        }
        while x < w {
            r_out[x] = row[x * 3] as f32;
            g_out[x] = row[x * 3 + 1] as f32;
            b_out[x] = row[x * 3 + 2] as f32;
            x += 1;
        }
    }
}

/// Convert one u16 RGBA row to f32, keeping the interleaved layout (a
/// pixel stays one f32x4 lane group).
#[target_feature(enable = "avx2,fma")]
unsafe fn stage_row_x4(row: &[u16], stage: &mut [f32]) {
    unsafe {
        use std::arch::x86_64::*;
        let n = row.len();
        let mut i = 0usize;
        while i + 8 <= n {
            _mm256_storeu_ps(stage.as_mut_ptr().add(i), widen8(row.as_ptr().add(i)));
            i += 8;
        }
        while i < n {
            stage[i] = row[i] as f32;
            i += 1;
        }
    }
}

/// Sum the eight lanes of `v` (extract-high + three shuffled adds).
/// Safe to call from `avx2,fma` contexts: value-only intrinsics.
#[inline]
#[target_feature(enable = "avx2,fma")]
fn hsum(v: std::arch::x86_64::__m256) -> f32 {
    use std::arch::x86_64::*;
    let s = _mm_add_ps(_mm256_castps256_ps128(v), _mm256_extractf128_ps::<1>(v));
    let s = _mm_add_ps(s, _mm_movehl_ps(s, s));
    let s = _mm_add_ss(s, _mm_movehdup_ps(s));
    _mm_cvtss_f32(s)
}

/// One horizontal row, 3 channels, reading planar f32 staged rows:
/// per output pixel, 8-tap blocks accumulate each channel in a f32x8
/// register, followed by a lane reduction and NEON-identical scalar
/// tail taps.
#[target_feature(enable = "avx2,fma")]
unsafe fn horiz_row_x3(
    stage: &[f32],
    src_w: usize,
    w: &Windows,
    ring: &mut [f32],
    plane: usize,
    slot: usize,
    dst_w: usize,
) {
    unsafe {
        use std::arch::x86_64::*;
        let r_in = stage.as_ptr();
        let g_in = stage.as_ptr().add(src_w);
        let b_in = stage.as_ptr().add(2 * src_w);
        for ox in 0..dst_w {
            let start = w.starts[ox];
            let size = w.sizes[ox];
            let coeffs = &w.coeffs[ox * w.window_size..ox * w.window_size + size];

            let mut acc = [_mm256_setzero_ps(); 3];
            let mut k = 0usize;
            while k + 8 <= size {
                let cv = _mm256_loadu_ps(coeffs.as_ptr().add(k));
                acc[0] = _mm256_fmadd_ps(_mm256_loadu_ps(r_in.add(start + k)), cv, acc[0]);
                acc[1] = _mm256_fmadd_ps(_mm256_loadu_ps(g_in.add(start + k)), cv, acc[1]);
                acc[2] = _mm256_fmadd_ps(_mm256_loadu_ps(b_in.add(start + k)), cv, acc[2]);
                k += 8;
            }
            let mut sums = [hsum(acc[0]), hsum(acc[1]), hsum(acc[2])];
            while k < size {
                let c = coeffs[k];
                sums[0] += *r_in.add(start + k) * c;
                sums[1] += *g_in.add(start + k) * c;
                sums[2] += *b_in.add(start + k) * c;
                k += 1;
            }
            ring[slot + ox] = sums[0];
            ring[plane + slot + ox] = sums[1];
            ring[2 * plane + slot + ox] = sums[2];
        }
    }
}

/// One horizontal row, 4 channels, reading the interleaved f32 staged
/// row: each pixel is a natural f32x4 lane group. Where NEON (128-bit
/// vectors) applies one tap per FMA, a 256-bit load here covers two
/// adjacent pixels, so each FMA applies two taps against a lanewise
/// coefficient-pair broadcast; two accumulator streams cover four taps
/// per iteration and halve the dependent-FMA chain. The final lane
/// reduction sums (even taps) + (odd taps) — a different tree than
/// NEON's single accumulator, bounded by the f64 ground-truth tests.
#[target_feature(enable = "avx2,fma")]
unsafe fn horiz_row_x4(
    stage: &[f32],
    w: &Windows,
    ring: &mut [f32],
    plane: usize,
    slot: usize,
    dst_w: usize,
) {
    unsafe {
        use std::arch::x86_64::*;
        // Coefficient-pair broadcasts: [c0 c0 c0 c0 | c1 c1 c1 c1] etc.
        let idx01 = _mm256_setr_epi32(0, 0, 0, 0, 1, 1, 1, 1);
        let idx23 = _mm256_setr_epi32(2, 2, 2, 2, 3, 3, 3, 3);
        for ox in 0..dst_w {
            let start = w.starts[ox];
            let size = w.sizes[ox];
            let coeffs = &w.coeffs[ox * w.window_size..ox * w.window_size + size];

            let mut acc_a = _mm256_setzero_ps();
            let mut acc_b = _mm256_setzero_ps();
            let mut k = 0usize;
            while k + 4 <= size {
                let c4 = _mm_loadu_ps(coeffs.as_ptr().add(k));
                let cv = _mm256_set_m128(c4, c4);
                let p01 = _mm256_loadu_ps(stage.as_ptr().add((start + k) * 4));
                let p23 = _mm256_loadu_ps(stage.as_ptr().add((start + k + 2) * 4));
                acc_a = _mm256_fmadd_ps(p01, _mm256_permutevar8x32_ps(cv, idx01), acc_a);
                acc_b = _mm256_fmadd_ps(p23, _mm256_permutevar8x32_ps(cv, idx23), acc_b);
                k += 4;
            }
            let mut acc = _mm_add_ps(
                _mm_add_ps(
                    _mm256_castps256_ps128(acc_a),
                    _mm256_extractf128_ps::<1>(acc_a),
                ),
                _mm_add_ps(
                    _mm256_castps256_ps128(acc_b),
                    _mm256_extractf128_ps::<1>(acc_b),
                ),
            );
            while k < size {
                let p = _mm_loadu_ps(stage.as_ptr().add((start + k) * 4));
                acc = _mm_fmadd_ps(p, _mm_set1_ps(coeffs[k]), acc);
                k += 1;
            }
            let mut out = [0f32; 4];
            _mm_storeu_ps(out.as_mut_ptr(), acc);
            for (c, v) in out.iter().enumerate() {
                ring[c * plane + slot + ox] = *v;
            }
        }
    }
}

/// acc[x] = sum over taps of coeff * ring_row[x], taps applied in
/// ascending order (`offs[k]` is the precomputed ring offset of tap k's
/// row). Accumulators stay in registers for a whole 32-column tile
/// across every tap (the tap-order additions per element are unchanged
/// from a per-tap memory accumulator).
#[target_feature(enable = "avx2,fma")]
unsafe fn vert_accumulate(
    plane: &[f32],
    coeffs: &[f32],
    offs: &[usize],
    dst_w: usize,
    acc: &mut [f32],
) {
    unsafe {
        use std::arch::x86_64::*;
        let mut x = 0usize;
        while x + 32 <= dst_w {
            let mut a0 = _mm256_setzero_ps();
            let mut a1 = _mm256_setzero_ps();
            let mut a2 = _mm256_setzero_ps();
            let mut a3 = _mm256_setzero_ps();
            for (&off, &c) in offs.iter().zip(coeffs) {
                let row = plane.as_ptr().add(off + x);
                let cv = _mm256_set1_ps(c);
                a0 = _mm256_fmadd_ps(_mm256_loadu_ps(row), cv, a0);
                a1 = _mm256_fmadd_ps(_mm256_loadu_ps(row.add(8)), cv, a1);
                a2 = _mm256_fmadd_ps(_mm256_loadu_ps(row.add(16)), cv, a2);
                a3 = _mm256_fmadd_ps(_mm256_loadu_ps(row.add(24)), cv, a3);
            }
            _mm256_storeu_ps(acc.as_mut_ptr().add(x), a0);
            _mm256_storeu_ps(acc.as_mut_ptr().add(x + 8), a1);
            _mm256_storeu_ps(acc.as_mut_ptr().add(x + 16), a2);
            _mm256_storeu_ps(acc.as_mut_ptr().add(x + 24), a3);
            x += 32;
        }
        while x + 8 <= dst_w {
            let mut a = _mm256_setzero_ps();
            for (&off, &c) in offs.iter().zip(coeffs) {
                a = _mm256_fmadd_ps(
                    _mm256_loadu_ps(plane.as_ptr().add(off + x)),
                    _mm256_set1_ps(c),
                    a,
                );
            }
            _mm256_storeu_ps(acc.as_mut_ptr().add(x), a);
            x += 8;
        }
        while x < dst_w {
            let mut a = 0f32;
            for (&off, &c) in offs.iter().zip(coeffs) {
                a += plane[off + x] * c;
            }
            acc[x] = a;
            x += 1;
        }
    }
}

/// Round-to-nearest-even f32 -> u16 with saturation on both ends for
/// eight values: `_mm256_cvtps_epi32` rounds under the default MXCSR
/// mode (nearest-even, matching NEON's vcvtnq; Rust never changes it),
/// and `_mm256_packus_epi32` saturates i32 -> u16 (negatives to 0,
/// overflow to 65535). The signed i32 conversion cannot itself
/// overflow: inputs are convolutions of u16 samples with ~unit-sum
/// kernels, bounded far below 2^31. packus interleaves 128-bit lanes,
/// so a 64-bit-lane permute restores element order.
#[inline]
#[target_feature(enable = "avx2,fma")]
unsafe fn narrow8(p: *const f32) -> [u16; 8] {
    unsafe {
        use std::arch::x86_64::*;
        let v = _mm256_cvtps_epi32(_mm256_loadu_ps(p));
        let packed = _mm256_permute4x64_epi64::<0b1101_1000>(_mm256_packus_epi32(v, v));
        let mut out = [0u16; 8];
        _mm_storeu_si128(out.as_mut_ptr().cast(), _mm256_castsi256_si128(packed));
        out
    }
}

/// Round-to-nearest f32 -> u16 with saturation on both ends (negative
/// converts to 0, overflow narrows to 65535), interleaving three
/// planes. The interleave itself is scalar: the store stage touches
/// each output value once and is noise next to the convolutions.
#[target_feature(enable = "avx2,fma")]
unsafe fn store_row_x3(acc: &[f32], dst_w: usize, out: &mut [u16]) {
    unsafe {
        let (r, rest) = acc.split_at(dst_w);
        let (g, b) = rest.split_at(dst_w);
        let mut x = 0usize;
        while x + 8 <= dst_w {
            let rv = narrow8(r.as_ptr().add(x));
            let gv = narrow8(g.as_ptr().add(x));
            let bv = narrow8(b.as_ptr().add(x));
            for j in 0..8 {
                out[(x + j) * 3] = rv[j];
                out[(x + j) * 3 + 1] = gv[j];
                out[(x + j) * 3 + 2] = bv[j];
            }
            x += 8;
        }
        while x < dst_w {
            out[x * 3] = clamp_u16(r[x]);
            out[x * 3 + 1] = clamp_u16(g[x]);
            out[x * 3 + 2] = clamp_u16(b[x]);
            x += 1;
        }
    }
}

#[target_feature(enable = "avx2,fma")]
unsafe fn store_row_x4(acc: &[f32], dst_w: usize, out: &mut [u16]) {
    unsafe {
        let mut x = 0usize;
        while x + 8 <= dst_w {
            let ch = [
                narrow8(acc.as_ptr().add(x)),
                narrow8(acc.as_ptr().add(dst_w + x)),
                narrow8(acc.as_ptr().add(2 * dst_w + x)),
                narrow8(acc.as_ptr().add(3 * dst_w + x)),
            ];
            for j in 0..8 {
                for (c, plane) in ch.iter().enumerate() {
                    out[(x + j) * 4 + c] = plane[j];
                }
            }
            x += 8;
        }
        while x < dst_w {
            for c in 0..4 {
                out[x * 4 + c] = clamp_u16(acc[c * dst_w + x]);
            }
            x += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::resize_kernel::testkit;

    /// AVX2+FMA is not part of the x86-64 baseline; on hosts without it
    /// the kernel correctly refuses at runtime, so skip (loudly) rather
    /// than fail.
    fn detected() -> bool {
        if Avx2::detect() {
            true
        } else {
            eprintln!("skipping: host lacks avx2+fma");
            false
        }
    }

    #[test]
    fn strip_schedule_equals_full_intermediate_schedule_exactly() {
        if !detected() {
            return;
        }
        testkit::assert_schedule_equality::<Avx2>();
    }

    #[test]
    fn streaming_with_trailing_rows_matches_full_frame() {
        if !detected() {
            return;
        }
        testkit::assert_streaming_with_trailing_rows::<Avx2>();
    }

    #[test]
    fn tracks_ground_truth_for_rgb() {
        if !detected() {
            return;
        }
        for (sw, sh, dw, dh) in [
            (2040, 1356, 512, 340),
            (640, 480, 512, 384),
            (333, 217, 100, 65),
            (17, 11, 5, 3),
        ] {
            testkit::assert_accuracy::<Avx2>(
                sw,
                sh,
                dw,
                dh,
                3,
                &format!("rgb {sw}x{sh}->{dw}x{dh}"),
            );
        }
    }

    #[test]
    fn tracks_ground_truth_for_rgba() {
        if !detected() {
            return;
        }
        for (sw, sh, dw, dh) in [(801, 601, 256, 192), (64, 64, 17, 9)] {
            testkit::assert_accuracy::<Avx2>(
                sw,
                sh,
                dw,
                dh,
                4,
                &format!("rgba {sw}x{sh}->{dw}x{dh}"),
            );
        }
    }

    #[test]
    fn tracks_ground_truth_when_upscaling() {
        if !detected() {
            return;
        }
        testkit::assert_accuracy::<Avx2>(50, 40, 120, 96, 3, "rgb upscale");
    }

    #[test]
    fn rejects_empty_dimensions() {
        let src = [0u16; 12];
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let mut dst = [0u16; 12];
        let dst_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
        assert!(resize_u16_avx2(src_bytes, 2, 2, dst_bytes, 0, 1, 3).is_err());
        assert!(resize_u16_avx2(src_bytes, 0, 2, dst_bytes, 1, 1, 3).is_err());
    }
}
