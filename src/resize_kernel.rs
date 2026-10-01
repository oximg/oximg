//! Arch-neutral core of the u16 SIMD resize kernels: Lanczos3 window
//! computation (fir-identical math), the strip-mined separable-convolution
//! driver, and a row-push streaming API that lets a decoder feed source
//! rows as they arrive while output rows are emitted as soon as their
//! vertical windows complete.
//!
//! The SIMD row stages live in per-arch modules (`resize_neon` on
//! aarch64) implementing [`RowKernel`]; the driver here is shared, so
//! the schedule invariants and their tests are written once.
//!
//! Correctness contract (inherited from the NEON kernel): the f32
//! operation sequence per output value is independent of scheduling —
//! staging converts each source sample exactly once (exact u16 -> f32),
//! ring placement only changes where a row is stored, and vertical
//! accumulation applies taps in ascending order. Streamed emission
//! performs the same operations in the same per-value order as the
//! full-frame driver, so their outputs are bit-identical (asserted by
//! tests per arch). The one deliberate exception is a u8 stream on a
//! kernel with [`RowKernel::half_u8`]: its horizontal pass reads f16
//! samples and taps (f32 accumulation), within one 8-bit level of the
//! f32 result; the scheduling guarantees above still hold within it.

use anyhow::{Result, ensure};
use std::sync::Arc;

pub(crate) fn lanczos3(x: f64) -> f64 {
    fn sinc(x: f64) -> f64 {
        if x == 0.0 {
            1.0
        } else {
            let x = x * std::f64::consts::PI;
            x.sin() / x
        }
    }
    if (-3.0..3.0).contains(&x) {
        sinc(x) * sinc(x / 3.0)
    } else {
        0.0
    }
}

/// Per-axis convolution windows, identical math to fir's
/// `precompute_coefficients` with no crop box.
pub(crate) struct Windows {
    pub(crate) window_size: usize,
    /// Coefficient row stride: `window_size` rounded up to a multiple
    /// of 8, so horizontal kernels can run whole SIMD tap-blocks over
    /// the zero padding instead of a scalar tail. A zero coefficient
    /// times any finite staged value contributes exactly +0.0, so the
    /// padded blocks change no output value (staged data is converted
    /// from u16 and therefore always finite).
    pub(crate) stride: usize,
    /// First source index of each output pixel's window.
    pub(crate) starts: Vec<usize>,
    /// Tap count of each window.
    pub(crate) sizes: Vec<usize>,
    /// f32 coefficients, `stride` apart per output pixel, zero-padded
    /// past each window's size.
    pub(crate) coeffs: Vec<f32>,
    /// `coeffs` as IEEE half-precision bits, built on first use by the
    /// half-precision horizontal pass (see [`Windows::coeffs_f16`]).
    // Only the NEON kernel runs that pass.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    coeffs_h: std::sync::OnceLock<Vec<u16>>,
}

impl Windows {
    pub(crate) fn new(in_size: usize, out_size: usize) -> Windows {
        let scale = in_size as f64 / out_size as f64;
        let filter_scale = scale.max(1.0);
        let filter_radius = 3.0 * filter_scale;
        let window_size = filter_radius.ceil() as usize * 2 + 1;
        let stride = window_size.next_multiple_of(8);
        let recip = 1.0 / filter_scale;

        let mut starts = Vec::with_capacity(out_size);
        let mut sizes = Vec::with_capacity(out_size);
        let mut coeffs = vec![0f32; stride * out_size];
        let mut window = vec![0f64; window_size];

        for out_x in 0..out_size {
            let in_center = (out_x as f64 + 0.5) * scale;
            let x_min = (in_center - filter_radius).floor().max(0.0) as usize;
            let x_max = ((in_center + filter_radius).ceil() as usize).min(in_size);
            let center = in_center - 0.5;

            let mut ww = 0.0;
            let mut n = 0usize;
            let mut lead_trim = 0usize;
            for x in x_min..x_max {
                let w = lanczos3((x as f64 - center) * recip);
                if n == 0 && w == 0.0 {
                    lead_trim += 1; // trim leading zero taps
                } else {
                    window[n] = w;
                    ww += w;
                    n += 1;
                }
            }
            let x_min = x_min + lead_trim;
            while n > 1 && window[n - 1] == 0.0 {
                n -= 1; // trim trailing zero taps
            }
            let dst = &mut coeffs[out_x * stride..(out_x + 1) * stride];
            if ww != 0.0 {
                for (d, w) in dst.iter_mut().zip(&window[..n]) {
                    *d = (*w / ww) as f32;
                }
            }
            starts.push(x_min);
            sizes.push(n);
        }
        Windows {
            window_size,
            stride,
            starts,
            sizes,
            coeffs,
            coeffs_h: std::sync::OnceLock::new(),
        }
    }

    /// The coefficients rounded to f16, same layout as `coeffs`. Plain
    /// rounding lets a window's sum (the DC gain) drift by up to ~1e-4,
    /// several u16 steps on a flat bright area, so each window is
    /// rounded largest tap first, carrying the error so far into the
    /// next: what is left over is under half an ulp of the smallest tap.
    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    pub(crate) fn coeffs_f16(&self) -> &[u16] {
        self.coeffs_h.get_or_init(|| {
            let mut out = vec![0u16; self.coeffs.len()];
            let mut order = Vec::with_capacity(self.window_size);
            for (o, &n) in self.sizes.iter().enumerate() {
                let (want, taps) = (
                    &self.coeffs[o * self.stride..][..n],
                    &mut out[o * self.stride..][..n],
                );
                order.clear();
                order.extend(0..n);
                order.sort_by(|&a, &b| want[b].abs().total_cmp(&want[a].abs()));
                let mut carry = 0f64;
                for &k in &order {
                    let v = want[k] as f64 + carry;
                    taps[k] = f32_to_f16(v as f32);
                    carry = v - f16_to_f32(taps[k]) as f64;
                }
            }
            out
        })
    }
}

