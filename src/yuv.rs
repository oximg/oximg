//! Row-level YUV-to-RGB conversion kernels for the AVIF decode path.
//!
//! Each operation has a scalar reference implementation (the source of
//! truth, cross-validated against avifdec at the pipeline level) and, on
//! aarch64, a NEON implementation that mirrors the scalar arithmetic
//! operation for operation — same multiply/add/divide order, no FMA
//! contraction — so the two produce bit-identical output. That equality
//! is asserted exhaustively in this module's tests; the NEON path is
//! selected at runtime.

/// Color-space constants for one decoded picture, on the 0..255 output
/// scale. `a_r`/`b_b` are the precomputed Cr->R and Cb->B gains.
#[derive(Clone, Copy)]
pub(crate) struct Csc {
    pub y_off: f32,
    pub y_mul: f32,
    pub center: f32,
    pub c_mul: f32,
    pub kr: f32,
    pub kb: f32,
    pub kg: f32,
}

/// One plane row: 8-bit samples or little-endian high-bit-depth samples.
#[derive(Clone, Copy)]
pub(crate) enum Row<'a> {
    B8(&'a [u8]),
    B16(&'a [u16]),
}

impl Row<'_> {
    pub(crate) fn len(&self) -> usize {
        match self {
            Row::B8(s) => s.len(),
            Row::B16(s) => s.len(),
        }
    }

    #[inline]
    pub(crate) fn at(&self, i: usize) -> f32 {
        match self {
            Row::B8(s) => s[i] as f32,
            Row::B16(s) => s[i] as f32,
        }
    }
}

/// Vertical chroma blend at chroma resolution: (3*near + other) / 4.
pub(crate) fn chroma_blend(near: Row, other: Row, out: &mut [f32]) {
    #[cfg(target_arch = "aarch64")]
    if neon() {
        return unsafe { neon::chroma_blend(near, other, out) };
    }
    chroma_blend_scalar(near, other, out);
}

fn chroma_blend_scalar(near: Row, other: Row, out: &mut [f32]) {
    match (near, other) {
        (Row::B8(n), Row::B8(o)) => {
            for ((&n, &o), out) in n.iter().zip(o).zip(out.iter_mut()) {
                *out = (3.0 * n as f32 + o as f32) * 0.25;
            }
        }
        (Row::B16(n), Row::B16(o)) => {
            for ((&n, &o), out) in n.iter().zip(o).zip(out.iter_mut()) {
                *out = (3.0 * n as f32 + o as f32) * 0.25;
            }
        }
        _ => {
            for (i, o) in out.iter_mut().enumerate() {
                *o = (3.0 * near.at(i) + other.at(i)) * 0.25;
            }
        }
    }
}

/// Direct widen of one chroma row to f32 (no vertical subsampling).
pub(crate) fn chroma_widen(row: Row, out: &mut [f32]) {
    #[cfg(target_arch = "aarch64")]
    if neon() {
        return unsafe { neon::chroma_widen(row, out) };
    }
    chroma_widen_scalar(row, out);
}

fn chroma_widen_scalar(row: Row, out: &mut [f32]) {
    match row {
        Row::B8(s) => {
            for (&v, o) in s.iter().zip(out.iter_mut()) {
                *o = v as f32;
            }
        }
        Row::B16(s) => {
            for (&v, o) in s.iter().zip(out.iter_mut()) {
                *o = v as f32;
            }
        }
    }
}

/// Horizontal 2x chroma upsample with the center-sited bilinear kernel:
/// even outputs blend toward the previous chroma sample, odd outputs
/// toward the next, 3:1, clamped at the row ends. `mid.len()` must be
/// `out.len().div_ceil(2)`.
pub(crate) fn chroma_upsample_h(mid: &[f32], out: &mut [f32]) {
    debug_assert_eq!(mid.len(), out.len().div_ceil(2));
    #[cfg(target_arch = "aarch64")]
    if neon() {
        return unsafe { neon::chroma_upsample_h(mid, out) };
    }
    chroma_upsample_h_scalar(mid, out, 0);
}

/// Scalar reference; `from` allows the NEON path to delegate its edges
/// and tail.
#[allow(clippy::needless_range_loop)] // x drives two computed indices into `mid`
fn chroma_upsample_h_scalar(mid: &[f32], out: &mut [f32], from: usize) {
    let cw = mid.len();
    for x in from..out.len() {
        let cx = x >> 1;
        let other = if x & 1 == 1 {
            (cx + 1).min(cw - 1)
        } else {
            cx.saturating_sub(1)
        };
        out[x] = (3.0 * mid[cx] + mid[other]) * 0.25;
    }
}

