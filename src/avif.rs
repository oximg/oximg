//! AVIF encoding via SVT-AV1 (sync-path settings validated in the encoder
//! study: preset 8, tune=3, 10-bit 4:2:0) and decoding via dav1d. The SVT
//! session setup mirrors libavif's codec_svt.c so measurements against
//! `avifenc -c svt` transfer.

use crate::svt::bindings as svt;
use crate::yuv::{self, Row};
use anyhow::{Context, Result, ensure};

/// libavif's quality -> quantizer mapping (codec_svt.c).
fn quality_to_qp(quality: u8) -> u32 {
    ((100 - quality as u32) * 63 + 50) / 100
}

/// RGB(A)8 -> 10-bit 4:2:0 YUV, BT.601 matrix, full range (matching the
/// avifenc defaults used in the encoder study). Chroma is averaged over
/// each 2x2 block; an alpha channel, if present, is ignored here (it is
/// encoded as a separate auxiliary image). The scalar rows are the
/// reference; the aarch64 NEON rows mirror their arithmetic operation
/// for operation and are asserted bit-identical in tests (the yuv.rs
/// contract). x86-64 AVX2 rows are a possible follow-up.
fn rgb_to_yuv420_10bit(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    y_plane: &mut Vec<u16>,
    cb_plane: &mut Vec<u16>,
    cr_plane: &mut Vec<u16>,
) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    // Grow-only scratch; every element of all three planes is written by
    // the loops below.
    for (plane, len) in [
        (&mut *y_plane, w * h),
        (&mut *cb_plane, cw * ch),
        (&mut *cr_plane, cw * ch),
    ] {
        if plane.len() < len {
            plane.resize(len, 0);
        }
        plane.truncate(len);
    }

    luma_rows(&pixels[..w * h * channels], channels, y_plane);
    chroma_rows(pixels, w, h, channels, cb_plane, cr_plane);
}

/// Luma: Y10 = (0.299 R + 0.587 G + 0.114 B) * 1023/255, fixed point.
/// Coefficients sum to 4096, so the pre-scale maximum is 4096*255 and the
/// rounding divide maps 255 -> exactly 1023 (never 1024: out-of-range
/// samples make SVT emit full-scale luma garbage in the affected blocks).
/// The math is position-independent, so any whole-pixel slice works — a
/// single row (the pipeline's fused AVIF path converts rows as the
/// resize emits them) or the full frame.
pub(crate) fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
    #[cfg(target_arch = "aarch64")]
    if crate::yuv::neon() {
        return unsafe { neon_enc::luma_rows(pixels, channels, y_plane) };
    }
    #[cfg(target_arch = "x86_64")]
    if avx2_enc::detect() {
        return unsafe { avx2_enc::luma_rows(pixels, channels, y_plane) };
    }
    luma_rows_scalar(pixels, channels, y_plane);
}

fn luma_rows_scalar(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
    for (px, y) in pixels.chunks_exact(channels).zip(y_plane.iter_mut()) {
        *y = luma_px(px[0], px[1], px[2]);
    }
}

#[inline]
fn luma_px(r: u8, g: u8, b: u8) -> u16 {
    let (r, g, b) = (r as u32, g as u32, b as u32);
    (((1225 * r + 2404 * g + 467 * b) * 1023 + 522_240) / 1_044_480) as u16
}

fn chroma_rows(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    cb_plane: &mut [u16],
    cr_plane: &mut [u16],
) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let row_bytes = w * channels;
    for cy in 0..ch {
        let y0 = cy * 2;
        let row0 = &pixels[y0 * row_bytes..][..row_bytes];
        let row1 = (y0 + 1 < h).then(|| &pixels[(y0 + 1) * row_bytes..][..row_bytes]);
        chroma_row_pair(
            row0,
            row1,
            w,
            channels,
            &mut cb_plane[cy * cw..][..cw],
            &mut cr_plane[cy * cw..][..cw],
        );
    }
}

/// One chroma row from a source row pair; `row1` is `None` on the
/// odd-height bottom row (vertical averages then cover one row). Fills
/// `cb_row`/`cr_row`, both `w.div_ceil(2)` long. pub(crate) so the
/// pipeline's fused AVIF path can convert as resize rows emit.
pub(crate) fn chroma_row_pair(
    row0: &[u8],
    row1: Option<&[u8]>,
    w: usize,
    channels: usize,
    cb_row: &mut [u16],
    cr_row: &mut [u16],
) {
    // The vector paths want both rows (every block a full 2x2); the
    // odd-height bottom row is a single pass of scalar blocks.
    if let Some(row1) = row1 {
        #[cfg(target_arch = "aarch64")]
        if crate::yuv::neon() {
            return unsafe { neon_enc::chroma_row_pair(row0, row1, w, channels, cb_row, cr_row) };
        }
        #[cfg(target_arch = "x86_64")]
        if avx2_enc::detect() {
            return unsafe { avx2_enc::chroma_row_pair(row0, row1, w, channels, cb_row, cr_row) };
        }
    }
    for cx in 0..w.div_ceil(2) {
        let (cb, cr) = chroma_block_rows(row0, row1, channels, cx, w);
        cb_row[cx] = cb;
        cr_row[cx] = cr;
    }
}

/// Chroma from the 2x2-averaged RGB block at column `cx` of a row pair;
/// partial blocks at the right edge (and the missing bottom row when
/// `row1` is None) average the pixels that exist. The scalar reference
/// for the vector paths' edge handling and the dispatcher's fallback.
#[inline]
fn chroma_block_rows(
    row0: &[u8],
    row1: Option<&[u8]>,
    channels: usize,
    cx: usize,
    w: usize,
) -> (u16, u16) {
    let (mut rs, mut gs, mut bs, mut n) = (0u32, 0u32, 0u32, 0u32);
    for row in [Some(row0), row1].into_iter().flatten() {
        for dx in 0..2 {
            let x = cx * 2 + dx;
            if x < w {
                let p = x * channels;
                rs += row[p] as u32;
                gs += row[p + 1] as u32;
                bs += row[p + 2] as u32;
                n += 1;
            }
        }
    }
    let (r, g, b) = (
        rs as f32 / n as f32,
        gs as f32 / n as f32,
        bs as f32 / n as f32,
    );
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = (b - y) * (0.5 / (1.0 - 0.114)) * (1023.0 / 255.0) + 512.0;
    let cr = (r - y) * (0.5 / (1.0 - 0.299)) * (1023.0 / 255.0) + 512.0;
    (
        (cb.round() as i32).clamp(0, 1023) as u16,
        (cr.round() as i32).clamp(0, 1023) as u16,
    )
}

/// Encode-side RGB(A)8 -> YUV NEON rows, mirroring the scalar reference
/// operation for operation (same integer formula for luma via an exact
/// division identity, same f32 order for chroma, no FMA contraction) so
/// output is bit-identical — asserted exhaustively in tests.
#[cfg(target_arch = "aarch64")]
#[allow(unused_unsafe)] // see avx2_enc: toolchain-dependent lint
mod neon_enc {
    use std::arch::aarch64::*;

    /// Deinterleave 8 RGB(A) pixels starting at `p`.
    /// Safety: caller guarantees 8 full pixels at `p`; NEON enabled.
    #[inline]
    unsafe fn load8(p: *const u8, channels: usize) -> (uint8x8_t, uint8x8_t, uint8x8_t) {
        unsafe {
            if channels == 3 {
                let v = vld3_u8(p);
                (v.0, v.1, v.2)
            } else {
                let v = vld4_u8(p);
                (v.0, v.1, v.2)
            }
        }
    }

    /// The scalar luma divide, vectorized exactly: the divisor factors
    /// as 1044480 = 4096 * 255, floor division composes through the
    /// factors, and (t * 8421505) >> 31 == floor(t / 255) — the
    /// round-up magic m = ceil(2^31/255) with error e = m*255 - 2^31 =
    /// 127, exact for every t <= floor(2^31/127) = 16.9M, far above
    /// this path's t <= 260992. Proven exhaustively in
    /// tests::luma_divider_identity_is_exact.
    #[inline]
    unsafe fn luma4(r: uint16x4_t, g: uint16x4_t, b: uint16x4_t) -> uint16x4_t {
        unsafe {
            let acc = vmlal_n_u16(vmlal_n_u16(vmull_n_u16(r, 1225), g, 2404), b, 467);
            let acc = vaddq_u32(vmulq_n_u32(acc, 1023), vdupq_n_u32(522_240));
            let t = vshrq_n_u32::<12>(acc);
            let lo = vshrq_n_u64::<31>(vmull_n_u32(vget_low_u32(t), 8_421_505));
            let hi = vshrq_n_u64::<31>(vmull_n_u32(vget_high_u32(t), 8_421_505));
            vmovn_u32(vcombine_u32(vmovn_u64(lo), vmovn_u64(hi)))
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
        unsafe {
            let n = y_plane.len();
            let mut i = 0;
            while i + 8 <= n {
                let (r, g, b) = load8(pixels.as_ptr().add(i * channels), channels);
                let (r, g, b) = (vmovl_u8(r), vmovl_u8(g), vmovl_u8(b));
                let y = vcombine_u16(
                    luma4(vget_low_u16(r), vget_low_u16(g), vget_low_u16(b)),
                    luma4(vget_high_u16(r), vget_high_u16(g), vget_high_u16(b)),
                );
                vst1q_u16(y_plane.as_mut_ptr().add(i), y);
                i += 8;
            }
            for (j, y) in y_plane.iter_mut().enumerate().skip(i) {
                let p = j * channels;
                *y = super::luma_px(pixels[p], pixels[p + 1], pixels[p + 2]);
            }
        }
    }

    /// One chroma row from a full row pair (the dispatcher guarantees
    /// both rows exist; the odd-height bottom row never reaches here).
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn chroma_row_pair(
        row0: &[u8],
        row1: &[u8],
        w: usize,
        channels: usize,
        cb_row: &mut [u16],
        cr_row: &mut [u16],
    ) {
        // The same f32 constants the scalar reference spells inline.
        const CB1: f32 = 0.5 / (1.0 - 0.114);
        const CR1: f32 = 0.5 / (1.0 - 0.299);
        const SCALE: f32 = 1023.0 / 255.0;
        unsafe {
            let cw = w.div_ceil(2);
            let four = vdupq_n_f32(4.0);
            let v512 = vdupq_n_f32(512.0);
            let vmax = vdupq_n_s32(1023);
            let mut cx = 0usize;
            // Vector path: 4 chroma columns = 8 source pixels, every
            // block a full 2x2 (n = 4).
            while cx * 2 + 8 <= w {
                let p0 = row0.as_ptr().add(cx * 2 * channels);
                let p1 = row1.as_ptr().add(cx * 2 * channels);
                let (r0, g0, b0) = load8(p0, channels);
                let (r1, g1, b1) = load8(p1, channels);
                // Pairwise-add columns, then the two rows: the 2x2 sums
                // for 4 chroma columns (max 1020).
                let rs = vadd_u16(vpaddl_u8(r0), vpaddl_u8(r1));
                let gs = vadd_u16(vpaddl_u8(g0), vpaddl_u8(g1));
                let bs = vadd_u16(vpaddl_u8(b0), vpaddl_u8(b1));
                // Mirror the scalar: sums / n, then the mul/add chain in
                // source order (no FMA).
                let r = vdivq_f32(vcvtq_f32_u32(vmovl_u16(rs)), four);
                let g = vdivq_f32(vcvtq_f32_u32(vmovl_u16(gs)), four);
                let b = vdivq_f32(vcvtq_f32_u32(vmovl_u16(bs)), four);
                let y = vaddq_f32(
                    vaddq_f32(vmulq_n_f32(r, 0.299), vmulq_n_f32(g, 0.587)),
                    vmulq_n_f32(b, 0.114),
                );
                let cb = vaddq_f32(vmulq_n_f32(vmulq_n_f32(vsubq_f32(b, y), CB1), SCALE), v512);
                let cr = vaddq_f32(vmulq_n_f32(vmulq_n_f32(vsubq_f32(r, y), CR1), SCALE), v512);
                // round() then clamp(0, 1023): ties-away convert, min
                // against 1023, saturating-unsigned narrow (which floors
                // negatives at 0).
                let cb = vqmovun_s32(vminq_s32(vcvtaq_s32_f32(cb), vmax));
                let cr = vqmovun_s32(vminq_s32(vcvtaq_s32_f32(cr), vmax));
                vst1_u16(cb_row.as_mut_ptr().add(cx), cb);
                vst1_u16(cr_row.as_mut_ptr().add(cx), cr);
                cx += 4;
            }
            // Right-edge columns take the scalar reference block.
            for cx in cx..cw {
                let (cb, cr) = super::chroma_block_rows(row0, Some(row1), channels, cx, w);
                cb_row[cx] = cb;
                cr_row[cx] = cr;
            }
        }
    }
}

/// Encode-side RGB(A)8 -> YUV AVX2 rows for x86-64 — the neon_enc
/// counterpart, under the same contract: luma runs the scalar integer
/// formula through the exhaustively-proven exact magic-multiply
/// division, chroma mirrors the scalar f32 arithmetic operation for
/// operation, and the tests assert bit-identical planes. Rounding note:
/// the scalar `f32::round` (ties away from zero) is reproduced as
/// `floor(x + 0.5)` — valid because cb/cr are >= 0 by construction (the
/// BT.601 offsets map the extremes to 512 +/- 511.5), and for
/// non-negative x the two are equal, ties included.
#[cfg(target_arch = "x86_64")]
// Newer toolchains let closures inside #[target_feature] fns call
// intrinsics without an inner `unsafe {}`; older ones require it.
// Keep the blocks (they are load-bearing on the older compilers) and
// silence the newer compilers' unused_unsafe instead.
#[allow(unused_unsafe)]
mod avx2_enc {
    use std::arch::x86_64::*;

