//! NEON f32 separable convolution for interleaved u16 pixel rows on
//! aarch64. Replaces fast_image_resize's u16 path there: fir accumulates
//! in i64 pairs (two multiply-accumulates per instruction), while f32
//! accumulation runs four FMA lanes per instruction on the same window
//! math. The window/coefficient computation mirrors fir's Lanczos3
//! convolution (adaptive kernel size, sum-normalized, zero-trimmed
//! bounds) so output stays within one quantization step of the fir
//! path; the f32 intermediate rows are not quantized between passes,
//! which fir's u16 intermediate is.

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

thread_local! {
    /// Planar f32 intermediate (dst_w x src_h per channel) plus one
    /// accumulator row set, reused across requests.
    static SCRATCH: std::cell::RefCell<(Vec<f32>, Vec<f32>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new())) };
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
    let (pre, src, post) = unsafe { src_bytes.align_to::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 src");
    let (pre, dst, post) = unsafe { dst_bytes.align_to_mut::<u16>() };
    ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 dst");
    ensure!(src.len() >= src_w * src_h * channels, "src too small");
    ensure!(dst.len() >= dst_w * dst_h * channels, "dst too small");

    let wh = Windows::new(src_w, dst_w);
    let wv = Windows::new(src_h, dst_h);

    SCRATCH.with(|s| {
        let (mid, acc) = &mut *s.borrow_mut();
        let plane = dst_w * src_h;
        mid.clear();
        mid.resize(plane * channels, 0.0);
        acc.clear();
        acc.resize(dst_w * channels, 0.0);

        unsafe {
            // Horizontal pass: interleaved u16 -> planar f32.
            for y in 0..src_h {
                let row = &src[y * src_w * channels..(y + 1) * src_w * channels];
                if channels == 3 {
                    horiz_row_x3(row, &wh, &mut mid[..], plane, y * dst_w, dst_w);
                } else {
                    horiz_row_x4(row, &wh, &mut mid[..], plane, y * dst_w, dst_w);
                }
            }
            // Vertical pass: planar f32 -> interleaved u16.
            for oy in 0..dst_h {
                let start = wv.starts[oy];
                let size = wv.sizes[oy];
                let coeffs = &wv.coeffs[oy * wv.window_size..oy * wv.window_size + size];
                for c in 0..channels {
                    vert_accumulate(
                        &mid[c * plane..(c + 1) * plane],
                        coeffs,
                        start,
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
        Ok(())
    })
}

/// One horizontal row, 3 channels: deinterleave 8 taps at a time and
/// accumulate each channel in a f32x4 register.
#[target_feature(enable = "neon")]
unsafe fn horiz_row_x3(
    row: &[u16],
    w: &Windows,
    mid: &mut [f32],
    plane: usize,
    mid_row: usize,
    dst_w: usize,
) {
    unsafe {
        use std::arch::aarch64::*;
        for ox in 0..dst_w {
            let start = w.starts[ox];
            let size = w.sizes[ox];
            let coeffs = &w.coeffs[ox * w.window_size..ox * w.window_size + size];

            let mut acc = [vdupq_n_f32(0.0); 3];
            let mut k = 0usize;
            while k + 8 <= size {
                let px = vld3q_u16(row.as_ptr().add((start + k) * 3));
                let c_lo = vld1q_f32(coeffs.as_ptr().add(k));
                let c_hi = vld1q_f32(coeffs.as_ptr().add(k + 4));
                macro_rules! ch {
                    ($i:tt) => {{
                        let lo = vcvtq_f32_u32(vmovl_u16(vget_low_u16(px.$i)));
                        let hi = vcvtq_f32_u32(vmovl_u16(vget_high_u16(px.$i)));
                        acc[$i] = vfmaq_f32(acc[$i], lo, c_lo);
                        acc[$i] = vfmaq_f32(acc[$i], hi, c_hi);
                    }};
                }
                ch!(0);
                ch!(1);
                ch!(2);
                k += 8;
            }
            let mut sums = [vaddvq_f32(acc[0]), vaddvq_f32(acc[1]), vaddvq_f32(acc[2])];
            while k < size {
                let c = coeffs[k];
                let p = (start + k) * 3;
                sums[0] += row[p] as f32 * c;
                sums[1] += row[p + 1] as f32 * c;
                sums[2] += row[p + 2] as f32 * c;
                k += 1;
            }
            mid[mid_row + ox] = sums[0];
            mid[plane + mid_row + ox] = sums[1];
            mid[2 * plane + mid_row + ox] = sums[2];
        }
    }
}

/// One horizontal row, 4 channels: each pixel is a natural f32x4 lane
/// group; four taps share one coefficient vector via lane-indexed FMA.
#[target_feature(enable = "neon")]
unsafe fn horiz_row_x4(
    row: &[u16],
    w: &Windows,
    mid: &mut [f32],
    plane: usize,
    mid_row: usize,
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
                        let p = vcvtq_f32_u32(vmovl_u16(vld1_u16(
                            row.as_ptr().add((start + k + $j) * 4),
                        )));
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
                let p = vcvtq_f32_u32(vmovl_u16(vld1_u16(row.as_ptr().add((start + k) * 4))));
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
                mid[c * plane + mid_row + ox] = *v;
            }
        }
    }
}

/// acc[x] = sum over taps of coeff * plane_row[x]; full-width FMA.
#[target_feature(enable = "neon")]
unsafe fn vert_accumulate(
    plane: &[f32],
    coeffs: &[f32],
    start: usize,
    dst_w: usize,
    acc: &mut [f32],
) {
    unsafe {
        use std::arch::aarch64::*;
        acc.fill(0.0);
        for (k, &c) in coeffs.iter().enumerate() {
            let src_row = &plane[(start + k) * dst_w..(start + k + 1) * dst_w];
            let cv = vdupq_n_f32(c);
            let mut x = 0usize;
            while x + 16 <= dst_w {
                let a0 = vfmaq_f32(
                    vld1q_f32(acc.as_ptr().add(x)),
                    vld1q_f32(src_row.as_ptr().add(x)),
                    cv,
                );
                let a1 = vfmaq_f32(
                    vld1q_f32(acc.as_ptr().add(x + 4)),
                    vld1q_f32(src_row.as_ptr().add(x + 4)),
                    cv,
                );
                let a2 = vfmaq_f32(
                    vld1q_f32(acc.as_ptr().add(x + 8)),
                    vld1q_f32(src_row.as_ptr().add(x + 8)),
                    cv,
                );
                let a3 = vfmaq_f32(
                    vld1q_f32(acc.as_ptr().add(x + 12)),
                    vld1q_f32(src_row.as_ptr().add(x + 12)),
                    cv,
                );
                vst1q_f32(acc.as_mut_ptr().add(x), a0);
                vst1q_f32(acc.as_mut_ptr().add(x + 4), a1);
                vst1q_f32(acc.as_mut_ptr().add(x + 8), a2);
                vst1q_f32(acc.as_mut_ptr().add(x + 12), a3);
                x += 16;
            }
            while x + 4 <= dst_w {
                let a = vfmaq_f32(
                    vld1q_f32(acc.as_ptr().add(x)),
                    vld1q_f32(src_row.as_ptr().add(x)),
                    cv,
                );
                vst1q_f32(acc.as_mut_ptr().add(x), a);
                x += 4;
            }
            while x < dst_w {
                acc[x] += src_row[x] * c;
                x += 1;
            }
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
}
