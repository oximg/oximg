use anyhow::{Context, Result};
use fast_image_resize::images::Image;
use fast_image_resize::{FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer};
use mozjpeg::{ColorSpace, Compress, Decompress};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicUsize, Ordering};

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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
}

impl ImageFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Avif => "image/avif",
        }
    }

    /// Detect the format from the first bytes; extensions are not trusted.
    fn sniff(header: &[u8; 12]) -> Option<ImageFormat> {
        if header.starts_with(&[0xFF, 0xD8]) {
            Some(ImageFormat::Jpeg)
        } else if header.starts_with(b"\x89PNG\r\n\x1a\n") {
            Some(ImageFormat::Png)
        } else if &header[0..4] == b"RIFF" && &header[8..12] == b"WEBP" {
            Some(ImageFormat::Webp)
        } else if &header[4..8] == b"ftyp"
            && (&header[8..12] == b"avif" || &header[8..12] == b"avis")
        {
            Some(ImageFormat::Avif)
        } else {
            None
        }
    }
}

/// Cheap header probe: format + source dimensions without decoding pixels.
pub fn probe(bytes: &[u8]) -> Result<(ImageFormat, usize, usize)> {
    let mut header = [0u8; 12];
    anyhow::ensure!(bytes.len() >= 12, "source too short");
    header.copy_from_slice(&bytes[..12]);
    let format = ImageFormat::sniff(&header).context("unsupported image format")?;
    match format {
        ImageFormat::Jpeg => {
            let dec = Decompress::new_mem(bytes).context("parse JPEG")?;
            let (w, h) = dec.size();
            Ok((format, w, h))
        }
        ImageFormat::Png => {
            let mut r = png::Decoder::new(std::io::Cursor::new(bytes))
                .read_info()
                .context("parse PNG")?;
            let info = r.info();
            let dims = (info.width as usize, info.height as usize);
            let _ = r.next_row();
            Ok((format, dims.0, dims.1))
        }
        ImageFormat::Webp => unsafe {
            use libwebp_sys as w;
            let mut features: w::WebPBitstreamFeatures = std::mem::zeroed();
            let status = w::WebPGetFeatures(bytes.as_ptr(), bytes.len(), &mut features);
            anyhow::ensure!(
                status == w::VP8StatusCode::VP8_STATUS_OK,
                "parse WebP header"
            );
            Ok((format, features.width as usize, features.height as usize))
        },
        #[cfg(feature = "avif")]
        ImageFormat::Avif => {
            let (w, h) = crate::avif::probe_avif(bytes)?;
            Ok((format, w, h))
        }
        #[cfg(not(feature = "avif"))]
        ImageFormat::Avif => anyhow::bail!("AVIF support is not enabled in this build"),
    }
}

pub fn process(bytes: &[u8], p: &Params) -> Result<(Vec<u8>, ImageFormat)> {
    process_reader(std::io::Cursor::new(bytes), p)
}

/// Sniff the source format, then resize + re-encode in the same format.
/// JPEG keeps its fully streaming decode path; PNG streams through the
/// png crate; WebP requires the whole compressed source in memory
/// (libwebp has no incremental one-shot API).
fn process_reader<R: std::io::Read>(mut reader: R, p: &Params) -> Result<(Vec<u8>, ImageFormat)> {
    let mut header = [0u8; 12];
    std::io::Read::read_exact(&mut reader, &mut header).context("source too short")?;
    let format = ImageFormat::sniff(&header).context("unsupported image format")?;
    let reader = std::io::BufReader::new(std::io::Read::chain(&header[..], reader));

    let _active = ActiveGuard::enter();
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        match format {
            ImageFormat::Jpeg => {
                let dec = Decompress::new_reader(reader).context("parse JPEG")?;
                // Fused decode/resize+encode: on unless disabled (see
                // overlap_gate). Band-parallel resize keeps the serial
                // path so OXIMG_PAR semantics are unchanged.
                let fuse_quality =
                    (p.encoder == Encoder::Jpegli && p.parallel <= 1 && overlap_gate())
                        .then_some(p.quality);
                let out = match decode_resize(
                    s,
                    dec,
                    p.max_width,
                    p.max_height,
                    p.parallel,
                    fuse_quality,
                )? {
                    Decoded::Encoded(out) => out,
                    Decoded::Pixels { dst_w, dst_h } => {
                        encode(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, p)?
                    }
                };
                Ok((out, ImageFormat::Jpeg))
            }
            ImageFormat::Png => Ok((process_png(s, reader, p)?, ImageFormat::Png)),
            ImageFormat::Webp => Ok((process_webp(s, reader, p)?, ImageFormat::Webp)),
            #[cfg(feature = "avif")]
            ImageFormat::Avif => Ok((process_avif(s, reader, p)?, ImageFormat::Avif)),
            #[cfg(not(feature = "avif"))]
            ImageFormat::Avif => anyhow::bail!("AVIF support is not enabled in this build"),
        }
    })
}

/// Streaming variant: decode straight from the file instead of buffering
/// the whole JPEG on the heap. For large sources (10MB+) under high
/// concurrency this saves concurrency x file-size of resident memory;
/// entropy decoding is a sequential read anyway, so the page cache
/// serves it fine.
pub fn process_path(path: &std::path::Path, p: &Params) -> Result<(Vec<u8>, ImageFormat)> {
    let file = std::fs::File::open(path).context("open source")?;
    process_reader(file, p)
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
pub fn process_url(url: &str, p: &Params) -> Result<(Vec<u8>, ImageFormat)> {
    let resp = http_agent().get(url).call().map_err(|e| match e {
        // Preserve source 404s so the HTTP layer can pass them through.
        ureq::Error::StatusCode(404) => anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "source returned 404",
        )),
        other => anyhow::Error::new(other).context("fetch source"),
    })?;
    let reader = std::io::Read::take(resp.into_body().into_reader(), max_source_bytes());
    process_reader(reader, p)
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
    // Compressed source bytes for formats whose decoders need the whole
    // buffer (png's Seek bound, libwebp's one-shot API). JPEG never uses
    // this: it streams.
    srcbuf: Vec<u8>,
    // Final RGB pixels also live in scratch: output sizes vary per request
    // (every distinct target width is a distinct allocation size), and that
    // churn is what the allocator retains across thread heaps.
    out8: Vec<u8>,
    resizer: Option<Resizer>,
}