    #[inline]
    pub(super) fn detect() -> bool {
        is_x86_feature_detected!("avx2")
    }

    /// Deinterleave 16 RGB(A) pixels starting at `p` into r/g/b u8x16.
    /// Safety: caller guarantees 16 full pixels at `p`; AVX2 enabled.
    /// The target_feature attribute is load-bearing: AVX2 is not in the
    /// x86-64 baseline, so without it this helper compiles to SSE2-era
    /// codegen and cannot inline into its AVX2 callers (measured 12x
    /// slower on the luma path).
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn load16(p: *const u8, channels: usize) -> (__m128i, __m128i, __m128i) {
        unsafe {
            if channels == 3 {
                // 48 bytes: r0 g0 b0 r1 ... r15 g15 b15
                let x0 = _mm_loadu_si128(p.cast()); // bytes 0..16
                let x1 = _mm_loadu_si128(p.add(16).cast()); // bytes 16..32
                let x2 = _mm_loadu_si128(p.add(32).cast()); // bytes 32..48
                let z = -1i8;
                // r: 0,3,6,9,12,15 | 18,21,24,27,30 | 33,36,39,42,45
                let r = _mm_or_si128(
                    _mm_or_si128(
                        _mm_shuffle_epi8(
                            x0,
                            _mm_setr_epi8(0, 3, 6, 9, 12, 15, z, z, z, z, z, z, z, z, z, z),
                        ),
                        _mm_shuffle_epi8(
                            x1,
                            _mm_setr_epi8(z, z, z, z, z, z, 2, 5, 8, 11, 14, z, z, z, z, z),
                        ),
                    ),
                    _mm_shuffle_epi8(
                        x2,
                        _mm_setr_epi8(z, z, z, z, z, z, z, z, z, z, z, 1, 4, 7, 10, 13),
                    ),
                );
                // g: 1,4,7,10,13 | 16,19,22,25,28,31 | 34,37,40,43,46
                let g = _mm_or_si128(
                    _mm_or_si128(
                        _mm_shuffle_epi8(
                            x0,
                            _mm_setr_epi8(1, 4, 7, 10, 13, z, z, z, z, z, z, z, z, z, z, z),
                        ),
                        _mm_shuffle_epi8(
                            x1,
                            _mm_setr_epi8(z, z, z, z, z, 0, 3, 6, 9, 12, 15, z, z, z, z, z),
                        ),
                    ),
                    _mm_shuffle_epi8(
                        x2,
                        _mm_setr_epi8(z, z, z, z, z, z, z, z, z, z, z, 2, 5, 8, 11, 14),
                    ),
                );
                // b: 2,5,8,11,14 | 17,20,23,26,29 | 32,35,38,41,44,47
                let b = _mm_or_si128(
                    _mm_or_si128(
                        _mm_shuffle_epi8(
                            x0,
                            _mm_setr_epi8(2, 5, 8, 11, 14, z, z, z, z, z, z, z, z, z, z, z),
                        ),
                        _mm_shuffle_epi8(
                            x1,
                            _mm_setr_epi8(z, z, z, z, z, 1, 4, 7, 10, 13, z, z, z, z, z, z),
                        ),
                    ),
                    _mm_shuffle_epi8(
                        x2,
                        _mm_setr_epi8(z, z, z, z, z, z, z, z, z, z, 0, 3, 6, 9, 12, 15),
                    ),
                );
                (r, g, b)
            } else {
                // 64 bytes: rgba x16; select every 4th byte per channel.
                let x0 = _mm_loadu_si128(p.cast());
                let x1 = _mm_loadu_si128(p.add(16).cast());
                let x2 = _mm_loadu_si128(p.add(32).cast());
                let x3 = _mm_loadu_si128(p.add(48).cast());
                let z = -1i8;
                let pick = |off: i8| -> [__m128i; 4] {
                    let m = |s: i8| unsafe {
                        _mm_setr_epi8(
                            if s == 0 { off } else { z },
                            if s == 0 { off + 4 } else { z },
                            if s == 0 { off + 8 } else { z },
                            if s == 0 { off + 12 } else { z },
                            if s == 1 { off } else { z },
                            if s == 1 { off + 4 } else { z },
                            if s == 1 { off + 8 } else { z },
                            if s == 1 { off + 12 } else { z },
                            if s == 2 { off } else { z },
                            if s == 2 { off + 4 } else { z },
                            if s == 2 { off + 8 } else { z },
                            if s == 2 { off + 12 } else { z },
                            if s == 3 { off } else { z },
                            if s == 3 { off + 4 } else { z },
                            if s == 3 { off + 8 } else { z },
                            if s == 3 { off + 12 } else { z },
                        )
                    };
                    [m(0), m(1), m(2), m(3)]
                };
                let gather = |off: i8| unsafe {
                    let m = pick(off);
                    _mm_or_si128(
                        _mm_or_si128(_mm_shuffle_epi8(x0, m[0]), _mm_shuffle_epi8(x1, m[1])),
                        _mm_or_si128(_mm_shuffle_epi8(x2, m[2]), _mm_shuffle_epi8(x3, m[3])),
                    )
                };
                (gather(0), gather(1), gather(2))
            }
        }
    }

    /// The scalar luma divide, vectorized exactly as in neon_enc::luma4:
    /// 1044480 = 4096 * 255 and (t * 8421505) >> 31 == floor(t / 255)
    /// for t <= 16.9M (proven in tests::luma_divider_identity_is_exact).
    /// Input: 8 pixels as u16x8 lanes; output u16x8 luma.
    #[inline]
    #[target_feature(enable = "avx2")]
    unsafe fn luma8(r: __m128i, g: __m128i, b: __m128i) -> __m128i {
        unsafe {
            let r = _mm256_cvtepu16_epi32(r);
            let g = _mm256_cvtepu16_epi32(g);
            let b = _mm256_cvtepu16_epi32(b);
            let acc = _mm256_add_epi32(
                _mm256_add_epi32(
                    _mm256_mullo_epi32(r, _mm256_set1_epi32(1225)),
                    _mm256_mullo_epi32(g, _mm256_set1_epi32(2404)),
                ),
                _mm256_mullo_epi32(b, _mm256_set1_epi32(467)),
            );
            let acc = _mm256_add_epi32(
                _mm256_mullo_epi32(acc, _mm256_set1_epi32(1023)),
                _mm256_set1_epi32(522_240),
            );
            let t = _mm256_srli_epi32::<12>(acc);
            let m = _mm256_set1_epi64x(8_421_505);
            let even = _mm256_srli_epi64::<31>(_mm256_mul_epu32(t, m));
            let odd = _mm256_srli_epi64::<31>(_mm256_mul_epu32(_mm256_srli_epi64::<32>(t), m));
            let y32 = _mm256_or_si256(even, _mm256_slli_epi64::<32>(odd));
            let packed = _mm256_packus_epi32(y32, y32);
            let packed = _mm256_permute4x64_epi64::<0b11_01_10_00>(packed);
            _mm256_castsi256_si128(packed)
        }
    }

    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
        unsafe {
            let n = y_plane.len();
            let mut i = 0;
            while i + 16 <= n {
                let (r, g, b) = load16(pixels.as_ptr().add(i * channels), channels);
                let lo = luma8(
                    _mm_cvtepu8_epi16(r),
                    _mm_cvtepu8_epi16(g),
                    _mm_cvtepu8_epi16(b),
                );
                let hi = luma8(
                    _mm_cvtepu8_epi16(_mm_srli_si128::<8>(r)),
                    _mm_cvtepu8_epi16(_mm_srli_si128::<8>(g)),
                    _mm_cvtepu8_epi16(_mm_srli_si128::<8>(b)),
                );
                _mm_storeu_si128(y_plane.as_mut_ptr().add(i).cast(), lo);
                _mm_storeu_si128(y_plane.as_mut_ptr().add(i + 8).cast(), hi);
                i += 16;
            }
            for (j, y) in y_plane.iter_mut().enumerate().skip(i) {
                let p = j * channels;
                *y = super::luma_px(pixels[p], pixels[p + 1], pixels[p + 2]);
            }
        }
    }

    /// One chroma row from a full row pair (the dispatcher guarantees
    /// both rows exist; the odd-height bottom row never reaches here).
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn chroma_row_pair(
        row0: &[u8],
        row1: &[u8],
        w: usize,
        channels: usize,
        cb_row: &mut [u16],
        cr_row: &mut [u16],
    ) {
        // The same f32 constants the scalar reference spells inline.
        const CB1: f32 = 0.5 / (1.0 - 0.114);
        const CR1: f32 = 0.5 / (1.0 - 0.299);
        const SCALE: f32 = 1023.0 / 255.0;
        unsafe {
            let cw = w.div_ceil(2);
            let ones = _mm_set1_epi8(1);
            let four = _mm256_set1_ps(4.0);
            let half = _mm256_set1_ps(0.5);
            let v512 = _mm256_set1_ps(512.0);
            let vmax = _mm256_set1_epi32(1023);
            let zero = _mm256_setzero_si256();
            // Pairwise horizontal sums of both rows: the 2x2 sums for 8
            // chroma columns as u16x8 (max 1020), then the scalar f32
            // chain mirrored lane-wise.
            let pair16 = |x: __m128i| unsafe { _mm_maddubs_epi16(x, ones) };
            let mut cx = 0usize;
            // Vector path: 8 chroma columns = 16 source pixels, every
            // block a full 2x2 (n = 4).
            while cx * 2 + 16 <= w {
                let p0 = row0.as_ptr().add(cx * 2 * channels);
                let p1 = row1.as_ptr().add(cx * 2 * channels);
                let (r0, g0, b0) = load16(p0, channels);
                let (r1, g1, b1) = load16(p1, channels);
                let rs = _mm_add_epi16(pair16(r0), pair16(r1));
                let gs = _mm_add_epi16(pair16(g0), pair16(g1));
                let bs = _mm_add_epi16(pair16(b0), pair16(b1));
                let to_f = |s: __m128i| unsafe {
                    _mm256_div_ps(_mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(s)), four)
                };
                let (r, g, b) = (to_f(rs), to_f(gs), to_f(bs));
                let y = _mm256_add_ps(
                    _mm256_add_ps(
                        _mm256_mul_ps(r, _mm256_set1_ps(0.299)),
                        _mm256_mul_ps(g, _mm256_set1_ps(0.587)),
                    ),
                    _mm256_mul_ps(b, _mm256_set1_ps(0.114)),
                );
                let chan = |base: __m256, c1: f32| unsafe {
                    let v = _mm256_add_ps(
                        _mm256_mul_ps(
                            _mm256_mul_ps(_mm256_sub_ps(base, y), _mm256_set1_ps(c1)),
                            _mm256_set1_ps(SCALE),
                        ),
                        v512,
                    );
                    // round-half-away == floor(v + 0.5) for v >= 0,
                    // then the scalar clamp(0, 1023).
                    let i32s = _mm256_cvttps_epi32(_mm256_floor_ps(_mm256_add_ps(v, half)));
                    let i32s = _mm256_min_epi32(_mm256_max_epi32(i32s, zero), vmax);
                    let p = _mm256_packus_epi32(i32s, i32s);
                    _mm256_castsi256_si128(_mm256_permute4x64_epi64::<0b11_01_10_00>(p))
                };
                let cb = chan(b, CB1);
                let cr = chan(r, CR1);
                _mm_storeu_si128(cb_row.as_mut_ptr().add(cx).cast(), cb);
                _mm_storeu_si128(cr_row.as_mut_ptr().add(cx).cast(), cr);
                cx += 8;
            }
            // Right-edge columns take the scalar reference block.
            for cx in cx..cw {
                let (cb, cr) = super::chroma_block_rows(row0, Some(row1), channels, cx, w);
                cb_row[cx] = cb;
                cr_row[cx] = cr;
            }
        }
    }
}

