use anyhow::{Context, Result};
use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use mozjpeg::{ColorSpace, Compress, Decompress};
use std::sync::OnceLock;

/// sRGB u8 -> linear u16 (exact transfer function)
fn fwd_lut() -> &'static [u16; 256] {
    static LUT: OnceLock<[u16; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0u16; 256];
        for (i, v) in t.iter_mut().enumerate() {
            let s = i as f64 / 255.0;
            let lin = if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            };
            *v = (lin * 65535.0 + 0.5) as u16;
        }
        t
    })
}

/// linear u16 -> sRGB u8 (64KB global LUT, single lookup per component)
fn back_lut() -> &'static [u8; 65536] {
    static LUT: OnceLock<Box<[u8; 65536]>> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = vec![0u8; 65536].into_boxed_slice();
        for (i, v) in t.iter_mut().enumerate() {
            let lin = i as f64 / 65535.0;
            let s = if lin <= 0.003_130_8 {
                12.92 * lin
            } else {
                1.055 * lin.powf(1.0 / 2.4) - 0.055
            };
            *v = (s * 255.0 + 0.5) as u8;
        }
        t.try_into().unwrap()
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Encoder {
    /// jpegli: trellis-class compression at roughly half the CPU of
    /// mozjpeg's trellis path. The default.
    Jpegli,
    /// mozjpeg fastest profile + optimized Huffman: libjpeg-turbo-class
    /// output at the lowest encode cost.
    MozFast,
    /// mozjpeg trellis + progressive: smallest mozjpeg output.
    MozSmall,
}

impl Encoder {
    /// Parse the PRESET env value; unknown values fall back to the default.
    pub fn from_preset(preset: &str) -> Self {
        match preset {
            "fast" => Encoder::MozFast,
            "small" => Encoder::MozSmall,
            _ => Encoder::Jpegli,
        }
    }
}

pub struct Params {
    pub max_width: u32,
    pub max_height: u32,
    pub quality: f32,
    pub encoder: Encoder,
    /// Thread count for the resize stage (1 = single-threaded). Band threads
    /// are short bursts that deliberately bypass the CPU semaphore; they
    /// trade mild transient oversubscription for lower latency at light
    /// load.
    pub parallel: usize,
}

/// Proportionally shrink to fit within max_w x max_h (never enlarges).
fn fit_dims(src_w: usize, src_h: usize, max_w: u32, max_h: u32) -> (usize, usize) {
    let scale = f64::min(
        max_w as f64 / src_w as f64,
        f64::min(max_h as f64 / src_h as f64, 1.0),
    );
    (
        ((src_w as f64 * scale).round() as usize).max(1),
        ((src_h as f64 * scale).round() as usize).max(1),
    )
}

/// Pick the smallest num (num/8 DCT scaling) whose decoded size stays at
/// or above target size x margin. libjpeg's scaled size is
/// ceil(dim * num / 8).
/// margin=1.0 is fastest (greedy), but DCT truncation destroys the
/// high-frequency headroom Lanczos needs and end-to-end quality drops
/// visibly; around margin 2.0 the remaining shrink is done by SIMD
/// Lanczos and quality approaches a full decode.
fn dct_scale_num(src_w: usize, src_h: usize, dst_w: usize, dst_h: usize, margin: f64) -> u8 {
    let (need_w, need_h) = (
        (dst_w as f64 * margin).ceil() as usize,
        (dst_h as f64 * margin).ceil() as usize,
    );
    for num in 1..=8u8 {
        let sw = (src_w * num as usize).div_ceil(8);
        let sh = (src_h * num as usize).div_ceil(8);
        if (sw >= need_w && sh >= need_h) || (sw >= src_w && sh >= src_h) {
            return num;
        }
    }
    8
}

fn dct_margin() -> f64 {
    std::env::var("OXIMG_DCT_MARGIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.7)
}

pub fn process(jpeg: &[u8], p: &Params) -> Result<Vec<u8>> {
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let dec = Decompress::new_mem(jpeg).context("invalid JPEG")?;
        let (dst_w, dst_h) = decode_resize(s, dec, p.max_width, p.max_height, p.parallel)?;
        encode(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, p)
    })
}