/// f32 -> IEEE binary16 bits, round to nearest even (what the NEON
/// `FCVTN` conversion does), with overflow to infinity. Portable so the
/// coefficient tables need no FP16 hardware to build.
pub(crate) fn f32_to_f16(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let exp = ((b >> 23) & 0xff) as i32;
    let man = b & 0x7f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if man != 0 { 0x200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    // Normal results keep 10 of the 23 fraction bits; subnormal ones
    // (e <= 0) are the 24-bit significand shifted down to units of 2^-24.
    let (q, shift) = if e > 0 {
        (((e as u32) << 10) | (man >> 13), 13)
    } else if e < -10 {
        return sign;
    } else {
        let shift = (14 - e) as u32;
        ((man | 0x80_0000) >> shift, shift)
    };
    let full = if e > 0 { man } else { man | 0x80_0000 };
    let rem = full & ((1 << shift) - 1);
    let half = 1 << (shift - 1);
    // A carry out of the fraction bumps the exponent, which is the
    // correctly rounded result (up to infinity).
    let q = if rem > half || (rem == half && q & 1 == 1) {
        q + 1
    } else {
        q
    };
    sign | q as u16
}

/// IEEE binary16 bits -> f32 (exact).
#[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
pub(crate) fn f16_to_f32(h: u16) -> f32 {
    let sign = ((h & 0x8000) as u32) << 16;
    let exp = ((h >> 10) & 0x1f) as u32;
    let man = (h & 0x3ff) as u32;
    let bits = match exp {
        0 if man == 0 => sign,
        0 => {
            // Subnormal: man * 2^-24, renormalized.
            let lz = man.leading_zeros() - 21;
            sign | ((113 - lz) << 23) | ((man << lz) & 0x3ff) << 13
        }
        0x1f => sign | 0x7f80_0000 | man << 13,
        _ => sign | (exp + 112) << 23 | man << 13,
    };
    f32::from_bits(bits)
}

thread_local! {
    /// Windows are pure functions of (in_size, out_size); servers hit a
    /// handful of shapes over and over, and recomputing one costs ~20K
    /// f64 sin() calls. Bounded: reset when it grows past 64 shapes.
    static WINDOWS: std::cell::RefCell<std::collections::HashMap<(usize, usize), Arc<Windows>>> =
        std::cell::RefCell::new(std::collections::HashMap::new());
    /// Reusable work buffers, pooled per thread so both the full-frame
    /// path and per-request streaming resizers avoid reallocation.
    static SCRATCH_POOL: std::cell::RefCell<Vec<Scratch>> = const { std::cell::RefCell::new(Vec::new()) };
}

pub(crate) fn cached_windows(in_size: usize, out_size: usize) -> Arc<Windows> {
    WINDOWS.with(|w| {
        let mut w = w.borrow_mut();
        if w.len() > 64 {
            w.clear();
        }
        w.entry((in_size, out_size))
            .or_insert_with(|| Arc::new(Windows::new(in_size, out_size)))
            .clone()
    })
}

/// Work buffers: one staged source row (f32), the ring of
/// horizontally-convolved rows, one accumulator row set, the ring slot
/// offsets of the current vertical window, and one emitted output row.
/// Grow-only; every element is written before it is read, so stale
/// contents are never observed.
#[derive(Default)]
struct Scratch {
    stage: Lines,
    ring: Lines,
    acc: Vec<f32>,
    offs: Vec<usize>,
    outrow: Vec<u16>,
    /// The current chroma row's terms for [`RowKernel::stage_ycc_h2`].
    ycc: Vec<u8>,
}

/// The f32 batch buffer viewed as u16 for f16 staging.
fn as_u16(v: &[f32]) -> &[u16] {
    // SAFETY: same allocation, `len * 2` u16; f32 alignment exceeds u16's
    // and every bit pattern is a valid u16.
    unsafe { std::slice::from_raw_parts(v.as_ptr().cast(), v.len() * 2) }
}

fn as_u16_mut(v: &mut [f32]) -> &mut [u16] {
    // SAFETY: as in `as_u16`, with the unique borrow carried over.
    unsafe { std::slice::from_raw_parts_mut(v.as_mut_ptr().cast(), v.len() * 2) }
}

/// f32 storage aligned to 64-byte cache lines. Ring rows and staged rows
/// start on a line boundary (`ring_stride` and `stage_row_stride` are
/// whole lines), so the vertical pass's vector loads never straddle two
/// lines, and the horizontal pass's straddle only as its window starts
/// dictate rather than as the allocator happened to place the buffer.
/// On Zen 4 that is -8% on a 2040x1356 -> 512x340 u16 resize and -3% of
/// the server's cycles per DIV2K request at fit 512 (-4% at 1024), with
/// identical output; neutral on Apple M2.
#[derive(Default)]
struct Lines(Vec<Line>);

#[derive(Clone, Copy)]
#[repr(C, align(64))]
struct Line([f32; LINE_F32]);

const LINE_F32: usize = 16;

impl Lines {
    fn grow(&mut self, len: usize) {
        let lines = len.div_ceil(LINE_F32);
        if self.0.len() < lines {
            self.0.resize(lines, Line([0.0; LINE_F32]));
        }
    }
}

impl std::ops::Deref for Lines {
    type Target = [f32];
    fn deref(&self) -> &[f32] {
        // SAFETY: `Line` is `repr(C)` over `LINE_F32` f32s with no padding
        // (64 bytes, its alignment), so the vector is that many contiguous
        // initialized f32 per element.
        unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast(), self.0.len() * LINE_F32) }
    }
}

impl std::ops::DerefMut for Lines {
    fn deref_mut(&mut self) -> &mut [f32] {
        // SAFETY: as in `deref`, with the unique borrow carried over.
        unsafe {
            std::slice::from_raw_parts_mut(self.0.as_mut_ptr().cast(), self.0.len() * LINE_F32)
        }
    }
}

fn grow(buf: &mut Vec<f32>, len: usize) {
    if buf.len() < len {
        buf.resize(len, 0.0);
    }
}

/// One architecture's SIMD row stages. All methods are `unsafe` because
/// implementations are `#[target_feature]` functions; construction of a
/// [`StreamResize`] checks [`RowKernel::detect`] once, which makes the
/// internal calls sound.
pub(crate) trait RowKernel {
    /// f32s per pixel in the 3-channel staged layout: 3 for planar
    /// (NEON), 4 for interleaved RGBX (AVX2, which pays one zero lane
    /// to keep each pixel a broadcast-FMA lane group and skip the
    /// horizontal lane reductions entirely).
    const STAGE3_FLOATS_PER_PIXEL: usize = 3;
    /// Runtime CPU feature check for this kernel.
    fn detect() -> bool;
    /// Stage one u16 RGB row as f32 in this kernel's 3-channel layout.
    // SAFETY: caller must have verified `Self::detect()` and pass
    // `row.len() >= 3 * w` and `stage.len() >= w * Self::STAGE3_FLOATS_PER_PIXEL`.
    unsafe fn stage_x3(row: &[u16], stage: &mut [f32], w: usize);
    /// Stage one u8 RGB row as f32 through a 256-entry lookup table
    /// (fusing e.g. the sRGB -> linear transfer into staging, so no
    /// separate full-image pass or u16 intermediate is needed). The
    /// table holds exact f32 images of the u16 LUT values, making this
    /// bit-identical to `lut[v] as u16` followed by [`RowKernel::stage_x3`].
    // SAFETY: caller contract as for `stage_x3` (`Self::detect()` verified,
    // `row.len() >= 3 * w`, `stage.len() >= w * Self::STAGE3_FLOATS_PER_PIXEL`).
    // The default body forwards exactly that contract to
    // [`stage_x3_u8_words`].
    unsafe fn stage_x3_u8(row: &[u8], lut: &[f32; 256], stage: &mut [f32], w: usize) {
        unsafe { stage_x3_u8_words(row, lut, stage, w, Self::STAGE3_FLOATS_PER_PIXEL == 4) }
    }
    /// Whether this CPU runs u8 streams at half precision: rows staged
    /// as planar f16 and the horizontal pass fed f16 samples and
    /// coefficients, accumulating in f32. That halves the pass's loads,
    /// which bound it. The rounding (2^-11 relative, on samples and
    /// coefficients) is far below one 8-bit output step, which is all
    /// a u8 source feeds; u16 streams always stay f32.
    fn half_u8() -> bool {
        false
    }
    /// Stage one u8 RGB row as three planar f16 rows (`stage[0..w]`,
    /// `[w..2w]`, `[2w..3w]`) through `lut`, which holds f16 bits.
    // SAFETY: caller must have verified `Self::half_u8()` and pass
    // `row.len() >= 3 * w` and `stage.len() >= 3 * w`.
    unsafe fn stage_x3_u8_half(row: &[u8], lut: &[u16; 256], stage: &mut [u16], w: usize) {
        unsafe { stage_x3_u8_words(row, lut, stage, w, false) }
    }
    /// Whether this CPU stages YCbCr rows with 2x horizontally subsampled
    /// chroma (JPEG 4:2:0 and 4:2:2) directly, fusing libjpeg's
    /// replicating upsample and color conversion into the LUT staging
    /// ([`RowKernel::ycc_terms_h2`], [`RowKernel::stage_ycc_h2`]).
    fn ycc_h2() -> bool {
        false
    }
    /// Precompute one chroma row's color terms for
    /// [`RowKernel::stage_ycc_h2`]; every luma row sharing the chroma
    /// row reuses them.
    // SAFETY: caller must have verified `Self::ycc_h2()` and pass
    // `cb.len() >= w.div_ceil(2)`, `cr.len() >= w.div_ceil(2)` and
    // `terms.len() >= 6 * ycc_stride(w)`.
    unsafe fn ycc_terms_h2(_cb: &[u8], _cr: &[u8], _w: usize, _terms: &mut [u8]) {
        unreachable!("ycc_h2() is false for this kernel")
    }
    /// Stage one luma row through the terms of its chroma row and `lut`,
    /// bit-identical to [`RowKernel::stage_x3_u8`] of the RGB row libjpeg
    /// would output ([`ycc_to_rgb`] per pixel, chroma sample `x / 2`).
    // SAFETY: caller must have verified `Self::ycc_h2()` and pass
    // `y.len() >= w`, `terms` as written by `ycc_terms_h2` for the same
    // `w`, and `stage.len() >= w * Self::STAGE3_FLOATS_PER_PIXEL`.
    unsafe fn stage_ycc_h2(
        _y: &[u8],
        _terms: &[u8],
        _lut: &[f32; 256],
        _stage: &mut [f32],
        _w: usize,
    ) {
        unreachable!("ycc_h2() is false for this kernel")
    }
    /// Half-precision counterpart of [`RowKernel::horiz_x3_batch`] over
    /// rows staged by [`RowKernel::stage_x3_u8_half`] (at 1/65536 scale,
    /// which the pass multiplies back out), with `w.coeffs_f16()` taps.
    #[allow(clippy::too_many_arguments)]
    // SAFETY: caller must have verified `Self::half_u8()`; per row i < n,
    // `stage[i * row_stride..]` holds (src_w + w.stride) * 3 u16 of finite
    // f16 values, `slots[i] + dst_w <= plane` and `ring.len() >= 3 * plane`.
    unsafe fn horiz_x3_batch_half(
        _stage: &[u16],
        _row_stride: usize,
        _n: usize,
        _src_w: usize,
        _w: &Windows,
        _ring: &mut [f32],
        _plane: usize,
        _slots: &[usize; 4],
        _dst_w: usize,
    ) {
        unreachable!("half_u8() is false for this kernel")
    }
    /// Convert one u16 RGBA row to f32, keeping the interleaved layout.
    // SAFETY: caller must have verified `Self::detect()` and pass
    // `stage.len() >= row.len()`.
    unsafe fn stage_x4(row: &[u16], stage: &mut [f32]);
    /// Horizontally convolve one staged 3-channel row into ring `slot`.
    // SAFETY: caller must have verified `Self::detect()`; `w` = windows for
    // `src_w -> dst_w`; `stage` holds (src_w + w.stride) * STAGE3_FLOATS_PER_PIXEL
    // f32 laid out by `stage_x3` (the padded tap-blocks read into the slack);
    // `slot + dst_w <= plane` and `ring.len() >= 3 * plane`.
    unsafe fn horiz_x3(
        stage: &[f32],
        src_w: usize,
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slot: usize,
        dst_w: usize,
    );
    /// Horizontally convolve one staged 4-channel row into ring `slot`.
    // SAFETY: caller must have verified `Self::detect()`; `w` = windows for
    // `src_w -> dst_w`; `stage` holds (src_w + w.stride) * 4 f32 laid out by
    // `stage_x4` (the padded tap-blocks read into the slack);
    // `slot + dst_w <= plane` and `ring.len() >= 4 * plane`.
    unsafe fn horiz_x4(
        stage: &[f32],
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slot: usize,
        dst_w: usize,
    );
    /// acc[x] = sum over taps of coeff * ring_row[x], taps ascending.
    // SAFETY: caller must have verified `Self::detect()` and guarantee
    // `off + dst_w <= plane.len()` for every `off` in `offs` and
    // `acc.len() >= dst_w`.
    unsafe fn vert(plane: &[f32], coeffs: &[f32], offs: &[usize], dst_w: usize, acc: &mut [f32]);
    /// Round-to-nearest f32 -> u16 with saturation, interleaving 3 planes.
    // SAFETY: caller must have verified `Self::detect()` and pass
    // `acc.len() >= 3 * dst_w` (three planar rows) and `out.len() >= 3 * dst_w`.
    unsafe fn store_x3(acc: &[f32], dst_w: usize, out: &mut [u16]);
    /// Round-to-nearest f32 -> u16 with saturation, interleaving 4 planes.
    // SAFETY: caller must have verified `Self::detect()` and pass
    // `acc.len() >= 4 * dst_w` (four planar rows) and `out.len() >= 4 * dst_w`.
    unsafe fn store_x4(acc: &[f32], dst_w: usize, out: &mut [u16]);