thread_local! {
    /// Encode-side plane scratch, reused across requests.
    static ENC_SCRATCH: std::cell::RefCell<(Vec<u16>, Vec<u16>, Vec<u16>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
}

#[derive(Clone, Copy)]
pub struct AvifParams {
    /// 0-100, libavif semantics.
    pub quality: u8,
    /// 0-100, quality for the alpha auxiliary image.
    pub alpha_quality: u8,
    /// SVT preset (enc_mode); the sync-path setting is 8.
    pub speed: i8,
    /// SVT logical processors; 1 keeps the CPU-slot model honest.
    pub threads: u32,
}

impl Default for AvifParams {
    fn default() -> Self {
        AvifParams {
            quality: 60,
            alpha_quality: 60,
            speed: 8,
            threads: 1,
        }
    }
}

/// Encode interleaved RGB8 (`channels == 3`) or straight-alpha RGBA8
/// (`channels == 4`) as AVIF. Alpha is carried as an auxiliary AV1 image:
/// SVT-AV1 cannot encode 4:0:0, so — like libavif's codec_svt.c — the
/// alpha plane is encoded as the luma of a 4:2:0 image with zeroed
/// placeholder chroma, which flat-codes to almost nothing.
pub fn encode_avif(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    p: &AvifParams,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    ensure!(
        channels == 3 || channels == 4,
        "unsupported channel count {channels}"
    );
    ensure!(pixels.len() >= w * h * channels, "pixel buffer too small");
    let color = ENC_SCRATCH.with(|s| {
        let (y_plane, cb_plane, cr_plane) = &mut *s.borrow_mut();
        rgb_to_yuv420_10bit(pixels, w, h, channels, y_plane, cb_plane, cr_plane);
        encode_svt(
            y_plane,
            cb_plane,
            cr_plane,
            w,
            h,
            quality_to_qp(p.quality),
            false,
            p,
        )
    })?;
    // A fully opaque alpha plane would still cost a whole second SVT
    // session; the scan below is one early-exit pass (first transparent
    // pixel aborts it), so alpha-bearing images pay ~nothing and opaque
    // RGBA drops to the 3-channel output — byte-identical to encoding
    // the same pixels as RGB, since the color path ignores px[3].
    let has_alpha = channels == 4 && pixels[..w * h * 4].chunks_exact(4).any(|px| px[3] != 255);
    let alpha = if has_alpha {
        let a_plane: Vec<u16> = pixels
            .chunks_exact(4)
            .map(|px| ((px[3] as u32 * 1023 + 128) / 255) as u16)
            .collect();
        // One zeroed buffer serves as both placeholder chroma planes.
        let uv = vec![0u16; w.div_ceil(2) * h.div_ceil(2)];
        Some(encode_svt(
            &a_plane,
            &uv,
            &uv,
            w,
            h,
            quality_to_qp(p.alpha_quality),
            true,
            p,
        )?)
    } else {
        None
    };
    Ok(finish_avif(&color, alpha.as_deref(), w, h, icc))
}

/// Start a color (non-alpha) encoder session for the pipeline's fused
/// AVIF path — created on the fused worker while the JPEG decode is
/// still running, so its ~1ms setup hides behind the decode wall.
pub(crate) fn start_color_session(w: usize, h: usize, p: &AvifParams) -> Result<SvtSession> {
    SvtSession::create(w, h, quality_to_qp(p.quality), false, p)
}

/// Encode pre-filled 10-bit 4:2:0 planes (no alpha) on an
/// already-created session and assemble the container. The planes must
/// come from the row conversion API at the session's dimensions, so
/// output is byte-identical to [`encode_avif`] on the same pixels.
pub(crate) fn encode_avif_with_session(
    session: SvtSession,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let (w, h) = (session.w, session.h);
    let color = session.encode(y_plane, cb_plane, cr_plane)?;
    Ok(finish_avif(&color, None, w, h, icc))
}

/// Encode interleaved RGB8 with an already-created color session —
/// the oriented-target preheat path. Same conversion scratch, same
/// encode as [`encode_avif`] on 3-channel input, so the output is
/// byte-identical to the serial path (asserted in tests).
pub(crate) fn encode_avif_rgb_with_session(
    session: SvtSession,
    pixels: &[u8],
    w: usize,
    h: usize,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    ensure!(pixels.len() >= w * h * 3, "pixel buffer too small");
    ensure!(
        (session.w, session.h) == (w, h),
        "preheated session dims mismatch"
    );
    let color = ENC_SCRATCH.with(|s| {
        let (y_plane, cb_plane, cr_plane) = &mut *s.borrow_mut();
        rgb_to_yuv420_10bit(pixels, w, h, 3, y_plane, cb_plane, cr_plane);
        session.encode(y_plane, cb_plane, cr_plane)
    })?;
    Ok(finish_avif(&color, None, w, h, icc))
}

/// Assemble the AVIF container around the encoded AV1 item(s); with a
/// profile, splice the `colr` (`prof`) property in afterwards
/// (avif-serialize speaks CICP only). The nclx `colr` stays alongside
/// it — matrix coefficients still describe the YUV→RGB step, while
/// the ICC profile governs the resulting RGB, exactly as in JPEG.
fn finish_avif(
    color: &[u8],
    alpha: Option<&[u8]>,
    w: usize,
    h: usize,
    icc: Option<&[u8]>,
) -> Vec<u8> {
    let mut fy = avif_serialize::Aviffy::new();
    fy.matrix_coefficients(avif_serialize::constants::MatrixCoefficients::Bt601)
        .full_color_range(true)
        .set_chroma_subsampling((true, true));
    let out = fy.to_vec(color, alpha, w as u32, h as u32, 10);
    if let Some(patched) = icc.and_then(|icc| embed_icc(&out, icc)) {
        return patched;
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn encode_svt(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    w: usize,
    h: usize,
    qp: u32,
    aux_alpha: bool,
    p: &AvifParams,
) -> Result<Vec<u8>> {
    let session = SvtSession::create(w, h, qp, aux_alpha, p)?;
    session.encode(y_plane, cb_plane, cr_plane)
}

/// An initialized SVT-AV1 still-image encoder session. Creation costs
/// ~1ms of wall (SVT spawns its internal pipeline threads there) — a
/// measurable share of a thumbnail encode — so the fused AVIF path
/// creates the session on its worker thread while the JPEG decode is
/// still running and moves it back for the encode.
pub(crate) struct SvtSession {
    handle: *mut svt::EbComponentType,
    w: usize,
    h: usize,
    aux_alpha: bool,
}

// Safety: the handle is only ever used by one thread at a time (the
// session moves by ownership); SVT itself is internally threaded and
// makes no thread-affinity assumptions about its API caller.
unsafe impl Send for SvtSession {}

impl Drop for SvtSession {
    fn drop(&mut self) {
        unsafe {
            svt::svt_av1_enc_deinit(self.handle);
            svt::svt_av1_enc_deinit_handle(self.handle);
        }
    }
}

impl SvtSession {
    /// init_handle + parameters + enc_init, mirroring libavif's
    /// codec_svt.c for a single still image.
    pub(crate) fn create(
        w: usize,
        h: usize,
        qp: u32,
        aux_alpha: bool,
        p: &AvifParams,
    ) -> Result<SvtSession> {
        let timing = std::env::var("OXIMG_TIMING").is_ok();
        let t0 = std::time::Instant::now();
        unsafe {
            let mut handle: *mut svt::EbComponentType = std::ptr::null_mut();
            let mut config: svt::EbSvtAv1EncConfiguration = std::mem::zeroed();
            let err = svt::svt_av1_enc_init_handle(&mut handle, &mut config);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt init_handle: {err:?}"
            );
            // Guard: every exit below must deinit the handle.
            let session = SvtSession {
                handle,
                w,
                h,
                aux_alpha,
            };

            config.encoder_color_format = svt::EbColorFormat::EB_YUV420;
            config.encoder_bit_depth = 10;
            if aux_alpha {
                // CICP does not apply to the alpha auxiliary image
                // (AV1-AVIF spec section 4); its color range shall be full.
                config.color_primaries = 2; // unspecified
                config.transfer_characteristics = 2; // unspecified
                config.matrix_coefficients = 2; // unspecified
            } else {
                config.color_primaries = 1; // BT.709
                config.transfer_characteristics = 13; // sRGB
                config.matrix_coefficients = 6; // BT.601
            }
            config.color_range = 1; // full
            config.source_width = w as u32;
            config.source_height = h as u32;
            config.level_of_parallelism = p.threads;
            config.aq_mode = 2;
            config.rate_control_mode = 0;
            config.min_qp_allowed = 0;
            config.max_qp_allowed = 63;
            config.qp = qp;
            config.enc_mode = p.speed;
            config.force_key_frames = true;
            config.avif = true;
            let tune = std::ffi::CString::new("tune").unwrap();
            let three = std::ffi::CString::new("3").unwrap();
            ensure!(
                svt::svt_av1_enc_parse_parameter(&mut config, tune.as_ptr(), three.as_ptr())
                    == svt::EbErrorType::EB_ErrorNone,
                "svt tune=3"
            );

            let err = svt::svt_av1_enc_set_parameter(handle, &mut config);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt set_parameter: {err:?}"
            );
            let err = svt::svt_av1_enc_init(handle);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt enc_init: {err:?}"
            );
            if timing {
                eprintln!(
                    "timing svt-init({w}x{h}{}) {:.1}ms",
                    if aux_alpha { " alpha" } else { "" },
                    t0.elapsed().as_secs_f64() * 1e3,
                );
            }
            Ok(session)
        }
    }

    /// Send the planes, flush, and drain the AV1 payload; the session
    /// is consumed (deinit on drop).
    pub(crate) fn encode(
        self,
        y_plane: &[u16],
        cb_plane: &[u16],
        cr_plane: &[u16],
    ) -> Result<Vec<u8>> {
        let (w, h) = (self.w, self.h);
        let cw = w.div_ceil(2);
        let timing = std::env::var("OXIMG_TIMING").is_ok();
        let t0 = std::time::Instant::now();
        unsafe {
            let mut io: svt::EbSvtIOFormat = std::mem::zeroed();
            io.luma = y_plane.as_ptr() as *mut u8;
            io.cb = cb_plane.as_ptr() as *mut u8;
            io.cr = cr_plane.as_ptr() as *mut u8;
            io.y_stride = w as u32;
            io.cb_stride = cw as u32;
            io.cr_stride = cw as u32;

            let mut input: svt::EbBufferHeaderType = std::mem::zeroed();
            input.size = std::mem::size_of::<svt::EbBufferHeaderType>() as u32;
            input.p_buffer = (&mut io) as *mut svt::EbSvtIOFormat as *mut u8;
            input.n_filled_len = (y_plane.len() * 2 + (cb_plane.len() + cr_plane.len()) * 2) as u32;
            input.pic_type = svt::EbAv1PictureType::EB_AV1_KEY_PICTURE;
            input.pts = 0;

            let err = svt::svt_av1_enc_send_picture(self.handle, &mut input);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt send_picture: {err:?}"
            );

            // EOS flush.
            let mut eos: svt::EbBufferHeaderType = std::mem::zeroed();
            eos.size = std::mem::size_of::<svt::EbBufferHeaderType>() as u32;
            eos.flags = svt::EB_BUFFERFLAG_EOS;
            let err = svt::svt_av1_enc_send_picture(self.handle, &mut eos);
            ensure!(err == svt::EbErrorType::EB_ErrorNone, "svt eos: {err:?}");

            // Drain packets until EOS.
            let mut av1 = Vec::new();
            loop {
                let mut out: *mut svt::EbBufferHeaderType = std::ptr::null_mut();
                let res = svt::svt_av1_enc_get_packet(self.handle, &mut out, 1);
                if !out.is_null() {
                    let ob = &*out;
                    if !ob.p_buffer.is_null() && ob.n_filled_len > 0 {
                        av1.extend_from_slice(std::slice::from_raw_parts(
                            ob.p_buffer,
                            ob.n_filled_len as usize,
                        ));
                    }
                    let at_eos = ob.flags & svt::EB_BUFFERFLAG_EOS != 0;
                    svt::svt_av1_enc_release_out_buffer(&mut out);
                    if at_eos {
                        break;
                    }
                }
                ensure!(
                    res == svt::EbErrorType::EB_ErrorNone,
                    "svt get_packet: {res:?}"
                );
            }
            if timing {
                eprintln!(
                    "timing svt-enc({w}x{h}{}) {:.1}ms",
                    if self.aux_alpha { " alpha" } else { "" },
                    t0.elapsed().as_secs_f64() * 1e3,
                );
            }
            ensure!(!av1.is_empty(), "svt produced no output");
            Ok(av1)
        }
    }
}