/// Grow-only scratch access: ensures length without re-zeroing retained
/// bytes (a full-size memset per request on multi-megabyte buffers) and
/// returns the exactly-sized view. Callers must fully overwrite the
/// returned slice before reading it.
fn scratch_u16(buf: &mut Vec<u16>, len: usize) -> &mut [u16] {
    if buf.len() < len {
        buf.resize(len, 0);
    }
    &mut buf[..len]
}

/// See [`scratch_u16`].
fn scratch_u8(buf: &mut Vec<u8>, len: usize) -> &mut [u8] {
    if buf.len() < len {
        buf.resize(len, 0);
    }
    &mut buf[..len]
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
        match decode_resize(s, dec, max_w, max_h, parallel, None)? {
            Decoded::Pixels { dst_w, dst_h } => {
                Ok((s.out8[..dst_w * dst_h * 3].to_vec(), dst_w, dst_h))
            }
            Decoded::Encoded(_) => unreachable!("no fuse quality was requested"),
        }
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
    // The pipeline premultiplies before this call and unpremultiplies
    // after, so fir's own alpha multiply/divide pass must stay off: with
    // it, already-premultiplied colors get weighted by alpha a second
    // time inside the convolution.
    let opts = ResizeOptions::new()
        .resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3))
        .use_alpha(false);
    let src_view =
        fast_image_resize::images::ImageRef::new(dec_w as u32, dec_h as u32, src_bytes, px)?;

    if threads <= 1 || dst_h < 2 * threads {
        // x86-64 full-frame dispatch (this fn is not on the fused JPEG
        // path, which streams through the AVX2 row kernel directly):
        // U16x3 keeps pic-scale — its row-batched horizontal pass is
        // ~1.9x the in-tree AVX2 kernel full-frame and every full-frame
        // format (PNG/WebP/AVIF, mozjpeg presets) always takes the same
        // backend, so bytes stay per-URL stable. U16x4 (alpha) moves
        // from fir to the AVX2 kernel (1.33x on the benchmark shape;
        // see examples/resize_bench_x86.rs).
        #[cfg(target_arch = "x86_64")]
        if std::env::var("OXIMG_RESIZE_BACKEND").as_deref() != Ok("fir") {
            if px == PixelType::U16x3 {
                return resize_u16x3_picscale(src_bytes, dec_w, dec_h, dst_bytes, dst_w, dst_h);
            }
            if px == PixelType::U16x4 && crate::resize_avx2::Avx2::available() {
                return crate::resize_avx2::resize_u16_avx2(
                    src_bytes, dec_w, dec_h, dst_bytes, dst_w, dst_h, 4,
                );
            }
        }
        #[cfg(target_arch = "aarch64")]
        if matches!(px, PixelType::U16x3 | PixelType::U16x4)
            && std::env::var("OXIMG_RESIZE_BACKEND").as_deref() != Ok("fir")
            && std::arch::is_aarch64_feature_detected!("neon")
        {
            return crate::resize_neon::resize_u16_neon(
                src_bytes,
                dec_w,
                dec_h,
                dst_bytes,
                dst_w,
                dst_h,
                px.size() / 2,
            );
        }
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

#[cfg(target_arch = "x86_64")]
fn resize_u16x3_picscale(
    src_bytes: &[u8],
    src_w: usize,
    src_h: usize,
    dst_bytes: &mut [u8],
    dst_w: usize,
    dst_h: usize,
) -> Result<()> {
    use pic_scale::{ImageStore, ImageStoreMut, ResamplingFunction, Scaler, ThreadingPolicy};
    let (pre, src16, post) = unsafe { src_bytes.align_to::<u16>() };
    anyhow::ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 src");
    let (pre, dst16, post) = unsafe { dst_bytes.align_to_mut::<u16>() };
    anyhow::ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 dst");
    let src_store = ImageStore::<u16, 3>::from_slice(src16, src_w, src_h)
        .map_err(|e| anyhow::anyhow!("pic-scale src: {e:?}"))?;
    let mut dst_store = ImageStoreMut::<u16, 3>::from_slice(dst16, dst_w, dst_h)
        .map_err(|e| anyhow::anyhow!("pic-scale dst: {e:?}"))?;
    dst_store.bit_depth = 16;
    let scaler =
        Scaler::new(ResamplingFunction::Lanczos3).set_threading_policy(ThreadingPolicy::Single);
    let plan = scaler
        .plan_rgb_resampling16(src_store.size(), dst_store.size(), 16)
        .map_err(|e| anyhow::anyhow!("pic-scale plan: {e:?}"))?;
    plan.resample(&src_store, &mut dst_store)
        .map_err(|e| anyhow::anyhow!("pic-scale resample: {e:?}"))?;
    Ok(())
}

/// Result of the JPEG decode stage: either resized pixels left in
/// `Scratch::out8` for a separate encode, or — on the fused path — the
/// finished JPEG bytes (decode overlapped with resize+encode).
enum Decoded {
    Pixels { dst_w: usize, dst_h: usize },
    Encoded(Vec<u8>),
}

/// Requests currently inside the pixel pipeline, all formats. Used as
/// the load signal for the overlap gate.
static ACTIVE_PIPELINES: AtomicUsize = AtomicUsize::new(0);

struct ActiveGuard;

impl ActiveGuard {
    fn enter() -> ActiveGuard {
        ACTIVE_PIPELINES.fetch_add(1, Ordering::Relaxed);
        ActiveGuard
    }
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        ACTIVE_PIPELINES.fetch_sub(1, Ordering::Relaxed);
    }
}

fn logical_cpus() -> usize {
    static N: OnceLock<usize> = OnceLock::new();
    *N.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    })
}