    /// Rows the driver batches per horizontal pass. A batched pass
    /// convolves several staged rows against each window's coefficients
    /// once, amortizing the coefficient loads and shuffles; 1 keeps the
    /// row-at-a-time behavior.
    const HORIZ_BATCH: usize = 1;
    /// Horizontally convolve `n` staged rows (`n <= HORIZ_BATCH`), row
    /// `i` living at `stage[i * row_stride..]` and landing in ring slot
    /// offset `slots[i]`. Each row's math is identical to
    /// [`RowKernel::horiz_x3`], so batching cannot change any value.
    #[allow(clippy::too_many_arguments)]
    // SAFETY: caller contract as for `horiz_x3`, per row: for each i < n,
    // `stage[i * row_stride..]` and ring slot `slots[i]` must satisfy the
    // `horiz_x3` preconditions. The default body forwards exactly those
    // per-row preconditions.
    unsafe fn horiz_x3_batch(
        stage: &[f32],
        row_stride: usize,
        n: usize,
        src_w: usize,
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slots: &[usize; 4],
        dst_w: usize,
    ) {
        unsafe {
            for (i, &slot) in slots.iter().enumerate().take(n) {
                Self::horiz_x3(&stage[i * row_stride..], src_w, w, ring, plane, slot, dst_w);
            }
        }
    }
    /// 4-channel counterpart of [`RowKernel::horiz_x3_batch`].
    #[allow(clippy::too_many_arguments)]
    // SAFETY: caller contract as for `horiz_x4`, per row: for each i < n,
    // `stage[i * row_stride..]` and ring slot `slots[i]` must satisfy the
    // `horiz_x4` preconditions. The default body forwards exactly those
    // per-row preconditions.
    unsafe fn horiz_x4_batch(
        stage: &[f32],
        row_stride: usize,
        n: usize,
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slots: &[usize; 4],
        dst_w: usize,
    ) {
        unsafe {
            for (i, &slot) in slots.iter().enumerate().take(n) {
                Self::horiz_x4(&stage[i * row_stride..], w, ring, plane, slot, dst_w);
            }
        }
    }
}

/// Portable u8 RGB staging through `lut` (f32 values, or f16 bits for
/// the half-precision path), planar or (`rgbx`) interleaved with a zero
/// fourth lane: the body of [`RowKernel::stage_x3_u8`] and
/// [`RowKernel::stage_x3_u8_half`] for kernels without a faster lookup.
//
// SAFETY: caller must pass `row.len() >= 3 * w` and `stage.len() >= w * 4`
// (`rgbx`) or `>= w * 3` (planar). The `get_unchecked` indices are bounded
// by exactly those lengths (the 12-byte group reads end at
// `3 * (x + 4) <= 3 * w`), and every `lut` index is one byte, always < 256.
//
// The cost is the three table reads per pixel, which NEON and AVX2 have no
// lookup wide enough to vectorize (a 256-entry f32 table); what is left to
// save is the byte loads, so four pixels' bytes come in as one u64 and one
// u32 and are split in registers (-22% on an M2 Max, -7% on Zen 4).
#[inline(always)]
pub(crate) unsafe fn stage_x3_u8_words<T: Copy + Default>(
    row: &[u8],
    lut: &[T; 256],
    stage: &mut [T],
    w: usize,
    rgbx: bool,
) {
    #[inline(always)]
    unsafe fn group(row: &[u8], x: usize) -> [usize; 12] {
        // SAFETY: caller guarantees `3 * (x + 4) <= row.len()`.
        let (a, b) = unsafe {
            let p = row.as_ptr().add(x * 3);
            (
                (p as *const u64).read_unaligned().to_le(),
                (p.add(8) as *const u32).read_unaligned().to_le(),
            )
        };
        std::array::from_fn(|i| {
            if i < 8 {
                ((a >> (i * 8)) & 0xff) as usize
            } else {
                ((b >> ((i - 8) * 8)) & 0xff) as usize
            }
        })
    }
    unsafe {
        let mut x0 = 0;
        if rgbx {
            while x0 + 4 <= w {
                let v = group(row, x0);
                for k in 0..4 {
                    let o = stage.get_unchecked_mut((x0 + k) * 4..(x0 + k) * 4 + 4);
                    o[0] = *lut.get_unchecked(v[3 * k]);
                    o[1] = *lut.get_unchecked(v[3 * k + 1]);
                    o[2] = *lut.get_unchecked(v[3 * k + 2]);
                    o[3] = T::default();
                }
                x0 += 4;
            }
            for x in x0..w {
                *stage.get_unchecked_mut(x * 4) = lut[*row.get_unchecked(x * 3) as usize];
                *stage.get_unchecked_mut(x * 4 + 1) = lut[*row.get_unchecked(x * 3 + 1) as usize];
                *stage.get_unchecked_mut(x * 4 + 2) = lut[*row.get_unchecked(x * 3 + 2) as usize];
                *stage.get_unchecked_mut(x * 4 + 3) = T::default();
            }
        } else {
            while x0 + 4 <= w {
                let v = group(row, x0);
                for k in 0..4 {
                    *stage.get_unchecked_mut(x0 + k) = *lut.get_unchecked(v[3 * k]);
                    *stage.get_unchecked_mut(w + x0 + k) = *lut.get_unchecked(v[3 * k + 1]);
                    *stage.get_unchecked_mut(2 * w + x0 + k) = *lut.get_unchecked(v[3 * k + 2]);
                }
                x0 += 4;
            }
            for x in x0..w {
                *stage.get_unchecked_mut(x) = lut[*row.get_unchecked(x * 3) as usize];
                *stage.get_unchecked_mut(w + x) = lut[*row.get_unchecked(x * 3 + 1) as usize];
                *stage.get_unchecked_mut(2 * w + x) = lut[*row.get_unchecked(x * 3 + 2) as usize];
            }
        }
    }
}