/// dav1d reports errors as negative errno values.
#[cfg(target_os = "linux")]
const EAGAIN: std::os::raw::c_int = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: std::os::raw::c_int = 35;

thread_local! {
    /// Set while the deliberately unwind-caught avif-parse call runs, so
    /// the filtering panic hook stays silent for it: without this, every
    /// attacker-supplied malformed AVIF would print a crash-shaped trace
    /// (and, under RUST_BACKTRACE, serialize on the global backtrace
    /// lock) even though the request fails cleanly.
    static SUPPRESS_PANIC_LOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install (once) a panic hook that skips logging for panics this
/// module catches on purpose and delegates to the previous hook for
/// everything else.
fn install_quiet_panic_hook() {
    static HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !SUPPRESS_PANIC_LOG.with(|s| s.get()) {
                prev(info);
            }
        }));
    });
}

/// avif-parse can panic on truncated/malformed containers (internal
/// parser-state assertions, observed in 2.1.0); malformed input must
/// surface as a parse error, not a crash, so the call is unwind-caught.
/// The crate is pure Rust and the input is a shared slice, so no state
/// can be left torn.
fn read_avif_container(data: &[u8]) -> Result<avif_parse::AvifData> {
    install_quiet_panic_hook();
    struct Unsuppress;
    impl Drop for Unsuppress {
        fn drop(&mut self) {
            SUPPRESS_PANIC_LOG.with(|s| s.set(false));
        }
    }
    SUPPRESS_PANIC_LOG.with(|s| s.set(true));
    let _guard = Unsuppress;
    match std::panic::catch_unwind(|| avif_parse::read_avif(&mut std::io::Cursor::new(data))) {
        Ok(parsed) => parsed.context("parse AVIF container"),
        // Keep the assertion text: it identifies which upstream bug fired.
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(anyhow::anyhow!("AVIF container parse panicked: {msg}"))
        }
    }
}

// ---------------------------------------------------------------------
// ICC (`colr` of type `prof`) support. Neither avif-parse (2.1.0) nor
// avif-serialize (0.8.9) exposes ICC in any released version, so both
// directions run on a bounded ISOBMFF walk of our own: extraction
// resolves the primary item's property associations; embedding splices
// a `colr` property into an avif-serialize container and patches the
// affected box sizes and item locations, then proves the result by
// re-extracting (any mismatch falls back to the unprofiled bytes).

/// Read one box header at `i` within `end`: returns
/// `(fourcc, payload_range, box_end)`. `None` on truncation or a size
/// that cannot frame its own header.
fn next_box(buf: &[u8], i: usize, end: usize) -> Option<([u8; 4], std::ops::Range<usize>, usize)> {
    let size = u32::from_be_bytes(buf.get(i..i.checked_add(4)?)?.try_into().ok()?) as usize;
    let typ: [u8; 4] = buf.get(i + 4..i + 8)?.try_into().ok()?;
    let (hdr, sz) = match size {
        0 => (8, end.checked_sub(i)?), // box extends to the end
        1 => {
            let big = u64::from_be_bytes(buf.get(i + 8..i + 16)?.try_into().ok()?);
            (16, usize::try_from(big).ok()?)
        }
        s => (8, s),
    };
    let box_end = i.checked_add(sz)?;
    if sz < hdr || box_end > end {
        return None;
    }
    Some((typ, i + hdr..box_end, box_end))
}

/// First child box of `range` with the given fourcc:
/// `(payload_range, box_range)`.
fn find_box(
    buf: &[u8],
    range: std::ops::Range<usize>,
    fourcc: &[u8; 4],
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    let mut i = range.start;
    while i + 8 <= range.end {
        let (typ, payload, box_end) = next_box(buf, i, range.end)?;
        if typ == *fourcc {
            return Some((payload, i..box_end));
        }
        i = box_end;
    }
    None
}

/// The `meta` payload (past the FullBox version/flags) of an AVIF file.
fn meta_payload(avif: &[u8]) -> Option<std::ops::Range<usize>> {
    let (meta, _) = find_box(avif, 0..avif.len(), b"meta")?;
    Some(meta.start.checked_add(4)?..meta.end)
}

/// The primary item id from `pitm`.
fn primary_item_id(avif: &[u8], meta: std::ops::Range<usize>) -> Option<u32> {
    let (pitm, _) = find_box(avif, meta, b"pitm")?;
    let p = avif.get(pitm.clone())?;
    Some(if *p.first()? == 0 {
        u16::from_be_bytes(p.get(4..6)?.try_into().ok()?) as u32
    } else {
        u32::from_be_bytes(p.get(4..8)?.try_into().ok()?)
    })
}

/// Walk an `ipma` payload, calling `f(item_id, prop_index)` for every
/// association; returns the byte width of association entries and, for
/// `item`, the offsets of its association-count byte and entry end.
struct IpmaEntry {
    count_pos: usize,
    entry_end: usize,
    wide: bool,
}

fn ipma_walk(p: &[u8], item: u32, mut f: impl FnMut(u32, usize)) -> Option<IpmaEntry> {
    let version = *p.first()?;
    let wide = p.get(3)? & 1 == 1;
    let entry_count = u32::from_be_bytes(p.get(4..8)?.try_into().ok()?);
    let mut off = 8usize;
    let mut found = None;
    // Still images carry a handful of items; 64 bounds hostile counts.
    for _ in 0..entry_count.min(64) {
        let item_id = if version < 1 {
            let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
            off += 2;
            v
        } else {
            let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
            off += 4;
            v
        };
        let count_pos = off;
        let assoc_count = *p.get(off)? as usize;
        off += 1;
        for _ in 0..assoc_count {
            let idx = if wide {
                let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) & 0x7FFF;
                off += 2;
                v as usize
            } else {
                let v = (*p.get(off)? & 0x7F) as usize;
                off += 1;
                v
            };
            f(item_id, idx);
        }
        if item_id == item {
            found = Some(IpmaEntry {
                count_pos,
                entry_end: off,
                wide,
            });
        }
    }
    found
}

/// Extract the primary item's ICC profile (`colr` box of colour type
/// `prof`/`ricc`) from an AVIF container. `None` for a missing profile
/// or anything malformed — never an error.
pub fn extract_icc(avif: &[u8]) -> Option<Vec<u8>> {
    let meta = meta_payload(avif)?;
    let primary = primary_item_id(avif, meta.clone())?;
    let (iprp, _) = find_box(avif, meta, b"iprp")?;
    let (ipco, _) = find_box(avif, iprp.clone(), b"ipco")?;
    // ipco children in order; association indices are 1-based.
    let mut props: Vec<([u8; 4], std::ops::Range<usize>)> = Vec::new();
    let mut i = ipco.start;
    // 1024 comfortably covers wide (15-bit) association indices seen
    // in practice while bounding hostile property counts.
    while i + 8 <= ipco.end && props.len() < 1024 {
        let (typ, payload, box_end) = next_box(avif, i, ipco.end)?;
        props.push((typ, payload));
        i = box_end;
    }
    let (ipma, _) = find_box(avif, iprp, b"ipma")?;
    let mut icc = None;
    ipma_walk(avif.get(ipma)?, primary, |item_id, idx| {
        if item_id != primary || icc.is_some() || idx == 0 {
            return;
        }
        if let Some((typ, payload)) = props.get(idx - 1)
            && typ == b"colr"
        {
            let c = &avif[payload.clone()];
            if (c.get(0..4) == Some(b"prof".as_slice()) || c.get(0..4) == Some(b"ricc".as_slice()))
                && c.len() > 4
                && c.len() - 4 <= crate::pipeline::ICC_CAP
            {
                icc = Some(c[4..].to_vec());
            }
        }
    })?;
    icc
}

/// The primary item's `irot`/`imir` transforms composed as an
/// EXIF-style orientation. MIAF-conformant writers associate the
/// rotation before the mirror and HEIF applies transforms in
/// association order, so both orders are honored: mirror-first files
/// (non-MIAF but spec-legal, and rendered that way by libheif) use the
/// dihedral identity `rot_a ∘ mirror = mirror ∘ rot_{-a}` to reduce to
/// the rotation-first table. Upright on anything absent or malformed.
pub(crate) fn extract_orientation(avif: &[u8]) -> crate::meta::Orientation {
    fn inner(avif: &[u8]) -> Option<(u8, Option<u8>, bool)> {
        let meta = meta_payload(avif)?;
        let primary = primary_item_id(avif, meta.clone())?;
        let (iprp, _) = find_box(avif, meta, b"iprp")?;
        let (ipco, _) = find_box(avif, iprp.clone(), b"ipco")?;
        let mut props: Vec<([u8; 4], std::ops::Range<usize>)> = Vec::new();
        let mut i = ipco.start;
        while i + 8 <= ipco.end && props.len() < 1024 {
            let (typ, payload, box_end) = next_box(avif, i, ipco.end)?;
            props.push((typ, payload));
            i = box_end;
        }
        let (ipma, _) = find_box(avif, iprp, b"ipma")?;
        let mut angle = 0u8;
        let mut mirror: Option<u8> = None;
        let mut saw_irot = false;
        let mut mirror_first = false;
        ipma_walk(avif.get(ipma)?, primary, |item_id, idx| {
            if item_id != primary || idx == 0 {
                return;
            }
            let Some((typ, payload)) = props.get(idx - 1) else {
                return;
            };
            match typ {
                b"irot" if !saw_irot => {
                    if let Some(&a) = avif[payload.clone()].first() {
                        angle = a & 3;
                        saw_irot = true;
                    }
                }
                b"imir" if mirror.is_none() => {
                    mirror = avif[payload.clone()].first().map(|m| m & 1);
                    mirror_first = mirror.is_some() && !saw_irot;
                }
                _ => {}
            }
        })?;
        Some((angle, mirror, mirror_first && saw_irot))
    }
    match inner(avif) {
        Some((angle, mirror, mirror_first)) => {
            let angle = if mirror_first { (4 - angle) & 3 } else { angle };
            crate::meta::Orientation::from_rot_mirror(angle, mirror)
        }
        None => crate::meta::Orientation::UPRIGHT,
    }
}

