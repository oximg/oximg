//! x86-64 resize backend micro-benchmark: fir (fast_image_resize) vs
//! pic-scale (3-channel only) vs the AVX2 kernel, on the pipeline's u16
//! pixel shapes.
//! Usage: resize_bench_x86 [iters]

#[cfg(not(target_arch = "x86_64"))]
fn main() {
    eprintln!("resize_bench_x86 requires an x86-64 host");
}

#[cfg(target_arch = "x86_64")]
fn test_image(w: usize, h: usize, ch: usize) -> Vec<u16> {
    let mut seed = 0x2545F491u32;
    let mut px = Vec::with_capacity(w * h * ch);
    for i in 0..w * h * ch {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        px.push((((i * 7) as u32 + (seed >> 18)) & 0xFFFF) as u16);
    }
    px
}

#[cfg(target_arch = "x86_64")]
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

/// Same call sequence as the pipeline's `resize_u16x3_picscale`
/// (plan + resample per call, single-threaded).
#[cfg(target_arch = "x86_64")]
fn picscale_resize(src: &[u16], sw: usize, sh: usize, dst: &mut [u16], dw: usize, dh: usize) {
    use pic_scale::{ImageStore, ImageStoreMut, ResamplingFunction, Scaler, ThreadingPolicy};
    let src_store = ImageStore::<u16, 3>::from_slice(src, sw, sh).unwrap();
    let mut dst_store = ImageStoreMut::<u16, 3>::from_slice(dst, dw, dh).unwrap();
    dst_store.bit_depth = 16;
    let scaler =
        Scaler::new(ResamplingFunction::Lanczos3).set_threading_policy(ThreadingPolicy::Single);
    let plan = scaler
        .plan_rgb_resampling16(src_store.size(), dst_store.size(), 16)
        .unwrap();
    plan.resample(&src_store, &mut dst_store).unwrap();
}

#[cfg(target_arch = "x86_64")]
fn main() {
    use fast_image_resize::images::{Image, ImageRef};
    use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};

    let iters: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(20);
    let avx2 =
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma");
    if !avx2 {
        eprintln!("host lacks avx2+fma: skipping the AVX2 kernel rows");
    }
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
        bench(
            &format!("fir       {sw}x{sh}->{dw}x{dh} x{ch}"),
            iters,
            || {
                let src_view = ImageRef::new(sw as u32, sh as u32, src_bytes, px).unwrap();
                let dst_bytes: &mut [u8] = unsafe {
                    std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2)
                };
                let mut dst_view =
                    Image::from_slice_u8(dw as u32, dh as u32, dst_bytes, px).unwrap();
                resizer.resize(&src_view, &mut dst_view, &opts).unwrap();
            },
        );

        if ch == 3 {
            let mut ps_dst = vec![0u16; dw * dh * ch];
            bench(
                &format!("pic-scale {sw}x{sh}->{dw}x{dh} x{ch}"),
                iters,
                || picscale_resize(&src, sw, sh, &mut ps_dst, dw, dh),
            );
        }

        if avx2 {
            bench(
                &format!("avx2      {sw}x{sh}->{dw}x{dh} x{ch}"),
                iters,
                || {
                    let dst_bytes: &mut [u8] = unsafe {
                        std::slice::from_raw_parts_mut(dst.as_mut_ptr().cast(), dst.len() * 2)
                    };
                    oximg::resize_avx2::resize_u16_avx2(src_bytes, sw, sh, dst_bytes, dw, dh, ch)
                        .unwrap();
                },
            );
        }
    }
}