/// libjpeg's YCbCr -> RGB for one pixel (jdcolor.c and jdmerge.c): 16-bit
/// fixed-point coefficients, rounded, range-limited to 0..=255. The
/// reference the YCbCr staging kernels are tested against.
#[cfg(test)]
pub(crate) fn ycc_to_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    const HALF: i32 = 1 << 15;
    let (y, cb, cr) = (y as i32, cb as i32 - 128, cr as i32 - 128);
    let r = (YCC_FIX_R * cr + HALF) >> 16;
    let g = (-YCC_FIX_GB * cb - YCC_FIX_GR * cr + HALF) >> 16;
    let b = (YCC_FIX_B * cb + HALF) >> 16;
    [y + r, y + g, y + b].map(|v| v.clamp(0, 255) as u8)
}

/// libjpeg's FIX(1.40200), FIX(0.34414), FIX(0.71414), FIX(1.77200).
#[cfg(any(test, target_arch = "x86_64"))]
pub(crate) const YCC_FIX_R: i32 = 91881;
#[cfg(any(test, target_arch = "x86_64"))]
pub(crate) const YCC_FIX_GB: i32 = 22554;
#[cfg(any(test, target_arch = "x86_64"))]
pub(crate) const YCC_FIX_GR: i32 = 46802;
#[cfg(any(test, target_arch = "x86_64"))]
pub(crate) const YCC_FIX_B: i32 = 116130;

/// Bytes per term plane of [`RowKernel::ycc_terms_h2`]: whole 64-pixel
/// blocks.
pub(crate) fn ycc_stride(w: usize) -> usize {
    w.next_multiple_of(64)
}

pub(crate) fn clamp_u16(v: f32) -> u16 {
    (v + 0.5).clamp(0.0, 65535.0) as u16
}

/// Row-push streaming resizer: feed source rows top-to-bottom with
/// [`StreamResize::push_row`]; each completed output row is handed to the
/// callback immediately. Ring capacity bounds memory at
/// `window_size * dst_w * channels` f32s instead of a full intermediate
/// image, exactly like the strip-mined full-frame driver.
pub(crate) struct StreamResize<K: RowKernel> {
    wh: Arc<Windows>,
    wv: Arc<Windows>,
    channels: usize,
    src_w: usize,
    dst_w: usize,
    dst_h: usize,
    cap: usize,
    /// f32s between consecutive ring rows: `dst_w` rounded up to whole
    /// cache lines.
    ring_stride: usize,
    plane: usize,
    /// Source rows past this index influence no output row; they are
    /// accepted and dropped (the full-frame driver never touches them).
    last_needed: usize,
    next_row: usize,
    /// Staged rows not yet horizontally convolved (tail of `next_row`).
    pending: usize,
    /// f32s between consecutive staged rows in the batch buffer.
    stage_row_stride: usize,
    /// Rows are staged as f16 ([`RowKernel::half_u8`]); decided by the
    /// first row, since a stream is fed u8 or u16 rows, never both.
    half: bool,
    /// The stream's u8 LUT as f16 bits at 1/65536 scale (half mode).
    lut_h: [u16; 256],
    /// `scratch.ycc` holds terms for this stream's current chroma row.
    ycc_ready: bool,
    oy: usize,
    scratch: Scratch,
    _k: std::marker::PhantomData<K>,
}

impl<K: RowKernel> StreamResize<K> {
    /// `cap_override` shrinks/expands the ring (tests use `src_h` to run
    /// the full-intermediate reference schedule); `None` picks the
    /// strip-mined capacity.
    pub(crate) fn with_capacity(
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
        channels: usize,
        cap_override: Option<usize>,
    ) -> Result<Self> {
        ensure!(channels == 3 || channels == 4, "unsupported channel count");
        ensure!(
            src_w > 0 && src_h > 0 && dst_w > 0 && dst_h > 0,
            "empty dimensions"
        );
        ensure!(K::detect(), "kernel not supported on this CPU");
        let wh = cached_windows(src_w, dst_w);
        let wv = cached_windows(src_h, dst_h);
        // Ring capacity: every vertical window's span is <= window_size
        // (the raw span ceil(c+r)-floor(c-r) < 2r+2 <= window_size+1, and
        // clamping or zero-trimming only shrinks it), and window ends are
        // non-decreasing in oy, so end-driven fill never evicts a live
        // row.
        let cap = cap_override.unwrap_or_else(|| wv.window_size.min(src_h).max(1));
        let ring_stride = dst_w.next_multiple_of(LINE_F32);
        let plane = cap * ring_stride;
        let last_needed = wv.starts[dst_h - 1] + wv.sizes[dst_h - 1];

        let mut scratch = SCRATCH_POOL
            .with(|p| p.borrow_mut().pop())
            .unwrap_or_default();
        // The extra `stride` per channel lets horizontal kernels read
        // whole tap-blocks past a window's real size: those lanes meet
        // zero coefficients, and the slack only ever holds finite
        // values (fresh zeros or staged u16 data from earlier use).
        let stage_px = if channels == 3 {
            K::STAGE3_FLOATS_PER_PIXEL
        } else {
            channels
        };
        const { assert!(K::HORIZ_BATCH >= 1 && K::HORIZ_BATCH <= 4) };
        let stage_row_stride = ((src_w + wh.stride) * stage_px).next_multiple_of(LINE_F32);
        scratch.stage.grow(stage_row_stride * K::HORIZ_BATCH);
        scratch.ring.grow(plane * channels);
        grow(&mut scratch.acc, dst_w * channels);
        if scratch.offs.len() < wv.window_size {
            scratch.offs.resize(wv.window_size, 0);
        }
        if scratch.outrow.len() < dst_w * channels {
            scratch.outrow.resize(dst_w * channels, 0);
        }

        Ok(StreamResize {
            wh,
            wv,
            channels,
            src_w,
            dst_w,
            dst_h,
            cap,
            ring_stride,
            plane,
            last_needed,
            next_row: 0,
            pending: 0,
            stage_row_stride,
            half: false,
            lut_h: [0; 256],
            ycc_ready: false,
            oy: 0,
            scratch,
            _k: std::marker::PhantomData,
        })
    }

    pub(crate) fn new(
        src_w: usize,
        src_h: usize,
        dst_w: usize,
        dst_h: usize,
        channels: usize,
    ) -> Result<Self> {
        Self::with_capacity(src_w, src_h, dst_w, dst_h, channels, None)
    }

    /// Number of source rows that influence the output; callers may stop
    /// pushing after this many rows.
    pub(crate) fn rows_needed(&self) -> usize {
        self.last_needed
    }