/// General-matrix conversion of one row to interleaved RGB8:
/// r = y' + 2(1-kr)cr, b = y' + 2(1-kb)cb, g = (y' - kr*r - kb*b)/kg,
/// each rounded with +0.5 and clamped to 0..255.
pub(crate) fn yuv_row_to_rgb(y: Row, cb: &[f32], cr: &[f32], c: &Csc, out: &mut [u8]) {
    debug_assert!(y.len() >= out.len() / 3);
    #[cfg(target_arch = "aarch64")]
    if neon() {
        return unsafe { neon::yuv_row_to_rgb(y, cb, cr, c, out) };
    }
    yuv_row_to_rgb_scalar(y, cb, cr, c, out, 0);
}

fn yuv_row_to_rgb_scalar(y: Row, cb: &[f32], cr: &[f32], c: &Csc, out: &mut [u8], from: usize) {
    // The Row match is hoisted out of the loop and all element access
    // goes through zipped iterators: with per-pixel indexing, bounds
    // checks kept some compilers from producing a sane loop body (~10x
    // on the container toolchain, amplified further on some Intel
    // cores), which dominated AVIF decode on x86-64.
    let n = out.len() / 3;
    match y {
        Row::B8(s) => yuv_px_scalar(
            s[from..n].iter().map(|&v| v as f32),
            &cb[from..n],
            &cr[from..n],
            c,
            &mut out[from * 3..n * 3],
        ),
        Row::B16(s) => yuv_px_scalar(
            s[from..n].iter().map(|&v| v as f32),
            &cb[from..n],
            &cr[from..n],
            c,
            &mut out[from * 3..n * 3],
        ),
    }
}

#[inline(always)]
fn yuv_px_scalar(ys: impl Iterator<Item = f32>, cb: &[f32], cr: &[f32], c: &Csc, out: &mut [u8]) {
    for (((yv, &cbv), &crv), px) in ys.zip(cb).zip(cr).zip(out.chunks_exact_mut(3)) {
        let yf = (yv - c.y_off) * c.y_mul;
        let cbf = (cbv - c.center) * c.c_mul;
        let crf = (crv - c.center) * c.c_mul;
        let r = yf + 2.0 * (1.0 - c.kr) * crf;
        let b = yf + 2.0 * (1.0 - c.kb) * cbf;
        let g = (yf - c.kr * r - c.kb * b) / c.kg;
        px[0] = (r + 0.5).clamp(0.0, 255.0) as u8;
        px[1] = (g + 0.5).clamp(0.0, 255.0) as u8;
        px[2] = (b + 0.5).clamp(0.0, 255.0) as u8;
    }
}

/// Alpha (or any single-plane) row normalized to u8:
/// clamp((v - off) * mul + 0.5).
pub(crate) fn alpha_row(src: Row, off: f32, mul: f32, out: &mut [u8]) {
    #[cfg(target_arch = "aarch64")]
    if neon() {
        return unsafe { neon::alpha_row(src, off, mul, out) };
    }
    alpha_row_scalar(src, off, mul, out);
}