/// Splice `icc` into an AVIF container as a `colr` (`prof`) property
/// associated with the primary item: the property is appended to
/// `ipco` (existing 1-based indices keep their meaning), one
/// association is appended to the primary item's `ipma` entry, the
/// enclosing box sizes grow accordingly, and absolute `iloc` offsets
/// shift by the inserted length. The property surgery is proven by
/// re-extraction before the result ships; the `iloc` patch is *not*
/// covered by that proof (extraction reads properties, not item
/// data), so anything the patcher does not fully recognize — exotic
/// versions, oversized item tables — is refused outright rather than
/// partially patched. `None` always means "leave the container
/// unprofiled". In production the input is always our own
/// serializer's output (see `finish_avif`); the layout-agnostic
/// parsing is defense against that dependency evolving, and the
/// decode-roundtrip tests pin the layout actually in use.
pub(crate) fn embed_icc(avif: &[u8], icc: &[u8]) -> Option<Vec<u8>> {
    let meta_pl = meta_payload(avif)?;
    let (_, meta_box) = find_box(avif, 0..avif.len(), b"meta")?;
    let primary = primary_item_id(avif, meta_pl.clone())?;
    let (iprp_pl, iprp_box) = find_box(avif, meta_pl.clone(), b"iprp")?;
    let (ipco_pl, ipco_box) = find_box(avif, iprp_pl.clone(), b"ipco")?;
    let mut prop_count = 0usize;
    let mut i = ipco_pl.start;
    while i + 8 <= ipco_pl.end {
        let (_, _, box_end) = next_box(avif, i, ipco_pl.end)?;
        prop_count += 1;
        i = box_end;
    }
    let (ipma_pl, ipma_box) = find_box(avif, iprp_pl, b"ipma")?;
    let entry = ipma_walk(avif.get(ipma_pl.clone())?, primary, |_, _| {})?;
    let new_idx = prop_count + 1;
    if new_idx > if entry.wide { 0x7FFF } else { 0x7F } {
        return None;
    }

    // colr box: size + "colr" + "prof" + profile bytes.
    let colr_len = 12 + icc.len();
    let mut colr = Vec::with_capacity(colr_len);
    colr.extend(u32::try_from(colr_len).ok()?.to_be_bytes());
    colr.extend_from_slice(b"colr");
    colr.extend_from_slice(b"prof");
    colr.extend_from_slice(icc);
    // avif-serialize writes narrow associations today, so the wide arm
    // is reachable only if that changes; the extractor's wide arm, by
    // contrast, runs on arbitrary third-party sources.
    let assoc: Vec<u8> = if entry.wide {
        (new_idx as u16).to_be_bytes().to_vec()
    } else {
        vec![new_idx as u8]
    };

    // Two insertion points, in file order (ipco precedes ipma inside
    // iprp in every writer we consume, but nothing below assumes it).
    let ins_colr = ipco_pl.end;
    let ins_assoc = ipma_pl.start + entry.entry_end;
    let (first, second) = if ins_colr <= ins_assoc {
        ((ins_colr, &colr), (ins_assoc, &assoc))
    } else {
        ((ins_assoc, &assoc), (ins_colr, &colr))
    };
    let delta = colr.len() + assoc.len();
    let mut out = Vec::with_capacity(avif.len() + delta);
    out.extend_from_slice(&avif[..first.0]);
    out.extend_from_slice(first.1);
    out.extend_from_slice(&avif[first.0..second.0]);
    out.extend_from_slice(second.1);
    out.extend_from_slice(&avif[second.0..]);

    // A position in the original maps into `out` shifted by whatever
    // was inserted before it.
    let shift = |pos: usize| -> usize {
        pos + if pos >= second.0 {
            delta
        } else if pos >= first.0 {
            first.1.len()
        } else {
            0
        }
    };
    // Grow the enclosing box sizes (size field = first 4 bytes of the
    // box; all four are ordinary compact-size boxes here).
    for (bx, grow) in [
        (&meta_box, delta),
        (&iprp_box, delta),
        (&ipco_box, colr.len()),
        (&ipma_box, assoc.len()),
    ] {
        let at = shift(bx.start);
        let old = u32::from_be_bytes(out.get(at..at + 4)?.try_into().ok()?);
        let new = old.checked_add(u32::try_from(grow).ok()?)?;
        out.get_mut(at..at + 4)?.copy_from_slice(&new.to_be_bytes());
    }
    // One more association on the primary item's entry.
    let count_at = shift(ipma_pl.start + entry.count_pos);
    let c = *out.get(count_at)?;
    if c == u8::MAX {
        return None;
    }
    *out.get_mut(count_at)? = c + 1;
    // Absolute iloc offsets move by however much landed before them.
    patch_iloc(avif, &mut out, meta_pl, &shift, |pos| {
        if pos >= second.0 {
            delta as u64
        } else if pos >= first.0 {
            first.1.len() as u64
        } else {
            0
        }
    })?;

    // Prove the surgery before shipping it.
    (extract_icc(&out).as_deref() == Some(icc)).then_some(out)
}

/// Add `value_delta(original_target)` to every absolute file offset in
/// the `iloc` box (construction method 0). Offset *fields* are located
/// on the original buffer and patched through `shift`.
fn patch_iloc(
    avif: &[u8],
    out: &mut [u8],
    meta: std::ops::Range<usize>,
    shift: &dyn Fn(usize) -> usize,
    value_delta: impl Fn(usize) -> u64,
) -> Option<()> {
    let (iloc, _) = find_box(avif, meta, b"iloc")?;
    let p = avif.get(iloc.clone())?;
    let version = *p.first()?;
    let mut off = 4usize;
    let sizes = *p.get(off)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0xF) as usize);
    let sizes2 = *p.get(off + 1)?;
    let base_offset_size = (sizes2 >> 4) as usize;
    let index_size = if version >= 1 {
        (sizes2 & 0xF) as usize
    } else {
        0
    };
    off += 2;
    if ![0, 4, 8].contains(&offset_size) || ![0, 4, 8].contains(&base_offset_size) {
        return None;
    }
    let item_count = if version < 2 {
        let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
        off += 2;
        v
    } else {
        let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
        off += 4;
        v
    };
    // Patch an offset field of `size` bytes at iloc-payload offset `at`.
    let patch = |out: &mut [u8], at: usize, size: usize| -> Option<()> {
        if size == 0 {
            return Some(());
        }
        let val = if size == 4 {
            u32::from_be_bytes(p.get(at..at + 4)?.try_into().ok()?) as u64
        } else {
            u64::from_be_bytes(p.get(at..at + 8)?.try_into().ok()?)
        };
        let target = usize::try_from(val).ok()?;
        let new = val.checked_add(value_delta(target))?;
        let dst = shift(iloc.start + at);
        if size == 4 {
            out.get_mut(dst..dst + 4)?
                .copy_from_slice(&u32::try_from(new).ok()?.to_be_bytes());
        } else {
            out.get_mut(dst..dst + 8)?
                .copy_from_slice(&new.to_be_bytes());
        }
        Some(())
    };
    // embed_icc only ever patches our own serializer's output (a
    // handful of items); anything bigger is refused outright, because
    // a *partially* patched iloc would not be caught by the caller's
    // re-extraction check (which reads properties, not item data).
    if item_count > 64 {
        return None;
    }
    for _ in 0..item_count {
        off += if version < 2 { 2 } else { 4 }; // item_id
        let method = if version >= 1 {
            let m = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) & 0xF;
            off += 2;
            m
        } else {
            0
        };
        off += 2; // data_reference_index
        let base_at = off;
        off += base_offset_size;
        if method == 0 && base_offset_size > 0 {
            patch(out, base_at, base_offset_size)?;
        }
        let extent_count = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as usize;
        off += 2;
        if extent_count > 64 {
            return None;
        }
        for _ in 0..extent_count {
            off += index_size;
            let ext_at = off;
            off += offset_size;
            off += length_size;
            if method == 0 && base_offset_size == 0 {
                patch(out, ext_at, offset_size)?;
            }
        }
    }
    Some(())
}

/// Does the container declare an image sequence (`avis` brand)?
/// MIAF requires such files to also carry a valid still primary item,
/// which is what the first-frame fallbacks below read.
fn is_animated_brand(data: &[u8]) -> bool {
    match find_box(data, 0..data.len(), b"ftyp") {
        Some((pl, _)) => data[pl].chunks(4).any(|b| b == b"avis"),
        None => false,
    }
}

/// The raw AV1 payload of the primary item, assembled from its `iloc`
/// extents (construction method 0 — absolute file offsets). Bounds-
/// checked throughout; `None` on anything the walk does not fully
/// recognize.
fn primary_item_bytes(avif: &[u8]) -> Option<Vec<u8>> {
    let meta = meta_payload(avif)?;
    let primary = primary_item_id(avif, meta.clone())?;
    let (iloc, _) = find_box(avif, meta, b"iloc")?;
    let p = avif.get(iloc)?;
    let version = *p.first()?;
    let sizes = *p.get(4)?;
    let (offset_size, length_size) = ((sizes >> 4) as usize, (sizes & 0xF) as usize);
    let sizes2 = *p.get(5)?;
    let base_offset_size = (sizes2 >> 4) as usize;
    let index_size = if version >= 1 {
        (sizes2 & 0xF) as usize
    } else {
        0
    };
    if ![0, 4, 8].contains(&offset_size)
        || ![0, 4, 8].contains(&length_size)
        || ![0, 4, 8].contains(&base_offset_size)
    {
        return None;
    }
    let mut off = 6usize;
    let read_u = |off: &mut usize, size: usize| -> Option<u64> {
        let v = match size {
            0 => 0,
            4 => u32::from_be_bytes(p.get(*off..*off + 4)?.try_into().ok()?) as u64,
            8 => u64::from_be_bytes(p.get(*off..*off + 8)?.try_into().ok()?),
            _ => return None,
        };
        *off += size;
        Some(v)
    };
    let item_count = if version < 2 {
        let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
        off += 2;
        v
    } else {
        let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
        off += 4;
        v
    };
    for _ in 0..item_count.min(64) {
        let item_id = if version < 2 {
            let v = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as u32;
            off += 2;
            v
        } else {
            let v = u32::from_be_bytes(p.get(off..off + 4)?.try_into().ok()?);
            off += 4;
            v
        };
        let method = if version >= 1 {
            let m = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) & 0xF;
            off += 2;
            m
        } else {
            0
        };
        off += 2; // data_reference_index
        let base = read_u(&mut off, base_offset_size)?;
        let extent_count = u16::from_be_bytes(p.get(off..off + 2)?.try_into().ok()?) as usize;
        off += 2;
        if extent_count > 64 {
            return None;
        }
        let mut out: Option<Vec<u8>> = (item_id == primary && method == 0).then(Vec::new);
        for _ in 0..extent_count {
            off += index_size;
            let ext_off = read_u(&mut off, offset_size)?;
            let ext_len = read_u(&mut off, length_size)?;
            if let Some(out) = out.as_mut() {
                let start = usize::try_from(base.checked_add(ext_off)?).ok()?;
                let len = usize::try_from(ext_len).ok()?;
                out.extend_from_slice(avif.get(start..start.checked_add(len)?)?);
            }
        }
        if let Some(out) = out {
            return (!out.is_empty()).then_some(out);
        }
    }
    None
}