/// Streaming variant: decode straight from the file instead of buffering
/// the whole JPEG on the heap. For large sources (10MB+) under high
/// concurrency this saves concurrency x file-size of resident memory;
/// entropy decoding is a sequential read anyway, so the page cache
/// serves it fine.
pub fn process_path(path: &std::path::Path, p: &Params) -> Result<Vec<u8>> {
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let dec = Decompress::new_path(path).context("open/parse JPEG")?;
        let (dst_w, dst_h) = decode_resize(s, dec, p.max_width, p.max_height, p.parallel)?;
        encode(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, p)
    })
}

fn http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into()
    })
}

fn max_source_bytes() -> u64 {
    std::env::var("OXIMG_MAX_SOURCE_BYTES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64 * 1024 * 1024)
}

/// Remote-source variant: stream the HTTP response body straight into the
/// decoder — decoding overlaps the download and the source is never
/// buffered whole, same as the file path.
pub fn process_url(url: &str, p: &Params) -> Result<Vec<u8>> {
    let resp = http_agent().get(url).call().map_err(|e| match e {
        // Preserve source 404s so the HTTP layer can pass them through.
        ureq::Error::StatusCode(404) => anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "source returned 404",
        )),
        other => anyhow::Error::new(other).context("fetch source"),
    })?;
    let reader = std::io::BufReader::new(std::io::Read::take(
        resp.into_body().into_reader(),
        max_source_bytes(),
    ));
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let dec = Decompress::new_reader(reader).context("parse JPEG")?;
        let (dst_w, dst_h) = decode_resize(s, dec, p.max_width, p.max_height, p.parallel)?;
        encode(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, p)
    })
}

thread_local! {
    // Per-blocking-pool-thread reusable work buffers: at 600 RPS this
    // removes ~4GB/s of malloc/free traffic (decode buffer, u16
    // intermediate image, Resizer internal temporaries).
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::default());
}

#[derive(Default)]
struct Scratch {
    chunk8: Vec<u8>,
    src16: Vec<u16>,
    dst16: Vec<u16>,
    // Final RGB pixels also live in scratch: output sizes vary per request
    // (every distinct target width is a distinct allocation size), and that
    // churn is what the allocator retains across thread heaps.
    out8: Vec<u8>,
    resizer: Option<Resizer>,
}

fn u16_as_bytes(buf: &[u16]) -> &[u8] {
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 2) }
}

fn u16_as_bytes_mut(buf: &mut [u16]) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast(), buf.len() * 2) }
}

/// Decode (with DCT shrink-on-load) + SIMD resize; returns RGB pixels and final dimensions.
pub fn decode_and_resize(
    jpeg: &[u8],
    max_w: u32,
    max_h: u32,
    parallel: usize,
) -> Result<(Vec<u8>, usize, usize)> {
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let dec = Decompress::new_mem(jpeg).context("invalid JPEG")?;
        let (dst_w, dst_h) = decode_resize(s, dec, max_w, max_h, parallel)?;
        Ok((s.out8[..dst_w * dst_h * 3].to_vec(), dst_w, dst_h))
    })
}

/// Split dst into row bands; each thread does a coordinate-consistent
/// partial resize via fir's crop box. The crop only affects coordinate
/// mapping — kernel taps still sample the full src, so band seams match
/// the single-threaded output (verified by
/// tests::band_resize_matches_single_thread).
#[allow(clippy::too_many_arguments)]
fn resize_bands(
    src_bytes: &[u8],
    dec_w: usize,
    dec_h: usize,
    dst_bytes: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    px: PixelType,
    threads: usize,
    fallback: &mut Option<Resizer>,
) -> Result<()> {
    let opts = ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3));
    let src_view =
        fast_image_resize::images::ImageRef::new(dec_w as u32, dec_h as u32, src_bytes, px)?;

    if threads <= 1 || dst_h < 2 * threads {
        let mut dst_view = Image::from_slice_u8(dst_w as u32, dst_h as u32, dst_bytes, px)?;
        let resizer = fallback.get_or_insert_with(Resizer::new);
        resizer.resize(&src_view, &mut dst_view, &opts)?;
        return Ok(());
    }

    let row_bytes = dst_w * px.size();
    let rows_per = dst_h.div_ceil(threads);
    let sy = dec_h as f64 / dst_h as f64;
    std::thread::scope(|sc| -> Result<()> {
        let mut handles = Vec::new();
        for (i, band) in dst_bytes.chunks_mut(rows_per * row_bytes).enumerate() {
            let band_h = band.len() / row_bytes;
            let crop_top = (i * rows_per) as f64 * sy;
            let crop_h = band_h as f64 * sy;
            let src_view = &src_view;
            handles.push(sc.spawn(move || -> Result<()> {
                let mut dst_view = Image::from_slice_u8(dst_w as u32, band_h as u32, band, px)?;
                Resizer::new().resize(
                    src_view,
                    &mut dst_view,
                    &opts.crop(0.0, crop_top, dec_w as f64, crop_h),
                )?;
                Ok(())
            }));
        }
        for h in handles {
            h.join().expect("resize band panicked")?;
        }
        Ok(())
    })
}

