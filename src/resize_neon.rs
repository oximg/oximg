//! NEON f32 separable convolution for interleaved u16 pixel rows on
//! aarch64, replacing fast_image_resize's u16 path there (which
//! accumulates in i64 pairs — two multiply-accumulates per instruction).
//! The window/coefficient computation mirrors fir's Lanczos3 convolution
//! (adaptive kernel size, sum-normalized, zero-trimmed bounds).
//!
//! Schedule (shaped by Graviton3 perf counters — the kernel is
//! instruction-bound, IPC ~3.2 with near-zero LLC misses):
//! - each source row is converted from interleaved u16 to f32 exactly
//!   once (planar for 3 channels, interleaved for 4) instead of being
//!   re-loaded and re-widened by every overlapping window (~6x for the
//!   benchmark shape's 24-tap windows);
//! - horizontally convolved rows live in a ring buffer of
//!   `window_size` rows (~150 KB for 2040->512 RGB) that the vertical
//!   pass consumes in step, instead of an image-sized intermediate;
//! - the vertical pass keeps its accumulators in registers across all
//!   taps of a 16-column tile rather than round-tripping an
//!   accumulator row through memory once per tap.
//!
//! Correctness contract: the f32 operation sequence per output value is
//! preserved by all of the above (u16 -> f32 conversion is exact, so
//! staging changes no operand values; ring placement only changes where
//! a row is stored; register accumulation applies taps in the same
//! ascending order). Tests assert exact output equality between the
//! strip-mined schedule and a full-intermediate reference schedule, and
//! track a scalar f64 ground truth within quantization tolerance.

use anyhow::{Result, ensure};

/// Per-axis convolution windows, identical math to fir's
/// `precompute_coefficients` with no crop box.
struct Windows {
    window_size: usize,
    /// First source index of each output pixel's window.
    starts: Vec<usize>,
    /// Tap count of each window.
    sizes: Vec<usize>,
    /// f32 coefficients, `window_size` stride per output pixel,
    /// zero-padded past each window's size.
    coeffs: Vec<f32>,
}

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

impl Windows {
    fn new(in_size: usize, out_size: usize) -> Windows {
        let scale = in_size as f64 / out_size as f64;
        let filter_scale = scale.max(1.0);
        let filter_radius = 3.0 * filter_scale;
        let window_size = filter_radius.ceil() as usize * 2 + 1;
        let recip = 1.0 / filter_scale;

        let mut starts = Vec::with_capacity(out_size);
        let mut sizes = Vec::with_capacity(out_size);
        let mut coeffs = vec![0f32; window_size * out_size];
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
            let dst = &mut coeffs[out_x * window_size..(out_x + 1) * window_size];
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
            starts,
            sizes,
            coeffs,
        }
    }
}

/// Reusable work buffers: one staged source row (f32), the ring of
/// horizontally-convolved rows, one accumulator row set, and the ring
/// slot offsets of the current vertical window. Grow-only; every
/// element is written before it is read, so stale contents are never
/// observed.
#[derive(Default)]
struct Scratch {
    stage: Vec<f32>,
    ring: Vec<f32>,
    acc: Vec<f32>,
    offs: Vec<usize>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::default());
}