    /// Push the next source row (interleaved u16, `src_w * channels`
    /// long). Emits `(oy, row)` for every output row whose vertical
    /// window is completed by this source row, in ascending `oy` order.
    pub(crate) fn push_row(&mut self, row: &[u16], emit: impl FnMut(usize, &[u16])) {
        assert!(row.len() >= self.src_w * self.channels, "short source row");
        assert!(!self.half, "u16 row pushed into a u8 stream");
        if self.next_row >= self.last_needed {
            self.next_row += 1;
            return; // trailing rows influence nothing
        }
        let base = self.pending * self.stage_row_stride;
        // SAFETY: constructor verified K::detect(); buffers were sized in
        // the constructor.
        unsafe {
            if self.channels == 3 {
                let px = K::STAGE3_FLOATS_PER_PIXEL;
                K::stage_x3(
                    row,
                    &mut self.scratch.stage[base..base + self.src_w * px],
                    self.src_w,
                );
            } else {
                K::stage_x4(
                    &row[..self.src_w * 4],
                    &mut self.scratch.stage[base..base + self.src_w * 4],
                );
            }
        }
        self.after_stage(emit);
    }

    /// Push the next source row as interleaved u8 RGB, staging through a
    /// u8 -> f32 lookup table (3-channel streams only). Values are
    /// bit-identical to applying the equivalent u16 LUT and calling
    /// [`StreamResize::push_row`], except on kernels that run u8 streams
    /// at half precision ([`RowKernel::half_u8`]), where they agree to
    /// one 8-bit sRGB level.
    pub(crate) fn push_row_u8(
        &mut self,
        row: &[u8],
        lut: &[f32; 256],
        emit: impl FnMut(usize, &[u16]),
    ) {
        assert_eq!(self.channels, 3, "u8 staging is 3-channel only");
        assert!(row.len() >= self.src_w * 3, "short source row");
        if self.next_row >= self.last_needed {
            self.next_row += 1;
            return; // trailing rows influence nothing
        }
        if self.next_row == 0 && K::half_u8() {
            self.half = true;
            // f16 tops out at 65504, below the u16 range; the power-of-two
            // scale is exact and the horizontal pass undoes it.
            self.lut_h = std::array::from_fn(|i| f32_to_f16(lut[i] / 65536.0));
        }
        if self.half {
            let n = self.half_row_stride();
            let w = self.src_w;
            let stage = &mut as_u16_mut(&mut self.scratch.stage)[self.pending * n..][..n];
            // SAFETY: half_u8() held when `half` was set; the row slice is
            // `3 * (w + stride) >= 3 * w` long.
            unsafe { K::stage_x3_u8_half(row, &self.lut_h, &mut stage[..3 * w], w) };
            // The padded tap-blocks read past the last plane: keep those
            // lanes finite (f32 leftovers read as f16 can be NaN, and a
            // zero coefficient does not cancel a NaN).
            stage[3 * w..].fill(0);
        } else {
            let base = self.pending * self.stage_row_stride;
            let px = K::STAGE3_FLOATS_PER_PIXEL;
            // SAFETY: as in push_row.
            unsafe {
                K::stage_x3_u8(
                    row,
                    lut,
                    &mut self.scratch.stage[base..base + self.src_w * px],
                    self.src_w,
                );
            }
        }
        self.after_stage(emit);
    }

    /// Push the next source row as YCbCr planes with 2x horizontally
    /// subsampled chroma, on kernels with [`RowKernel::ycc_h2`]
    /// (3-channel streams only). `chroma` carries the row's Cb and Cr
    /// rows whenever they differ from the previous row's: on the first
    /// row, and on every other row for 4:2:0. Staged values are
    /// bit-identical to [`StreamResize::push_row_u8`] of the RGB row
    /// libjpeg's replicating (merged) upsampler outputs.
    pub(crate) fn push_row_ycc(
        &mut self,
        y: &[u8],
        chroma: Option<(&[u8], &[u8])>,
        lut: &[f32; 256],
        emit: impl FnMut(usize, &[u16]),
    ) {
        assert_eq!(self.channels, 3, "YCbCr staging is 3-channel only");
        assert!(K::ycc_h2() && !self.half, "kernel lacks YCbCr staging");
        let w = self.src_w;
        assert!(y.len() >= w, "short luma row");
        if self.next_row >= self.last_needed {
            self.next_row += 1;
            return; // trailing rows influence nothing
        }
        if let Some((cb, cr)) = chroma {
            let cw = w.div_ceil(2);
            assert!(cb.len() >= cw && cr.len() >= cw, "short chroma row");
            let n = 6 * ycc_stride(w);
            if self.scratch.ycc.len() < n {
                self.scratch.ycc.resize(n, 0);
            }
            // SAFETY: ycc_h2() asserted; lengths checked above.
            unsafe { K::ycc_terms_h2(cb, cr, w, &mut self.scratch.ycc[..n]) };
            self.ycc_ready = true;
        }
        assert!(self.ycc_ready, "first YCbCr row without chroma");
        let base = self.pending * self.stage_row_stride;
        let px = K::STAGE3_FLOATS_PER_PIXEL;
        // SAFETY: as in push_row; the terms were written for this `w`.
        unsafe {
            K::stage_ycc_h2(
                y,
                &self.scratch.ycc,
                lut,
                &mut self.scratch.stage[base..base + w * px],
                w,
            );
        }
        self.after_stage(emit);
    }

    /// u16s between consecutive f16-staged rows: three planes plus the
    /// tap-block slack, inside the f32 batch buffer's first half.
    fn half_row_stride(&self) -> usize {
        (self.src_w + self.wh.stride) * 3
    }

    /// Shared continuation after a row lands in the batch buffer: flush
    /// the horizontal pass when due, then emit completed output rows.
    fn after_stage(&mut self, mut emit: impl FnMut(usize, &[u16])) {
        // The horizontal pass runs when the batch fills, the last needed
        // row arrives, or an output row below needs pending rows.
        self.pending += 1;
        self.next_row += 1;
        if self.pending == K::HORIZ_BATCH || self.next_row == self.last_needed {
            self.flush_batch();
        }

        // Emit every output row whose window end has now been staged
        // (window ends are non-decreasing in oy).
        while self.oy < self.dst_h {
            let start = self.wv.starts[self.oy];
            let size = self.wv.sizes[self.oy];
            if start + size > self.next_row {
                break;
            }
            if start + size > self.next_row - self.pending {
                self.flush_batch();
            }
            let s = &mut self.scratch;
            let coeffs = &self.wv.coeffs[self.oy * self.wv.stride..][..size];
            // Ring slot offsets for this window, one wrap-increment per
            // tap instead of a modulo in the accumulation inner loop.
            let mut slot = start % self.cap;
            for o in s.offs[..size].iter_mut() {
                *o = slot * self.ring_stride;
                slot += 1;
                if slot == self.cap {
                    slot = 0;
                }
            }
            let outrow = &mut s.outrow[..self.dst_w * self.channels];
            // SAFETY: as above.
            unsafe {
                for c in 0..self.channels {
                    K::vert(
                        &s.ring[c * self.plane..(c + 1) * self.plane],
                        coeffs,
                        &s.offs[..size],
                        self.dst_w,
                        &mut s.acc[c * self.dst_w..(c + 1) * self.dst_w],
                    );
                }
                if self.channels == 3 {
                    K::store_x3(&s.acc, self.dst_w, outrow);
                } else {
                    K::store_x4(&s.acc, self.dst_w, outrow);
                }
            }
            emit(self.oy, outrow);
            self.oy += 1;
        }
    }

    /// Run the horizontal pass over the staged batch. Ring slots are the
    /// consecutive row indices modulo the ring capacity.
    fn flush_batch(&mut self) {
        if self.pending == 0 {
            return;
        }
        let first = self.next_row - self.pending;
        let mut slots = [0usize; 4];
        for (i, slot) in slots.iter_mut().enumerate().take(self.pending) {
            *slot = ((first + i) % self.cap) * self.ring_stride;
        }
        let half_stride = self.half_row_stride();
        let s = &mut self.scratch;
        // SAFETY: constructor verified K::detect(); slice lengths include
        // the zero-coefficient slack the padded tap-blocks may read.
        unsafe {
            if self.half {
                K::horiz_x3_batch_half(
                    &as_u16(&s.stage)[..half_stride * self.pending],
                    half_stride,
                    self.pending,
                    self.src_w,
                    &self.wh,
                    &mut s.ring[..],
                    self.plane,
                    &slots,
                    self.dst_w,
                );
            } else if self.channels == 3 {
                K::horiz_x3_batch(
                    &s.stage[..self.stage_row_stride * self.pending],
                    self.stage_row_stride,
                    self.pending,
                    self.src_w,
                    &self.wh,
                    &mut s.ring[..],
                    self.plane,
                    &slots,
                    self.dst_w,
                );
            } else {
                K::horiz_x4_batch(
                    &s.stage[..self.stage_row_stride * self.pending],
                    self.stage_row_stride,
                    self.pending,
                    &self.wh,
                    &mut s.ring[..],
                    self.plane,
                    &slots,
                    self.dst_w,
                );
            }
        }
        self.pending = 0;
    }