/// OXIMG_OVERLAP: "0" = never fuse, "1" = always fuse, "auto" = fuse
/// while the machine has headroom. The default is auto on aarch64,
/// where the serial path resizes with the same NEON kernel and a URL's
/// bytes are identical either way; on x86-64 it is off, because the
/// serial path's pic-scale backend still out-runs the streaming AVX2
/// kernel full-frame — forcing "1" fuses every eligible request (all
/// through the AVX2 kernel, still deterministic per configuration).
fn overlap_mode() -> u8 {
    static M: OnceLock<u8> = OnceLock::new();
    *M.get_or_init(|| match std::env::var("OXIMG_OVERLAP").as_deref() {
        Ok("0") => 0,
        Ok("1") => 1,
        Ok("auto") => 2,
        _ => {
            if cfg!(target_arch = "aarch64") {
                2
            } else {
                0
            }
        }
    })
}

/// Fuse decode with resize+encode while the machine has headroom for
/// the second lane: each fused request runs two threads, so the auto
/// gate stops fusing once active requests exceed half the visible
/// CPUs. On a dedicated box fusing measured at or above serial
/// throughput at every concurrency on both Zen4 and SMT-less Apple
/// silicon — but when other CPU-hungry processes share the cores
/// (e.g. a co-located load generator, or a container cpuset shared
/// with a proxy), the extra threads regress throughput ~10%, so
/// saturation falls back to one core per request.
fn overlap_gate() -> bool {
    match overlap_mode() {
        0 => false,
        1 => true,
        _ => ACTIVE_PIPELINES.load(Ordering::Relaxed) * 2 <= logical_cpus(),
    }
}

fn decode_resize<R: std::io::BufRead>(
    s: &mut Scratch,
    mut dec: Decompress<R>,
    max_w: u32,
    max_h: u32,
    parallel: usize,
    fuse_quality: Option<f32>,
) -> Result<Decoded> {
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
        let out = scratch_u8(&mut s.out8, dec_w * dec_h * 3);
        started.read_scanlines_into(out).context("decode failed")?;
        started.finish().context("decode finish failed")?;
        if timing {
            eprintln!(
                "timing decode({dec_w}x{dec_h})={:.1}ms resize=0 (exact)",
                t0.elapsed().as_secs_f64() * 1e3
            );
        }
        return Ok(Decoded::Pixels { dst_w, dst_h });
    }

    if let Some(quality) = fuse_quality
        && linear
        && let Some(out) = fused_resize_encode(&mut started, dec_w, dec_h, dst_w, dst_h, quality)?
    {
        if timing {
            eprintln!(
                "timing fused({dec_w}x{dec_h}->{dst_w}x{dst_h}) total={:.1}ms",
                t0.elapsed().as_secs_f64() * 1e3
            );
        }
        started.finish().context("decode finish failed")?;
        return Ok(Decoded::Encoded(out));
    }

    if linear {
        // Decode in chunks and apply the sRGB u8 -> linear u16 LUT on the
        // fly: each chunk stays in L2, saving a second full-image memory
        // pass.
        let fwd = fwd_lut();
        // Fully filled by the chunked LUT loop below (filled reaches
        // dec_w*dec_h*3 or the decode errors out).
        scratch_u16(&mut s.src16, dec_w * dec_h * 3);
        let chunk_rows = (256 * 1024 / row_bytes).clamp(1, dec_h);
        scratch_u8(&mut s.chunk8, chunk_rows * row_bytes);
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
        scratch_u16(&mut s.dst16, dst_w * dst_h * 3);
        resize_bands(
            u16_as_bytes(&s.src16[..dec_w * dec_h * 3]),
            dec_w,
            dec_h,
            u16_as_bytes_mut(&mut s.dst16[..dst_w * dst_h * 3]),
            dst_w,
            dst_h,
            PixelType::U16x3,
            parallel,
            &mut s.resizer,
        )?;

        let back = back_lut();
        let out = scratch_u8(&mut s.out8, dst_w * dst_h * 3);
        for (d, src) in out.iter_mut().zip(&s.dst16[..dst_w * dst_h * 3]) {
            *d = back[*src as usize];
        }
        if timing {
            eprintln!(
                "timing decode+fwd({dec_w}x{dec_h})={:.1}ms resize+back={:.1}ms",
                t_decode.as_secs_f64() * 1e3,
                t1.elapsed().as_secs_f64() * 1e3
            );
        }
        Ok(Decoded::Pixels { dst_w, dst_h })
    } else {
        // Resize directly in sRGB space (speed mode)
        scratch_u8(&mut s.chunk8, dec_w * dec_h * 3);
        started
            .read_scanlines_into(&mut s.chunk8[..dec_w * dec_h * 3])
            .context("decode failed")?;
        started.finish().context("decode finish failed")?;
        let t_decode = t0.elapsed();

        let t1 = std::time::Instant::now();
        scratch_u8(&mut s.out8, dst_w * dst_h * 3);
        resize_bands(
            &s.chunk8[..dec_w * dec_h * 3],
            dec_w,
            dec_h,
            &mut s.out8[..dst_w * dst_h * 3],
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
        Ok(Decoded::Pixels { dst_w, dst_h })
    }
}

/// The SIMD row kernel driving the fused path on this architecture.
#[cfg(target_arch = "aarch64")]
type FuseKernel = crate::resize_neon::Neon;
#[cfg(target_arch = "x86_64")]
type FuseKernel = crate::resize_avx2::Avx2;

