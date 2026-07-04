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
//! tests per arch).

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
        }
    }
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
    stage: Vec<f32>,
    ring: Vec<f32>,
    acc: Vec<f32>,
    offs: Vec<usize>,
    outrow: Vec<u16>,
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
    unsafe fn stage_x3(row: &[u16], stage: &mut [f32], w: usize);
    /// Stage one u8 RGB row as f32 through a 256-entry lookup table
    /// (fusing e.g. the sRGB -> linear transfer into staging, so no
    /// separate full-image pass or u16 intermediate is needed). The
    /// table holds exact f32 images of the u16 LUT values, making this
    /// bit-identical to `lut[v] as u16` followed by [`RowKernel::stage_x3`].
    unsafe fn stage_x3_u8(row: &[u8], lut: &[f32; 256], stage: &mut [f32], w: usize) {
        unsafe {
            if Self::STAGE3_FLOATS_PER_PIXEL == 4 {
                for x in 0..w {
                    *stage.get_unchecked_mut(x * 4) = lut[*row.get_unchecked(x * 3) as usize];
                    *stage.get_unchecked_mut(x * 4 + 1) =
                        lut[*row.get_unchecked(x * 3 + 1) as usize];
                    *stage.get_unchecked_mut(x * 4 + 2) =
                        lut[*row.get_unchecked(x * 3 + 2) as usize];
                    *stage.get_unchecked_mut(x * 4 + 3) = 0.0;
                }
            } else {
                for x in 0..w {
                    *stage.get_unchecked_mut(x) = lut[*row.get_unchecked(x * 3) as usize];
                    *stage.get_unchecked_mut(w + x) = lut[*row.get_unchecked(x * 3 + 1) as usize];
                    *stage.get_unchecked_mut(2 * w + x) =
                        lut[*row.get_unchecked(x * 3 + 2) as usize];
                }
            }
        }
    }
    /// Convert one u16 RGBA row to f32, keeping the interleaved layout.
    unsafe fn stage_x4(row: &[u16], stage: &mut [f32]);
    /// Horizontally convolve one staged 3-channel row into ring `slot`.
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
    unsafe fn horiz_x4(
        stage: &[f32],
        w: &Windows,
        ring: &mut [f32],
        plane: usize,
        slot: usize,
        dst_w: usize,
    );
    /// acc[x] = sum over taps of coeff * ring_row[x], taps ascending.
    unsafe fn vert(plane: &[f32], coeffs: &[f32], offs: &[usize], dst_w: usize, acc: &mut [f32]);
    /// Round-to-nearest f32 -> u16 with saturation, interleaving 3 planes.
    unsafe fn store_x3(acc: &[f32], dst_w: usize, out: &mut [u16]);
    /// Round-to-nearest f32 -> u16 with saturation, interleaving 4 planes.
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
    plane: usize,
    /// Source rows past this index influence no output row; they are
    /// accepted and dropped (the full-frame driver never touches them).
    last_needed: usize,
    next_row: usize,
    /// Staged rows not yet horizontally convolved (tail of `next_row`).
    pending: usize,
    /// f32s between consecutive staged rows in the batch buffer.
    stage_row_stride: usize,
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
        let plane = cap * dst_w;
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
        let stage_row_stride = (src_w + wh.stride) * stage_px;
        grow(&mut scratch.stage, stage_row_stride * K::HORIZ_BATCH);
        grow(&mut scratch.ring, plane * channels);
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
            plane,
            last_needed,
            next_row: 0,
            pending: 0,
            stage_row_stride,
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
    /// [`StreamResize::push_row`].
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
        self.after_stage(emit);
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
                *o = slot * self.dst_w;
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
            *slot = ((first + i) % self.cap) * self.dst_w;
        }
        let s = &mut self.scratch;
        // SAFETY: constructor verified K::detect(); slice lengths include
        // the zero-coefficient slack the padded tap-blocks may read.
        unsafe {
            if self.channels == 3 {
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
    let (pre, src, post) = unsafe { src_bytes.align_to::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 src");
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
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
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