fn decode_resize<R: std::io::BufRead>(
    s: &mut Scratch,
    mut dec: Decompress<R>,
    max_w: u32,
    max_h: u32,
    parallel: usize,
) -> Result<(usize, usize)> {
    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();

    let (src_w, src_h) = dec.size();
    let (dst_w, dst_h) = fit_dims(src_w, src_h, max_w, max_h);

    dec.scale(dct_scale_num(src_w, src_h, dst_w, dst_h, dct_margin()));

    let mut started = dec.rgb().context("decode start failed")?;
    let (dec_w, dec_h) = (started.width(), started.height());
    let row_bytes = dec_w * 3;
    let linear = linear_light() && (dec_w, dec_h) != (dst_w, dst_h);

    if (dec_w, dec_h) == (dst_w, dst_h) {
        // Decoded size is already the target size: output directly; a
        // linear round-trip would be pure loss.
        s.out8.resize(dec_w * dec_h * 3, 0);
        started
            .read_scanlines_into(&mut s.out8)
            .context("decode failed")?;
        started.finish().context("decode finish failed")?;
        if timing {
            eprintln!(
                "timing decode({dec_w}x{dec_h})={:.1}ms resize=0 (exact)",
                t0.elapsed().as_secs_f64() * 1e3
            );
        }
        return Ok((dst_w, dst_h));
    }

    if linear {
        // Decode in chunks and apply the sRGB u8 -> linear u16 LUT on the
        // fly: each chunk stays in L2, saving a second full-image memory
        // pass.
        let fwd = fwd_lut();
        s.src16.resize(dec_w * dec_h * 3, 0);
        let chunk_rows = (256 * 1024 / row_bytes).clamp(1, dec_h);
        s.chunk8.resize(chunk_rows * row_bytes, 0);
        let mut filled = 0usize; // number of u16 components filled so far
        while filled < dec_w * dec_h * 3 {
            let want = (dec_h * row_bytes - filled).min(chunk_rows * row_bytes);
            let got = started
                .read_scanlines_into(&mut s.chunk8[..want])
                .context("decode failed")?
                .len();
            anyhow::ensure!(got > 0, "decoder returned no scanlines");
            for (d, src) in s.src16[filled..filled + got]
                .iter_mut()
                .zip(&s.chunk8[..got])
            {
                *d = fwd[*src as usize];
            }
            filled += got;
        }
        started.finish().context("decode finish failed")?;
        let t_decode = t0.elapsed();

        let t1 = std::time::Instant::now();
        s.dst16.resize(dst_w * dst_h * 3, 0);
        resize_bands(
            u16_as_bytes(&s.src16),
            dec_w,
            dec_h,
            u16_as_bytes_mut(&mut s.dst16),
            dst_w,
            dst_h,
            PixelType::U16x3,
            parallel,
            &mut s.resizer,
        )?;

        let back = back_lut();
        s.out8.resize(dst_w * dst_h * 3, 0);
        for (d, src) in s.out8.iter_mut().zip(&s.dst16) {
            *d = back[*src as usize];
        }
        if timing {
            eprintln!(
                "timing decode+fwd({dec_w}x{dec_h})={:.1}ms resize+back={:.1}ms",
                t_decode.as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok((dst_w, dst_h))
    } else {
        // Resize directly in sRGB space (speed mode)
        s.chunk8.resize(dec_w * dec_h * 3, 0);
        started
            .read_scanlines_into(&mut s.chunk8)
            .context("decode failed")?;
        started.finish().context("decode finish failed")?;
        let t_decode = t0.elapsed();

        let t1 = std::time::Instant::now();
        s.out8.resize(dst_w * dst_h * 3, 0);
        resize_bands(
            &s.chunk8,
            dec_w,
            dec_h,
            &mut s.out8,
            dst_w,
            dst_h,
            PixelType::U8x3,
            parallel,
            &mut s.resizer,
        )?;
        if timing {
            eprintln!(
                "timing decode({dec_w}x{dec_h})={:.1}ms resize={:.1}ms",
                t_decode.as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok((dst_w, dst_h))
    }
}

fn linear_light() -> bool {
    std::env::var("OXIMG_RESIZE").as_deref() != Ok("srgb")
}

pub fn encode(rgb: &[u8], w: usize, h: usize, p: &Params) -> Result<Vec<u8>> {
    if p.encoder == Encoder::Jpegli {
        return encode_jpegli(rgb, w, h, p.quality);
    }
    let mut comp = Compress::new(ColorSpace::JCS_RGB);
    if p.encoder == Encoder::MozFast {
        comp.set_fastest_defaults();
        // The fastest profile disables even Huffman optimization, making
        // output ~2% larger than plain turbo; turning it back on costs one
        // extra coefficient-statistics pass, negligible at thumbnail sizes.
        comp.set_optimize_coding(true);
    }
    comp.set_size(w, h);
    comp.set_quality(p.quality);
    let mut started = comp.start_compress(Vec::with_capacity(64 * 1024))?;
    started.write_scanlines(rgb)?;
    Ok(started.finish()?)
}

/// jpegli encode via its libjpeg-compatible API (symbols are
/// `jpegli_`-prefixed, so it links alongside mozjpeg without conflicts).
fn encode_jpegli(rgb: &[u8], w: usize, h: usize, quality: f32) -> Result<Vec<u8>> {
    let mut comp = jpegli::Compress::new(jpegli::ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(quality);
    // cjpegli emits progressive by default; the libjpeg-compat layer does
    // not. Progressive is worth several percent at these sizes.
    comp.set_progressive_mode();
    let mut started = comp.start_compress(Vec::with_capacity(64 * 1024))?;
    started.write_scanlines(rgb)?;
    Ok(started.finish()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_dims_shrinks_proportionally() {
        assert_eq!(fit_dims(7360, 4912, 500, 500), (500, 334));
        assert_eq!(fit_dims(4912, 7360, 500, 500), (334, 500));
    }

    #[test]
    fn fit_dims_never_enlarges() {
        assert_eq!(fit_dims(300, 200, 500, 500), (300, 200));
    }

    #[test]
    fn band_resize_matches_single_thread() {
        // Synthetic gradient image; verify 2/3-band parallel resize is
        // byte-identical to the single-threaded output.
        let (sw, sh, dw, dh) = (317usize, 211usize, 123usize, 81usize);
        let src: Vec<u8> = (0..sw * sh * 3).map(|i| ((i * 7919) % 251) as u8).collect();
        let mut single = vec![0u8; dw * dh * 3];
        resize_bands(
            &src,
            sw,
            sh,
            &mut single,
            dw,
            dh,
            PixelType::U8x3,
            1,
            &mut None,
        )
        .unwrap();
        for threads in [2, 3] {
            let mut banded = vec![0u8; dw * dh * 3];
            resize_bands(
                &src,
                sw,
                sh,
                &mut banded,
                dw,
                dh,
                PixelType::U8x3,
                threads,
                &mut None,
            )
            .unwrap();
            assert_eq!(single, banded, "threads={threads} output differs");
        }
    }

    #[test]
    fn dct_scale_picks_smallest_sufficient() {
        // 7360 * 1/8 = 920 >= 500 -> num = 1
        assert_eq!(dct_scale_num(7360, 4912, 500, 334, 1.0), 1);
        // 1000 * 4/8 = 500 >= 500, 667*4/8=334 >= 334 -> num = 4
        assert_eq!(dct_scale_num(1000, 667, 500, 334, 1.0), 4);
        // already at target size -> no scaling
        assert_eq!(dct_scale_num(500, 334, 500, 334, 1.0), 8);
    }
}
