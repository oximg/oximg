//! Encode-side RGB → 10-bit 4:2:0 conversion: scalar reference
//! plus the bit-identical NEON/AVX2 row kernels.

pub(super) fn quality_to_qp(quality: u8) -> u32 {
    ((100 - quality as u32) * 63 + 50) / 100
}

/// RGB(A)8 -> 10-bit 4:2:0 YUV, BT.601 matrix, full range (matching the
/// avifenc defaults used in the encoder study). Chroma is averaged over
/// each 2x2 block; an alpha channel, if present, is ignored here (it is
/// encoded as a separate auxiliary image). The scalar rows are the
/// reference; the aarch64 NEON rows mirror their arithmetic operation
/// for operation and are asserted bit-identical in tests (the yuv.rs
/// contract). x86-64 AVX2 rows are a possible follow-up.
pub(super) fn rgb_to_yuv420_10bit(
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
        // SAFETY: NEON verified by the runtime check above. The kernel needs
        // `pixels` to hold y_plane.len() whole pixels with channels 3 or 4;
        // all call sites pass exactly that (rgb_to_yuv420_10bit slices
        // w*h*channels with channels validated in encode_avif; the fused
        // path passes one dst_w*3 row against a dst_w luma slice).
        return unsafe { neon_enc::luma_rows(pixels, channels, y_plane) };
    }
    #[cfg(target_arch = "x86_64")]
    if avx2_enc::detect() {
        // SAFETY: AVX2 verified by the runtime check above. Same memory
        // contract as the NEON arm: `pixels` holds y_plane.len() whole pixels,
        // channels 3 or 4 — guaranteed by the same callers.
        return unsafe { avx2_enc::luma_rows(pixels, channels, y_plane) };
    }
    luma_rows_scalar(pixels, channels, y_plane);
}

pub(super) fn luma_rows_scalar(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
    for (px, y) in pixels.chunks_exact(channels).zip(y_plane.iter_mut()) {
        *y = luma_px(px[0], px[1], px[2]);
    }
}

#[inline]
pub(super) fn luma_px(r: u8, g: u8, b: u8) -> u16 {
    let (r, g, b) = (r as u32, g as u32, b as u32);
    (((1225 * r + 2404 * g + 467 * b) * 1023 + 522_240) / 1_044_480) as u16
}

pub(super) fn chroma_rows(
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
            // SAFETY: NEON verified by the runtime check above. The kernel needs
            // w pixels in each row, w.div_ceil(2)-long chroma rows, and channels
            // 3 or 4 — the lengths this fn's doc requires and which both callers
            // (chroma_rows and the fused pipeline path) slice exactly.
            return unsafe { neon_enc::chroma_row_pair(row0, row1, w, channels, cb_row, cr_row) };
        }
        #[cfg(target_arch = "x86_64")]
        if avx2_enc::detect() {
            // SAFETY: AVX2 verified by the runtime check above. Same contract as
            // the NEON arm: w pixels per row, w.div_ceil(2)-long chroma rows,
            // channels 3 or 4.
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
pub(super) fn chroma_block_rows(
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
pub(super) mod neon_enc {
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
    // SAFETY: caller must have NEON enabled (the sole caller, luma_rows,
    // is #[target_feature(enable = "neon")]). Register-only — the
    // intrinsics' feature requirement is the entire contract.
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
    pub(in crate::avif) unsafe fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
        unsafe {
            let n = y_plane.len();
            let mut i = 0;
            // SAFETY (fn contract): caller guarantees NEON is available and that
            // `pixels` holds y_plane.len() whole pixels with channels 3 or 4. The
            // loop bound keeps load8 within pixels ((i+8)*channels <= n*channels)
            // and the 8-lane store within y_plane; the tail is bounds-checked.
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
    pub(in crate::avif) unsafe fn chroma_row_pair(
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
            // SAFETY (fn contract): caller guarantees NEON is available, rows
            // holding w pixels each, cb_row/cr_row w.div_ceil(2) long, channels 3
            // or 4. The loop bound keeps load8 within both rows (cx*2 + 8 <= w
            // pixels) and the 4-lane stores within cb/cr (cx + 4 <= w/2 <= cw).
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
pub(super) mod avx2_enc {
    use std::arch::x86_64::*;

    #[inline]
    pub(in crate::avif) fn detect() -> bool {
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
    // SAFETY: caller must have AVX2 enabled (all callers are
    // #[target_feature(enable = "avx2")] fns). Register-only — the
    // intrinsics' feature requirement is the entire contract.
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
    pub(in crate::avif) unsafe fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
        unsafe {
            let n = y_plane.len();
            let mut i = 0;
            // SAFETY (fn contract): caller guarantees AVX2 is available and that
            // `pixels` holds y_plane.len() whole pixels with channels 3 or 4. The
            // loop bound keeps load16 within pixels ((i+16)*channels <= n*channels)
            // and both 8-lane stores within y_plane; the tail is bounds-checked.
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
    pub(in crate::avif) unsafe fn chroma_row_pair(
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
            // SAFETY (fn contract): caller guarantees AVX2 is available, rows
            // holding w pixels each, cb_row/cr_row w.div_ceil(2) long, channels 3
            // or 4. The loop bound keeps load16 within both rows (cx*2 + 16 <= w
            // pixels) and the 8-lane stores within cb/cr (cx + 8 <= w/2 <= cw).
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