/// The fused JPEG fast path: this (request) thread keeps the decoder at
/// its serial-decode floor while a worker thread converts each decoded
/// chunk to linear u16, streams it through the row-push resize kernel,
/// and feeds finished rows to an incremental jpegli encoder. Everything
/// downstream of the decoder hides behind the decode wall; the only
/// serial tail left is jpegli's entropy pass in `finish`.
///
/// Returns Ok(None) — with the decoder untouched — when no SIMD row
/// kernel exists for this CPU, so the caller falls back to the serial
/// path. Output bytes are identical to the serial jpegli path: the same
/// kernel produces the same u16 rows (streamed emission is bit-identical
/// to the full-frame schedule), and jpegli is deterministic for the same
/// scanlines and settings regardless of write granularity.
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
fn fused_resize_encode<R: std::io::BufRead>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    quality: f32,
) -> Result<Option<Vec<u8>>> {
    #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
    {
        Ok(None)
    }
    #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
    {
        let Ok(mut resizer) =
            crate::resize_kernel::StreamResize::<FuseKernel>::new(dec_w, dec_h, dst_w, dst_h, 3)
        else {
            return Ok(None);
        };

        let row_bytes = dec_w * 3;
        // Smaller chunks than the serial path's 256KB: granularity here
        // sets the post-decode tail (the last chunk's convert+resize+
        // encode work cannot hide behind the decode), and per-chunk
        // handoff costs are ~µs.
        let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
        // Decoded chunks flow A -> B; drained buffers flow back for reuse.
        // Capacity 2 gives the decoder one chunk of runway without letting
        // buffers pile up.
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize)>(2);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        let worker = std::thread::Builder::new()
            .name("oximg-fuse".into())
            .spawn(move || -> Result<Vec<u8>> {
                let fwd = fwd_lut();
                let back = back_lut();
                let mut row16 = vec![0u16; dec_w * 3];
                let mut row8 = vec![0u8; dst_w * 3];

                let mut comp = jpegli::Compress::new(jpegli::ColorSpace::JCS_RGB);
                comp.set_size(dst_w, dst_h);
                comp.set_quality(quality);
                // Mirrors encode_jpegli: progressive is worth several
                // percent at these sizes.
                comp.set_progressive_mode();
                let mut enc = comp.start_compress(Vec::with_capacity(64 * 1024))?;

                while let Ok((buf, rows)) = chunk_rx.recv() {
                    for r in 0..rows {
                        let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                        for (d, &v) in row16.iter_mut().zip(src) {
                            *d = fwd[v as usize];
                        }
                        let mut enc_result = Ok(());
                        resizer.push_row(&row16, |_, out| {
                            for (d, &v) in row8.iter_mut().zip(out) {
                                *d = back[v as usize];
                            }
                            if enc_result.is_ok() {
                                enc_result = enc.write_scanlines(&row8);
                            }
                        });
                        enc_result.context("fused encode failed")?;
                    }
                    let _ = recycle_tx.send(buf);
                }
                // Channel closed: either the decoder delivered everything
                // or it failed mid-image; only a complete image may be
                // finished into a JPEG.
                anyhow::ensure!(
                    resizer.rows_emitted() == dst_h,
                    "decode ended before the image was complete"
                );
                enc.finish().context("fused encode finish failed")
            })
            .context("spawn fuse worker")?;

        // Decode loop on the request thread: read a chunk, hand it to the
        // worker, reuse buffers the worker has drained.
        let decode_result = (|| -> Result<()> {
            let mut remaining = dec_h;
            while remaining > 0 {
                let mut buf = recycle_rx.try_recv().unwrap_or_default();
                let want = remaining.min(chunk_rows) * row_bytes;
                if buf.len() < want {
                    buf.resize(want, 0);
                }
                let got = started
                    .read_scanlines_into(&mut buf[..want])
                    .context("decode failed")?
                    .len();
                anyhow::ensure!(
                    got > 0 && got % row_bytes == 0,
                    "decoder returned a partial row"
                );
                let rows = got / row_bytes;
                remaining -= rows;
                if chunk_tx.send((buf, rows)).is_err() {
                    // Worker died; its join below reports the real error.
                    anyhow::bail!("fuse worker exited early");
                }
            }
            Ok(())
        })();
        drop(chunk_tx);

        let encoded = worker
            .join()
            .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
        // A decode error is the root cause; report it over the worker's
        // consequent "incomplete image" error.
        decode_result?;
        Ok(Some(encoded?))
    }
}

fn linear_light() -> bool {
    std::env::var("OXIMG_RESIZE").as_deref() != Ok("srgb")
}