/// The primary item's `ispe` dimensions via the property associations.
fn primary_ispe(avif: &[u8]) -> Option<(usize, usize)> {
    let meta = meta_payload(avif)?;
    let primary = primary_item_id(avif, meta.clone())?;
    let (iprp, _) = find_box(avif, meta, b"iprp")?;
    let (ipco, _) = find_box(avif, iprp.clone(), b"ipco")?;
    let mut props: Vec<([u8; 4], std::ops::Range<usize>)> = Vec::new();
    let mut i = ipco.start;
    while i + 8 <= ipco.end && props.len() < 1024 {
        let (typ, payload, box_end) = next_box(avif, i, ipco.end)?;
        props.push((typ, payload));
        i = box_end;
    }
    let (ipma, _) = find_box(avif, iprp, b"ipma")?;
    let mut dims = None;
    ipma_walk(avif.get(ipma)?, primary, |item_id, idx| {
        if item_id != primary || dims.is_some() || idx == 0 {
            return;
        }
        if let Some((typ, payload)) = props.get(idx - 1)
            && typ == b"ispe"
        {
            let p = &avif[payload.clone()];
            if let (Some(w), Some(h)) = (p.get(4..8), p.get(8..12)) {
                let w = u32::from_be_bytes(w.try_into().unwrap()) as usize;
                let h = u32::from_be_bytes(h.try_into().unwrap()) as usize;
                if w > 0 && h > 0 {
                    dims = Some((w, h));
                }
            }
        }
    })?;
    dims
}

/// Container-level probe: dimensions from the primary item's AV1
/// sequence header, no pixel decoding. Animated containers fall back
/// to the still primary item's `ispe`.
pub fn probe_avif(data: &[u8]) -> Result<(usize, usize)> {
    let avif = match read_avif_container(data) {
        Ok(a) => a,
        Err(e) => {
            if is_animated_brand(data)
                && let Some(dims) = primary_ispe(data)
            {
                return Ok(dims);
            }
            return Err(e);
        }
    };
    let meta = avif
        .primary_item_metadata()
        .context("parse AV1 sequence header")?;
    Ok((
        meta.max_frame_width.get() as usize,
        meta.max_frame_height.get() as usize,
    ))
}

/// Decode an AVIF file via dav1d to interleaved RGB8 or, when the file
/// carries an alpha auxiliary image, straight-alpha RGBA8. Handles
/// 4:0:0/4:2:0/4:2:2/4:4:4 at 8/10/12 bits, identity/BT.601/BT.709/
/// BT.2020-NCL matrices, and both color ranges; premultiplied alpha is
/// converted to straight alpha. Returns (pixels, width, height, channels).
pub fn decode_avif(data: &[u8]) -> Result<(Vec<u8>, usize, usize, usize)> {
    let mut out = Vec::new();
    let (w, h, channels) = decode_avif_into(data, &mut out)?;
    Ok((out, w, h, channels))
}

