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

/// The fwd LUT's values as f32 (exact: u16 -> f32 is lossless), for
/// kernels that stage u8 sources straight to f32.
fn fwd_lut_f32() -> &'static [f32; 256] {
    static LUT: OnceLock<[f32; 256]> = OnceLock::new();
    LUT.get_or_init(|| {
        let mut t = [0f32; 256];
        let fwd = fwd_lut();
        for (d, &v) in t.iter_mut().zip(fwd.iter()) {
            *d = v as f32;
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
    /// Output format; None re-encodes in the sniffed source format
    /// (the original contract, byte-identical to before this field).
    pub output: Option<ImageFormat>,
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
    crate::config::config().dct_margin
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
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

    /// Parse an output-format token (the URL's `@{fmt}` suffix and the
    /// OXIMG_AUTO_FORMAT list). Unlike source extensions — which are
    /// never trusted — these name the *requested* output format.
    /// Returns Avif even in non-avif builds; availability is the
    /// caller's check (HTTP rejects before spending a CPU slot).
    pub fn from_token(token: &str) -> Option<ImageFormat> {
        match token {
            "jpg" | "jpeg" => Some(ImageFormat::Jpeg),
            "png" => Some(ImageFormat::Png),
            "webp" => Some(ImageFormat::Webp),
            "avif" => Some(ImageFormat::Avif),
            _ => None,
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

/// Cheap header probe: format + *stored* source dimensions without
/// decoding pixels. EXIF orientation is not consulted — with
/// auto-rotation on (the default), `process` fits and emits the
/// *displayed* frame, so for orientations 5-8 its output axes are
/// swapped relative to these dimensions.
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
        // SAFETY: a zeroed WebPBitstreamFeatures is a valid out-param (plain C data);
        // WebPGetFeatures reads at most `bytes.len()` bytes from the live slice and
        // writes only `features`. The status check also rejects libwebp's internal
        // ABI-version mismatch, so the fields are read only after a successful parse.
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

/// Sniff the source format, then resize + re-encode in the target
/// format (`p.output`, defaulting to the source's own). JPEG keeps its
/// fully streaming decode path; PNG streams through the png crate; WebP
/// requires the whole compressed source in memory (libwebp has no
/// incremental one-shot API). Decode-side optimizations (DCT
/// shrink-on-load, WebP decode-scaler) are per-source and stay active
/// for every target.
fn process_reader<R: std::io::Read>(mut reader: R, p: &Params) -> Result<(Vec<u8>, ImageFormat)> {
    let mut header = [0u8; 12];
    std::io::Read::read_exact(&mut reader, &mut header).context("source too short")?;
    let format = ImageFormat::sniff(&header).context("unsupported image format")?;
    let target = p.output.unwrap_or(format);
    // Fail before decode work; the HTTP layer rejects earlier still,
    // this covers library callers.
    #[cfg(not(feature = "avif"))]
    anyhow::ensure!(
        target != ImageFormat::Avif,
        "AVIF support is not enabled in this build"
    );
    let reader = std::io::BufReader::new(std::io::Read::chain(&header[..], reader));

    let _active = ActiveGuard::enter();
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let out = match format {
            ImageFormat::Jpeg => jpeg::process_jpeg(s, reader, target, p)?,
            ImageFormat::Png => process_png(s, reader, target, p)?,
            ImageFormat::Webp => process_webp(s, reader, target, p)?,
            #[cfg(feature = "avif")]
            ImageFormat::Avif => process_avif(s, reader, target, p)?,
            #[cfg(not(feature = "avif"))]
            ImageFormat::Avif => anyhow::bail!("AVIF support is not enabled in this build"),
        };
        Ok((out, target))
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
    crate::config::config().max_source_bytes
}

/// Marker attached (as anyhow context) to failures that are the
/// server's fault — encoding, worker infrastructure — as opposed to
/// undecodable client input. The HTTP layer maps these to 500 with a
/// generic body instead of 422 with error text.
#[derive(Debug, Clone, Copy)]
pub struct ServerFault;

impl std::fmt::Display for ServerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("internal image-processing error")
    }
}

/// Marker for remote-origin failures (transport errors, non-404 error
/// statuses): the HTTP layer answers 502, not 422 — the client's
/// request was fine, the upstream wasn't.
#[derive(Debug, Clone, Copy)]
pub struct UpstreamFault;

impl std::fmt::Display for UpstreamFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream image fetch failed")
    }
}

/// Read wrapper that fails with [`std::io::ErrorKind::FileTooLarge`]
/// once more than `cap` bytes are produced. Silently truncating (what
/// `Read::take` alone did) surfaced as a misleading decode error; the
/// distinct kind lets the HTTP layer answer 413. Exactly-cap-sized
/// sources are fine: the post-cap probe read distinguishes EOF from
/// more data.
struct CappedReader<R> {
    inner: R,
    remaining: u64,
}

impl<R: std::io::Read> std::io::Read for CappedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 {
            let mut probe = [0u8; 1];
            return match self.inner.read(&mut probe)? {
                0 => Ok(0),
                _ => Err(std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    "source exceeds OXIMG_MAX_SOURCE_BYTES",
                )),
            };
        }
        let want = buf
            .len()
            .min(usize::try_from(self.remaining).unwrap_or(usize::MAX));
        let n = self.inner.read(&mut buf[..want])?;
        self.remaining -= n as u64;
        Ok(n)
    }
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
        // Origin 5xx/4xx and transport failures are the upstream's
        // fault, not the request's.
        other => anyhow::Error::new(other)
            .context("fetch source")
            .context(UpstreamFault),
    })?;
    // Content-Length lets us refuse before decoding a byte; the capped
    // reader below backstops chunked (or lying) origins. Streaming
    // decoders may translate the mid-read error into their own decode
    // failure, but buffered formats surface it precisely.
    let cap = max_source_bytes();
    if let Some(len) = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        && len > cap
    {
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("source is {len} bytes, over the {cap}-byte limit"),
        )));
    }
    let reader = CappedReader {
        inner: resp.into_body().into_reader(),
        remaining: cap,
    };
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
    // 10-bit 4:2:0 planes for the fused AVIF path (converted during the
    // decode overlap; encode_avif_from_planes consumes them).
    #[cfg(feature = "avif")]
    y16: Vec<u16>,
    #[cfg(feature = "avif")]
    cb16: Vec<u16>,
    #[cfg(feature = "avif")]
    cr16: Vec<u16>,
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
    // SAFETY: the byte view covers exactly the memory of `buf` (len * 2 bytes,
    // one allocation), u8 needs no alignment, every u16 is valid as two u8s, and
    // the output lifetime is tied to the input borrow.
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 2) }
}