fn process_png<R: std::io::Read>(s: &mut Scratch, mut reader: R, p: &Params) -> Result<Vec<u8>> {
    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    s.srcbuf.clear();
    reader
        .read_to_end(&mut s.srcbuf)
        .context("read PNG source")?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&s.srcbuf[..]));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut png_reader = decoder.read_info().context("parse PNG")?;

    // Hot path: plain RGB8, non-interlaced, linear-light mode. Rows are
    // sRGB->linear LUT-mapped as they are decoded, so the pixels never
    // take a second full-image pass (and never exist as a full u8 copy).
    {
        let (ct, bits) = png_reader.output_color_type();
        let hdr = png_reader.info();
        let (src_w, src_h) = (hdr.width as usize, hdr.height as usize);
        let (dst_w, dst_h) = fit_dims(src_w, src_h, p.max_width, p.max_height);
        if ct == png::ColorType::Rgb
            && bits == png::BitDepth::Eight
            && !hdr.interlaced
            && (src_w, src_h) != (dst_w, dst_h)
            && linear_light()
        {
            let fwd = fwd_lut();
            // Fully filled row by row; the row-count check below rejects
            // truncated streams before the buffer is consumed.
            scratch_u16(&mut s.src16, src_w * src_h * 3);
            let mut y = 0usize;
            while let Some(row) = png_reader.next_row().context("decode PNG")? {
                let dst = &mut s.src16[y * src_w * 3..(y + 1) * src_w * 3];
                for (d, &b) in dst.iter_mut().zip(row.data()) {
                    *d = fwd[b as usize];
                }
                y += 1;
            }
            anyhow::ensure!(y == src_h, "PNG row count mismatch");
            let t_decode = t0.elapsed();
            let t1 = std::time::Instant::now();
            scratch_u16(&mut s.dst16, dst_w * dst_h * 3);
            resize_bands(
                u16_as_bytes(&s.src16[..src_w * src_h * 3]),
                src_w,
                src_h,
                u16_as_bytes_mut(&mut s.dst16[..dst_w * dst_h * 3]),
                dst_w,
                dst_h,
                PixelType::U16x3,
                p.parallel,
                &mut s.resizer,
            )?;
            let back = back_lut();
            let out = scratch_u8(&mut s.out8, dst_w * dst_h * 3);
            for (d, &v) in out.iter_mut().zip(&s.dst16[..dst_w * dst_h * 3]) {
                *d = back[v as usize];
            }
            let t_resize = t1.elapsed();
            let t2 = std::time::Instant::now();
            let out = encode_png(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, 3);
            if timing {
                eprintln!(
                    "timing png(fused) decode+fwd({src_w}x{src_h})={:.1}ms resize+back={:.1}ms encode={:.1}ms",
                    t_decode.as_secs_f64() * 1e3,
                    t_resize.as_secs_f64() * 1e3,
                    t2.elapsed().as_secs_f64() * 1e3
                );
            }
            return out;
        }
    }

    let buf_len = png_reader.output_buffer_size().context("PNG too large")?;
    scratch_u8(&mut s.chunk8, buf_len);
    let info = png_reader
        .next_frame(&mut s.chunk8[..buf_len])
        .context("decode PNG")?;
    let (src_w, src_h) = (info.width as usize, info.height as usize);
    let len = info.buffer_size();

    // Normalize to RGB8/RGBA8 (EXPAND leaves grayscale as 1-2 channels).
    let channels = match info.color_type {
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Grayscale => {
            gray_to_rgb(s, len, 1);
            3
        }
        png::ColorType::GrayscaleAlpha => {
            gray_to_rgb(s, len, 2);
            4
        }
        png::ColorType::Indexed => anyhow::bail!("unexpanded indexed PNG"),
    };

    let t_decode = t0.elapsed();
    let t1 = std::time::Instant::now();
    let (dst_w, dst_h) = resize_pixels(s, channels, src_w, src_h, p)?;
    let t_resize = t1.elapsed();
    let t2 = std::time::Instant::now();
    let out = encode_png(&s.out8[..dst_w * dst_h * channels], dst_w, dst_h, channels);
    if timing {
        eprintln!(
            "timing png decode({src_w}x{src_h})={:.1}ms resize={:.1}ms encode={:.1}ms",
            t_decode.as_secs_f64() * 1e3,
            t_resize.as_secs_f64() * 1e3,
            t2.elapsed().as_secs_f64() * 1e3
        );
    }
    out
}

/// Expand grayscale(+alpha) pixels in `chunk8[..len]` to RGB(A) in place.
fn gray_to_rgb(s: &mut Scratch, len: usize, in_ch: usize) {
    let out_ch = in_ch + 2;
    let pixels = len / in_ch;
    // The reverse loop below writes every output position.
    scratch_u8(&mut s.chunk8, pixels * out_ch);
    for i in (0..pixels).rev() {
        let g = s.chunk8[i * in_ch];
        let a = if in_ch == 2 {
            s.chunk8[i * in_ch + 1]
        } else {
            0
        };
        let o = i * out_ch;
        s.chunk8[o] = g;
        s.chunk8[o + 1] = g;
        s.chunk8[o + 2] = g;
        if in_ch == 2 {
            s.chunk8[o + 3] = a;
        }
    }
}

fn process_webp<R: std::io::Read>(s: &mut Scratch, mut reader: R, p: &Params) -> Result<Vec<u8>> {
    s.srcbuf.clear();
    reader
        .read_to_end(&mut s.srcbuf)
        .context("read WebP source")?;

    // Decode with libwebp's built-in scaler when we are shrinking well past
    // the target, keeping the same quality headroom as the JPEG DCT path:
    // decode at >= margin x target, then hand the remainder to linear-light
    // Lanczos. libwebp's scaler alone (scale straight to target) is what
    // costs other servers their score.
    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    let (src_w, src_h, channels, dec_w, dec_h) = webp_decode_into_chunk8(s, p)?;
    let _ = (src_w, src_h);
    let t_dec = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (dst_w, dst_h) = resize_pixels(s, channels, dec_w, dec_h, p)?;
    let t_resize = t1.elapsed();

    let t2 = std::time::Instant::now();
    let out = encode_webp(&s.out8[..dst_w * dst_h * channels], dst_w, dst_h, channels)?;
    if timing {
        eprintln!(
            "timing webp decode({dec_w}x{dec_h})={:.1}ms resize={:.1}ms encode={:.1}ms",
            t_dec.as_secs_f64() * 1e3,
            t_resize.as_secs_f64() * 1e3,
            t2.elapsed().as_secs_f64() * 1e3
        );
    }
    Ok(out)
}

/// AVIF: decode via dav1d, resize, re-encode via SVT-AV1. AV1 has no
/// reduced-resolution decode mode, so unlike JPEG/WebP the decode always
/// runs at full source resolution.
#[cfg(feature = "avif")]
fn process_avif<R: std::io::Read>(s: &mut Scratch, mut reader: R, p: &Params) -> Result<Vec<u8>> {
    s.srcbuf.clear();
    reader
        .read_to_end(&mut s.srcbuf)
        .context("read AVIF source")?;

    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    let (src_w, src_h, channels) = crate::avif::decode_avif_into(&s.srcbuf, &mut s.chunk8)?;
    let t_dec = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (dst_w, dst_h) = resize_pixels(s, channels, src_w, src_h, p)?;
    let t_resize = t1.elapsed();

    let t2 = std::time::Instant::now();
    let quality = avif_quality();
    let params = crate::avif::AvifParams {
        quality,
        alpha_quality: avif_alpha_quality(quality),
        ..Default::default()
    };
    let out = crate::avif::encode_avif(
        &s.out8[..dst_w * dst_h * channels],
        dst_w,
        dst_h,
        channels,
        &params,
    )?;
    if timing {
        eprintln!(
            "timing avif decode({src_w}x{src_h})={:.1}ms resize={:.1}ms encode={:.1}ms",
            t_dec.as_secs_f64() * 1e3,
            t_resize.as_secs_f64() * 1e3,
            t2.elapsed().as_secs_f64() * 1e3
        );
    }
    Ok(out)
}

