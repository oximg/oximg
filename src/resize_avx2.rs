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

use crate::resize_kernel::{
    RowKernel, Windows, YCC_FIX_B, YCC_FIX_GB, YCC_FIX_GR, YCC_FIX_R, clamp_u16, resize_u16,
    stage_x3_u8_words, ycc_stride,
};
use anyhow::Result;

/// Marker type implementing [`RowKernel`] with AVX2+FMA intrinsics.
pub(crate) struct Avx2;

impl Avx2 {
    /// Runtime check callers can use before dispatching to this kernel
    /// (AVX2+FMA is not part of the x86-64 baseline).
    pub(crate) fn available() -> bool {
        <Avx2 as RowKernel>::detect()
    }
}

// SAFETY (method bodies): each unsafe block only dispatches to the matching
// #[target_feature(enable = "avx2,fma")] fn below under the trait method's
// documented preconditions (`horiz_x3` as a one-row `horiz_rows_x3::<1>`
// batch); the trait contract's `detect()` check guarantees the features.
impl RowKernel for Avx2 {
    const STAGE3_FLOATS_PER_PIXEL: usize = 4;
    const HORIZ_BATCH: usize = 4;
    fn detect() -> bool {
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma")
    }
    unsafe fn stage_x3(row: &[u16], stage: &mut [f32], w: usize) {
        unsafe { stage_row_x3(row, stage, w) }
    }
    // SAFETY: the VBMI path is taken only after its own runtime check; the
    // fallback is the portable body under the same trait contract.
    unsafe fn stage_x3_u8(row: &[u8], lut: &[f32; 256], stage: &mut [f32], w: usize) {
        unsafe {
            if vbmi_detected() {
                stage_row_x3_u8_vbmi(row, lut, stage, w)
            } else {
                stage_x3_u8_words(row, lut, stage, w, true)
            }
        }
    }
    fn ycc_h2() -> bool {
        vbmi_detected()
    }
    // SAFETY (both): ycc_h2() is the VBMI check, verified by the caller.
    unsafe fn ycc_terms_h2(cb: &[u8], cr: &[u8], w: usize, terms: &mut [u8]) {
        unsafe { ycc_terms_h2_vbmi(cb, cr, w, terms) }
    }
    unsafe fn stage_ycc_h2(y: &[u8], terms: &[u8], lut: &[f32; 256], stage: &mut [f32], w: usize) {
        unsafe { stage_ycc_h2_vbmi(y, terms, lut, stage, w) }
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
        unsafe { Self::horiz_x3_batch(stage, 0, 1, src_w, w, ring, plane, &[slot, 0, 0, 0], dst_w) }
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
    unsafe fn horiz_x3_batch(
        stage: &[f32],
        row_stride: usize,
        n: usize,
        _src_w: usize,
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slots: &[usize; 4],
        dst_w: usize,
    ) {
        unsafe {
            if wide_horiz() {
                return match n {
                    0 => {}
                    4 => horiz_rows_x3_512::<4>(stage, row_stride, w, ring, plane, slots, dst_w),
                    3 => horiz_rows_x3_512::<3>(stage, row_stride, w, ring, plane, slots, dst_w),
                    2 => horiz_rows_x3_512::<2>(stage, row_stride, w, ring, plane, slots, dst_w),
                    _ => horiz_rows_x3_512::<1>(stage, row_stride, w, ring, plane, slots, dst_w),
                };
            }
            // An empty batch is a no-op, as in the default body.
            match n {
                0 => {}
                4 => horiz_rows_x3::<4>(stage, row_stride, w, ring, plane, slots, dst_w),
                3 => horiz_rows_x3::<3>(stage, row_stride, w, ring, plane, slots, dst_w),
                2 => horiz_rows_x3::<2>(stage, row_stride, w, ring, plane, slots, dst_w),
                _ => horiz_rows_x3::<1>(stage, row_stride, w, ring, plane, slots, dst_w),
            }
        }
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
// SAFETY: caller must ensure AVX2+FMA and 8 readable u16s at `p`
// (unaligned load; no alignment requirement).
unsafe fn widen8(p: *const u16) -> std::arch::x86_64::__m256 {
    unsafe {
        use std::arch::x86_64::*;
        _mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(_mm_loadu_si128(p.cast())))
    }
}

/// Stage one u16 RGB row as interleaved f32 RGBX (a zero fourth lane
/// per pixel). Each pixel becomes one f32x4 lane group, which lets the
/// horizontal pass use broadcast-FMA accumulation with no horizontal
/// lane reductions at all — the planar layout's three per-pixel lane
/// sums (hsum) cost more than its FMAs on this shape. Conversion is
/// exact (u16 < 2^24), so the convolution sees the same operand values
/// as widening on the fly.
#[target_feature(enable = "avx2,fma")]
// SAFETY: requires AVX2+FMA, `row.len() >= 3 * w`, `stage.len() >= 4 * w`.
// The vector loop stops at `x + 3 <= w`, so its 16-byte loads end at u16
// index 3x + 8 <= 3w - 1 and its 8-f32 stores at 4x + 8 <= 4w - 4; the
// scalar tail is bounds-checked.
unsafe fn stage_row_x3(row: &[u16], stage: &mut [f32], w: usize) {
    unsafe {
        use std::arch::x86_64::*;
        // Two RGB pixels (12 bytes) -> [r g b 0 | r g b 0] u16 lanes;
        // 0x80 zeroes the pad lanes.
        #[rustfmt::skip]
        let expand = _mm_setr_epi8(
            0, 1, 2, 3, 4, 5, -128, -128,
            6, 7, 8, 9, 10, 11, -128, -128,
        );
        let mut x = 0usize;
        // One 16-byte load covers two pixels plus two spare u16s, so
        // stop while a third pixel guarantees the tail is in bounds.
        while x + 3 <= w {
            let raw = _mm_loadu_si128(row.as_ptr().add(x * 3).cast());
            let rgbx = _mm_shuffle_epi8(raw, expand);
            let f = _mm256_cvtepi32_ps(_mm256_cvtepu16_epi32(rgbx));
            _mm256_storeu_ps(stage.as_mut_ptr().add(x * 4), f);
            x += 2;
        }
        while x < w {
            stage[x * 4] = row[x * 3] as f32;
            stage[x * 4 + 1] = row[x * 3 + 1] as f32;
            stage[x * 4 + 2] = row[x * 3 + 2] as f32;
            stage[x * 4 + 3] = 0.0;
            x += 1;
        }
    }
}

/// AVX-512F (Skylake-SP / Zen 4 and later); std caches the CPUID result.
fn avx512f_detected() -> bool {
    std::arch::is_x86_feature_detected!("avx512f")
}

/// Whether the 3-channel horizontal pass runs on 512-bit vectors: only on
/// Intel. Measured on the 2040 -> 512/683/256 bench: Sapphire Rapids
/// (c7i) gains 5/2/11% and Granite Rapids (c8i) 4/1/12%, while Zen 4,
/// which splits each 512-bit op in two, loses 6-10%. Zen 5 has
/// full-width units but is unmeasured.
fn wide_horiz() -> bool {
    static WIDE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *WIDE.get_or_init(|| {
        // SAFETY: CPUID leaf 0 exists on every x86-64 CPU. (Newer toolchains make
        // `__cpuid` safe; the block keeps the MSRV building.)
        #[allow(unused_unsafe)]
        let v = unsafe { std::arch::x86_64::__cpuid(0) };
        let intel = (v.ebx, v.edx, v.ecx) == (0x756e_6547, 0x4965_6e69, 0x6c65_746e);
        intel && avx512f_detected()
    })
}

/// AVX-512 VBMI (Ice Lake / Zen 4 and later) turns the u8 staging lookup
/// into register shuffles; std caches the CPUID result, so this is one
/// relaxed load per row.
fn vbmi_detected() -> bool {
    std::arch::is_x86_feature_detected!("avx512f")
        && std::arch::is_x86_feature_detected!("avx512bw")
        && std::arch::is_x86_feature_detected!("avx512vbmi")
}

/// Split the 256-entry table into 256-entry byte tables of its values'
/// low and high bytes, 64 entries per zmm (entries 64k..64k + 64 in
/// register k). Exact because every entry is an integer in 0..=65535
/// (the [`RowKernel::stage_x3_u8`] contract).
#[inline]
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
fn byte_tables(
    lut: &[f32; 256],
) -> (
    [std::arch::x86_64::__m512i; 4],
    [std::arch::x86_64::__m512i; 4],
) {
    use std::arch::x86_64::*;
    // Built per row, so in registers: 16 entries -> 16 dwords -> 16 bytes
    // of each half, four such quarters per register.
    let mut lo = [_mm512_setzero_si512(); 4];
    let mut hi = [_mm512_setzero_si512(); 4];
    for k in 0..4 {
        let mut ql = [_mm_setzero_si128(); 4];
        let mut qh = [_mm_setzero_si128(); 4];
        for j in 0..4 {
            // SAFETY: 16 f32s ending at 64k + 16j + 16 <= 256.
            let v = unsafe { _mm512_loadu_ps(lut.as_ptr().add(64 * k + 16 * j)) };
            let d = _mm512_cvtps_epi32(v);
            ql[j] = _mm512_cvtepi32_epi8(d);
            qh[j] = _mm512_cvtepi32_epi8(_mm512_srli_epi32::<8>(d));
        }
        for (t, q) in [(&mut lo[k], ql), (&mut hi[k], qh)] {
            let r = _mm512_castsi128_si512(q[0]);
            let r = _mm512_inserti32x4::<1>(r, q[1]);
            let r = _mm512_inserti32x4::<2>(r, q[2]);
            *t = _mm512_inserti32x4::<3>(r, q[3]);
        }
    }
    (lo, hi)
}

/// 64 byte-lane lookups into a 256-entry byte table held in 4 zmm: each
/// two-table permute resolves the low 7 index bits, the index's top bit
/// (`top`) picks between the two halves.
#[inline]
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
fn lookup_bytes(
    t: &[std::arch::x86_64::__m512i; 4],
    idx: std::arch::x86_64::__m512i,
    top: std::arch::x86_64::__mmask64,
) -> std::arch::x86_64::__m512i {
    use std::arch::x86_64::*;
    let a = _mm512_permutex2var_epi8(t[0], idx, t[1]);
    let b = _mm512_permutex2var_epi8(t[2], idx, t[3]);
    _mm512_mask_blend_epi8(top, a, b)
}

/// Stage one u8 RGB row as f32 RGBX through `lut` with no memory lookups:
/// the table lives in eight zmm as low/high byte halves, sixteen pixels'
/// bytes are spread to RGBX order, looked up as bytes, re-paired into u16
/// dwords, and converted. Bit-identical to the portable body (the table
/// values are exact integers); -63% on staging on Zen 4, where the three
/// scalar table reads per pixel cost as much as the horizontal pass.
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
// SAFETY: requires AVX-512 F/BW/VBMI, `row.len() >= 3 * w` and
// `stage.len() >= 4 * w`. The vector loop runs while `x + 16 <= w`: its
// masked load touches bytes [3x, 3x + 48) <= 3w and its four 16-f32 stores
// cover [4x, 4x + 64) <= 4w. The tail forwards the remaining `w - x` pixels
// to the portable body under the same contract.
unsafe fn stage_row_x3_u8_vbmi(row: &[u8], lut: &[f32; 256], stage: &mut [f32], w: usize) {
    use std::arch::x86_64::*;
    let (tlo, thi) = byte_tables(lut);
    // Byte 4p + c <- source byte 3p + c; the X lanes (c == 3) pick up a
    // neighbor byte and are discarded by `pix` below.
    const EXPAND: [u8; 64] = {
        let mut e = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            e[i] = (3 * (i / 4) + i % 4) as u8;
            i += 1;
        }
        e
    };
    let mut x = 0usize;
    unsafe {
        let expand = _mm512_loadu_si512(EXPAND.as_ptr().cast());
        let pair = pair_indices();
        let out = stage.as_mut_ptr();
        while x + 16 <= w {
            let src =
                _mm512_maskz_loadu_epi8(0x0000_FFFF_FFFF_FFFF, row.as_ptr().add(x * 3).cast());
            let idx = _mm512_permutexvar_epi8(expand, src);
            stage_rgbx16(&tlo, &thi, &pair, idx, out.add(x * 4));
            x += 16;
        }
        stage_x3_u8_words(&row[x * 3..], lut, &mut stage[x * 4..], w - x, true);
    }
}

/// [`stage_rgbx16`]'s pairing indices: output register k, dword i <-
/// [lo[16k + i], hi[16k + i], 0, 0] (indices >= 64 select from the second
/// source, `hi`).
#[inline]
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
fn pair_indices() -> [std::arch::x86_64::__m512i; 4] {
    use std::arch::x86_64::*;
    const PAIR: [[u8; 64]; 4] = {
        let mut t = [[0u8; 64]; 4];
        let mut k = 0;
        while k < 4 {
            let mut i = 0;
            while i < 16 {
                t[k][4 * i] = (16 * k + i) as u8;
                t[k][4 * i + 1] = (64 + 16 * k + i) as u8;
                i += 1;
            }
            k += 1;
        }
        t
    };
    // SAFETY: each load reads one 64-byte table row.
    PAIR.map(|p| unsafe { _mm512_loadu_si512(p.as_ptr().cast()) })
}

/// Stage sixteen pixels given as RGBX table indices (`idx`, X bytes
/// arbitrary) as RGBX f32 through the split table: look up both byte
/// halves, re-pair them into u16 dwords, convert, and store 64 f32 at
/// `out`. The `pix` mask keeps each channel's two bytes and zeroes the
/// rest, including every X dword — its looked-up value is a table entry,
/// not 0.
#[inline]
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
// SAFETY: requires AVX-512 F/BW/VBMI and 64 writable f32 at `out`.
unsafe fn stage_rgbx16(
    tlo: &[std::arch::x86_64::__m512i; 4],
    thi: &[std::arch::x86_64::__m512i; 4],
    pair: &[std::arch::x86_64::__m512i; 4],
    idx: std::arch::x86_64::__m512i,
    out: *mut f32,
) {
    use std::arch::x86_64::*;
    let pix: __mmask64 = 0x0333_0333_0333_0333;
    let top = _mm512_movepi8_mask(idx);
    let lo = lookup_bytes(tlo, idx, top);
    let hi = lookup_bytes(thi, idx, top);
    for (k, p) in pair.iter().enumerate() {
        let d = _mm512_maskz_permutex2var_epi8(pix, lo, *p, hi);
        // SAFETY: store k covers out[16k, 16k + 16) within the 64.
        unsafe { _mm512_storeu_ps(out.add(16 * k), _mm512_cvtepi32_ps(d)) };
    }
}

/// [`RowKernel::ycc_terms_h2`] for 64 pixels per step. libjpeg's
/// 16-bit fixed-point terms, computed exactly in 16-bit lanes:
/// FIX(1.402) = 65536 + 26345 and FIX(1.772) = 2 * 65536 - 14942 leave
/// one rounded product each (the high product plus the low product's
/// rounding bit), and G's two products are summed exactly by madd with
/// -FIX(0.71414) = -65536 + 18734. Each term is stored as saturating
/// byte addends (its positive and negative parts, planes R+, R-, G+, G-,
/// B+, B- of `ycc_stride(w)` bytes each), replicated over each pixel
/// pair, so a luma row costs two saturating byte ops per channel; the
/// saturation is exactly libjpeg's range limit since the two addends
/// never both apply.
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
// SAFETY: requires AVX-512 F/BW/VBMI, `cb` and `cr` at least
// `w.div_ceil(2)` long and `terms.len() >= 6 * ycc_stride(w)`. Block x
// (a multiple of 64 below w) loads chroma [x / 2, x / 2 + 32) masked to
// `w.div_ceil(2)`, and stores bytes [k * s + x, k * s + x + 64), within
// plane k < 6 of `s = ycc_stride(w)` bytes.
unsafe fn ycc_terms_h2_vbmi(cb: &[u8], cr: &[u8], w: usize, terms: &mut [u8]) {
    use std::arch::x86_64::*;
    let s = ycc_stride(w);
    let cw = w.div_ceil(2);
    debug_assert!(cb.len() >= cw && cr.len() >= cw && terms.len() >= 6 * s);
    // Pixel p of a block takes chroma word p / 2's low byte.
    const DUP: [u8; 64] = {
        let mut t = [0u8; 64];
        let mut i = 0;
        while i < 64 {
            t[i] = (2 * (i / 2)) as u8;
            i += 1;
        }
        t
    };
    const HALF: i32 = 1 << 15;
    // madd pairs [cb, cr] with [-FIX(0.34414), 65536 - FIX(0.71414)].
    const KG: i32 = (((65536 - YCC_FIX_GR) as u32) << 16 | (-YCC_FIX_GB) as u16 as u32) as i32;
    unsafe {
        let dup = _mm512_loadu_si512(DUP.as_ptr().cast());
        let c128 = _mm512_set1_epi16(128);
        let zero = _mm512_setzero_si512();
        let kg = _mm512_set1_epi32(KG);
        let half = _mm512_set1_epi32(HALF);
        // (a * k + 32768) >> 16.
        let mulround = |a: __m512i, k: i32| {
            let k = _mm512_set1_epi16(k as i16);
            let hi = _mm512_mulhi_epi16(a, k);
            _mm512_add_epi16(hi, _mm512_srli_epi16::<15>(_mm512_mullo_epi16(a, k)))
        };
        let widen = |c: &[u8], at: usize, m: __mmask64| {
            let v = _mm512_maskz_loadu_epi8(m, c.as_ptr().add(at).cast());
            _mm512_sub_epi16(_mm512_cvtepu8_epi16(_mm512_castsi512_si256(v)), c128)
        };
        let out = terms.as_mut_ptr();
        let mut x = 0;
        while x < w {
            let c = x / 2;
            let n = (cw - c).min(32);
            let m = (u64::MAX >> (64 - n)) as __mmask64;
            let (cb, cr) = (widen(cb, c, m), widen(cr, c, m));
            let r = _mm512_add_epi16(cr, mulround(cr, YCC_FIX_R - 65536));
            let b = _mm512_add_epi16(_mm512_add_epi16(cb, cb), mulround(cb, YCC_FIX_B - 131072));
            let gw = |v: __m512i| {
                _mm512_srai_epi32::<16>(_mm512_add_epi32(_mm512_madd_epi16(v, kg), half))
            };
            let g = _mm512_packs_epi32(
                gw(_mm512_unpacklo_epi16(cb, cr)),
                gw(_mm512_unpackhi_epi16(cb, cr)),
            );
            let g = _mm512_sub_epi16(g, cr);
            for (k, t) in [r, g, b].into_iter().enumerate() {
                let pos = _mm512_max_epi16(t, zero);
                let neg = _mm512_max_epi16(_mm512_sub_epi16(zero, t), zero);
                let at = out.add(2 * k * s + x);
                _mm512_storeu_si512(at.cast(), _mm512_permutexvar_epi8(dup, pos));
                _mm512_storeu_si512(at.add(s).cast(), _mm512_permutexvar_epi8(dup, neg));
            }
            x += 64;
        }
    }
}

/// [`RowKernel::stage_ycc_h2`]: per 64 pixels, R, G and B bytes from the
/// luma bytes and the chroma row's saturating addends, interleaved into
/// RGBX index order, then [`stage_rgbx16`] per sixteen pixels.
#[target_feature(enable = "avx512f,avx512bw,avx512vbmi")]
// SAFETY: requires AVX-512 F/BW/VBMI, `y.len() >= w`, `terms` as written by
// `ycc_terms_h2_vbmi` for `w` and `stage.len() >= 4 * w`. Full blocks load
// luma [x, x + 64) <= w and store stage [4x, 4x + 256) <= 4w; the last
// partial block loads luma masked to w and stages into a local buffer,
// of which the 4 (w - x) real f32 are copied out. Term loads stay within
// their plane (every block x < w lies below `ycc_stride(w)`).
unsafe fn stage_ycc_h2_vbmi(y: &[u8], terms: &[u8], lut: &[f32; 256], stage: &mut [f32], w: usize) {
    use std::arch::x86_64::*;
    let s = ycc_stride(w);
    debug_assert!(y.len() >= w && terms.len() >= 6 * s && stage.len() >= 4 * w);
    let (tlo, thi) = byte_tables(lut);
    // Group h, byte 4p + c <- channel c of block pixel 16h + p: R and G
    // from one two-source permute (G at indices >= 64), then B's lanes
    // (mask `kb`) from a second.
    const SEL: [[[u8; 64]; 2]; 4] = {
        let mut t = [[[0u8; 64]; 2]; 4];
        let mut h = 0;
        while h < 4 {
            let mut p = 0;
            while p < 16 {
                let q = (16 * h + p) as u8;
                t[h][0][4 * p] = q;
                t[h][0][4 * p + 1] = 64 + q;
                t[h][1][4 * p + 2] = q;
                p += 1;
            }
            h += 1;
        }
        t
    };
    let kb: __mmask64 = 0x4444_4444_4444_4444;
    unsafe {
        let sel = SEL.map(|g| g.map(|t| _mm512_loadu_si512(t.as_ptr().cast())));
        let pair = pair_indices();
        let t = terms.as_ptr();
        let block = |yv: __m512i, x: usize, out: *mut f32| {
            let ch = |k: usize| {
                let pos = _mm512_loadu_si512(t.add(2 * k * s + x).cast());
                let neg = _mm512_loadu_si512(t.add((2 * k + 1) * s + x).cast());
                _mm512_subs_epu8(_mm512_adds_epu8(yv, pos), neg)
            };
            let (r, g, b) = (ch(0), ch(1), ch(2));
            for (h, sel) in sel.iter().enumerate() {
                let rg = _mm512_permutex2var_epi8(r, sel[0], g);
                let idx = _mm512_mask_permutexvar_epi8(rg, kb, sel[1], b);
                stage_rgbx16(&tlo, &thi, &pair, idx, out.add(64 * h));
            }
        };
        let mut x = 0;
        while x + 64 <= w {
            block(
                _mm512_loadu_si512(y.as_ptr().add(x).cast()),
                x,
                stage.as_mut_ptr().add(4 * x),
            );
            x += 64;
        }
        if x < w {
            let n = w - x;
            let yv = _mm512_maskz_loadu_epi8(u64::MAX >> (64 - n), y.as_ptr().add(x).cast());
            let mut tmp = [0f32; 256];
            block(yv, x, tmp.as_mut_ptr());
            stage[4 * x..4 * w].copy_from_slice(&tmp[..4 * n]);
        }
    }
}

/// Convert one u16 RGBA row to f32, keeping the interleaved layout (a
/// pixel stays one f32x4 lane group).
#[target_feature(enable = "avx2,fma")]
// SAFETY: requires AVX2+FMA and `stage.len() >= row.len()`; the vector loop
// reads and writes [i, i + 8) only while `i + 8 <= row.len()`.
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

/// `N` horizontal rows, 3 channels, over RGBX staged rows (row `r` at
/// `stage[r * row_stride..]`): identical per-row accumulation to the
/// 4-channel path (one 256-bit load covers two pixels; each FMA
/// applies a lanewise coefficient-pair broadcast; two accumulator
/// streams cover four zero-padded taps per iteration), with each
/// window's coefficient loads and broadcasts shared across all `N`
/// rows. Per-row math is unchanged, so batching changes no value.
/// Outputs go to the ring four pixels at a time, transposed to one vector
/// store per channel plane: on Zen 4 that is -7% of the cycles of a
/// 2040x1356 -> 1024x681 u16 resize and -5% to 512x340, bit-identical.
#[target_feature(enable = "avx2,fma")]
// SAFETY: requires AVX2+FMA. Stage loads read pixels [start, start + padded)
// of each row r < N; `start + sizes <= src_w` and `padded <= w.stride`
// (Windows invariants), so each `stage[r * row_stride..]` row must hold
// (src_w + w.stride) * 4 readable f32. Coefficient loads stay inside the
// checked `padded`-long subslice; ring writes are bounds-checked.
unsafe fn horiz_rows_x3<const N: usize>(
    stage: &[f32],
    row_stride: usize,
    w: &Windows,
    ring: &mut [f32],
    plane: usize,
    slots: &[usize; 4],
    dst_w: usize,
) {
    unsafe {
        use std::arch::x86_64::*;
        let idx01 = _mm256_setr_epi32(0, 0, 0, 0, 1, 1, 1, 1);
        let idx23 = _mm256_setr_epi32(2, 2, 2, 2, 3, 3, 3, 3);
        // Output pixel `ox` of every row: its RGBX sums as one f32x4.
        let pixel = |ox: usize| -> [__m128; N] {
            let start = w.starts[ox];
            // Whole 4-tap blocks over the zero-padded coefficients.
            let padded = w.sizes[ox].div_ceil(4) * 4;
            let coeffs = &w.coeffs[ox * w.stride..ox * w.stride + padded];

            let mut acc_a = [_mm256_setzero_ps(); N];
            let mut acc_b = [_mm256_setzero_ps(); N];
            let mut k = 0usize;
            while k < padded {
                let c4 = _mm_loadu_ps(coeffs.as_ptr().add(k));
                let cv = _mm256_set_m128(c4, c4);
                let ca = _mm256_permutevar8x32_ps(cv, idx01);
                let cb = _mm256_permutevar8x32_ps(cv, idx23);
                for r in 0..N {
                    let base = stage.as_ptr().add(r * row_stride + (start + k) * 4);
                    acc_a[r] = _mm256_fmadd_ps(_mm256_loadu_ps(base), ca, acc_a[r]);
                    acc_b[r] = _mm256_fmadd_ps(_mm256_loadu_ps(base.add(8)), cb, acc_b[r]);
                }
                k += 4;
            }
            std::array::from_fn(|r| {
                _mm_add_ps(
                    _mm_add_ps(
                        _mm256_castps256_ps128(acc_a[r]),
                        _mm256_extractf128_ps::<1>(acc_a[r]),
                    ),
                    _mm_add_ps(
                        _mm256_castps256_ps128(acc_b[r]),
                        _mm256_extractf128_ps::<1>(acc_b[r]),
                    ),
                )
            })
        };
        // Four output pixels at a time, transposed from pixel-major RGBX to
        // one f32x4 per channel, so each ring plane takes a single vector
        // store instead of four scalar ones.
        let mut ox = 0;
        while ox + 4 <= dst_w {
            let p = [pixel(ox), pixel(ox + 1), pixel(ox + 2), pixel(ox + 3)];
            for r in 0..N {
                let at = slots[r] + ox;
                assert!(at + 4 <= plane && 2 * plane + at + 4 <= ring.len());
                let lo01 = _mm_unpacklo_ps(p[0][r], p[1][r]); // r0 r1 g0 g1
                let lo23 = _mm_unpacklo_ps(p[2][r], p[3][r]); // r2 r3 g2 g3
                let hi01 = _mm_unpackhi_ps(p[0][r], p[1][r]); // b0 b1 x0 x1
                let hi23 = _mm_unpackhi_ps(p[2][r], p[3][r]); // b2 b3 x2 x3
                let dst = ring.as_mut_ptr().add(at);
                _mm_storeu_ps(dst, _mm_movelh_ps(lo01, lo23));
                _mm_storeu_ps(dst.add(plane), _mm_movehl_ps(lo23, lo01));
                _mm_storeu_ps(dst.add(2 * plane), _mm_movelh_ps(hi01, hi23));
            }
            ox += 4;
        }
        while ox < dst_w {
            let p = pixel(ox);
            for r in 0..N {
                let mut out = [0f32; 4];
                _mm_storeu_ps(out.as_mut_ptr(), p[r]);
                ring[slots[r] + ox] = out[0];
                ring[plane + slots[r] + ox] = out[1];
                ring[2 * plane + slots[r] + ox] = out[2];
            }
            ox += 1;
        }
    }
}

/// [`horiz_rows_x3`] on 512-bit vectors: one load covers the four pixels
/// of a tap block and one FMA applies all four taps, where AVX2 splits
/// them across two accumulators (taps 0-1, taps 2-3). Every lane sees the
/// same FMA sequence as there, and the final reduction is the same
/// `(tap0 + tap1) + (tap2 + tap3)`, so the output is bit-identical.
#[target_feature(enable = "avx512f")]
// SAFETY: requires AVX-512F. Loads cover the same pixels
// [start, start + padded) of each row r < N as `horiz_rows_x3` (one 16-f32
// load instead of two 8-f32 loads), under the same row contract.
unsafe fn horiz_rows_x3_512<const N: usize>(
    stage: &[f32],
    row_stride: usize,
    w: &Windows,
    ring: &mut [f32],
    plane: usize,
    slots: &[usize; 4],
    dst_w: usize,
) {
    unsafe {
        use std::arch::x86_64::*;
        let idx = _mm512_setr_epi32(0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3);
        for ox in 0..dst_w {
            let start = w.starts[ox];
            let padded = w.sizes[ox].div_ceil(4) * 4;
            let coeffs = &w.coeffs[ox * w.stride..ox * w.stride + padded];

            let mut acc = [_mm512_setzero_ps(); N];
            let mut k = 0usize;
            while k < padded {
                let c4 = _mm512_castps128_ps512(_mm_loadu_ps(coeffs.as_ptr().add(k)));
                let c = _mm512_permutexvar_ps(idx, c4);
                for (r, a) in acc.iter_mut().enumerate() {
                    let base = stage.as_ptr().add(r * row_stride + (start + k) * 4);
                    *a = _mm512_fmadd_ps(_mm512_loadu_ps(base), c, *a);
                }
                k += 4;
            }
            for r in 0..N {
                let g0 = _mm512_castps512_ps128(acc[r]);
                let g1 = _mm512_extractf32x4_ps::<1>(acc[r]);
                let g2 = _mm512_extractf32x4_ps::<2>(acc[r]);
                let g3 = _mm512_extractf32x4_ps::<3>(acc[r]);
                let s = _mm_add_ps(_mm_add_ps(g0, g1), _mm_add_ps(g2, g3));
                let mut out = [0f32; 4];
                _mm_storeu_ps(out.as_mut_ptr(), s);
                ring[slots[r] + ox] = out[0];
                ring[plane + slots[r] + ox] = out[1];
                ring[2 * plane + slots[r] + ox] = out[2];
            }
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
// SAFETY: requires AVX2+FMA. Stage loads read pixels [start, start + padded);
// `start + sizes <= src_w` and `padded <= w.stride` (Windows invariants), so
// `stage` must hold (src_w + w.stride) * 4 readable f32 for `w`'s source
// width. Coefficient loads stay inside the checked `padded`-long subslice;
// ring writes are bounds-checked.
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
            // Whole 4-tap blocks over the zero-padded coefficients.
            let padded = w.sizes[ox].div_ceil(4) * 4;
            let coeffs = &w.coeffs[ox * w.stride..ox * w.stride + padded];

            let mut acc_a = _mm256_setzero_ps();
            let mut acc_b = _mm256_setzero_ps();
            let mut k = 0usize;
            while k < padded {
                let c4 = _mm_loadu_ps(coeffs.as_ptr().add(k));
                let cv = _mm256_set_m128(c4, c4);
                let p01 = _mm256_loadu_ps(stage.as_ptr().add((start + k) * 4));
                let p23 = _mm256_loadu_ps(stage.as_ptr().add((start + k + 2) * 4));
                acc_a = _mm256_fmadd_ps(p01, _mm256_permutevar8x32_ps(cv, idx01), acc_a);
                acc_b = _mm256_fmadd_ps(p23, _mm256_permutevar8x32_ps(cv, idx23), acc_b);
                k += 4;
            }
            let acc = _mm_add_ps(
                _mm_add_ps(
                    _mm256_castps256_ps128(acc_a),
                    _mm256_extractf128_ps::<1>(acc_a),
                ),
                _mm_add_ps(
                    _mm256_castps256_ps128(acc_b),
                    _mm256_extractf128_ps::<1>(acc_b),
                ),
            );
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
// SAFETY: requires AVX2+FMA, `acc.len() >= dst_w`, and
// `off + dst_w <= plane.len()` for every `off` in `offs`: tile loads read
// `plane[off + x .. off + x + 32]` with `x + 32 <= dst_w` (narrower in the
// tails), and `acc` stores end at or below `dst_w`.
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
// SAFETY: caller must ensure AVX2+FMA and 8 readable f32s at `p`
// (unaligned load; no alignment requirement).
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
// SAFETY: requires AVX2+FMA and `acc.len() >= 3 * dst_w`: each `split_at`
// plane then holds `dst_w` f32s, keeping the `narrow8` reads at
// `x + 8 <= dst_w` in bounds. `out` writes are bounds-checked.
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
// SAFETY: requires AVX2+FMA and `acc.len() >= 4 * dst_w`: `narrow8` reads
// `acc[c * dst_w + x ..][..8]` with `x + 8 <= dst_w` and `c <= 3`. `out`
// writes are bounds-checked.
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
    fn u8_staging_matches_u16() {
        if !detected() {
            return;
        }
        testkit::assert_u8_staging_matches_u16::<Avx2>();
    }

    /// The VBMI staging against the portable body, bit for bit: a table
    /// of arbitrary u16 values (every high and low byte pattern in play),
    /// every code in every channel position, and widths around the
    /// 16-pixel step. Skips on hosts without VBMI, which includes most
    /// CI x86 runners.
    #[test]
    fn vbmi_u8_staging_matches_portable() {
        if !vbmi_detected() {
            eprintln!("skipping: host lacks avx512f+bw+vbmi");
            return;
        }
        let mut seed = 0x9e37_79b9u32;
        let lut: [f32; 256] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as f32
        });
        for w in (0..=50).chain([255, 256, 257, 2040]) {
            let row: Vec<u8> = (0..3 * w).map(|i| (i * 7 + w) as u8).collect();
            let mut want = vec![1.0f32; 4 * w];
            let mut got = vec![2.0f32; 4 * w];
            // SAFETY: vbmi_detected() checked; row is 3w bytes, stages 4w f32.
            unsafe {
                stage_x3_u8_words(&row, &lut, &mut want, w, true);
                stage_row_x3_u8_vbmi(&row, &lut, &mut got, w);
            }
            assert!(
                want.iter()
                    .zip(&got)
                    .all(|(a, b)| a.to_bits() == b.to_bits()),
                "width {w}"
            );
        }
    }

    #[test]
    fn horiz_batch_matches_single_rows() {
        if !detected() {
            return;
        }
        testkit::assert_horiz_batch_matches_single::<Avx2>();
    }

    /// Staged RGBX rows of xorshift u16 values with the kernel's slack.
    fn rgbx_rows(sw: usize, rows: usize, w: &Windows) -> (Vec<f32>, usize) {
        let rs = (sw + w.stride) * 4;
        let mut x = 0x9e37_79b9_7f4a_7c15u64;
        let mut stage = vec![0f32; rows * rs];
        for y in 0..rows {
            for i in 0..sw {
                for c in 0..3 {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    stage[y * rs + i * 4 + c] = (x % 65536) as f32;
                }
            }
        }
        (stage, rs)
    }

    #[test]
    fn horiz_512_matches_avx2_bit_for_bit() {
        if !detected() || !avx512f_detected() {
            eprintln!("skipping: host lacks avx512f");
            return;
        }
        for (sw, dw) in [
            (2040usize, 512usize),
            (2040, 683),
            (333, 100),
            (50, 120),
            (17, 5),
            (7, 4),
            (1, 1),
        ] {
            let w = Windows::new(sw, dw);
            let (stage, rs) = rgbx_rows(sw, 4, &w);
            let slots: [usize; 4] = std::array::from_fn(|i| i * dw);
            let plane = 4 * dw;
            let (mut a, mut b) = (vec![0f32; 3 * plane], vec![0f32; 3 * plane]);
            // SAFETY: AVX2+FMA and AVX-512F checked above; four rows staged
            // with the kernel's slack.
            unsafe {
                horiz_rows_x3::<4>(&stage, rs, &w, &mut a, plane, &slots, dw);
                horiz_rows_x3_512::<4>(&stage, rs, &w, &mut b, plane, &slots, dw);
            }
            let (ab, bb): (Vec<u32>, Vec<u32>) = (
                a.iter().map(|v| v.to_bits()).collect(),
                b.iter().map(|v| v.to_bits()).collect(),
            );
            assert_eq!(ab, bb, "{sw}->{dw}");
        }
    }

    #[test]
    #[ignore]
    fn horiz_512_bench() {
        if !detected() || !avx512f_detected() {
            return;
        }
        for (sw, dw) in [(2040usize, 512usize), (2040, 683), (2040, 256)] {
            let rows = 1356;
            let w = Windows::new(sw, dw);
            let (stage, rs) = rgbx_rows(sw, rows, &w);
            let slots: [usize; 4] = std::array::from_fn(|i| i * dw);
            let plane = 4 * dw;
            let mut ring = vec![0f32; 3 * plane];
            for wide in [false, true] {
                let mut best = f64::MAX;
                for _ in 0..15 {
                    let t = std::time::Instant::now();
                    for y in (0..rows).step_by(4) {
                        let s = std::hint::black_box(&stage[y * rs..]);
                        // SAFETY: features checked above; rows y..y + 4 staged.
                        unsafe {
                            if wide {
                                horiz_rows_x3_512::<4>(s, rs, &w, &mut ring, plane, &slots, dw)
                            } else {
                                horiz_rows_x3::<4>(s, rs, &w, &mut ring, plane, &slots, dw)
                            }
                        }
                    }
                    best = best.min(t.elapsed().as_secs_f64() * 1e3);
                }
                println!(
                    "{sw}->{dw} {}: {best:.3} ms",
                    if wide { "avx512" } else { "avx2" }
                );
            }
        }
    }

    #[test]
    #[ignore]
    fn horiz_bench() {
        if !detected() {
            return;
        }
        testkit::bench_horiz::<Avx2>();
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
        // SAFETY (both casts): u16 arrays viewed as bytes — same allocation,
        // `len * 2` bytes, u8 has alignment 1 and no invalid bit patterns; `dst` is
        // not touched while `dst_bytes` is live.
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let mut dst = [0u16; 12];
        let dst_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
        assert!(resize_u16_avx2(src_bytes, 2, 2, dst_bytes, 0, 1, 3).is_err());
        assert!(resize_u16_avx2(src_bytes, 0, 2, dst_bytes, 1, 1, 3).is_err());
    }

    /// The fused YCbCr staging against libjpeg's replicating conversion
    /// (`ycc_to_rgb`) plus the portable RGB staging, bit for bit: every
    /// (Cb, Cr) pair, luma codes around both clamps, and widths around
    /// the 64-pixel block including odd ones.
    #[test]
    fn vbmi_ycc_staging_matches_rgb() {
        if !vbmi_detected() {
            eprintln!("skipping: host lacks avx512f+bw+vbmi");
            return;
        }
        let mut seed = 0x9e37_79b9u32;
        let lut: [f32; 256] = std::array::from_fn(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (seed >> 16) as f32
        });
        let check = |y: &[u8], cb: &[u8], cr: &[u8], w: usize| {
            let rgb: Vec<u8> = (0..w)
                .flat_map(|x| crate::resize_kernel::ycc_to_rgb(y[x], cb[x / 2], cr[x / 2]))
                .collect();
            let mut want = vec![1.0f32; 4 * w];
            let mut got = vec![2.0f32; 4 * w];
            let mut terms = vec![0u8; 6 * ycc_stride(w)];
            // SAFETY: vbmi_detected() checked; lengths as required.
            unsafe {
                stage_x3_u8_words(&rgb, &lut, &mut want, w, true);
                ycc_terms_h2_vbmi(cb, cr, w, &mut terms);
                stage_ycc_h2_vbmi(y, &terms, &lut, &mut got, w);
            }
            for (i, (a, b)) in want.iter().zip(&got).enumerate() {
                if i % 4 != 3 {
                    assert_eq!(a.to_bits(), b.to_bits(), "width {w} float {i}");
                }
            }
        };
        let cb: Vec<u8> = (0..=255).collect();
        for c in 0..=255u8 {
            let y: Vec<u8> = (0..512).map(|x| (x * 37 + c as usize * 11) as u8).collect();
            check(&y, &cb, &[c; 256], 512);
        }
        for w in (1..=130usize).chain([255, 257, 2039, 2040]) {
            let y: Vec<u8> = (0..w).map(|x| (x * 13 + w) as u8).collect();
            let cb: Vec<u8> = (0..w.div_ceil(2)).map(|x| (x * 29 + 3) as u8).collect();
            let cr: Vec<u8> = (0..w.div_ceil(2)).map(|x| (x * 53 + 7) as u8).collect();
            check(&y, &cb, &cr, w);
        }
    }
}