    /// Output rows emitted so far; equals `dst_h` once enough source
    /// rows were pushed.
    pub(crate) fn rows_emitted(&self) -> usize {
        self.oy
    }
}

impl<K: RowKernel> Drop for StreamResize<K> {
    fn drop(&mut self) {
        let scratch = std::mem::take(&mut self.scratch);
        SCRATCH_POOL.with(|p| {
            let mut p = p.borrow_mut();
            if p.len() < 4 {
                p.push(scratch);
            }
        });
    }
}

/// Full-frame resize over the streaming driver (identical operations in
/// identical per-value order; the stream just interleaves horizontal and
/// vertical stages differently, which no value depends on).
pub(crate) fn resize_u16<K: RowKernel>(
    src_bytes: &[u8],
    src_w: usize,
    src_h: usize,
    dst_bytes: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    channels: usize,
) -> Result<()> {
    // SAFETY: sound for any input — `align_to` only reinterprets the middle
    // bytes, and every bit pattern is a valid u16.
    let (pre, src, post) = unsafe { src_bytes.align_to::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 src");
    // SAFETY: as above; `align_to_mut` reborrows `dst_bytes` exclusively, and
    // u16 permits any bit pattern.
    let (pre, dst, post) = unsafe { dst_bytes.align_to_mut::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 dst");
    ensure!(src.len() >= src_w * src_h * channels, "src too small");
    ensure!(dst.len() >= dst_w * dst_h * channels, "dst too small");

    let mut sr = StreamResize::<K>::new(src_w, src_h, dst_w, dst_h, channels)?;
    run_rows(&mut sr, src, src_h, dst, dst_w, channels);
    Ok(())
}

pub(crate) fn run_rows<K: RowKernel>(
    sr: &mut StreamResize<K>,
    src: &[u16],
    src_h: usize,
    dst: &mut [u16],
    dst_w: usize,
    channels: usize,
) {
    let needed = sr.rows_needed().min(src_h);
    for y in 0..needed {
        let row = &src[y * sr.src_w * channels..(y + 1) * sr.src_w * channels];
        sr.push_row(row, |oy, out| {
            dst[oy * dst_w * channels..(oy + 1) * dst_w * channels].copy_from_slice(out);
        });
    }
    debug_assert_eq!(sr.rows_emitted(), sr.dst_h);
}

#[cfg(test)]
pub(crate) mod testkit {
    //! Kernel-parameterized correctness suite, instantiated by each
    //! arch's module so every implementation faces the same contract.
    use super::*;

    /// Deterministic synthetic image: gradients plus LCG noise so
    /// convolution windows see realistic variation.
    pub(crate) fn test_image(w: usize, h: usize, ch: usize) -> Vec<u16> {
        let mut seed = 0x2545F491u32;
        let mut px = Vec::with_capacity(w * h * ch);
        for y in 0..h {
            for x in 0..w {
                for c in 0..ch {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let noise = (seed >> 16) & 0x3FFF;
                    let base = (x * 48000 / w + y * 16000 / h + c * 999) as u32;
                    px.push(((base + noise).min(65535)) as u16);
                }
            }
        }
        px
    }

    // SAFETY (both raw-slice casts below): a `&[u16]`/`&mut [u16]` viewed as
    // bytes — same allocation, `len * 2` bytes, u8 has alignment 1 and no invalid
    // bit patterns; `dst` is not touched while `dst_bytes` is live.
    pub(crate) fn kernel_resize<K: RowKernel>(
        src: &[u16],
        sw: usize,
        sh: usize,
        dw: usize,
        dh: usize,
        ch: usize,
    ) -> Vec<u16> {
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let mut dst = vec![0u16; dw * dh * ch];
        let dst_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
        resize_u16::<K>(src_bytes, sw, sh, dst_bytes, dw, dh, ch).unwrap();
        dst
    }

    /// Reference schedule: same kernels, ring capacity = src_h (a full
    /// intermediate image, i.e. the unstripped two-pass schedule).
    pub(crate) fn reference_resize<K: RowKernel>(
        src: &[u16],
        sw: usize,
        sh: usize,
        dw: usize,
        dh: usize,
        ch: usize,
    ) -> Vec<u16> {
        let mut sr = StreamResize::<K>::with_capacity(sw, sh, dw, dh, ch, Some(sh)).unwrap();
        let mut dst = vec![0u16; dw * dh * ch];
        run_rows(&mut sr, src, sh, &mut dst, dw, ch);
        dst
    }

    /// Scalar f64 separable resize with un-quantized intermediate:
    /// ground truth for accuracy comparisons.
    pub(crate) fn ref_resize_f64(
        src: &[u16],
        sw: usize,
        sh: usize,
        dw: usize,
        dh: usize,
        ch: usize,
    ) -> Vec<u16> {
        struct W64 {
            starts: Vec<usize>,
            windows: Vec<Vec<f64>>,
        }
        fn windows64(in_size: usize, out_size: usize) -> W64 {
            let scale = in_size as f64 / out_size as f64;
            let fs = scale.max(1.0);
            let radius = 3.0 * fs;
            let recip = 1.0 / fs;
            let (mut starts, mut windows) = (Vec::new(), Vec::new());
            for o in 0..out_size {
                let center = (o as f64 + 0.5) * scale;
                let x_min = (center - radius).floor().max(0.0) as usize;
                let x_max = ((center + radius).ceil() as usize).min(in_size);
                let c = center - 0.5;
                let mut win = Vec::new();
                let mut lead = 0usize;
                for x in x_min..x_max {
                    let w = lanczos3((x as f64 - c) * recip);
                    if win.is_empty() && w == 0.0 {
                        lead += 1;
                    } else {
                        win.push(w);
                    }
                }
                let x_min = x_min + lead;
                while win.len() > 1 && *win.last().unwrap() == 0.0 {
                    win.pop();
                }
                let ww: f64 = win.iter().sum();
                if ww != 0.0 {
                    win.iter_mut().for_each(|w| *w /= ww);
                }
                starts.push(x_min);
                windows.push(win);
            }
            W64 { starts, windows }
        }
        let wh = windows64(sw, dw);
        let wv = windows64(sh, dh);
        let mut mid = vec![0f64; dw * sh * ch];
        for y in 0..sh {
            for ox in 0..dw {
                for c in 0..ch {
                    let mut s = 0f64;
                    for (k, &w) in wh.windows[ox].iter().enumerate() {
                        s += w * src[(y * sw + wh.starts[ox] + k) * ch + c] as f64;
                    }
                    mid[(y * dw + ox) * ch + c] = s;
                }
            }
        }
        let mut out = vec![0u16; dw * dh * ch];
        for oy in 0..dh {
            for x in 0..dw {
                for c in 0..ch {
                    let mut s = 0f64;
                    for (k, &w) in wv.windows[oy].iter().enumerate() {
                        s += w * mid[((wv.starts[oy] + k) * dw + x) * ch + c];
                    }
                    out[(oy * dw + x) * ch + c] = s.round().clamp(0.0, 65535.0) as u16;
                }
            }
        }
        out
    }

    pub(crate) fn rmse(a: &[u16], b: &[u16]) -> f64 {
        let se: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
            .sum();
        (se / a.len() as f64).sqrt()
    }

    /// Shape sweep used by the schedule-equality test: the benchmark
    /// shape, primes, tiny images (src_h < ring capacity), upscales
    /// (heavily overlapping windows), single-row outputs, and extreme
    /// aspect changes.
    pub(crate) const SHAPES: [(usize, usize, usize, usize); 10] = [
        (2040, 1356, 512, 340),
        (640, 480, 512, 384),
        (333, 217, 100, 65),
        (17, 11, 5, 3),
        (127, 83, 31, 29),
        (50, 40, 120, 96),
        (64, 64, 17, 9),
        (100, 7, 50, 3),
        (9, 300, 7, 150),
        (256, 199, 256, 1),
    ];

    /// Strip-mined ring == full-intermediate reference, bit-exact.
    pub(crate) fn assert_schedule_equality<K: RowKernel>() {
        for &(sw, sh, dw, dh) in &SHAPES {
            for ch in [3usize, 4] {
                let src = test_image(sw, sh, ch);
                let strip = kernel_resize::<K>(&src, sw, sh, dw, dh, ch);
                let full = reference_resize::<K>(&src, sw, sh, dw, dh, ch);
                assert_eq!(strip, full, "{sw}x{sh}->{dw}x{dh} x{ch}");
            }
        }
    }

    /// Pushing every source row (including trailing rows past the last
    /// vertical window, which a streaming decoder will do) emits each
    /// output row exactly once, in order, bit-identical to the
    /// full-frame path.
    pub(crate) fn assert_streaming_with_trailing_rows<K: RowKernel>() {
        for &(sw, sh, dw, dh) in &SHAPES {
            for ch in [3usize, 4] {
                let src = test_image(sw, sh, ch);
                let full = kernel_resize::<K>(&src, sw, sh, dw, dh, ch);
                let mut sr = StreamResize::<K>::new(sw, sh, dw, dh, ch).unwrap();
                let mut streamed = vec![0u16; dw * dh * ch];
                let mut emitted = Vec::new();
                for y in 0..sh {
                    sr.push_row(&src[y * sw * ch..(y + 1) * sw * ch], |oy, out| {
                        emitted.push(oy);
                        streamed[oy * dw * ch..(oy + 1) * dw * ch].copy_from_slice(out);
                    });
                }
                assert_eq!(
                    emitted,
                    (0..dh).collect::<Vec<_>>(),
                    "{sw}x{sh}->{dw}x{dh} x{ch}"
                );
                assert_eq!(sr.rows_emitted(), dh);
                assert_eq!(streamed, full, "{sw}x{sh}->{dw}x{dh} x{ch} streamed");
            }
        }
    }

    /// u8 staging through a LUT == the same LUT applied up front and staged
    /// as u16, bit-exact, including widths that leave a partial 4-pixel group.
    pub(crate) fn assert_u8_staging_matches_u16<K: RowKernel>() {
        let lut: [f32; 256] = std::array::from_fn(|v| {
            let c = v as f64 / 255.0;
            let l = if c <= 0.04045 {
                c / 12.92
            } else {
                ((c + 0.055) / 1.055).powf(2.4)
            };
            (l * 65535.0).round() as f32
        });
        let narrow = (1..=13usize).map(|w| (w, 5, w.div_ceil(2), 3));
        for (sw, sh, dw, dh) in SHAPES.iter().copied().chain(narrow) {
            let src: Vec<u8> = test_image(sw, sh, 3)
                .iter()
                .map(|&v| (v >> 8) as u8)
                .collect();
            let mut a = StreamResize::<K>::new(sw, sh, dw, dh, 3).unwrap();
            let mut b = StreamResize::<K>::new(sw, sh, dw, dh, 3).unwrap();
            let mut via_u8 = vec![0u16; dw * dh * 3];
            let mut via_u16 = vec![0u16; dw * dh * 3];
            for y in 0..sh {
                let row = &src[y * sw * 3..(y + 1) * sw * 3];
                a.push_row_u8(row, &lut, |oy, out| {
                    via_u8[oy * dw * 3..(oy + 1) * dw * 3].copy_from_slice(out)
                });
                let wide: Vec<u16> = row.iter().map(|&v| lut[v as usize] as u16).collect();
                b.push_row(&wide, |oy, out| {
                    via_u16[oy * dw * 3..(oy + 1) * dw * 3].copy_from_slice(out)
                });
            }
            if !K::half_u8() {
                assert_eq!(via_u8, via_u16, "{sw}x{sh}->{dw}x{dh} u8 staging");
                continue;
            }
            // Half precision: judged where a u8 stream lands, 8-bit sRGB.
            let enc = |v: u16| {
                let l = v as f64 / 65535.0;
                let c = if l <= 0.0031308 {
                    l * 12.92
                } else {
                    1.055 * l.powf(1.0 / 2.4) - 0.055
                };
                (c * 255.0).round() as i32
            };
            let (mut worst16, mut worst8, mut off8) = (0u16, 0i32, 0usize);
            for (&x, &y) in via_u8.iter().zip(&via_u16) {
                worst16 = worst16.max(x.abs_diff(y));
                let d = (enc(x) - enc(y)).abs();
                worst8 = worst8.max(d);
                off8 += (d != 0) as usize;
            }
            // Measured on an M2 Max: worst 26 u16, 1 level, at most 1.02%
            // of samples off (640x480 -> 512x384: 0.92%) beyond the
            // one-sample floor the tiny shapes need. FMLAL is IEEE with a
            // fixed order, so these are the same on any FEAT_FHM core;
            // the bounds (32, 1.25%) only leave room for coefficient-table
            // tweaks.
            let label = format!("{sw}x{sh}->{dw}x{dh} half-precision u8 staging");
            assert!(worst16 <= 32, "{label}: worst u16 diff {worst16}");
            assert!(worst8 <= 1, "{label}: worst 8-bit diff {worst8}");
            let allowed = (via_u8.len() / 80).max(1);
            assert!(
                off8 <= allowed,
                "{label}: {off8} of {} off by a level",
                via_u8.len()
            );
        }
    }

    /// Stage `rows` rows of a 3-channel test image the way the driver
    /// does: `(stage, row_stride)`, slack included.
    fn staged_x3<K: RowKernel>(sw: usize, rows: usize, w: &Windows) -> (Vec<f32>, usize) {
        let src = test_image(sw, rows, 3);
        let rs = (sw + w.stride) * K::STAGE3_FLOATS_PER_PIXEL;
        let mut stage = vec![0f32; rs * rows];
        for y in 0..rows {
            // SAFETY: tests call this only after K::detect(); the slice
            // holds `sw * STAGE3_FLOATS_PER_PIXEL` floats.
            unsafe { K::stage_x3(&src[y * sw * 3..(y + 1) * sw * 3], &mut stage[y * rs..], sw) };
        }
        (stage, rs)
    }

    /// Every batch size up to `HORIZ_BATCH` == one row at a time, bit-exact.
    pub(crate) fn assert_horiz_batch_matches_single<K: RowKernel>() {
        let narrow = (1..=13usize).map(|w| (w, 4, w.div_ceil(2), 1));
        for (sw, _, dw, _) in SHAPES.iter().copied().chain(narrow) {
            let w = Windows::new(sw, dw);
            let (stage, rs) = staged_x3::<K>(sw, 4, &w);
            // Ring slots out of order and a plane wider than 4 rows, so a
            // kernel mixing up rows or slots cannot pass.
            let plane = 5 * dw;
            let slots = [3 * dw, dw, 4 * dw, 0];
            let mut single = vec![0f32; 3 * plane];
            for (r, &slot) in slots.iter().enumerate() {
                // SAFETY: after K::detect(); stage rows carry the slack and
                // every slot + dst_w <= plane.
                unsafe { K::horiz_x3(&stage[r * rs..], sw, &w, &mut single, plane, slot, dw) };
            }
            // n == 0 included: the trait allows an empty batch, which must
            // leave the ring untouched. Slots past n must stay untouched too.
            const UNTOUCHED: f32 = -1.0;
            for n in 0..=K::HORIZ_BATCH {
                let mut batched = vec![UNTOUCHED; 3 * plane];
                // SAFETY: as above, for rows 0..n.
                unsafe {
                    K::horiz_x3_batch(&stage, rs, n, sw, &w, &mut batched, plane, &slots, dw)
                };
                for (r, &slot) in slots.iter().enumerate() {
                    for c in 0..3 {
                        let at = c * plane + slot..c * plane + slot + dw;
                        let same = if r < n {
                            batched[at.clone()]
                                .iter()
                                .zip(&single[at])
                                .all(|(a, b)| a.to_bits() == b.to_bits())
                        } else {
                            batched[at]
                                .iter()
                                .all(|v| v.to_bits() == UNTOUCHED.to_bits())
                        };
                        assert!(same, "{sw}->{dw} batch {n} row {r} channel {c}");
                    }
                }
            }
        }
    }

    /// Horizontal-pass timing over a DIV2K-sized frame, full batches
    /// (`cargo test --release -- --ignored --nocapture horiz_bench`).
    pub(crate) fn bench_horiz<K: RowKernel>() {
        for (sw, dw, rows) in [
            (2040usize, 512usize, 1356usize),
            (2040, 683, 1356),
            (2040, 256, 1356),
        ] {
            let w = Windows::new(sw, dw);
            let (stage, rs) = staged_x3::<K>(sw, rows, &w);
            let b = K::HORIZ_BATCH;
            let plane = b * dw;
            let slots: [usize; 4] = std::array::from_fn(|i| i * dw);
            let mut ring = vec![0f32; 3 * plane];
            let mut best = f64::MAX;
            for _ in 0..15 {
                let t = std::time::Instant::now();
                for y in (0..rows - rows % b).step_by(b) {
                    // SAFETY: after K::detect(); rows y..y + b are staged.
                    unsafe {
                        K::horiz_x3_batch(
                            std::hint::black_box(&stage[y * rs..]),
                            rs,
                            b,
                            sw,
                            &w,
                            &mut ring,
                            plane,
                            &slots,
                            dw,
                        )
                    };
                }
                best = best.min(t.elapsed().as_secs_f64() * 1e3);
            }
            println!("horiz {sw}->{dw} x{rows}: {best:.3} ms");
        }
    }

    // SAFETY (raw-slice cast below): `&[u16]` viewed as bytes — same allocation,
    // `len * 2` bytes, u8 has alignment 1 and no invalid bit patterns.
    fn fir_resize(src: &[u16], sw: usize, sh: usize, dw: usize, dh: usize, ch: usize) -> Vec<u16> {
        use fast_image_resize::images::{Image, ImageRef};
        use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
        let px = if ch == 3 {
            PixelType::U16x3
        } else {
            PixelType::U16x4
        };
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let src_view = ImageRef::new(sw as u32, sh as u32, src_bytes, px).unwrap();
        let mut dst = Image::new(dw as u32, dh as u32, px);
        let opts = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
            .use_alpha(false); // compare plain convolution on both sides
        Resizer::new().resize(&src_view, &mut dst, &opts).unwrap();
        dst.buffer()
            .as_chunks::<2>()
            .0
            .iter()
            .map(|&b| u16::from_le_bytes(b))
            .collect()
    }

    /// The kernel must track the f64 ground truth at least as closely as
    /// fir does (fir quantizes its intermediate image to u16; we keep
    /// f32 rows), and stay within a couple of quantization steps of the
    /// truth itself.
    pub(crate) fn assert_accuracy<K: RowKernel>(
        sw: usize,
        sh: usize,
        dw: usize,
        dh: usize,
        ch: usize,
        label: &str,
    ) {
        let src = test_image(sw, sh, ch);
        let ours = kernel_resize::<K>(&src, sw, sh, dw, dh, ch);
        let fir = fir_resize(&src, sw, sh, dw, dh, ch);
        let truth = ref_resize_f64(&src, sw, sh, dw, dh, ch);
        let ours_err = rmse(&ours, &truth);
        let fir_err = rmse(&fir, &truth);
        let worst = ours
            .iter()
            .zip(&truth)
            .map(|(&x, &y)| x.abs_diff(y))
            .max()
            .unwrap();
        assert!(
            ours_err <= fir_err + 0.05,
            "{label}: ours rmse {ours_err:.4} vs truth worse than fir {fir_err:.4}"
        );
        assert!(
            worst <= 2,
            "{label}: worst diff vs f64 truth {worst} > 2 (rmse {ours_err:.4})"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f16_conversion_round_trips_and_ties_to_even() {
        for h in 0..0x7c00u16 {
            for h in [h, h | 0x8000] {
                assert_eq!(f32_to_f16(f16_to_f32(h)), h, "{h:#06x}");
            }
            if h == 0x7bff {
                continue; // the next value up is infinity
            }
            // The midpoint to the next value (exact in f32) goes to the
            // even neighbor; anything above it goes up.
            let (a, b) = (f16_to_f32(h) as f64, f16_to_f32(h + 1) as f64);
            let mid = ((a + b) / 2.0) as f32;
            let even = if h & 1 == 0 { h } else { h + 1 };
            assert_eq!(f32_to_f16(mid), even, "tie above {h:#06x}");
            assert_eq!(
                f32_to_f16(f32::from_bits(mid.to_bits() + 1)),
                h + 1,
                "above tie {h:#06x}"
            );
        }
        assert_eq!(f32_to_f16(65520.0), 0x7c00, "rounds to infinity");
        assert_eq!(
            f32_to_f16(2f32.powi(-26)),
            0,
            "below half the smallest subnormal"
        );
    }

    #[test]
    fn f16_coefficients_keep_unit_dc_gain() {
        for (i, o) in [
            (2040, 512),
            (2040, 683),
            (1356, 340),
            (333, 100),
            (50, 120),
            (17, 5),
            (7, 4),
        ] {
            let w = Windows::new(i, o);
            let h = w.coeffs_f16();
            for x in 0..o {
                let f32_sum: f64 = w.coeffs[x * w.stride..][..w.sizes[x]]
                    .iter()
                    .map(|&c| c as f64)
                    .sum();
                let taps = &h[x * w.stride..(x + 1) * w.stride];
                let sum: f64 = taps.iter().map(|&t| f16_to_f32(t) as f64).sum();
                assert!(
                    (sum - f32_sum).abs() <= 2f64.powi(-13),
                    "{i}->{o} window {x}: {sum}"
                );
                assert!(
                    taps[w.sizes[x]..].iter().all(|&t| t == 0),
                    "{i}->{o} padding"
                );
            }
        }
    }

    #[test]
    fn ring_capacity_invariant_holds_for_all_small_dimensions() {
        // The strip schedule is safe iff, at the moment output row oy is
        // computed, every live row start..start+size still resides in the
        // ring: fill has reached exactly end = start+size, so the oldest
        // retained row is end - cap and the invariant is
        // end - start <= cap for cap = window_size.min(in_size).
        for in_size in 1..=64usize {
            for out_size in 1..=64usize {
                let w = Windows::new(in_size, out_size);
                let cap = w.window_size.min(in_size).max(1);
                for o in 0..out_size {
                    assert!(
                        w.sizes[o] <= cap,
                        "{in_size}->{out_size} window {o}: size {} > cap {cap}",
                        w.sizes[o]
                    );
                }
                // window ends must be non-decreasing for end-driven fill
                let mut prev_end = 0usize;
                for o in 0..out_size {
                    let end = w.starts[o] + w.sizes[o];
                    assert!(
                        end >= prev_end,
                        "{in_size}->{out_size}: end regressed at {o}"
                    );
                    prev_end = end;
                }
            }
        }
    }

    #[test]
    fn windows_are_normalized_and_in_bounds() {
        for (in_s, out_s) in [(2040, 512), (100, 99), (7, 3), (3, 7)] {
            let w = Windows::new(in_s, out_s);
            assert_eq!(w.stride % 8, 0, "{in_s}->{out_s}: unpadded stride");
            assert!(w.stride >= w.window_size);
            for i in 0..out_s {
                assert!(w.starts[i] + w.sizes[i] <= in_s, "{in_s}->{out_s} px {i}");
                let row = &w.coeffs[i * w.stride..(i + 1) * w.stride];
                let sum: f64 = row[..w.sizes[i]].iter().map(|&c| c as f64).sum();
                assert!(
                    (sum - 1.0).abs() < 1e-4,
                    "{in_s}->{out_s} px {i}: sum={sum}"
                );
                // The padding the SIMD tap-blocks run over must be zero.
                assert!(
                    row[w.sizes[i]..].iter().all(|&c| c == 0.0),
                    "{in_s}->{out_s} px {i}: nonzero padding"
                );
            }
        }
    }
}