/// Decode `srcbuf` (WebP) into `chunk8`, scaling during decode down to
/// margin x target when the source is much larger. Returns
/// (src_w, src_h, channels, decoded_w, decoded_h).
fn webp_decode_into_chunk8(
    s: &mut Scratch,
    p: &Params,
) -> Result<(usize, usize, usize, usize, usize)> {
    use libwebp_sys as w;
    unsafe {
        let mut config: w::WebPDecoderConfig = std::mem::zeroed();
        anyhow::ensure!(
            w::WebPInitDecoderConfig(&mut config),
            "libwebp ABI mismatch"
        );
        let status = w::WebPGetFeatures(s.srcbuf.as_ptr(), s.srcbuf.len(), &mut config.input);
        anyhow::ensure!(
            status == w::VP8StatusCode::VP8_STATUS_OK,
            "parse WebP header"
        );
        anyhow::ensure!(
            config.input.has_animation == 0,
            "animated WebP is unsupported"
        );
        let (src_w, src_h) = (config.input.width as usize, config.input.height as usize);
        let channels = if config.input.has_alpha != 0 { 4 } else { 3 };

        let (dst_w, dst_h) = fit_dims(src_w, src_h, p.max_width, p.max_height);
        let need_w = ((dst_w as f64) * dct_margin()).ceil() as usize;
        let (dec_w, dec_h) = if need_w < src_w {
            let scale = need_w as f64 / src_w as f64;
            (
                need_w.max(dst_w),
                (((src_h as f64) * scale).round() as usize).max(dst_h),
            )
        } else {
            (src_w, src_h)
        };
        if (dec_w, dec_h) != (src_w, src_h) {
            config.options.use_scaling = 1;
            config.options.scaled_width = dec_w as i32;
            config.options.scaled_height = dec_h as i32;
        }
        // libwebp's threaded decode pipelines entropy decoding and
        // reconstruction across two threads (the same setting libvips
        // ships); like band-parallel resize this briefly exceeds the CPU
        // slot without oversubscribing on average.
        if std::env::var("OXIMG_WEBP_DECODE_THREADS").as_deref() != Ok("0") {
            config.options.use_threads = 1;
        }
        config.output.colorspace = if channels == 4 {
            w::WEBP_CSP_MODE::MODE_RGBA
        } else {
            w::WEBP_CSP_MODE::MODE_RGB
        };

        let status = w::WebPDecode(s.srcbuf.as_ptr(), s.srcbuf.len(), &mut config);
        if status != w::VP8StatusCode::VP8_STATUS_OK {
            w::WebPFreeDecBuffer(&mut config.output);
            anyhow::bail!("decode WebP: {status:?}");
        }
        let buf = &config.output.u.RGBA;
        let stride = buf.stride as usize;
        let row = dec_w * channels;
        // Every row is copied below before the buffer is read.
        scratch_u8(&mut s.chunk8, dec_h * row);
        for y in 0..dec_h {
            let src_row = std::slice::from_raw_parts(buf.rgba.add(y * stride), row);
            s.chunk8[y * row..(y + 1) * row].copy_from_slice(src_row);
        }
        w::WebPFreeDecBuffer(&mut config.output);
        Ok((src_w, src_h, channels, dec_w, dec_h))
    }
}

/// Resize the fully decoded pixels in `chunk8` (3 or 4 channels) into
/// `out8`, returning the output dimensions. RGB follows the same
/// linear-light path as JPEG; alpha images are premultiplied before
/// resampling and unpremultiplied after.
fn resize_pixels(
    s: &mut Scratch,
    channels: usize,
    src_w: usize,
    src_h: usize,
    p: &Params,
) -> Result<(usize, usize)> {
    let (dst_w, dst_h) = fit_dims(src_w, src_h, p.max_width, p.max_height);
    let src_len = src_w * src_h * channels;
    if (src_w, src_h) == (dst_w, dst_h) {
        scratch_u8(&mut s.out8, src_len);
        let (chunk8, out8) = (&s.chunk8, &mut s.out8);
        out8[..src_len].copy_from_slice(&chunk8[..src_len]);
        return Ok((dst_w, dst_h));
    }

    if linear_light() {
        let (fwd, back) = (fwd_lut(), back_lut());
        // Fully overwritten by the LUT/premultiply loops just below.
        scratch_u16(&mut s.src16, src_len);
        if channels == 4 {
            for (d, src) in s.src16[..src_len]
                .chunks_exact_mut(4)
                .zip(s.chunk8[..src_len].chunks_exact(4))
            {
                let a = src[3] as u32 * 257;
                for c in 0..3 {
                    // Premultiply in linear light so resampling never bleeds
                    // color from fully transparent pixels.
                    d[c] = ((fwd[src[c] as usize] as u32 * a) / 65535) as u16;
                }
                d[3] = a as u16;
            }
        } else {
            for (d, src) in s.src16[..src_len].iter_mut().zip(&s.chunk8[..src_len]) {
                *d = fwd[*src as usize];
            }
        }
        let dst_len = dst_w * dst_h * channels;
        scratch_u16(&mut s.dst16, dst_len);
        resize_bands(
            u16_as_bytes(&s.src16[..src_len]),
            src_w,
            src_h,
            u16_as_bytes_mut(&mut s.dst16[..dst_len]),
            dst_w,
            dst_h,
            if channels == 4 {
                PixelType::U16x4
            } else {
                PixelType::U16x3
            },
            p.parallel,
            &mut s.resizer,
        )?;
        scratch_u8(&mut s.out8, dst_len);
        if channels == 4 {
            for (d, src) in s.out8[..dst_len]
                .chunks_exact_mut(4)
                .zip(s.dst16[..dst_len].chunks_exact(4))
            {
                let a = src[3] as u32;
                for (out, &pre) in d[..3].iter_mut().zip(&src[..3]) {
                    let un = (pre as u32 * 65535)
                        .checked_div(a)
                        .map_or(0, |v| v.min(65535)) as u16;
                    *out = back[un as usize];
                }
                d[3] = (a / 257) as u8;
            }
        } else {
            for (d, src) in s.out8[..dst_len].iter_mut().zip(&s.dst16[..dst_len]) {
                *d = back[*src as usize];
            }
        }
    } else {
        if channels == 4 {
            // Premultiply in place (u8 approximation for speed mode).
            for px in s.chunk8[..src_len].chunks_exact_mut(4) {
                let a = px[3] as u32;
                for c in px[..3].iter_mut() {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
        }
        let dst_len = dst_w * dst_h * channels;
        scratch_u8(&mut s.out8, dst_len);
        let (chunk8, out8) = (&s.chunk8, &mut s.out8);
        resize_bands(
            &chunk8[..src_len],
            src_w,
            src_h,
            &mut out8[..dst_len],
            dst_w,
            dst_h,
            if channels == 4 {
                PixelType::U8x4
            } else {
                PixelType::U8x3
            },
            p.parallel,
            &mut s.resizer,
        )?;
        if channels == 4 {
            for px in s.out8[..dst_len].chunks_exact_mut(4) {
                let a = px[3] as u32;
                for c in px[..3].iter_mut() {
                    *c = (*c as u32 * 255).checked_div(a).map_or(0, |v| v.min(255)) as u8;
                }
            }
        }
    }
    Ok((dst_w, dst_h))
}

fn png_compression() -> png::Compression {
    match std::env::var("OXIMG_PNG_EFFORT").as_deref() {
        Ok("fastest") => png::Compression::Fastest,
        // Balanced spends ~15ms/request more than Fast to shave ~14% of
        // the file; Fast still undercuts libvips' default output size.
        Ok("balanced") => png::Compression::Balanced,
        Ok("high") => png::Compression::High,
        _ => png::Compression::Fast,
    }
}

fn encode_png(pixels: &[u8], w: usize, h: usize, channels: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64 * 1024);
    let mut enc = png::Encoder::new(&mut out, w as u32, h as u32);
    enc.set_color(if channels == 4 {
        png::ColorType::Rgba
    } else {
        png::ColorType::Rgb
    });
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png_compression());
    let mut writer = enc.write_header().context("PNG header")?;
    writer.write_image_data(pixels).context("PNG encode")?;
    writer.finish().context("PNG finish")?;
    Ok(out)
}