fn u16_as_bytes_mut(buf: &mut [u16]) -> &mut [u8] {
    // SAFETY: as in u16_as_bytes; additionally the &mut borrow makes this the
    // only live view of the memory, and any byte pattern written through it is
    // a valid [u16].
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast(), buf.len() * 2) }
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
        // x86-64 full-frame dispatch. The linear JPEG path no longer
        // arrives here (it streams through the AVX2 row kernel, serial
        // and fused alike); what remains is PNG/WebP/AVIF, sRGB mode,
        // band-parallel requests, and the fir escape hatch. U16x3 keeps
        // pic-scale (still ~13% faster full-frame than the in-tree
        // kernel; every user of this fn always takes the same backend,
        // so bytes stay per-URL stable). U16x4 (alpha) uses the AVX2
        // kernel (1.33x over fir on the benchmark shape; see
        // examples/resize_bench_x86.rs).
        #[cfg(target_arch = "x86_64")]
        if !crate::config::config().fir_backend {
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
            && !crate::config::config().fir_backend
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
    // SAFETY: transmuting byte pairs to u16 is valid for every bit pattern;
    // align_to confines the view to the aligned middle, and the ensure! below
    // rejects any misaligned head/tail.
    let (pre, src16, post) = unsafe { src_bytes.align_to::<u16>() };
    anyhow::ensure!(pre.is_empty() && post.is_empty(), "unaligned u16 src");
    // SAFETY: same argument as the src view; the &mut slice guarantees exclusive
    // access and any u16 written back is valid as bytes.
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

/// OXIMG_OVERLAP: "0" = never fuse, "1" = always fuse, default "auto"
/// = fuse while the machine has headroom. The serial path streams rows
/// through the same SIMD kernel the fused path uses, so a URL's bytes
/// are identical on either side of the gate on every architecture.
fn overlap_mode() -> u8 {
    static M: OnceLock<u8> = OnceLock::new();
    *M.get_or_init(|| match std::env::var("OXIMG_OVERLAP").as_deref() {
        Ok("0") => 0,
        Ok("1") => 1,
        _ => 2,
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

fn linear_light() -> bool {
    crate::config::config().linear_light
}

/// OXIMG_AUTO_ROTATE: apply EXIF orientation (default on; "0"
/// disables, which also skips the pre-decode segment scan entirely).
fn auto_rotate() -> bool {
    crate::config::config().auto_rotate
}

/// OXIMG_ICC: carry the source's ICC profile into the output (default
/// on; "0" disables and skips profile extraction entirely). Pixels are
/// never color-converted — the profile bytes are passed through.
fn icc_passthrough() -> bool {
    crate::config::config().icc_passthrough
}

/// Targets that can carry an ICC profile — all of them when the avif
/// feature is on (AVIF embeds via the container splice in
/// `avif::embed_icc`).
fn target_supports_icc(target: ImageFormat) -> bool {
    match target {
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Webp => true,
        #[cfg(feature = "avif")]
        ImageFormat::Avif => true,
        #[cfg(not(feature = "avif"))]
        ImageFormat::Avif => false,
    }
}

/// Profiles larger than this are dropped rather than copied into every
/// resized output (real-world profiles top out around 2-3MB for
/// LUT-based print profiles; web images carry a few KB). The JPEG scan
/// is bounded tighter still by `meta::SCAN_CAP`.
pub(crate) const ICC_CAP: usize = 4 * 1024 * 1024;

/// Reject a source whose decoded size exceeds OXIMG_MAX_SRC_PIXELS
/// *before* any pixel-sized allocation: compressed-size caps do not
/// bound decoded size — a ~2MB flat-color 50000x50000 PNG would
/// otherwise force a 7.5GB allocation.
pub(crate) fn check_src_pixels(w: usize, h: usize) -> Result<()> {
    let cap = crate::config::config().max_src_pixels;
    let px = (w as u64).saturating_mul(h as u64);
    anyhow::ensure!(
        px <= cap,
        "source is {w}x{h} ({px} pixels), over the OXIMG_MAX_SRC_PIXELS limit ({cap})"
    );
    Ok(())
}

mod encode;
mod formats;
mod fuse;
mod jpeg;
#[cfg(test)]
mod tests;

pub use encode::encode;
use encode::*;
use formats::*;
use fuse::*;
pub use jpeg::decode_and_resize;
#[cfg_attr(not(test), allow(unused_imports))] // tests.rs reaches these via super::*
use jpeg::*;
