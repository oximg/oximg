//! NEON f32 row stages for the u16 separable-convolution resize on
//! aarch64, replacing fast_image_resize's u16 path there (which
//! accumulates in i64 pairs — two multiply-accumulates per instruction).
//! The shared driver, window math, and schedule invariants live in
//! [`crate::resize_kernel`].
//!
//! Stage design (shaped by Graviton3 perf counters — the kernel is
//! instruction-bound, IPC ~3.2 with near-zero LLC misses):
//! - each source row is converted from interleaved u16 to f32 exactly
//!   once (planar for 3 channels, interleaved for 4) instead of being
//!   re-loaded and re-widened by every overlapping window (~6x for the
//!   benchmark shape's 24-tap windows);
//! - deinterleaving uses three-register table lookups instead of LD3
//!   (a slow multi-uop structure load on Neoverse cores);
//! - the vertical pass keeps its accumulators in registers across all
//!   taps of a 16-column tile rather than round-tripping an
//!   accumulator row through memory once per tap.

use crate::resize_kernel::{RowKernel, Windows, clamp_u16, resize_u16};
use anyhow::Result;

/// Marker type implementing [`RowKernel`] with NEON intrinsics.
pub(crate) struct Neon;

impl RowKernel for Neon {
    fn detect() -> bool {
        std::arch::is_aarch64_feature_detected!("neon")
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
pub fn resize_u16_neon(
    src_bytes: &[u8],
    src_w: usize,
    src_h: usize,
    dst_bytes: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    channels: usize,
) -> Result<()> {
    resize_u16::<Neon>(src_bytes, src_w, src_h, dst_bytes, dst_w, dst_h, channels)
}

/// Deinterleave one u16 RGB row into three planar f32 rows
/// (stage[0..w], stage[w..2w], stage[2w..3w]). Exact conversion, so the
/// convolution below sees the same operand values as widening on the
/// fly. Deinterleaving uses three-register table lookups instead of
/// LD3: on Neoverse cores LD3 is a slow multi-uop structure load (it
/// showed up as ~25% of kernel time in perf annotate), while TBL on a
/// three-register table is cheap.
#[target_feature(enable = "neon")]
unsafe fn stage_row_x3(row: &[u16], stage: &mut [f32], w: usize) {
    unsafe {
        use std::arch::aarch64::*;
        // Byte indices of each channel's eight u16 samples within a
        // 48-byte (eight-pixel) RGB group.
        const IDX_R: [u8; 16] = [0, 1, 6, 7, 12, 13, 18, 19, 24, 25, 30, 31, 36, 37, 42, 43];
        const IDX_G: [u8; 16] = [2, 3, 8, 9, 14, 15, 20, 21, 26, 27, 32, 33, 38, 39, 44, 45];
        const IDX_B: [u8; 16] = [4, 5, 10, 11, 16, 17, 22, 23, 28, 29, 34, 35, 40, 41, 46, 47];
        let idx_r = vld1q_u8(IDX_R.as_ptr());
        let idx_g = vld1q_u8(IDX_G.as_ptr());
        let idx_b = vld1q_u8(IDX_B.as_ptr());

        let (r_out, rest) = stage.split_at_mut(w);
        let (g_out, b_out) = rest.split_at_mut(w);
        let mut x = 0usize;
        while x + 8 <= w {
            let p = row.as_ptr().add(x * 3) as *const u8;
            let tbl = uint8x16x3_t(vld1q_u8(p), vld1q_u8(p.add(16)), vld1q_u8(p.add(32)));
            macro_rules! ch {
                ($idx:expr, $out:expr) => {{
                    let v = vreinterpretq_u16_u8(vqtbl3q_u8(tbl, $idx));
                    let lo = vcvtq_f32_u32(vmovl_u16(vget_low_u16(v)));
                    let hi = vcvtq_f32_u32(vmovl_u16(vget_high_u16(v)));
                    vst1q_f32($out.as_mut_ptr().add(x), lo);
                    vst1q_f32($out.as_mut_ptr().add(x + 4), hi);
                }};
            }
            ch!(idx_r, r_out);
            ch!(idx_g, g_out);
            ch!(idx_b, b_out);
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
#[target_feature(enable = "neon")]
unsafe fn stage_row_x4(row: &[u16], stage: &mut [f32]) {
    unsafe {
        use std::arch::aarch64::*;
        let n = row.len();
        let mut i = 0usize;
        while i + 8 <= n {
            let v = vld1q_u16(row.as_ptr().add(i));
            let lo = vcvtq_f32_u32(vmovl_u16(vget_low_u16(v)));
            let hi = vcvtq_f32_u32(vmovl_u16(vget_high_u16(v)));
            vst1q_f32(stage.as_mut_ptr().add(i), lo);
            vst1q_f32(stage.as_mut_ptr().add(i + 4), hi);
            i += 8;
        }
        while i < n {
            stage[i] = row[i] as f32;
            i += 1;
        }
    }
}

/// One horizontal row, 3 channels, reading planar f32 staged rows:
/// per output pixel, 8-tap blocks accumulate each channel in a f32x4
/// register (same FMA sequence as widening from u16 directly).
#[target_feature(enable = "neon")]
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
        use std::arch::aarch64::*;
        let r_in = stage.as_ptr();
        let g_in = stage.as_ptr().add(src_w);
        let b_in = stage.as_ptr().add(2 * src_w);
        for ox in 0..dst_w {
            let start = w.starts[ox];
            // Whole 8-tap blocks over the zero-padded coefficients: no
            // scalar tail, no per-tap branch (padding contributes
            // exactly +0.0 against the finite staged slack).
            let padded = w.sizes[ox].div_ceil(8) * 8;
            let coeffs = &w.coeffs[ox * w.stride..ox * w.stride + padded];

            let mut acc = [vdupq_n_f32(0.0); 3];
            let mut k = 0usize;
            while k < padded {
                let c_lo = vld1q_f32(coeffs.as_ptr().add(k));
                let c_hi = vld1q_f32(coeffs.as_ptr().add(k + 4));
                macro_rules! ch {
                    ($i:tt, $in:expr) => {{
                        let lo = vld1q_f32($in.add(start + k));
                        let hi = vld1q_f32($in.add(start + k + 4));
                        acc[$i] = vfmaq_f32(acc[$i], lo, c_lo);
                        acc[$i] = vfmaq_f32(acc[$i], hi, c_hi);
                    }};
                }
                ch!(0, r_in);
                ch!(1, g_in);
                ch!(2, b_in);
                k += 8;
            }
            ring[slot + ox] = vaddvq_f32(acc[0]);
            ring[plane + slot + ox] = vaddvq_f32(acc[1]);
            ring[2 * plane + slot + ox] = vaddvq_f32(acc[2]);
        }
    }
}

/// One horizontal row, 4 channels, reading the interleaved f32 staged
/// row: each pixel is a natural f32x4 lane group; four taps share one
/// coefficient vector via lane-indexed FMA (same sequence as before).
#[target_feature(enable = "neon")]
unsafe fn horiz_row_x4(
    stage: &[f32],
    w: &Windows,
    ring: &mut [f32],
    plane: usize,
    slot: usize,
    dst_w: usize,
) {
    unsafe {
        use std::arch::aarch64::*;
        for ox in 0..dst_w {
            let start = w.starts[ox];
            // Whole 4-tap blocks over the zero-padded coefficients (the
            // tap order per lane is unchanged, so values are identical).
            let padded = w.sizes[ox].div_ceil(4) * 4;
            let coeffs = &w.coeffs[ox * w.stride..ox * w.stride + padded];

            let mut acc = vdupq_n_f32(0.0);
            let mut k = 0usize;
            while k < padded {
                let cv = vld1q_f32(coeffs.as_ptr().add(k));
                macro_rules! tap {
                    ($j:tt) => {{
                        let p = vld1q_f32(stage.as_ptr().add((start + k + $j) * 4));
                        acc = vfmaq_laneq_f32::<$j>(acc, p, cv);
                    }};
                }
                tap!(0);
                tap!(1);
                tap!(2);
                tap!(3);
                k += 4;
            }
            let out = [
                vgetq_lane_f32::<0>(acc),
                vgetq_lane_f32::<1>(acc),
                vgetq_lane_f32::<2>(acc),
                vgetq_lane_f32::<3>(acc),
            ];
            for (c, v) in out.iter().enumerate() {
                ring[c * plane + slot + ox] = *v;
            }
        }
    }
}

/// acc[x] = sum over taps of coeff * ring_row[x], taps applied in
/// ascending order (`offs[k]` is the precomputed ring offset of tap k's
/// row). Accumulators stay in registers for a whole 16-column tile
/// across every tap (the tap-order additions per element are unchanged
/// from a per-tap memory accumulator).
#[target_feature(enable = "neon")]
unsafe fn vert_accumulate(
    plane: &[f32],
    coeffs: &[f32],
    offs: &[usize],
    dst_w: usize,
    acc: &mut [f32],
) {
    unsafe {
        use std::arch::aarch64::*;
        let mut x = 0usize;
        while x + 16 <= dst_w {
            let mut a0 = vdupq_n_f32(0.0);
            let mut a1 = vdupq_n_f32(0.0);
            let mut a2 = vdupq_n_f32(0.0);
            let mut a3 = vdupq_n_f32(0.0);
            for (&off, &c) in offs.iter().zip(coeffs) {
                let row = plane.as_ptr().add(off + x);
                let cv = vdupq_n_f32(c);
                a0 = vfmaq_f32(a0, vld1q_f32(row), cv);
                a1 = vfmaq_f32(a1, vld1q_f32(row.add(4)), cv);
                a2 = vfmaq_f32(a2, vld1q_f32(row.add(8)), cv);
                a3 = vfmaq_f32(a3, vld1q_f32(row.add(12)), cv);
            }
            vst1q_f32(acc.as_mut_ptr().add(x), a0);
            vst1q_f32(acc.as_mut_ptr().add(x + 4), a1);
            vst1q_f32(acc.as_mut_ptr().add(x + 8), a2);
            vst1q_f32(acc.as_mut_ptr().add(x + 12), a3);
            x += 16;
        }
        while x + 4 <= dst_w {
            let mut a = vdupq_n_f32(0.0);
            for (&off, &c) in offs.iter().zip(coeffs) {
                a = vfmaq_f32(a, vld1q_f32(plane.as_ptr().add(off + x)), vdupq_n_f32(c));
            }
            vst1q_f32(acc.as_mut_ptr().add(x), a);
            x += 4;
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

/// Round-to-nearest f32 -> u16 with hardware saturation on both ends
/// (negative converts to 0, overflow narrows to 65535), interleaving
/// three planes.
#[target_feature(enable = "neon")]
unsafe fn store_row_x3(acc: &[f32], dst_w: usize, out: &mut [u16]) {
    unsafe {
        use std::arch::aarch64::*;
        let (r, rest) = acc.split_at(dst_w);
        let (g, b) = rest.split_at(dst_w);
        let mut x = 0usize;
        while x + 8 <= dst_w {
            macro_rules! narrow {
                ($p:expr) => {{
                    let lo = vcvtnq_u32_f32(vld1q_f32($p.as_ptr().add(x)));
                    let hi = vcvtnq_u32_f32(vld1q_f32($p.as_ptr().add(x + 4)));
                    vcombine_u16(vqmovn_u32(lo), vqmovn_u32(hi))
                }};
            }
            let v = uint16x8x3_t(narrow!(r), narrow!(g), narrow!(b));
            vst3q_u16(out.as_mut_ptr().add(x * 3), v);
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

#[target_feature(enable = "neon")]
unsafe fn store_row_x4(acc: &[f32], dst_w: usize, out: &mut [u16]) {
    unsafe {
        use std::arch::aarch64::*;
        let mut x = 0usize;
        while x + 8 <= dst_w {
            macro_rules! narrow {
                ($off:expr) => {{
                    let p = &acc[$off * dst_w..];
                    let lo = vcvtnq_u32_f32(vld1q_f32(p.as_ptr().add(x)));
                    let hi = vcvtnq_u32_f32(vld1q_f32(p.as_ptr().add(x + 4)));
                    vcombine_u16(vqmovn_u32(lo), vqmovn_u32(hi))
                }};
            }
            let v = uint16x8x4_t(narrow!(0), narrow!(1), narrow!(2), narrow!(3));
            vst4q_u16(out.as_mut_ptr().add(x * 4), v);
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

    #[test]
    fn strip_schedule_equals_full_intermediate_schedule_exactly() {
        testkit::assert_schedule_equality::<Neon>();
    }

    #[test]
    fn streaming_with_trailing_rows_matches_full_frame() {
        testkit::assert_streaming_with_trailing_rows::<Neon>();
    }

    #[test]
    fn tracks_ground_truth_for_rgb() {
        for (sw, sh, dw, dh) in [
            (2040, 1356, 512, 340),
            (640, 480, 512, 384),
            (333, 217, 100, 65),
            (17, 11, 5, 3),
        ] {
            testkit::assert_accuracy::<Neon>(
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
        for (sw, sh, dw, dh) in [(801, 601, 256, 192), (64, 64, 17, 9)] {
            testkit::assert_accuracy::<Neon>(
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
        testkit::assert_accuracy::<Neon>(50, 40, 120, 96, 3, "rgb upscale");
    }

    #[test]
    fn rejects_empty_dimensions() {
        let src = [0u16; 12];
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let mut dst = [0u16; 12];
        let dst_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
        assert!(resize_u16_neon(src_bytes, 2, 2, dst_bytes, 0, 1, 3).is_err());
        assert!(resize_u16_neon(src_bytes, 0, 2, dst_bytes, 1, 1, 3).is_err());
    }
}