/// Like [`decode_avif`], but reuses `out` as the pixel buffer.
pub fn decode_avif_into(data: &[u8], out: &mut Vec<u8>) -> Result<(usize, usize, usize)> {
    let avif = match read_avif_container(data) {
        Ok(a) => a,
        Err(e) => {
            // Animated AVIF: avif-parse rejects sequences outright, but
            // MIAF requires them to carry a valid still primary item —
            // decode that (first-frame rendering, like other image
            // proxies). Alpha tracks are not decoded.
            if is_animated_brand(data)
                && let Some(item) = primary_item_bytes(data)
            {
                let (w, h) = with_decoded_picture(&item, |pic| picture_to_rgb(pic, out))?;
                return Ok((w, h, 3));
            }
            return Err(e);
        }
    };
    let (w, h) = with_decoded_picture(&avif.primary_item, |pic| picture_to_rgb(pic, out))?;
    let Some(alpha_item) = avif.alpha_item.as_deref() else {
        return Ok((w, h, 3));
    };

    let alpha = with_decoded_picture(alpha_item, |pic| picture_to_alpha(pic, w, h))
        .context("decode alpha item")?;
    // Expand RGB to RGBA in place, back to front (writes at i*4.. never
    // overlap reads at j*3..j*3+3 for j < i); every output position is
    // written, so growth does not need to re-zero.
    if out.len() < w * h * 4 {
        out.resize(w * h * 4, 0);
    }
    out.truncate(w * h * 4);
    for i in (0..w * h).rev() {
        let (r, g, b) = (out[i * 3], out[i * 3 + 1], out[i * 3 + 2]);
        let a = alpha[i];
        let (r, g, b) = if avif.premultiplied_alpha && a != 255 {
            if a == 0 {
                (0, 0, 0)
            } else {
                let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                (un(r), un(g), un(b))
            }
        } else {
            (r, g, b)
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
    Ok((w, h, 4))
}

/// dav1d worker threads. The default is architecture-aware: on x86-64,
/// two threads ride the second SMT sibling of the request's core (the
/// same rationale as libwebp's two-thread decode, which libvips also
/// ships), improving both latency and saturated throughput. aarch64
/// server cores have no SMT, so a second thread costs a full core:
/// wall latency improves at light load but saturated throughput drops
/// (measured -6% requests/s on Graviton3) — single-threaded decoding
/// is the default there. OXIMG_AVIF_DECODE_THREADS overrides.
fn dav1d_threads() -> std::os::raw::c_int {
    std::env::var("OXIMG_AVIF_DECODE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(target_arch = "x86_64") { 2 } else { 1 })
}

/// Run one dav1d session over a single-frame AV1 stream and hand the
/// decoded picture to `f`.
fn with_decoded_picture<T>(
    av1: &[u8],
    f: impl FnOnce(&dav1d_sys::Dav1dPicture) -> Result<T>,
) -> Result<T> {
    use dav1d_sys as d;
    unsafe {
        let mut settings: d::Dav1dSettings = std::mem::zeroed();
        d::dav1d_default_settings(&mut settings);
        settings.n_threads = dav1d_threads();
        settings.max_frame_delay = 1;

        let mut ctx: *mut d::Dav1dContext = std::ptr::null_mut();
        ensure!(d::dav1d_open(&mut ctx, &settings) == 0, "dav1d_open");
        struct Ctx(*mut d::Dav1dContext);
        impl Drop for Ctx {
            fn drop(&mut self) {
                unsafe { d::dav1d_close(&mut self.0) }
            }
        }
        let _ctx_guard = Ctx(ctx);

        // Borrow the OBU buffer; it outlives the decoder, so the free
        // callback (which dav1d requires to be non-null) is a no-op.
        unsafe extern "C" fn no_free(_buf: *const u8, _cookie: *mut std::ffi::c_void) {}
        let mut data: d::Dav1dData = std::mem::zeroed();
        ensure!(
            d::dav1d_data_wrap(
                &mut data,
                av1.as_ptr(),
                av1.len(),
                Some(no_free),
                std::ptr::null_mut()
            ) == 0,
            "dav1d_data_wrap"
        );
        struct Data(*mut d::Dav1dData);
        impl Drop for Data {
            fn drop(&mut self) {
                unsafe {
                    if !(*self.0).data.is_null() {
                        d::dav1d_data_unref(self.0);
                    }
                }
            }
        }
        let _data_guard = Data(&mut data);

        let mut pic: d::Dav1dPicture = std::mem::zeroed();
        loop {
            if data.sz > 0 {
                let res = d::dav1d_send_data(ctx, &mut data);
                ensure!(res == 0 || res == -EAGAIN, "dav1d_send_data: {res}");
            }
            let res = d::dav1d_get_picture(ctx, &mut pic);
            if res == 0 {
                break;
            }
            ensure!(
                res == -EAGAIN && data.sz > 0,
                "dav1d_get_picture: {res} (no picture in stream)"
            );
        }
        struct Pic(*mut d::Dav1dPicture);
        impl Drop for Pic {
            fn drop(&mut self) {
                unsafe { d::dav1d_picture_unref(self.0) }
            }
        }
        let _pic_guard = Pic(&mut pic);

        f(&pic)
    }
}

/// Extract the alpha plane (the luma of the auxiliary image) as u8.
fn picture_to_alpha(pic: &dav1d_sys::Dav1dPicture, w: usize, h: usize) -> Result<Vec<u8>> {
    ensure!(
        (pic.p.w as usize, pic.p.h as usize) == (w, h),
        "alpha dimensions do not match the color image"
    );
    let bpc = pic.p.bpc as u32;
    ensure!(matches!(bpc, 8 | 10 | 12), "unsupported bit depth {bpc}");
    let seq = unsafe { &*pic.seq_hdr };
    // The spec requires full range for alpha; honor the flag regardless.
    let max = ((1u32 << bpc) - 1) as f32;
    let scale8 = (1u32 << (bpc - 8)) as f32;
    let (a_mul, a_off) = if seq.color_range != 0 {
        (255.0 / max, 0.0)
    } else {
        (255.0 / (219.0 * scale8), 16.0 * scale8)
    };

    let hbd = bpc > 8;
    let mut alpha = vec![0u8; w * h];
    for y in 0..h {
        let row = &mut alpha[y * w..(y + 1) * w];
        let src = plane_row(pic, 0, y, w, hbd);
        match src {
            Row::B8(s) if seq.color_range != 0 => row.copy_from_slice(s),
            src => yuv::alpha_row(src, a_off, a_mul, row),
        }
    }
    Ok(alpha)
}

/// Borrow one row of a dav1d plane as typed samples. Plane 0 uses the
/// luma stride; planes 1/2 share the chroma stride. Strides are bytes.
fn plane_row(
    pic: &dav1d_sys::Dav1dPicture,
    plane: usize,
    y: usize,
    len: usize,
    hbd: bool,
) -> Row<'_> {
    let (ptr, stride) = if plane == 0 {
        (pic.data[0], pic.stride[0] as usize)
    } else {
        (pic.data[plane], pic.stride[1] as usize)
    };
    unsafe {
        if hbd {
            Row::B16(std::slice::from_raw_parts(
                (ptr as *const u16).add(y * (stride / 2)),
                len,
            ))
        } else {
            Row::B8(std::slice::from_raw_parts(
                (ptr as *const u8).add(y * stride),
                len,
            ))
        }
    }
}

/// Convert a decoded dav1d picture (planar YUV) to interleaved RGB8.
fn picture_to_rgb(pic: &dav1d_sys::Dav1dPicture, out: &mut Vec<u8>) -> Result<(usize, usize)> {
    use dav1d_sys as d;
    let (w, h) = (pic.p.w as usize, pic.p.h as usize);
    let bpc = pic.p.bpc as u32;
    ensure!(matches!(bpc, 8 | 10 | 12), "unsupported bit depth {bpc}");
    let seq = unsafe { &*pic.seq_hdr };
    let full_range = seq.color_range != 0;
    let monochrome = pic.p.layout == d::DAV1D_PIXEL_LAYOUT_I400;
    let (sx, sy) = match pic.p.layout {
        d::DAV1D_PIXEL_LAYOUT_I420 => (1u32, 1u32),
        d::DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
        _ => (0, 0),
    };

    let hbd = bpc > 8;
    let max = ((1u32 << bpc) - 1) as f32;
    let center = ((1u32 << bpc) / 2) as f32;
    let scale8 = (1u32 << (bpc - 8)) as f32;
    // Normalize to the 0..255 scale: full range divides by the sample
    // maximum; limited range maps 16..235 (luma) / 16..240 (chroma).
    let (y_mul, y_off, c_mul) = if full_range {
        (255.0 / max, 0.0, 255.0 / max)
    } else {
        (
            255.0 / (219.0 * scale8),
            16.0 * scale8,
            255.0 / (224.0 * scale8),
        )
    };

    // Matrix coefficients from the sequence header. Unspecified and exotic
    // matrices fall back to BT.601 so a slightly mistagged file still
    // serves a reasonable image instead of a 5xx.
    let identity = seq.mtrx == 0 && !monochrome;
    if identity {
        ensure!(
            pic.p.layout == d::DAV1D_PIXEL_LAYOUT_I444,
            "identity matrix requires 4:4:4"
        );
    }
    let (kr, kb) = match seq.mtrx {
        1 => (0.2126, 0.0722),     // BT.709
        9 => (0.2627, 0.0593),     // BT.2020 NCL
        _ => (0.299f32, 0.114f32), // BT.601 and fallback
    };
    let kg = 1.0 - kr - kb;

    // Subsampled chroma is upsampled with the separable center-sited
    // bilinear kernel (9:3:3:1) that libyuv and libjpeg's "fancy
    // upsampling" use, so output matches avifdec instead of showing
    // nearest-neighbor chroma blocking.
    let cw = if sx == 1 { w.div_ceil(2) } else { w };
    let ch = if sy == 1 { h.div_ceil(2) } else { h };
    let mut cb_mid = vec![0f32; cw];
    let mut cr_mid = vec![0f32; cw];
    let mut cb_row = vec![0f32; w];
    let mut cr_row = vec![0f32; w];

    out.clear();
    out.resize(w * h * 3, 0);
    let csc = yuv::Csc {
        y_off,
        y_mul,
        center,
        c_mul,
        kr,
        kb,
        kg,
    };
    for y in 0..h {
        if !monochrome {
            // Vertical pass at chroma horizontal resolution.
            let (near, other) = if sy == 1 {
                let near = y >> 1;
                let other = if y & 1 == 1 {
                    (near + 1).min(ch - 1)
                } else {
                    near.saturating_sub(1)
                };
                (near, other)
            } else {
                (y, y)
            };
            for (plane, mid) in [(1usize, &mut cb_mid), (2, &mut cr_mid)] {
                if sy == 1 {
                    yuv::chroma_blend(
                        plane_row(pic, plane, near, cw, hbd),
                        plane_row(pic, plane, other, cw, hbd),
                        mid,
                    );
                } else {
                    yuv::chroma_widen(plane_row(pic, plane, near, cw, hbd), mid);
                }
            }
            // Horizontal pass to full resolution.
            if sx == 1 {
                yuv::chroma_upsample_h(&cb_mid, &mut cb_row);
                yuv::chroma_upsample_h(&cr_mid, &mut cr_row);
            } else {
                cb_row.copy_from_slice(&cb_mid);
                cr_row.copy_from_slice(&cr_mid);
            }
        }

        let y_row = plane_row(pic, 0, y, w, hbd);
        let row = &mut out[y * w * 3..(y + 1) * w * 3];
        if !monochrome && !identity {
            yuv::yuv_row_to_rgb(y_row, &cb_row, &cr_row, &csc, row);
            continue;
        }
        for (x, px) in row.chunks_exact_mut(3).enumerate() {
            let yf = (y_row.at(x) - y_off) * y_mul;
            let (r, g, b) = if monochrome {
                (yf, yf, yf)
            } else {
                // Identity: G=Y, B=U, R=V, chroma is not centered.
                (cr_row[x] * y_mul, yf, cb_row[x] * y_mul)
            };
            px[0] = (r + 0.5).clamp(0.0, 255.0) as u8;
            px[1] = (g + 0.5).clamp(0.0, 255.0) as u8;
            px[2] = (b + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out.truncate(w * h * 3);
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Embed → extract must be the identity, the patched container must
    /// decode to the same pixels, and the growth must be exactly the
    /// colr box plus one association byte.
    #[test]
    fn icc_embed_extract_roundtrip() {
        let rgb = pixel_samples(64 * 48 * 3, 7);
        let plain = encode_avif(&rgb, 64, 48, 3, &AvifParams::default(), None).unwrap();
        assert_eq!(extract_icc(&plain), None, "no profile without embedding");

        let icc: Vec<u8> = (0..900u32).map(|i| (i % 251) as u8).collect();
        let profiled = encode_avif(&rgb, 64, 48, 3, &AvifParams::default(), Some(&icc)).unwrap();
        assert_eq!(extract_icc(&profiled).as_deref(), Some(&icc[..]));
        assert_eq!(
            profiled.len(),
            plain.len() + 12 + icc.len() + 1,
            "growth = colr box + one association"
        );
        let (a, aw, ah, _) = decode_avif(&plain).unwrap();
        let (b, bw, bh, _) = decode_avif(&profiled).unwrap();
        assert_eq!((aw, ah), (bw, bh));
        assert_eq!(a, b, "profile splice must not disturb the image data");
        assert_eq!(probe_avif(&profiled).unwrap(), (64, 48));
    }

    /// Alpha adds a second item whose iloc entry must shift correctly
    /// too.
    #[test]
    fn icc_embed_survives_alpha_items() {
        let mut rgba = pixel_samples(48 * 32 * 4, 11);
        rgba[3] = 128; // ensure a transparent pixel keeps the alpha item
        let icc: Vec<u8> = (0..300u32).map(|i| (i * 7 % 251) as u8).collect();
        let profiled = encode_avif(&rgba, 48, 32, 4, &AvifParams::default(), Some(&icc)).unwrap();
        assert_eq!(extract_icc(&profiled).as_deref(), Some(&icc[..]));
        let (_, w, h, ch) = decode_avif(&profiled).unwrap();
        assert_eq!((w, h, ch), (48, 32, 4), "alpha item survives the splice");
    }

    /// Simple compact box for hand-built container tests.
    fn fbox(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = ((payload.len() + 8) as u32).to_be_bytes().to_vec();
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    /// The extractor must read layouts our own serializer never
    /// writes: version-1 `pitm`/`ipma` (u32 item ids), wide (15-bit)
    /// associations, and essential-flagged narrow associations.
    #[test]
    fn icc_extract_reads_wide_and_versioned_layouts() {
        let icc = [0xAB_u8; 96];
        let mut colr = b"prof".to_vec();
        colr.extend_from_slice(&icc);
        // A filler property first, so colr is property index 2.
        let ipco = fbox(
            b"ipco",
            &[fbox(b"free", &[0u8; 4]), fbox(b"colr", &colr)].concat(),
        );

        // Wide + version 1: item id 7 as u32, association as u16.
        let mut ipma_pl = vec![1, 0, 0, 1];
        ipma_pl.extend(1u32.to_be_bytes()); // entry_count
        ipma_pl.extend(7u32.to_be_bytes()); // item_id
        ipma_pl.push(1); // association count
        ipma_pl.extend(2u16.to_be_bytes()); // wide index 2
        let iprp = fbox(b"iprp", &[ipco.clone(), fbox(b"ipma", &ipma_pl)].concat());
        let mut pitm_pl = vec![1, 0, 0, 0];
        pitm_pl.extend(7u32.to_be_bytes());
        let meta_pl = [vec![0, 0, 0, 0], fbox(b"pitm", &pitm_pl), iprp].concat();
        let avif = fbox(b"meta", &meta_pl);
        assert_eq!(extract_icc(&avif).as_deref(), Some(&icc[..]), "wide/v1");

        // Narrow + version 0 with the essential bit set on the index.
        let mut ipma_pl = vec![0, 0, 0, 0];
        ipma_pl.extend(1u32.to_be_bytes());
        ipma_pl.extend(7u16.to_be_bytes());
        ipma_pl.push(1);
        ipma_pl.push(0x80 | 2); // essential + index 2
        let iprp = fbox(b"iprp", &[ipco, fbox(b"ipma", &ipma_pl)].concat());
        let mut pitm_pl = vec![0, 0, 0, 0];
        pitm_pl.extend(7u16.to_be_bytes());
        let meta_pl = [vec![0, 0, 0, 0], fbox(b"pitm", &pitm_pl), iprp].concat();
        let avif = fbox(b"meta", &meta_pl);
        assert_eq!(
            extract_icc(&avif).as_deref(),
            Some(&icc[..]),
            "narrow/essential"
        );
    }

    /// HEIF applies transformative properties in association order.
    /// MIAF writers put irot before imir (the rotation-first table);
    /// a spec-legal mirror-first file must reduce via the dihedral
    /// identity instead — pinned against libheif 1.23's rendering of
    /// exactly this byte layout (association bytes swapped on the
    /// irot1+imir1 fixture: libheif shows mirror-then-rotation).
    #[test]
    fn orientation_honors_association_order() {
        let build = |first: (&[u8; 4], u8), second: (&[u8; 4], u8)| -> Vec<u8> {
            let ipco = fbox(
                b"ipco",
                &[fbox(first.0, &[first.1]), fbox(second.0, &[second.1])].concat(),
            );
            let mut ipma_pl = vec![0, 0, 0, 0];
            ipma_pl.extend(1u32.to_be_bytes());
            ipma_pl.extend(7u16.to_be_bytes());
            ipma_pl.push(2); // two associations, in ipco order
            ipma_pl.push(0x80 | 1);
            ipma_pl.push(0x80 | 2);
            let iprp = fbox(b"iprp", &[ipco, fbox(b"ipma", &ipma_pl)].concat());
            let mut pitm_pl = vec![0, 0, 0, 0];
            pitm_pl.extend(7u16.to_be_bytes());
            let meta_pl = [vec![0, 0, 0, 0], fbox(b"pitm", &pitm_pl), iprp].concat();
            fbox(b"meta", &meta_pl)
        };
        // MIAF order: rotation then mirror → EXIF 7 (transverse).
        let rot_first = build((b"irot", 1), (b"imir", 1));
        assert_eq!(
            extract_orientation(&rot_first),
            crate::meta::Orientation::from_rot_mirror(1, Some(1))
        );
        // Mirror first: libheif renders mirror-then-rotation, which the
        // identity rot_a ∘ mirror = mirror ∘ rot_{-a} maps to the
        // rotation-first table at the negated angle → EXIF 5.
        let mirror_first = build((b"imir", 1), (b"irot", 1));
        assert_eq!(
            extract_orientation(&mirror_first),
            crate::meta::Orientation::from_rot_mirror(3, Some(1))
        );
        assert_ne!(
            extract_orientation(&rot_first),
            extract_orientation(&mirror_first)
        );
    }

    /// The extractor never errors on hostile input, and the embedder
    /// declines rather than corrupts.
    #[test]
    fn icc_walkers_are_fail_safe() {
        let rgb = pixel_samples(32 * 24 * 3, 3);
        let plain = encode_avif(&rgb, 32, 24, 3, &AvifParams::default(), None).unwrap();
        for cut in [0, 8, 40, plain.len() / 2] {
            assert_eq!(extract_icc(&plain[..cut]), None);
        }
        assert_eq!(extract_icc(b"not an avif at all"), None);
        assert!(embed_icc(b"garbage", &[1, 2, 3]).is_none());
        assert!(embed_icc(&plain[..40], &[1, 2, 3]).is_none());
    }

    /// Deterministic pseudo-random pixels covering 0 and 255 exactly.
    fn pixel_samples(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|i| match i % 17 {
                0 => 0,
                1 => 255,
                _ => {
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    (s >> 24) as u8
                }
            })
            .collect()
    }

    /// Manual micro-benchmark: kernel-vs-scalar conversion cost.
    /// cargo test --release --features avif bench_yuv -- --ignored --nocapture
    #[cfg(target_arch = "x86_64")]
    #[test]
    #[ignore = "manual micro-benchmark"]
    fn bench_yuv_kernels() {
        let (w, h) = (512usize, 340usize);
        let px = pixel_samples(w * h * 3, 42);
        let mut y = vec![0u16; w * h];
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (mut cb, mut cr) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
        let iters = 3000u32;
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1e3 / iters as f64;

        let t = std::time::Instant::now();
        for _ in 0..iters {
            luma_rows_scalar(&px, 3, &mut y);
            std::hint::black_box(&y);
        }
        eprintln!("luma scalar(auto-vec):  {:.4} ms/frame", ms(t));

        if avx2_enc::detect() {
            let t = std::time::Instant::now();
            for _ in 0..iters {
                unsafe { avx2_enc::luma_rows(&px, 3, &mut y) };
                std::hint::black_box(&y);
            }
            eprintln!("luma avx2 intrinsics:   {:.4} ms/frame", ms(t));
        }

        let rb = w * 3;
        let t = std::time::Instant::now();
        for _ in 0..iters {
            for cy in 0..ch {
                let row0 = &px[cy * 2 * rb..][..rb];
                let row1 = (cy * 2 + 1 < h).then(|| &px[(cy * 2 + 1) * rb..][..rb]);
                for cx in 0..cw {
                    let (b, r) = chroma_block_rows(row0, row1, 3, cx, w);
                    cb[cy * cw + cx] = b;
                    cr[cy * cw + cx] = r;
                }
            }
            std::hint::black_box((&cb, &cr));
        }
        eprintln!("chroma scalar blocks:   {:.4} ms/frame", ms(t));

        if avx2_enc::detect() {
            let t = std::time::Instant::now();
            for _ in 0..iters {
                for cy in 0..ch {
                    let row0 = &px[cy * 2 * rb..][..rb];
                    if cy * 2 + 1 < h {
                        let row1 = &px[(cy * 2 + 1) * rb..][..rb];
                        unsafe {
                            avx2_enc::chroma_row_pair(
                                row0,
                                row1,
                                w,
                                3,
                                &mut cb[cy * cw..][..cw],
                                &mut cr[cy * cw..][..cw],
                            )
                        };
                    }
                }
                std::hint::black_box((&cb, &cr));
            }
            eprintln!("chroma avx2 intrinsics: {:.4} ms/frame", ms(t));
        }
    }

    /// Frame-level scalar chroma reference, built from the row-pair
    /// block the vector paths must match.
    fn chroma_frame_scalar(px: &[u8], w: usize, h: usize, channels: usize) -> (Vec<u16>, Vec<u16>) {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let (mut cb, mut cr) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
        let rb = w * channels;
        for cy in 0..ch {
            let row0 = &px[cy * 2 * rb..][..rb];
            let row1 = (cy * 2 + 1 < h).then(|| &px[(cy * 2 + 1) * rb..][..rb]);
            for cx in 0..cw {
                let (b, r) = chroma_block_rows(row0, row1, channels, cx, w);
                cb[cy * cw + cx] = b;
                cr[cy * cw + cx] = r;
            }
        }
        (cb, cr)
    }

    /// The fused AVIF path converts row by row instead of over the full
    /// frame; the vector main loops restart (and re-tail) per row, so
    /// pin that chunking never changes values.
    #[test]
    fn row_wise_luma_matches_full_frame() {
        let (w, h, channels) = (61usize, 9, 3);
        let px = pixel_samples(w * h * channels, 11);
        let mut full = vec![0u16; w * h];
        luma_rows(&px, channels, &mut full);
        let mut rows = vec![0u16; w * h];
        for y in 0..h {
            luma_rows(
                &px[y * w * channels..][..w * channels],
                channels,
                &mut rows[y * w..][..w],
            );
        }
        assert_eq!(full, rows);
    }

    /// The NEON luma path replaces `acc / 1044480` with
    /// `((acc >> 12) * 8421505) >> 31`; prove both identities it stacks
    /// (factor split and magic-multiply /255) over the full domain the
    /// pipeline can produce.
    #[test]
    fn luma_divider_identity_is_exact() {
        for x in 0..=1_044_480u32 {
            let acc = x * 1023 + 522_240;
            let reference = acc / 1_044_480;
            let vectorized = (((acc >> 12) as u64 * 8_421_505) >> 31) as u32;
            assert_eq!(reference, vectorized, "x={x}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_luma_rows_match_scalar_bit_exactly() {
        if !avx2_enc::detect() {
            return;
        }
        // Odd lengths exercise the scalar tail after the 16-wide loop.
        for (n, channels, seed) in [(1024, 3, 1), (1013, 3, 2), (1024, 4, 3), (777, 4, 4)] {
            let px = pixel_samples(n * channels, seed);
            let mut scalar = vec![0u16; n];
            let mut vector = vec![0u16; n];
            luma_rows_scalar(&px, channels, &mut scalar);
            unsafe { avx2_enc::luma_rows(&px, channels, &mut vector) };
            assert_eq!(scalar, vector, "n={n} channels={channels}");
        }
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_chroma_rows_match_scalar_bit_exactly() {
        if !avx2_enc::detect() {
            return;
        }
        for (w, h, channels, seed) in [
            (128, 64, 3, 5),
            (127, 63, 3, 6),
            (17, 5, 3, 7),
            (130, 62, 4, 8),
            (33, 7, 4, 9),
        ] {
            let px = pixel_samples(w * h * channels, seed);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (cb_s, cr_s) = chroma_frame_scalar(&px, w, h, channels);
            let (mut cb_v, mut cr_v) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
            chroma_rows(&px, w, h, channels, &mut cb_v, &mut cr_v);
            assert_eq!(cb_s, cb_v, "cb {w}x{h} channels={channels}");
            assert_eq!(cr_s, cr_v, "cr {w}x{h} channels={channels}");
        }
    }

    /// The AVX2 chroma path rounds with floor(x + 0.5) instead of the
    /// scalar ties-away `round()` (valid for x >= 0). Sweep uniform 2x2
    /// blocks across the color cube to hunt rounding-boundary
    /// disagreements the randomized tests might miss.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn avx2_chroma_rounding_matches_on_uniform_sweep() {
        if !avx2_enc::detect() {
            return;
        }
        // 16 blocks per 32x2 image; r dense, b stepped, g coarse.
        let (w, h) = (32usize, 2usize);
        for g in [0u8, 37, 128, 219, 255] {
            for b0 in (0..256usize).step_by(3) {
                let mut px = vec![0u8; w * h * 3];
                for blk in 0..16 {
                    let r = ((b0 + blk * 16) % 256) as u8;
                    let b = ((b0 + blk) % 256) as u8;
                    for dy in 0..2 {
                        for dx in 0..2 {
                            let p = (dy * w + blk * 2 + dx) * 3;
                            px[p] = r;
                            px[p + 1] = g;
                            px[p + 2] = b;
                        }
                    }
                }
                let cw = w / 2;
                let (cb_s, cr_s) = chroma_frame_scalar(&px, w, h, 3);
                let (mut cb_v, mut cr_v) = (vec![0u16; cw], vec![0u16; cw]);
                chroma_rows(&px, w, h, 3, &mut cb_v, &mut cr_v);
                assert_eq!(cb_s, cb_v, "cb g={g} b0={b0}");
                assert_eq!(cr_s, cr_v, "cr g={g} b0={b0}");
            }
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_luma_rows_match_scalar_bit_exactly() {
        if !crate::yuv::neon() {
            return;
        }
        // Odd lengths exercise the scalar tail after the 8-wide loop.
        for (n, channels, seed) in [(1024, 3, 1), (1021, 3, 2), (1024, 4, 3), (777, 4, 4)] {
            let px = pixel_samples(n * channels, seed);
            let mut scalar = vec![0u16; n];
            let mut neon = vec![0u16; n];
            luma_rows_scalar(&px, channels, &mut scalar);
            unsafe { neon_enc::luma_rows(&px, channels, &mut neon) };
            assert_eq!(scalar, neon, "n={n} channels={channels}");
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_chroma_rows_match_scalar_bit_exactly() {
        if !crate::yuv::neon() {
            return;
        }
        // Odd dims exercise the right-edge columns and bottom row that
        // fall back to the scalar block.
        for (w, h, channels, seed) in [
            (128, 64, 3, 5),
            (127, 63, 3, 6),
            (9, 5, 3, 7),
            (130, 62, 4, 8),
            (33, 7, 4, 9),
        ] {
            let px = pixel_samples(w * h * channels, seed);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (cb_s, cr_s) = chroma_frame_scalar(&px, w, h, channels);
            let (mut cb_n, mut cr_n) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
            chroma_rows(&px, w, h, channels, &mut cb_n, &mut cr_n);
            assert_eq!(cb_s, cb_n, "cb {w}x{h} channels={channels}");
            assert_eq!(cr_s, cr_n, "cr {w}x{h} channels={channels}");
        }
    }

    #[test]
    fn encodes_a_decodable_avif() {
        let (w, h) = (128, 96);
        let rgb: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as u8;
                let y = (i / w) as u8;
                [x.wrapping_mul(2), y.wrapping_mul(2), x ^ y]
            })
            .collect();
        let out = encode_avif(&rgb, w, h, 3, &AvifParams::default(), None).unwrap();
        assert!(out.len() > 100, "suspiciously small: {}", out.len());
        // container sanity: ftyp avif brand near the start
        assert_eq!(&out[4..12], b"ftypavif", "not an avif container");
    }

    #[test]
    fn encode_decode_roundtrip_preserves_the_image() {
        let (w, h) = (160, 120);
        // Smooth gradient: compresses well, so quality loss stays small
        // and any plane/matrix/range mix-up shows up as a huge error.
        let rgb: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as f32 / (w - 1) as f32;
                let y = (i / w) as f32 / (h - 1) as f32;
                [
                    (x * 255.0) as u8,
                    (y * 255.0) as u8,
                    ((1.0 - x) * 200.0) as u8,
                ]
            })
            .collect();
        let params = AvifParams {
            quality: 85,
            ..AvifParams::default()
        };
        let encoded = encode_avif(&rgb, w, h, 3, &params, None).unwrap();
        let (decoded, dw, dh, channels) = decode_avif(&encoded).unwrap();
        assert_eq!((dw, dh, channels), (w, h, 3));
        assert_eq!(decoded.len(), rgb.len());
        let se: f64 = rgb
            .iter()
            .zip(&decoded)
            .map(|(&a, &b)| ((a as f64) - (b as f64)).powi(2))
            .sum();
        let rmse = (se / rgb.len() as f64).sqrt();
        assert!(rmse < 6.0, "roundtrip rmse too high: {rmse:.2}");
    }

    #[test]
    fn probe_reports_dimensions_without_decoding() {
        let rgb = vec![128u8; 96 * 64 * 3];
        let encoded = encode_avif(&rgb, 96, 64, 3, &AvifParams::default(), None).unwrap();
        assert_eq!(probe_avif(&encoded).unwrap(), (96, 64));
    }

    #[test]
    fn rgba_roundtrip_preserves_color_and_alpha() {
        let (w, h) = (160, 120);
        // Color gradient with an alpha ramp: left edge transparent,
        // right edge opaque.
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as f32 / (w - 1) as f32;
                let y = (i / w) as f32 / (h - 1) as f32;
                [
                    (x * 255.0) as u8,
                    (y * 255.0) as u8,
                    ((1.0 - x) * 200.0) as u8,
                    (x * 255.0) as u8,
                ]
            })
            .collect();
        let params = AvifParams {
            quality: 85,
            alpha_quality: 85,
            ..AvifParams::default()
        };
        let encoded = encode_avif(&rgba, w, h, 4, &params, None).unwrap();
        let (decoded, dw, dh, channels) = decode_avif(&encoded).unwrap();
        assert_eq!((dw, dh, channels), (w, h, 4));
        let a_se: f64 = rgba
            .chunks_exact(4)
            .zip(decoded.chunks_exact(4))
            .map(|(s, d)| ((s[3] as f64) - (d[3] as f64)).powi(2))
            .sum();
        let a_rmse = (a_se / (w * h) as f64).sqrt();
        assert!(a_rmse < 3.0, "alpha rmse too high: {a_rmse:.2}");
        // Color must survive where alpha is meaningful.
        let (mut c_se, mut n) = (0f64, 0u32);
        for (s, d) in rgba.chunks_exact(4).zip(decoded.chunks_exact(4)) {
            if s[3] > 128 {
                for c in 0..3 {
                    c_se += ((s[c] as f64) - (d[c] as f64)).powi(2);
                }
                n += 3;
            }
        }
        let c_rmse = (c_se / n as f64).sqrt();
        assert!(c_rmse < 8.0, "color rmse too high: {c_rmse:.2}");
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_avif(b"not an avif at all").is_err());
        assert!(decode_avif(&[]).is_err());
    }

    #[test]
    fn yuv_conversion_hits_known_anchors() {
        // white -> Y=1023, Cb=Cr=512; black -> Y=0, Cb=Cr=512
        let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
        rgb_to_yuv420_10bit(
            &[255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            2,
            2,
            3,
            &mut y,
            &mut cb,
            &mut cr,
        );
        assert!(y.iter().all(|&v| v >= 1022), "{y:?}");
        assert_eq!((cb[0], cr[0]), (512, 512));
        let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
        rgb_to_yuv420_10bit(&[0; 12], 2, 2, 3, &mut y, &mut cb, &mut cr);
        assert!(y.iter().all(|&v| v == 0));
        assert_eq!((cb[0], cr[0]), (512, 512));
    }
}