fn alpha_row_scalar(src: Row, off: f32, mul: f32, out: &mut [u8]) {
    match src {
        Row::B8(s) => {
            for (&v, o) in s.iter().zip(out.iter_mut()) {
                *o = ((v as f32 - off) * mul + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
        Row::B16(s) => {
            for (&v, o) in s.iter().zip(out.iter_mut()) {
                *o = ((v as f32 - off) * mul + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
#[inline]
pub(crate) fn neon() -> bool {
    std::arch::is_aarch64_feature_detected!("neon")
}

#[cfg(target_arch = "aarch64")]
mod neon {
    use super::{Csc, Row};
    use std::arch::aarch64::*;

    /// Widen 8 samples starting at `i` to two f32x4 vectors.
    /// Safety: caller guarantees i + 8 <= row length; NEON enabled.
    #[inline]
    unsafe fn load8(row: Row, i: usize) -> (float32x4_t, float32x4_t) {
        unsafe {
            match row {
                Row::B8(s) => {
                    let v = vmovl_u8(vld1_u8(s.as_ptr().add(i)));
                    (
                        vcvtq_f32_u32(vmovl_u16(vget_low_u16(v))),
                        vcvtq_f32_u32(vmovl_u16(vget_high_u16(v))),
                    )
                }
                Row::B16(s) => {
                    let v = vld1q_u16(s.as_ptr().add(i));
                    (
                        vcvtq_f32_u32(vmovl_u16(vget_low_u16(v))),
                        vcvtq_f32_u32(vmovl_u16(vget_high_u16(v))),
                    )
                }
            }
        }
    }

    /// (v + 0.5) truncated with saturation on both ends — identical to
    /// the scalar `(v + 0.5).clamp(0.0, 255.0) as u8` (float-to-unsigned
    /// conversion saturates negatives to zero, and the narrowing steps
    /// saturate above 255).
    #[inline]
    unsafe fn round_u8x8(lo: float32x4_t, hi: float32x4_t, half: float32x4_t) -> uint8x8_t {
        unsafe {
            let lo = vcvtq_u32_f32(vaddq_f32(lo, half));
            let hi = vcvtq_u32_f32(vaddq_f32(hi, half));
            vqmovn_u16(vcombine_u16(vqmovn_u32(lo), vqmovn_u32(hi)))
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn chroma_blend(near: Row, other: Row, out: &mut [f32]) {
        unsafe {
            let three = vdupq_n_f32(3.0);
            let quarter = vdupq_n_f32(0.25);
            let n = out.len();
            let mut i = 0;
            while i + 8 <= n {
                let (n_lo, n_hi) = load8(near, i);
                let (o_lo, o_hi) = load8(other, i);
                // (3*near + other) * 0.25, mul/add exactly as scalar
                let lo = vmulq_f32(vaddq_f32(vmulq_f32(three, n_lo), o_lo), quarter);
                let hi = vmulq_f32(vaddq_f32(vmulq_f32(three, n_hi), o_hi), quarter);
                vst1q_f32(out.as_mut_ptr().add(i), lo);
                vst1q_f32(out.as_mut_ptr().add(i + 4), hi);
                i += 8;
            }
            for (j, o) in out.iter_mut().enumerate().skip(i) {
                *o = (3.0 * near.at(j) + other.at(j)) * 0.25;
            }
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn chroma_widen(row: Row, out: &mut [f32]) {
        unsafe {
            let n = out.len();
            let mut i = 0;
            while i + 8 <= n {
                let (lo, hi) = load8(row, i);
                vst1q_f32(out.as_mut_ptr().add(i), lo);
                vst1q_f32(out.as_mut_ptr().add(i + 4), hi);
                i += 8;
            }
            for (j, o) in out.iter_mut().enumerate().skip(i) {
                *o = row.at(j);
            }
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn chroma_upsample_h(mid: &[f32], out: &mut [f32]) {
        unsafe {
            let cw = mid.len();
            let w = out.len();
            // Edges (x = 0, 1) and anything touching the clamped last
            // chroma sample stay scalar; the body handles chroma indices
            // j where j-1 and j+4 are in bounds.
            super::chroma_upsample_h_scalar(mid, &mut out[..2.min(w)], 0);
            let three = vdupq_n_f32(3.0);
            let quarter = vdupq_n_f32(0.25);
            let mut j = 1usize;
            while j + 5 <= cw && 2 * j + 8 <= w {
                let m = vld1q_f32(mid.as_ptr().add(j));
                let prev = vld1q_f32(mid.as_ptr().add(j - 1));
                let next = vld1q_f32(mid.as_ptr().add(j + 1));
                let e = vmulq_f32(vaddq_f32(vmulq_f32(three, m), prev), quarter);
                let o = vmulq_f32(vaddq_f32(vmulq_f32(three, m), next), quarter);
                vst2q_f32(out.as_mut_ptr().add(2 * j), float32x4x2_t(e, o));
                j += 4;
            }
            super::chroma_upsample_h_scalar(mid, out, 2 * j);
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn yuv_row_to_rgb(y: Row, cb: &[f32], cr: &[f32], c: &Csc, out: &mut [u8]) {
        unsafe {
            let w = out.len() / 3;
            let y_off = vdupq_n_f32(c.y_off);
            let y_mul = vdupq_n_f32(c.y_mul);
            let center = vdupq_n_f32(c.center);
            let c_mul = vdupq_n_f32(c.c_mul);
            let a_r = vdupq_n_f32(2.0 * (1.0 - c.kr));
            let b_b = vdupq_n_f32(2.0 * (1.0 - c.kb));
            let kr = vdupq_n_f32(c.kr);
            let kb = vdupq_n_f32(c.kb);
            let kg = vdupq_n_f32(c.kg);
            let half = vdupq_n_f32(0.5);

            let mut x = 0usize;
            while x + 8 <= w {
                let (y_lo, y_hi) = load8(y, x);
                macro_rules! quad {
                    ($yv:expr, $i:expr) => {{
                        let yf = vmulq_f32(vsubq_f32($yv, y_off), y_mul);
                        let cbf =
                            vmulq_f32(vsubq_f32(vld1q_f32(cb.as_ptr().add(x + $i)), center), c_mul);
                        let crf =
                            vmulq_f32(vsubq_f32(vld1q_f32(cr.as_ptr().add(x + $i)), center), c_mul);
                        // r = yf + a_r*crf; b = yf + b_b*cbf;
                        // g = (yf - kr*r - kb*b) / kg — mul/sub/div in the
                        // scalar path's exact order, no FMA contraction.
                        let r = vaddq_f32(yf, vmulq_f32(a_r, crf));
                        let b = vaddq_f32(yf, vmulq_f32(b_b, cbf));
                        let g = vdivq_f32(
                            vsubq_f32(vsubq_f32(yf, vmulq_f32(kr, r)), vmulq_f32(kb, b)),
                            kg,
                        );
                        (r, g, b)
                    }};
                }
                let (r0, g0, b0) = quad!(y_lo, 0);
                let (r1, g1, b1) = quad!(y_hi, 4);
                let rgb = uint8x8x3_t(
                    round_u8x8(r0, r1, half),
                    round_u8x8(g0, g1, half),
                    round_u8x8(b0, b1, half),
                );
                vst3_u8(out.as_mut_ptr().add(x * 3), rgb);
                x += 8;
            }
            super::yuv_row_to_rgb_scalar(y, cb, cr, c, out, x);
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn alpha_row(src: Row, off: f32, mul: f32, out: &mut [u8]) {
        unsafe {
            let offv = vdupq_n_f32(off);
            let mulv = vdupq_n_f32(mul);
            let half = vdupq_n_f32(0.5);
            let n = out.len();
            let mut i = 0;
            while i + 8 <= n {
                let (lo, hi) = load8(src, i);
                let lo = vmulq_f32(vsubq_f32(lo, offv), mulv);
                let hi = vmulq_f32(vsubq_f32(hi, offv), mulv);
                vst1_u8(out.as_mut_ptr().add(i), round_u8x8(lo, hi, half));
                i += 8;
            }
            for (j, o) in out.iter_mut().enumerate().skip(i) {
                *o = ((src.at(j) - off) * mul + 0.5).clamp(0.0, 255.0) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random samples covering the full bit-depth
    /// range, including exact 0 and max values.
    fn samples(n: usize, bpc: u32, seed: u32) -> (Vec<u8>, Vec<u16>) {
        let max = (1u32 << bpc) - 1;
        let mut s = seed;
        let mut v16 = Vec::with_capacity(n);
        for i in 0..n {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let v = match i % 17 {
                0 => 0,
                1 => max,
                _ => (s >> 8) % (max + 1),
            };
            v16.push(v as u16);
        }
        let v8 = v16.iter().map(|&v| v as u8).collect();
        (v8, v16)
    }

    fn cscs() -> Vec<Csc> {
        let mut out = Vec::new();
        for bpc in [8u32, 10, 12] {
            let max = ((1u32 << bpc) - 1) as f32;
            let center = ((1u32 << bpc) / 2) as f32;
            let scale8 = (1u32 << (bpc - 8)) as f32;
            for full in [true, false] {
                let (y_mul, y_off, c_mul) = if full {
                    (255.0 / max, 0.0, 255.0 / max)
                } else {
                    (
                        255.0 / (219.0 * scale8),
                        16.0 * scale8,
                        255.0 / (224.0 * scale8),
                    )
                };
                for (kr, kb) in [(0.299f32, 0.114f32), (0.2126, 0.0722), (0.2627, 0.0593)] {
                    out.push(Csc {
                        y_off,
                        y_mul,
                        center,
                        c_mul,
                        kr,
                        kb,
                        kg: 1.0 - kr - kb,
                    });
                }
            }
        }
        out
    }

    const WIDTHS: [usize; 12] = [1, 2, 3, 7, 8, 9, 15, 16, 17, 63, 129, 512];

    #[test]
    fn neon_chroma_blend_matches_scalar_bit_exactly() {
        for &w in &WIDTHS {
            let (n8, n16) = samples(w, 12, 1);
            let (o8, o16) = samples(w, 12, 2);
            for (near, other) in [
                (Row::B8(&n8[..]), Row::B8(&o8[..])),
                (Row::B16(&n16[..]), Row::B16(&o16[..])),
            ] {
                let mut a = vec![0f32; w];
                let mut b = vec![0f32; w];
                chroma_blend(near, other, &mut a);
                chroma_blend_scalar(near, other, &mut b);
                assert_eq!(a, b, "blend w={w}");

                chroma_widen(near, &mut a);
                chroma_widen_scalar(near, &mut b);
                assert_eq!(a, b, "widen w={w}");
            }
        }
    }

    #[test]
    fn neon_chroma_upsample_matches_scalar_bit_exactly() {
        for &w in &WIDTHS {
            let cw = w.div_ceil(2);
            let (_, m16) = samples(cw, 12, 3);
            let mid: Vec<f32> = m16.iter().map(|&v| v as f32 * 0.25 + 0.3).collect();
            let mut a = vec![0f32; w];
            let mut b = vec![0f32; w];
            chroma_upsample_h(&mid, &mut a);
            chroma_upsample_h_scalar(&mid, &mut b, 0);
            assert_eq!(a, b, "upsample w={w}");
        }
    }

    #[test]
    fn neon_yuv_row_matches_scalar_bit_exactly() {
        for csc in cscs() {
            let bpc_max = if csc.center > 512.0 {
                12
            } else if csc.center > 128.0 {
                10
            } else {
                8
            };
            for &w in &WIDTHS {
                let (y8, y16) = samples(w, bpc_max, 4);
                let (_, cb16) = samples(w, bpc_max, 5);
                let (_, cr16) = samples(w, bpc_max, 6);
                let cb: Vec<f32> = cb16.iter().map(|&v| v as f32).collect();
                let cr: Vec<f32> = cr16.iter().map(|&v| v as f32).collect();
                for y in [Row::B8(&y8[..]), Row::B16(&y16[..])] {
                    let mut a = vec![0u8; w * 3];
                    let mut b = vec![0u8; w * 3];
                    yuv_row_to_rgb(y, &cb, &cr, &csc, &mut a);
                    yuv_row_to_rgb_scalar(y, &cb, &cr, &csc, &mut b, 0);
                    assert_eq!(a, b, "yuv w={w} center={}", csc.center);
                }
            }
        }
    }

    #[test]
    fn neon_alpha_row_matches_scalar_bit_exactly() {
        for &w in &WIDTHS {
            let (a8, a16) = samples(w, 12, 7);
            for (src, off, mul) in [
                (Row::B8(&a8[..]), 0.0f32, 1.0f32),
                (Row::B8(&a8[..]), 16.0, 255.0 / 219.0),
                (Row::B16(&a16[..]), 0.0, 255.0 / 4095.0),
                (Row::B16(&a16[..]), 64.0, 255.0 / (219.0 * 4.0)),
            ] {
                let mut a = vec![0u8; w];
                let mut b = vec![0u8; w];
                alpha_row(src, off, mul, &mut a);
                alpha_row_scalar(src, off, mul, &mut b);
                assert_eq!(a, b, "alpha w={w} off={off}");
            }
        }
    }

    #[test]
    fn upsample_anchors() {
        // Constant chroma stays constant through both taps.
        let mid = [100.0f32; 4];
        let mut out = [0f32; 8];
        chroma_upsample_h_scalar(&mid, &mut out, 0);
        assert!(out.iter().all(|&v| v == 100.0));
        // A step edge blends 3:1 in both directions.
        let mid = [0.0f32, 100.0];
        let mut out = [0f32; 4];
        chroma_upsample_h_scalar(&mid, &mut out, 0);
        assert_eq!(out, [0.0, 25.0, 75.0, 100.0]);
    }

    #[test]
    fn yuv_scalar_anchors() {
        // 10-bit full-range BT.601: white, black, and neutral gray map to
        // the exact 8-bit values.
        let csc = Csc {
            y_off: 0.0,
            y_mul: 255.0 / 1023.0,
            center: 512.0,
            c_mul: 255.0 / 1023.0,
            kr: 0.299,
            kb: 0.114,
            kg: 1.0 - 0.299 - 0.114,
        };
        let y = [1023u16, 0, 512];
        let cb = [512.0f32; 3];
        let cr = [512.0f32; 3];
        let mut out = [0u8; 9];
        yuv_row_to_rgb_scalar(Row::B16(&y), &cb, &cr, &csc, &mut out, 0);
        assert_eq!(&out[0..3], &[255, 255, 255]);
        assert_eq!(&out[3..6], &[0, 0, 0]);
        assert_eq!(&out[6..9], &[128, 128, 128]);
    }
}