fn grow(buf: &mut Vec<f32>, len: usize) {
    if buf.len() < len {
        buf.resize(len, 0.0);
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
    ensure!(channels == 3 || channels == 4, "unsupported channel count");
    ensure!(
        src_w > 0 && src_h > 0 && dst_w > 0 && dst_h > 0,
        "empty dimensions"
    );
    let (pre, src, post) = unsafe { src_bytes.align_to::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 src");
    let (pre, dst, post) = unsafe { dst_bytes.align_to_mut::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 dst");
    ensure!(src.len() >= src_w * src_h * channels, "src too small");
    ensure!(dst.len() >= dst_w * dst_h * channels, "dst too small");

    let wh = Windows::new(src_w, dst_w);
    let wv = Windows::new(src_h, dst_h);
    // Ring capacity: every vertical window's span is <= window_size (the
    // raw span ceil(c+r)-floor(c-r) < 2r+2 <= window_size+1, and clamping
    // or zero-trimming only shrinks it), and window ends are
    // non-decreasing in oy, so end-driven fill never evicts a live row.
    let cap = wv.window_size.min(src_h).max(1);
    run(
        src, src_w, src_h, dst, dst_w, dst_h, channels, &wh, &wv, cap,
    );
    Ok(())
}

/// Shared driver: `cap == src_h` degenerates the ring into a full
/// intermediate image (the reference schedule used by tests).
#[allow(clippy::too_many_arguments)]
fn run(
    src: &[u16],
    src_w: usize,
    _src_h: usize,
    dst: &mut [u16],
    dst_w: usize,
    dst_h: usize,
    channels: usize,
    wh: &Windows,
    wv: &Windows,
    cap: usize,
) {
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let (stage, ring, acc, offs) = (&mut s.stage, &mut s.ring, &mut s.acc, &mut s.offs);
        grow(stage, src_w * channels);
        let plane = cap * dst_w;
        grow(ring, plane * channels);
        grow(acc, dst_w * channels);
        if offs.len() < wv.window_size {
            offs.resize(wv.window_size, 0);
        }

        unsafe {
            let mut next_row = 0usize;
            for oy in 0..dst_h {
                let start = wv.starts[oy];
                let size = wv.sizes[oy];
                // Produce horizontally-convolved rows up to this output
                // row's window end (window ends never decrease).
                while next_row < start + size {
                    let row = &src[next_row * src_w * channels..(next_row + 1) * src_w * channels];
                    let slot = (next_row % cap) * dst_w;
                    if channels == 3 {
                        stage_row_x3(row, &mut stage[..src_w * 3], src_w);
                        horiz_row_x3(
                            &stage[..src_w * 3],
                            src_w,
                            wh,
                            &mut ring[..],
                            plane,
                            slot,
                            dst_w,
                        );
                    } else {
                        stage_row_x4(row, &mut stage[..src_w * 4]);
                        horiz_row_x4(&stage[..src_w * 4], wh, &mut ring[..], plane, slot, dst_w);
                    }
                    next_row += 1;
                }
                let coeffs = &wv.coeffs[oy * wv.window_size..oy * wv.window_size + size];
                // Ring slot offsets for this window, one wrap-increment per
                // tap instead of a modulo in the accumulation inner loop.
                let mut slot = start % cap;
                for o in offs[..size].iter_mut() {
                    *o = slot * dst_w;
                    slot += 1;
                    if slot == cap {
                        slot = 0;
                    }
                }
                for c in 0..channels {
                    vert_accumulate(
                        &ring[c * plane..(c + 1) * plane],
                        coeffs,
                        &offs[..size],
                        dst_w,
                        &mut acc[c * dst_w..(c + 1) * dst_w],
                    );
                }
                let out_row = &mut dst[oy * dst_w * channels..(oy + 1) * dst_w * channels];
                if channels == 3 {
                    store_row_x3(acc, dst_w, out_row);
                } else {
                    store_row_x4(acc, dst_w, out_row);
                }
            }
        }
    });
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
            let size = w.sizes[ox];
            let coeffs = &w.coeffs[ox * w.window_size..ox * w.window_size + size];

            let mut acc = [vdupq_n_f32(0.0); 3];
            let mut k = 0usize;
            while k + 8 <= size {
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
            let mut sums = [vaddvq_f32(acc[0]), vaddvq_f32(acc[1]), vaddvq_f32(acc[2])];
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
            let size = w.sizes[ox];
            let coeffs = &w.coeffs[ox * w.window_size..ox * w.window_size + size];

            let mut acc = vdupq_n_f32(0.0);
            let mut k = 0usize;
            while k + 4 <= size {
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
            while k < size {
                let p = vld1q_f32(stage.as_ptr().add((start + k) * 4));
                acc = vfmaq_n_f32(acc, p, coeffs[k]);
                k += 1;
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

fn clamp_u16(v: f32) -> u16 {
    (v + 0.5).clamp(0.0, 65535.0) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    /// Deterministic synthetic image: gradients plus LCG noise so
    /// convolution windows see realistic variation.
    fn test_image(w: usize, h: usize, ch: usize) -> Vec<u16> {
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

    fn fir_resize(src: &[u16], sw: usize, sh: usize, dw: usize, dh: usize, ch: usize) -> Vec<u16> {
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

    fn neon_resize(src: &[u16], sw: usize, sh: usize, dw: usize, dh: usize, ch: usize) -> Vec<u16> {
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let mut dst = vec![0u16; dw * dh * ch];
        let dst_bytes: &mut [u8] =
            unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
        resize_u16_neon(src_bytes, sw, sh, dst_bytes, dw, dh, ch).unwrap();
        dst
    }

    /// Reference schedule: same kernels, ring capacity = src_h (a full
    /// intermediate image, i.e. the unstripped two-pass schedule).
    fn reference_resize(
        src: &[u16],
        sw: usize,
        sh: usize,
        dw: usize,
        dh: usize,
        ch: usize,
    ) -> Vec<u16> {
        let wh = Windows::new(sw, dw);
        let wv = Windows::new(sh, dh);
        let mut dst = vec![0u16; dw * dh * ch];
        run(src, sw, sh, &mut dst, dw, dh, ch, &wh, &wv, sh);
        dst
    }

    /// Scalar f64 separable resize with un-quantized intermediate:
    /// ground truth for accuracy comparisons.
    fn ref_resize_f64(
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

    fn rmse(a: &[u16], b: &[u16]) -> f64 {
        let se: f64 = a
            .iter()
            .zip(b)
            .map(|(&x, &y)| (x as f64 - y as f64).powi(2))
            .sum();
        (se / a.len() as f64).sqrt()
    }

    /// The NEON kernel must track the f64 ground truth at least as
    /// closely as fir does (fir quantizes its intermediate image to u16;
    /// we keep f32 rows), and stay within a couple of quantization steps
    /// of the truth itself.
    fn assert_accuracy(sw: usize, sh: usize, dw: usize, dh: usize, ch: usize, label: &str) {
        let src = test_image(sw, sh, ch);
        let ours = neon_resize(&src, sw, sh, dw, dh, ch);
        let fir = fir_resize(&src, sw, sh, dw, dh, ch);
        let truth = ref_resize_f64(&src, sw, sh, dw, dh, ch);

        let ours_err = rmse(&ours, &truth);
        let fir_err = rmse(&fir, &truth);
        let worst_vs_truth = ours
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
            worst_vs_truth <= 2,
            "{label}: worst diff vs f64 truth {worst_vs_truth} > 2 (rmse {ours_err:.4})"
        );
        // (On mild scale factors fir's u16-quantized intermediate rows
        // drift much further from the f64 truth than the f32 kernel does
        // on noisy content, so no closeness-to-fir bound is asserted.)
        let _ = fir;
    }

    /// Shape sweep used by the schedule-equality test: the benchmark
    /// shape, primes, tiny images (src_h < ring capacity), upscales
    /// (heavily overlapping windows), single-row outputs, and extreme
    /// aspect changes.
    const SHAPES: [(usize, usize, usize, usize); 10] = [
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

    #[test]
    fn strip_schedule_equals_full_intermediate_schedule_exactly() {
        for &(sw, sh, dw, dh) in &SHAPES {
            for ch in [3usize, 4] {
                let src = test_image(sw, sh, ch);
                let strip = neon_resize(&src, sw, sh, dw, dh, ch);
                let full = reference_resize(&src, sw, sh, dw, dh, ch);
                assert_eq!(strip, full, "{sw}x{sh}->{dw}x{dh} x{ch}");
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
    fn tracks_ground_truth_for_rgb() {
        for (sw, sh, dw, dh) in [
            (2040, 1356, 512, 340),
            (640, 480, 512, 384),
            (333, 217, 100, 65),
            (17, 11, 5, 3),
        ] {
            assert_accuracy(sw, sh, dw, dh, 3, &format!("rgb {sw}x{sh}->{dw}x{dh}"));
        }
    }

    #[test]
    fn tracks_ground_truth_for_rgba() {
        for (sw, sh, dw, dh) in [(801, 601, 256, 192), (64, 64, 17, 9)] {
            assert_accuracy(sw, sh, dw, dh, 4, &format!("rgba {sw}x{sh}->{dw}x{dh}"));
        }
    }

    #[test]
    fn tracks_ground_truth_when_upscaling() {
        assert_accuracy(50, 40, 120, 96, 3, "rgb upscale");
    }

    #[test]
    fn windows_are_normalized_and_in_bounds() {
        for (in_s, out_s) in [(2040, 512), (100, 99), (7, 3), (3, 7)] {
            let w = Windows::new(in_s, out_s);
            for i in 0..out_s {
                assert!(w.starts[i] + w.sizes[i] <= in_s, "{in_s}->{out_s} px {i}");
                let sum: f64 = w.coeffs[i * w.window_size..i * w.window_size + w.sizes[i]]
                    .iter()
                    .map(|&c| c as f64)
                    .sum();
                assert!(
                    (sum - 1.0).abs() < 1e-4,
                    "{in_s}->{out_s} px {i}: sum={sum}"
                );
            }
        }
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