/// AVIF quality (libavif semantics). Nominal quality numbers are not
/// comparable across encoders; this default is chosen by operating
/// point: at 55, the 10-bit SVT-AV1 output is smaller than what the
/// common imgproxy/libvips default (q65, 8-bit aom) produces and still
/// scores several SSIMULACRA2 points higher (see bench/quality).
#[cfg(feature = "avif")]
fn avif_quality() -> u8 {
    std::env::var("OXIMG_AVIF_QUALITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(55)
}

/// Alpha-item quality; defaults to the color quality.
#[cfg(feature = "avif")]
fn avif_alpha_quality(color_quality: u8) -> u8 {
    std::env::var("OXIMG_AVIF_ALPHA_QUALITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(color_quality)
}

fn webp_quality() -> f32 {
    std::env::var("OXIMG_WEBP_QUALITY")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(75.0)
}

fn webp_effort() -> i32 {
    std::env::var("OXIMG_WEBP_EFFORT")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2)
}

fn encode_webp(pixels: &[u8], w: usize, h: usize, channels: usize) -> Result<Vec<u8>> {
    use libwebp_sys as wp;
    unsafe {
        let mut config: wp::WebPConfig = std::mem::zeroed();
        anyhow::ensure!(wp::WebPInitConfig(&mut config), "libwebp ABI mismatch");
        config.quality = webp_quality();
        config.method = webp_effort().clamp(0, 6);

        let mut pic: wp::WebPPicture = std::mem::zeroed();
        anyhow::ensure!(wp::WebPPictureInit(&mut pic), "libwebp ABI mismatch");
        pic.width = w as i32;
        pic.height = h as i32;
        let imported = if channels == 4 {
            wp::WebPPictureImportRGBA(&mut pic, pixels.as_ptr(), (w * 4) as i32)
        } else {
            wp::WebPPictureImportRGB(&mut pic, pixels.as_ptr(), (w * 3) as i32)
        };
        anyhow::ensure!(imported != 0, "webp picture import");

        let mut writer: wp::WebPMemoryWriter = std::mem::zeroed();
        wp::WebPMemoryWriterInit(&mut writer);
        pic.writer = Some(wp::WebPMemoryWrite);
        pic.custom_ptr = (&mut writer) as *mut _ as *mut std::ffi::c_void;

        let ok = wp::WebPEncode(&config, &mut pic);
        wp::WebPPictureFree(&mut pic);
        if ok == 0 {
            wp::WebPMemoryWriterClear(&mut writer);
            anyhow::bail!("webp encode failed (error {:?})", pic.error_code);
        }
        let out = std::slice::from_raw_parts(writer.mem, writer.size).to_vec();
        wp::WebPMemoryWriterClear(&mut writer);
        Ok(out)
    }
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

    /// Deterministic RGB test frame encoded to a real JPEG source.
    fn make_test_jpeg(w: usize, h: usize, gray: bool) -> Vec<u8> {
        let ch = if gray { 1 } else { 3 };
        let mut seed = 0x9E3779B9u32;
        let mut px = Vec::with_capacity(w * h * ch);
        for y in 0..h {
            for x in 0..w {
                for c in 0..ch {
                    seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                    let noise = (seed >> 24) as usize;
                    px.push(((x * 200 / w + y * 40 / h + c * 5 + noise / 4).min(255)) as u8);
                }
            }
        }
        let mut comp = Compress::new(if gray {
            ColorSpace::JCS_GRAYSCALE
        } else {
            ColorSpace::JCS_RGB
        });
        comp.set_size(w, h);
        comp.set_quality(90.0);
        let mut started = comp.start_compress(Vec::new()).unwrap();
        started.write_scanlines(&px).unwrap();
        started.finish().unwrap()
    }

    fn run_jpeg(jpeg: &[u8], fuse_quality: Option<f32>) -> Vec<u8> {
        let p = Params {
            max_width: 320,
            max_height: 320,
            quality: 80.0,
            encoder: Encoder::Jpegli,
            parallel: 1,
        };
        let mut s = Scratch::default();
        let dec = Decompress::new_mem(jpeg).unwrap();
        match decode_resize(&mut s, dec, 320, 320, 1, fuse_quality).unwrap() {
            Decoded::Encoded(out) => {
                assert!(fuse_quality.is_some(), "fused output without fuse request");
                out
            }
            Decoded::Pixels { dst_w, dst_h } => {
                assert!(
                    fuse_quality.is_none(),
                    "fused path was requested but not taken"
                );
                encode(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, &p).unwrap()
            }
        }
    }

    #[test]
    fn serial_jpeg_path_produces_valid_output() {
        let jpeg = make_test_jpeg(400, 300, false);
        let out = run_jpeg(&jpeg, None);
        assert!(out.starts_with(&[0xFF, 0xD8]), "not a JPEG");
    }

    fn fuse_kernel_available() -> bool {
        use crate::resize_kernel::RowKernel;
        if FuseKernel::detect() {
            true
        } else {
            eprintln!("skipping: no SIMD row kernel on this host");
            false
        }
    }

    /// aarch64's serial path resizes with the same NEON kernel the
    /// fused path streams through, so the bytes must match exactly.
    /// (x86-64's serial path uses pic-scale, which rounds differently;
    /// there the fused plumbing is covered by the determinism test
    /// below plus the kernel testkit's streamed==full-frame proofs.)
    #[cfg(target_arch = "aarch64")]
    #[test]
    fn fused_path_bytes_match_serial_jpegli() {
        if !fuse_kernel_available() {
            return;
        }
        // Odd dimensions exercise chunk boundaries and scalar tails.
        let jpeg = make_test_jpeg(799, 601, false);
        assert_eq!(run_jpeg(&jpeg, None), run_jpeg(&jpeg, Some(80.0)));
    }

    #[test]
    fn fused_path_is_deterministic_and_valid() {
        if !fuse_kernel_available() {
            return;
        }
        let jpeg = make_test_jpeg(799, 601, false);
        let a = run_jpeg(&jpeg, Some(80.0));
        let b = run_jpeg(&jpeg, Some(80.0));
        assert!(a.starts_with(&[0xFF, 0xD8]), "not a JPEG");
        assert_eq!(a, b, "fused output must not vary run to run");
        let (fmt, w, h) = probe(&a).unwrap();
        assert_eq!(fmt, ImageFormat::Jpeg);
        assert_eq!((w, h), (320, 241));
    }

    #[test]
    fn fused_path_handles_grayscale_sources() {
        if !fuse_kernel_available() {
            return;
        }
        let jpeg = make_test_jpeg(400, 300, true);
        let fused = run_jpeg(&jpeg, Some(80.0));
        assert!(fused.starts_with(&[0xFF, 0xD8]), "not a JPEG");
        // Serial and fused resize with the same kernel on aarch64, so
        // the bytes must match; x86-64's serial backend differs.
        #[cfg(target_arch = "aarch64")]
        assert_eq!(run_jpeg(&jpeg, None), fused);
    }

    #[test]
    fn fused_path_survives_truncated_sources() {
        // Truncation mid-scan must neither hang the worker handoff nor
        // panic; libjpeg may error out or complete with fill data
        // depending on where the cut lands — both are acceptable here.
        let jpeg = make_test_jpeg(799, 601, false);
        let cut = &jpeg[..jpeg.len() * 3 / 5];
        let p = Params {
            max_width: 320,
            max_height: 320,
            quality: 80.0,
            encoder: Encoder::Jpegli,
            parallel: 1,
        };
        let mut s = Scratch::default();
        if let Ok(dec) = Decompress::new_mem(cut) {
            let _ = decode_resize(&mut s, dec, 320, 320, 1, Some(p.quality));
        }
    }

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
    fn luts_roundtrip_every_srgb_value() {
        // back(fwd(v)) must be the identity for all 256 sRGB values, or
        // unresized regions would shift colors through the linear path.
        let (fwd, back) = (fwd_lut(), back_lut());
        for v in 0..=255u8 {
            assert_eq!(back[fwd[v as usize] as usize], v, "value {v}");
        }
    }

    #[test]
    fn preset_parsing_maps_and_defaults() {
        assert_eq!(Encoder::from_preset("fast"), Encoder::MozFast);
        assert_eq!(Encoder::from_preset("small"), Encoder::MozSmall);
        assert_eq!(Encoder::from_preset("jpegli"), Encoder::Jpegli);
        assert_eq!(Encoder::from_preset(""), Encoder::Jpegli);
        assert_eq!(Encoder::from_preset("bogus"), Encoder::Jpegli);
    }

    #[test]
    fn content_types_match_formats() {
        assert_eq!(ImageFormat::Jpeg.content_type(), "image/jpeg");
        assert_eq!(ImageFormat::Png.content_type(), "image/png");
        assert_eq!(ImageFormat::Webp.content_type(), "image/webp");
        assert_eq!(ImageFormat::Avif.content_type(), "image/avif");
    }

    #[test]
    fn sniff_detects_formats_by_magic_bytes() {
        let jpeg = *b"\xFF\xD8\xFF\xE0\x00\x10JFIF\x00\x01";
        assert_eq!(ImageFormat::sniff(&jpeg), Some(ImageFormat::Jpeg));
        let png = *b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0D";
        assert_eq!(ImageFormat::sniff(&png), Some(ImageFormat::Png));
        let webp = *b"RIFF\x00\x01\x00\x00WEBP";
        assert_eq!(ImageFormat::sniff(&webp), Some(ImageFormat::Webp));
        assert_eq!(
            ImageFormat::sniff(b"\x00\x00\x00\x1cftypavif"),
            Some(ImageFormat::Avif)
        );
        assert_eq!(ImageFormat::sniff(b"GIF89a\x00\x00\x00\x00\x00\x00"), None);
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
