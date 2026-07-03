//! Resize backend micro-benchmark: fir (fast_image_resize) vs the
//! platform kernel, on the pipeline's u16 pixel shapes.
//! Usage: resize_bench [iters]

use fast_image_resize::images::{Image, ImageRef};
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

fn test_image(w: usize, h: usize, ch: usize) -> Vec<u16> {
    let mut seed = 0x2545F491u32;
    let mut px = Vec::with_capacity(w * h * ch);
    for i in 0..w * h * ch {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        px.push((((i * 7) as u32 + (seed >> 18)) & 0xFFFF) as u16);
    }
    px
}

fn bench(label: &str, iters: usize, mut f: impl FnMut()) {
    // warmup
    for _ in 0..3 {
        f();
    }
    let mut times: Vec<f64> = (0..iters)
        .map(|_| {
            let t = std::time::Instant::now();
            f();
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    times.sort_by(f64::total_cmp);
    println!(
        "{label}: median {:.2} ms  min {:.2} ms",
        times[iters / 2],
        times[0]
    );
}

fn main() {
    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);
    for (sw, sh, dw, dh, ch) in [
        (2040usize, 1356usize, 512usize, 340usize, 3usize), // AVIF harness shape (full decode)
        (1020, 678, 512, 340, 3),                           // JPEG post-DCT shape
        (870, 578, 512, 340, 3),                            // WebP margin-decode shape
        (2040, 1356, 512, 340, 4),                          // alpha variant
    ] {
        let src = test_image(sw, sh, ch);
        let src_bytes: &[u8] =
            unsafe { std::slice::from_raw_parts(src.as_ptr().cast(), src.len() * 2) };
        let px = if ch == 3 {
            PixelType::U16x3
        } else {
            PixelType::U16x4
        };
        let mut dst = vec![0u16; dw * dh * ch];

        let opts = ResizeOptions::new()
            .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
            .use_alpha(false);
        let mut resizer = Resizer::new();
        bench(&format!("fir  {sw}x{sh}->{dw}x{dh} x{ch}"), iters, || {
            let src_view = ImageRef::new(sw as u32, sh as u32, src_bytes, px).unwrap();
            let dst_bytes: &mut [u8] =
                unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
            let mut dst_view = Image::from_slice_u8(dw as u32, dh as u32, dst_bytes, px).unwrap();
            resizer.resize(&src_view, &mut dst_view, &opts).unwrap();
        });

        #[cfg(target_arch = "aarch64")]
        bench(&format!("neon {sw}x{sh}->{dw}x{dh} x{ch}"), iters, || {
            let dst_bytes: &mut [u8] =
                unsafe { std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2) };
            oximg::resize_neon::resize_u16_neon(src_bytes, sw, sh, dst_bytes, dw, dh, ch).unwrap();
        });
    }
}
