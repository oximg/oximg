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
    std::env::var("OXIMG_DCT_MARGIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1.7)
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
            ImageFormat::Jpeg => {
                // A bounded pre-scan of the header segments (through
                // the tables, up to SOS — the span libjpeg's marker
                // saving covers) extracts the EXIF orientation and,
                // for profile-capable targets, the APP2 ICC chain; the
                // scanned bytes are re-chained in front of the stream
                // so the decoder sees identical input. libjpeg-side marker saving is deliberately not
                // used: its per-request memory scales with the number
                // of attacker-supplied APP1 segments, while this buffer
                // is hard-capped (see meta::SCAN_CAP).
                let mut reader = reader;
                let mut scan_prefix = Vec::new();
                let want_icc = icc_passthrough() && target_supports_icc(target);
                let meta = if auto_rotate() || want_icc {
                    crate::meta::scan_jpeg_meta(&mut reader, &mut scan_prefix, want_icc)
                } else {
                    crate::meta::JpegMeta::NONE
                };
                let orientation = if auto_rotate() {
                    meta.orientation
                } else {
                    crate::meta::Orientation::UPRIGHT
                };
                let icc = meta.icc;
                let reader = std::io::Read::chain(&scan_prefix[..], reader);
                let dec = Decompress::new_reader(reader).context("parse JPEG")?;
                // Fused decode overlap: on unless disabled (see
                // overlap_gate). Band-parallel resize keeps the serial
                // path so OXIMG_PAR semantics are unchanged. Jpegli
                // JPEG-out additionally overlaps the incremental encode;
                // every other encoder (mozjpeg presets and cross-format
                // targets) overlaps decode with resize into out8/planes
                // and runs its one-shot encode after.
                // The fir escape hatch must also disable fusing: the
                // fused workers run the in-tree SIMD kernel, and fir vs
                // kernel are byte-different backends, so fusing under
                // fir would make a URL's bytes load-dependent.
                let cross_fuse = || -> Fuse {
                    #[cfg(feature = "avif")]
                    if target == ImageFormat::Avif {
                        // Mirrors encode_output's AVIF arm (the tuned
                        // operating point); the session the fused worker
                        // creates from these is what encodes the planes.
                        let quality = avif_quality();
                        return Fuse::Yuv {
                            params: crate::avif::AvifParams {
                                quality,
                                alpha_quality: avif_alpha_quality(quality),
                                speed: avif_speed(),
                                ..Default::default()
                            },
                        };
                    }
                    Fuse::Pixels
                };
                let fuse = if p.parallel > 1
                    || !overlap_gate()
                    || std::env::var("OXIMG_RESIZE_BACKEND").as_deref() == Ok("fir")
                {
                    Fuse::Off
                } else if !orientation.is_upright()
                    || (icc.is_some() && target != ImageFormat::Avif)
                {
                    // Rotation happens on the resized frame before the
                    // one-shot encode — incompatible with streaming rows
                    // into jpegli or into the YUV planes, so oriented
                    // sources take the pixel fuse and rotate after.
                    // TODO(orient-fuse): a rotation-aware row sink could
                    // recover the jpegli/YUV overlap for the flip-only
                    // orientations (2-4); oriented traffic is a small
                    // minority, so correctness ships first.
                    // ICC likewise for jpegli targets: the profile must
                    // be written before the incremental encoder's first
                    // scanline, so profiled sources take the one-shot
                    // encode. (AVIF is exempt — its profile is spliced
                    // into the container after the encode, so it rides
                    // the Yuv fuse.)
                    // TODO(icc-fuse): thread the profile into the fused
                    // jpegli worker to recover the overlap.
                    Fuse::Pixels
                } else if target == ImageFormat::Jpeg {
                    if p.encoder == Encoder::Jpegli {
                        Fuse::Jpegli { quality: p.quality }
                    } else {
                        // mozjpeg presets have no incremental encoder,
                        // but the decode still overlaps the resize into
                        // out8 (byte-identical to the serial path); the
                        // one-shot mozjpeg encode runs after.
                        Fuse::Pixels
                    }
                } else {
                    cross_fuse()
                };
                match decode_resize(
                    s,
                    dec,
                    p.max_width,
                    p.max_height,
                    p.parallel,
                    orientation,
                    fuse,
                )? {
                    Decoded::Encoded(out) => out,
                    Decoded::Pixels { dst_w, dst_h } => {
                        let (dw, dh) = if orientation.is_upright() {
                            (dst_w, dst_h)
                        } else {
                            // Rotate the resized frame into chunk8 (free
                            // at this point) and swap it in as out8.
                            let dims = crate::meta::apply_orientation(
                                &s.out8[..dst_w * dst_h * 3],
                                dst_w,
                                dst_h,
                                3,
                                orientation,
                                &mut s.chunk8,
                            );
                            std::mem::swap(&mut s.out8, &mut s.chunk8);
                            dims
                        };
                        encode_output(s, dw, dh, 3, target, p, icc.as_deref())?
                    }
                    #[cfg(feature = "avif")]
                    Decoded::YuvPlanes { session } => {
                        // Conversion and encoder setup already happened
                        // inside the decode overlap; only the encode
                        // itself remains. The plane vectors are truncated
                        // to exactly this frame by the fused branch.
                        crate::avif::encode_avif_with_session(
                            session,
                            &s.y16,
                            &s.cb16,
                            &s.cr16,
                            icc.as_deref(),
                        )?
                    }
                }
            }
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
    unsafe { std::slice::from_raw_parts(buf.as_ptr().cast(), buf.len() * 2) }
}

fn u16_as_bytes_mut(buf: &mut [u16]) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(buf.as_mut_ptr().cast(), buf.len() * 2) }
}

/// Decode (with DCT shrink-on-load) + SIMD resize; returns RGB pixels
/// and final dimensions. Honors EXIF auto-rotation exactly like the
/// server pipeline (`OXIMG_AUTO_ROTATE=0` disables).
pub fn decode_and_resize(
    jpeg: &[u8],
    max_w: u32,
    max_h: u32,
    parallel: usize,
) -> Result<(Vec<u8>, usize, usize)> {
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        let orientation = if auto_rotate() {
            let mut prefix = Vec::new();
            let mut r = jpeg;
            crate::meta::scan_jpeg_meta(&mut r, &mut prefix, false).orientation
        } else {
            crate::meta::Orientation::UPRIGHT
        };
        let dec = Decompress::new_mem(jpeg).context("invalid JPEG")?;
        match decode_resize(s, dec, max_w, max_h, parallel, orientation, Fuse::Off)? {
            Decoded::Pixels { dst_w, dst_h } => {
                if orientation.is_upright() {
                    Ok((s.out8[..dst_w * dst_h * 3].to_vec(), dst_w, dst_h))
                } else {
                    let mut rotated = Vec::new();
                    let (dw, dh) = crate::meta::apply_orientation(
                        &s.out8[..dst_w * dst_h * 3],
                        dst_w,
                        dst_h,
                        3,
                        orientation,
                        &mut rotated,
                    );
                    Ok((rotated, dw, dh))
                }
            }
            #[cfg(feature = "avif")]
            Decoded::YuvPlanes { .. } => unreachable!("no fuse was requested"),
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
    Pixels {
        dst_w: usize,
        dst_h: usize,
    },
    Encoded(Vec<u8>),
    /// 10-bit 4:2:0 planes left in `Scratch::{y16,cb16,cr16}`, plus the
    /// encoder session the fused worker already created during the
    /// decode overlap — only the SVT encode itself remains.
    #[cfg(feature = "avif")]
    YuvPlanes {
        session: crate::avif::SvtSession,
    },
}

/// How the JPEG decode overlaps with downstream work (all variants
/// produce identical pixels/bytes; see overlap_gate).
#[derive(Clone, Copy)]
enum Fuse {
    /// Serial: decode, then resize inline on the same thread.
    Off,
    /// Decode ∥ resize + incremental jpegli encode on a worker thread —
    /// the same-format JPEG fast path; yields `Decoded::Encoded`.
    Jpegli { quality: f32 },
    /// Decode ∥ resize into `Scratch::out8` on a worker thread; the
    /// (one-shot) target encoder runs after. Used for cross-format
    /// targets, hiding the resize behind the decode wall.
    Pixels,
    /// Decode ∥ resize + RGB→YUV conversion straight into the 10-bit
    /// planes on the worker thread, which also creates the SVT session
    /// while the decode runs — AVIF targets, hiding the resize, the
    /// conversion, and the encoder setup behind the decode wall.
    #[cfg(feature = "avif")]
    Yuv { params: crate::avif::AvifParams },
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

fn decode_resize<R: std::io::BufRead>(
    s: &mut Scratch,
    mut dec: Decompress<R>,
    max_w: u32,
    max_h: u32,
    parallel: usize,
    orientation: crate::meta::Orientation,
    fuse: Fuse,
) -> Result<Decoded> {
    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();

    let (src_w, src_h) = dec.size();
    // The target box constrains the *displayed* frame; for the
    // axis-swapping orientations the resize target (still in stored
    // orientation — the rotation happens on the resized frame) is the
    // fitted box with its axes swapped back.
    let (disp_w, disp_h) = orientation.display_dims(src_w, src_h);
    let (fit_w, fit_h) = fit_dims(disp_w, disp_h, max_w, max_h);
    let (dst_w, dst_h) = if orientation.swaps_axes() {
        (fit_h, fit_w)
    } else {
        (fit_w, fit_h)
    };

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

    if let Fuse::Jpegli { quality } = fuse
        && linear
        && let Some((out, decode_ms)) =
            fused_resize_encode(&mut started, dec_w, dec_h, dst_w, dst_h, quality)?
    {
        if timing {
            let total = t0.elapsed().as_secs_f64() * 1e3;
            eprintln!(
                "timing fused({dec_w}x{dec_h}->{dst_w}x{dst_h}) decode={decode_ms:.1}ms tail={:.1}ms total={total:.1}ms",
                total - decode_ms
            );
        }
        started.finish().context("decode finish failed")?;
        return Ok(Decoded::Encoded(out));
    }

    if let Fuse::Pixels = fuse
        && linear
    {
        scratch_u8(&mut s.out8, dst_w * dst_h * 3);
        if let Some(decode_ms) = fused_resize_pixels(
            &mut started,
            dec_w,
            dec_h,
            dst_w,
            dst_h,
            &mut s.out8[..dst_w * dst_h * 3],
        )? {
            if timing {
                let total = t0.elapsed().as_secs_f64() * 1e3;
                eprintln!(
                    "timing fused-px({dec_w}x{dec_h}->{dst_w}x{dst_h}) decode={decode_ms:.1}ms tail={:.1}ms total={total:.1}ms",
                    total - decode_ms
                );
            }
            started.finish().context("decode finish failed")?;
            return Ok(Decoded::Pixels { dst_w, dst_h });
        }
    }

    #[cfg(feature = "avif")]
    if let Fuse::Yuv { params } = fuse
        && linear
    {
        let (cw, chh) = (dst_w.div_ceil(2), dst_h.div_ceil(2));
        // Truncate to the exact frame: the session encode consumes the
        // whole vectors (their length feeds SVT's n_filled_len).
        scratch_u16(&mut s.y16, dst_w * dst_h);
        s.y16.truncate(dst_w * dst_h);
        scratch_u16(&mut s.cb16, cw * chh);
        s.cb16.truncate(cw * chh);
        scratch_u16(&mut s.cr16, cw * chh);
        s.cr16.truncate(cw * chh);
        if let Some((decode_ms, session)) = fused_resize_yuv(
            &mut started,
            dec_w,
            dec_h,
            dst_w,
            dst_h,
            &params,
            &mut s.y16,
            &mut s.cb16,
            &mut s.cr16,
        )? {
            if timing {
                let total = t0.elapsed().as_secs_f64() * 1e3;
                eprintln!(
                    "timing fused-yuv({dec_w}x{dec_h}->{dst_w}x{dst_h}) decode={decode_ms:.1}ms tail={:.1}ms total={total:.1}ms",
                    total - decode_ms
                );
            }
            started.finish().context("decode finish failed")?;
            return Ok(Decoded::YuvPlanes { session });
        }
    }

    if linear {
        // Stream each decoded chunk's rows through the SIMD resize
        // kernel — the exact consumer the fused path runs on its worker
        // thread, inline: the sRGB -> linear LUT fuses into row staging
        // (no u16 intermediate image), and completed output rows go
        // through the back LUT as they emit. Serial and fused therefore
        // produce identical bytes on every architecture.
        #[cfg(any(target_arch = "aarch64", target_arch = "x86_64"))]
        if parallel <= 1
            && std::env::var("OXIMG_RESIZE_BACKEND").as_deref() != Ok("fir")
            && let Ok(mut resizer) =
                crate::resize_kernel::StreamResize::<FuseKernel>::new(dec_w, dec_h, dst_w, dst_h, 3)
        {
            let fwd = fwd_lut_f32();
            let back = back_lut();
            let chunk_rows = (256 * 1024 / row_bytes).clamp(1, dec_h);
            scratch_u8(&mut s.chunk8, chunk_rows * row_bytes);
            scratch_u8(&mut s.out8, dst_w * dst_h * 3);
            let out8 = &mut s.out8;
            let mut remaining = dec_h;
            while remaining > 0 {
                let want = remaining.min(chunk_rows) * row_bytes;
                let got = started
                    .read_scanlines_into(&mut s.chunk8[..want])
                    .context("decode failed")?
                    .len();
                anyhow::ensure!(
                    got > 0 && got % row_bytes == 0,
                    "decoder returned a partial row"
                );
                remaining -= got / row_bytes;
                for row in s.chunk8[..got].chunks_exact(row_bytes) {
                    resizer.push_row_u8(row, fwd, |oy, out| {
                        for (d, &v) in out8[oy * dst_w * 3..(oy + 1) * dst_w * 3]
                            .iter_mut()
                            .zip(out)
                        {
                            *d = back[v as usize];
                        }
                    });
                }
            }
            anyhow::ensure!(
                resizer.rows_emitted() == dst_h,
                "decode ended before the image was complete"
            );
            started.finish().context("decode finish failed")?;
            if timing {
                eprintln!(
                    "timing streamed({dec_w}x{dec_h}->{dst_w}x{dst_h}) total={:.1}ms",
                    t0.elapsed().as_secs_f64() * 1e3
                );
            }
            return Ok(Decoded::Pixels { dst_w, dst_h });
        }

        // Full-frame fallback (band-parallel resize, OXIMG_RESIZE_BACKEND
        // =fir, or CPUs without the SIMD kernel): decode in chunks and
        // apply the sRGB u8 -> linear u16 LUT on the fly; each chunk
        // stays in L2, saving a second full-image memory pass.
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
/// On success returns the encoded bytes and the wall milliseconds the
/// decode loop took on this thread (the fused pipeline's floor).
fn fused_resize_encode<R: std::io::BufRead>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    quality: f32,
) -> Result<Option<(Vec<u8>, f64)>> {
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

        std::thread::scope(|sc| -> Result<Option<(Vec<u8>, f64)>> {
            // Borrowed, not moved: the resizer's Drop must run on this
            // long-lived blocking-pool thread so its kernel scratch
            // returns to this thread's pool instead of dying with the
            // ephemeral worker's TLS.
            let resizer = &mut resizer;
            let spawned = std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || -> Result<Vec<u8>> {
                    let fwd = fwd_lut_f32();
                    let back = back_lut();
                    let mut row8 = vec![0u8; dst_w * 3];

                    let mut comp = jpegli::Compress::new(jpegli::ColorSpace::JCS_RGB);
                    comp.set_size(dst_w, dst_h);
                    comp.set_quality(quality);
                    // Mirrors encode_jpegli (including the progressive knob).
                    if jpegli_progressive() {
                        comp.set_progressive_mode();
                    }
                    let mut enc = comp.start_compress(Vec::with_capacity(64 * 1024))?;

                    while let Ok((buf, rows)) = chunk_rx.recv() {
                        for r in 0..rows {
                            let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                            let mut enc_result = Ok(());
                            resizer.push_row_u8(src, fwd, |_, out| {
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
                });
            // Spawn failure (thread limits, transient EAGAIN) leaves the
            // decoder untouched, exactly like a missing kernel — fall
            // back to the byte-identical serial path instead of failing.
            let Ok(worker) = spawned else {
                return Ok(None);
            };

            // Decode loop on the request thread: read a chunk, hand it to
            // the worker, reuse buffers the worker has drained.
            let t_decode = std::time::Instant::now();
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
            let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
            drop(chunk_tx);

            let encoded = worker
                .join()
                .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
            // A decode error is the root cause; report it over the worker's
            // consequent "incomplete image" error.
            decode_result?;
            Ok(Some((encoded?, decode_ms)))
        })
    }
}

/// The cross-format sibling of [`fused_resize_encode`]: the request
/// thread runs the same decode loop while a scoped worker streams rows
/// through the SIMD kernel straight into `out8` — the exact writes the
/// serial path performs inline, so pixels are byte-identical to it.
/// The (one-shot) target encoder runs after, on the request thread;
/// only the encode stays outside the decode wall, which is as much
/// overlap as WebP/AVIF/PNG's full-frame encode APIs allow.
///
/// Returns Ok(None) — decoder untouched — when no SIMD row kernel
/// exists for this CPU; on success returns the decode-loop wall
/// milliseconds, with `out8` fully written.
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
fn fused_resize_pixels<R: std::io::BufRead>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    out8: &mut [u8],
) -> Result<Option<f64>> {
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
        // Chunking and channel shapes mirror fused_resize_encode.
        let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize)>(2);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::scope(|sc| -> Result<Option<f64>> {
            // The worker borrows the resizer instead of consuming it, so
            // its Drop runs on this (long-lived blocking-pool) thread and
            // the kernel scratch returns to this thread's pool — dropping
            // it on the ephemeral worker would leak the pool entry into
            // that thread's dying TLS.
            let resizer = &mut resizer;
            let spawned = std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || -> Result<()> {
                    let fwd = fwd_lut_f32();
                    let back = back_lut();
                    while let Ok((buf, rows)) = chunk_rx.recv() {
                        for r in 0..rows {
                            let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                            resizer.push_row_u8(src, fwd, |oy, out| {
                                for (d, &v) in out8[oy * dst_w * 3..(oy + 1) * dst_w * 3]
                                    .iter_mut()
                                    .zip(out)
                                {
                                    *d = back[v as usize];
                                }
                            });
                        }
                        let _ = recycle_tx.send(buf);
                    }
                    anyhow::ensure!(
                        resizer.rows_emitted() == dst_h,
                        "decode ended before the image was complete"
                    );
                    Ok(())
                });
            // Spawn failure (thread limits, transient EAGAIN) leaves the
            // decoder untouched, exactly like a missing kernel — fall
            // back to the byte-identical serial path instead of failing.
            let Ok(worker) = spawned else {
                return Ok(None);
            };

            let t_decode = std::time::Instant::now();
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
            let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
            drop(chunk_tx);

            let worker_result = worker
                .join()
                .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
            // A decode error is the root cause; report it over the
            // worker's consequent "incomplete image" error.
            decode_result?;
            worker_result?;
            Ok(Some(decode_ms))
        })
    }
}

/// The AVIF sibling of [`fused_resize_pixels`]: the worker converts
/// each resized row straight into the 10-bit 4:2:0 planes (luma per
/// row, chroma per row pair via the same row API the full-frame
/// conversion uses, so the planes are bit-identical to converting
/// `out8` afterwards) — both the resize and the RGB→YUV conversion hide
/// behind the decode wall, and the resized frame never exists as an
/// interleaved RGB copy. Only the one-shot SVT encode remains outside.
///
/// Returns Ok(None) — decoder untouched — when no SIMD row kernel
/// exists; on success returns the decode-loop wall milliseconds with
/// all three planes fully written.
#[cfg(feature = "avif")]
#[cfg_attr(
    not(any(target_arch = "aarch64", target_arch = "x86_64")),
    allow(unused_variables)
)]
#[allow(clippy::too_many_arguments)]
fn fused_resize_yuv<R: std::io::BufRead>(
    started: &mut mozjpeg::decompress::DecompressStarted<R>,
    dec_w: usize,
    dec_h: usize,
    dst_w: usize,
    dst_h: usize,
    params: &crate::avif::AvifParams,
    y_plane: &mut [u16],
    cb_plane: &mut [u16],
    cr_plane: &mut [u16],
) -> Result<Option<(f64, crate::avif::SvtSession)>> {
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
        // Chunking mirrors fused_resize_encode, but with double the
        // channel runway: the worker spends its first ~1ms creating the
        // SVT session, and four in-flight chunks let the decoder keep
        // running instead of stalling on the bounded channel meanwhile.
        let chunk_rows = (64 * 1024 / row_bytes).clamp(1, dec_h);
        let (chunk_tx, chunk_rx) = std::sync::mpsc::sync_channel::<(Vec<u8>, usize)>(4);
        let (recycle_tx, recycle_rx) = std::sync::mpsc::channel::<Vec<u8>>();

        std::thread::scope(|sc| -> Result<Option<(f64, crate::avif::SvtSession)>> {
            // Borrowed, not moved — see fused_resize_pixels.
            let resizer = &mut resizer;
            let cw = dst_w.div_ceil(2);
            let spawned = std::thread::Builder::new()
                .name("oximg-fuse".into())
                .spawn_scoped(sc, move || -> Result<crate::avif::SvtSession> {
                    // Encoder setup first: its ~1ms overlaps the
                    // decoder's first chunks instead of the tail.
                    let session = crate::avif::start_color_session(dst_w, dst_h, params)?;
                    let fwd = fwd_lut_f32();
                    let back = back_lut();
                    let mut row8 = vec![0u8; dst_w * 3];
                    // Chroma needs the row pair; even rows park here.
                    let mut prev_row = vec![0u8; dst_w * 3];
                    while let Ok((buf, rows)) = chunk_rx.recv() {
                        for r in 0..rows {
                            let src = &buf[r * row_bytes..(r + 1) * row_bytes];
                            resizer.push_row_u8(src, fwd, |oy, out| {
                                for (d, &v) in row8.iter_mut().zip(out) {
                                    *d = back[v as usize];
                                }
                                crate::avif::luma_rows(
                                    &row8,
                                    3,
                                    &mut y_plane[oy * dst_w..][..dst_w],
                                );
                                if oy % 2 == 1 {
                                    let cy = oy / 2;
                                    crate::avif::chroma_row_pair(
                                        &prev_row,
                                        Some(&row8),
                                        dst_w,
                                        3,
                                        &mut cb_plane[cy * cw..][..cw],
                                        &mut cr_plane[cy * cw..][..cw],
                                    );
                                } else {
                                    prev_row.copy_from_slice(&row8);
                                }
                            });
                        }
                        let _ = recycle_tx.send(buf);
                    }
                    anyhow::ensure!(
                        resizer.rows_emitted() == dst_h,
                        "decode ended before the image was complete"
                    );
                    // Odd height: the last row's chroma has no partner.
                    if dst_h % 2 == 1 {
                        let cy = dst_h / 2;
                        crate::avif::chroma_row_pair(
                            &prev_row,
                            None,
                            dst_w,
                            3,
                            &mut cb_plane[cy * cw..][..cw],
                            &mut cr_plane[cy * cw..][..cw],
                        );
                    }
                    Ok(session)
                });
            // Spawn failure leaves the decoder untouched — fall back to
            // the byte-identical serial path instead of failing.
            let Ok(worker) = spawned else {
                return Ok(None);
            };

            let t_decode = std::time::Instant::now();
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
            let decode_ms = t_decode.elapsed().as_secs_f64() * 1e3;
            drop(chunk_tx);

            let worker_result = worker
                .join()
                .map_err(|_| anyhow::anyhow!("fuse worker panicked"))?;
            // A decode error is the root cause; report it over the
            // worker's consequent "incomplete image" error.
            decode_result?;
            let session = worker_result?;
            Ok(Some((decode_ms, session)))
        })
    }
}

fn linear_light() -> bool {
    std::env::var("OXIMG_RESIZE").as_deref() != Ok("srgb")
}

/// OXIMG_AUTO_ROTATE: apply EXIF orientation (default on; "0"
/// disables, which also skips the pre-decode segment scan entirely).
fn auto_rotate() -> bool {
    static A: OnceLock<bool> = OnceLock::new();
    *A.get_or_init(|| std::env::var("OXIMG_AUTO_ROTATE").as_deref() != Ok("0"))
}

/// OXIMG_ICC: carry the source's ICC profile into the output (default
/// on; "0" disables and skips profile extraction entirely). Pixels are
/// never color-converted — the profile bytes are passed through.
fn icc_passthrough() -> bool {
    static I: OnceLock<bool> = OnceLock::new();
    *I.get_or_init(|| std::env::var("OXIMG_ICC").as_deref() != Ok("0"))
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

fn process_png<R: std::io::Read>(
    s: &mut Scratch,
    mut reader: R,
    target: ImageFormat,
    p: &Params,
) -> Result<Vec<u8>> {
    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    s.srcbuf.clear();
    reader
        .read_to_end(&mut s.srcbuf)
        .context("read PNG source")?;
    let mut decoder = png::Decoder::new(std::io::Cursor::new(&s.srcbuf[..]));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut png_reader = decoder.read_info().context("parse PNG")?;
    // eXIf orientation (raw TIFF per the PNG spec; a stray JPEG-style
    // prefix is tolerated like browsers do). Only chunks ahead of the
    // image data are seen — the orientation must steer the resize box,
    // so it has to be known before decoding. The registration allows
    // post-IDAT placement, but Chrome and Firefox also honor only
    // pre-IDAT eXIf, so serving those unrotated is browser parity, and
    // fail-safe besides.
    let orientation = if auto_rotate() {
        png_reader
            .info()
            .exif_metadata
            .as_ref()
            .and_then(|d| crate::meta::Orientation::from_exif_payload(d))
            .unwrap_or(crate::meta::Orientation::UPRIGHT)
    } else {
        crate::meta::Orientation::UPRIGHT
    };
    // iCCP profile (the png crate has already inflated it), bytes
    // passed through untouched.
    let icc: Option<Vec<u8>> = if icc_passthrough() && target_supports_icc(target) {
        png_reader
            .info()
            .icc_profile
            .as_ref()
            .filter(|c| !c.is_empty() && c.len() <= ICC_CAP)
            .map(|c| c.to_vec())
    } else {
        None
    };

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
            // Oriented PNGs are rare; they take the general arm below
            // rather than teaching the row-streaming path to rotate.
            && orientation.is_upright()
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
            let out = encode_output(s, dst_w, dst_h, 3, target, p, icc.as_deref());
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
    let (dst_w, dst_h) = resize_pixels_oriented(s, channels, src_w, src_h, orientation, p)?;
    let t_resize = t1.elapsed();
    let t2 = std::time::Instant::now();
    let out = encode_output(s, dst_w, dst_h, channels, target, p, icc.as_deref());
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

fn process_webp<R: std::io::Read>(
    s: &mut Scratch,
    mut reader: R,
    target: ImageFormat,
    p: &Params,
) -> Result<Vec<u8>> {
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
    // One mux parse serves both metadata chunks: the ICC profile and
    // the EXIF orientation (raw TIFF or JPEG-style prefixed; writers
    // disagree, browsers accept both).
    let (icc, exif) = webp_metadata(
        &s.srcbuf,
        icc_passthrough() && target_supports_icc(target),
        auto_rotate(),
    );
    let orientation = exif
        .and_then(|d| crate::meta::Orientation::from_exif_payload(&d))
        .unwrap_or(crate::meta::Orientation::UPRIGHT);
    let (src_w, src_h, channels, dec_w, dec_h) = webp_decode_into_chunk8(s, orientation, p)?;
    let _ = (src_w, src_h);
    let t_dec = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (dst_w, dst_h) = resize_pixels_oriented(s, channels, dec_w, dec_h, orientation, p)?;
    let t_resize = t1.elapsed();

    let t2 = std::time::Instant::now();
    let out = encode_output(s, dst_w, dst_h, channels, target, p, icc.as_deref())?;
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
fn process_avif<R: std::io::Read>(
    s: &mut Scratch,
    mut reader: R,
    target: ImageFormat,
    p: &Params,
) -> Result<Vec<u8>> {
    s.srcbuf.clear();
    reader
        .read_to_end(&mut s.srcbuf)
        .context("read AVIF source")?;

    let timing = std::env::var("OXIMG_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    // avif-parse exposes neither colr nor irot/imir; both come from
    // our own bounded container walk.
    let icc = if icc_passthrough() && target_supports_icc(target) {
        crate::avif::extract_icc(&s.srcbuf)
    } else {
        None
    };
    let orientation = if auto_rotate() {
        crate::avif::extract_orientation(&s.srcbuf)
    } else {
        crate::meta::Orientation::UPRIGHT
    };
    let (src_w, src_h, channels) = crate::avif::decode_avif_into(&s.srcbuf, &mut s.chunk8)?;
    let t_dec = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (dst_w, dst_h) = resize_pixels_oriented(s, channels, src_w, src_h, orientation, p)?;
    let t_resize = t1.elapsed();

    let t2 = std::time::Instant::now();
    let out = encode_output(s, dst_w, dst_h, channels, target, p, icc.as_deref())?;
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
    orientation: crate::meta::Orientation,
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

        // The stored-space resize target: fit the *displayed* frame,
        // swap back for axis-swapping orientations. Deciding the decode
        // scale from the unoriented fit would under-decode sources
        // whose displayed aspect fits the box differently.
        let (disp_w, disp_h) = orientation.display_dims(src_w, src_h);
        let (fit_w, fit_h) = fit_dims(disp_w, disp_h, p.max_width, p.max_height);
        let (dst_w, dst_h) = if orientation.swaps_axes() {
            (fit_h, fit_w)
        } else {
            (fit_w, fit_h)
        };
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

/// Resize the fully decoded pixels in `chunk8` honoring an EXIF-style
/// orientation: the box fits the *displayed* frame, the resize runs in
/// the stored orientation, and the pixels rotate afterwards on the
/// small output frame — the same strategy as the JPEG arm, shared by
/// the PNG/WebP/AVIF full-frame paths.
fn resize_pixels_oriented(
    s: &mut Scratch,
    channels: usize,
    src_w: usize,
    src_h: usize,
    orientation: crate::meta::Orientation,
    p: &Params,
) -> Result<(usize, usize)> {
    let (disp_w, disp_h) = orientation.display_dims(src_w, src_h);
    let (fit_w, fit_h) = fit_dims(disp_w, disp_h, p.max_width, p.max_height);
    let (dst_w, dst_h) = if orientation.swaps_axes() {
        (fit_h, fit_w)
    } else {
        (fit_w, fit_h)
    };
    resize_pixels_to(s, channels, src_w, src_h, dst_w, dst_h, p.parallel)?;
    if orientation.is_upright() {
        return Ok((dst_w, dst_h));
    }
    // chunk8 held the decoded source; it is free once the resize is
    // done, so rotate into it and swap it in as out8.
    let dims = crate::meta::apply_orientation(
        &s.out8[..dst_w * dst_h * channels],
        dst_w,
        dst_h,
        channels,
        orientation,
        &mut s.chunk8,
    );
    std::mem::swap(&mut s.out8, &mut s.chunk8);
    Ok(dims)
}

/// Resize the fully decoded pixels in `chunk8` (3 or 4 channels) into
/// `out8` at exactly `dst_w`x`dst_h`. RGB follows the same
/// linear-light path as JPEG; alpha images are premultiplied before
/// resampling and unpremultiplied after.
fn resize_pixels_to(
    s: &mut Scratch,
    channels: usize,
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    parallel: usize,
) -> Result<(usize, usize)> {
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
            parallel,
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
            parallel,
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

fn encode_png(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64 * 1024);
    // The no-profile arm constructs the encoder exactly as the pre-ICC
    // code did, keeping profile-less output byte-identical.
    let mut enc = match icc {
        Some(icc) => {
            let mut info = png::Info::with_size(w as u32, h as u32);
            info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
            png::Encoder::with_info(&mut out, info).context("PNG info")?
        }
        None => png::Encoder::new(&mut out, w as u32, h as u32),
    };
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

/// OXIMG_AVIF_SPEED: SVT preset (enc_mode). The default (8) is the
/// benchmarked sync-path operating point; 9 trades some quality per
/// byte for a faster encode (see QUALITY.md before changing it fleet-
/// wide).
#[cfg(feature = "avif")]
fn avif_speed() -> i8 {
    std::env::var("OXIMG_AVIF_SPEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8)
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

fn encode_webp(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let out = encode_webp_bare(pixels, w, h, channels)?;
    match icc {
        Some(icc) => wrap_webp_icc(&out, icc),
        None => Ok(out),
    }
}

fn encode_webp_bare(pixels: &[u8], w: usize, h: usize, channels: usize) -> Result<Vec<u8>> {
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

/// Re-container an encoded WebP with an `ICCP` chunk via WebPMux (the
/// mux sets the required VP8X flags itself). Only profiled sources pay
/// for the extra assembly copy.
fn wrap_webp_icc(webp: &[u8], icc: &[u8]) -> Result<Vec<u8>> {
    use libwebp_sys as wp;
    unsafe {
        let data = wp::WebPData {
            bytes: webp.as_ptr(),
            size: webp.len(),
        };
        // copy_data=0: the mux borrows `webp`, which outlives it.
        let mux = wp::WebPMuxCreateInternal(&data, 0, wp::WEBP_MUX_ABI_VERSION as _);
        anyhow::ensure!(!mux.is_null(), "webp mux parse");
        let chunk = wp::WebPData {
            bytes: icc.as_ptr(),
            size: icc.len(),
        };
        let rc = wp::WebPMuxSetChunk(mux, c"ICCP".as_ptr(), &chunk, 1);
        if rc != wp::WebPMuxError::WEBP_MUX_OK {
            wp::WebPMuxDelete(mux);
            anyhow::bail!("webp ICCP set failed ({rc:?})");
        }
        let mut assembled: wp::WebPData = std::mem::zeroed();
        let rc = wp::WebPMuxAssemble(mux, &mut assembled);
        wp::WebPMuxDelete(mux);
        if rc != wp::WebPMuxError::WEBP_MUX_OK {
            wp::WebPDataClear(&mut assembled);
            anyhow::bail!("webp mux assemble failed ({rc:?})");
        }
        let out = std::slice::from_raw_parts(assembled.bytes, assembled.size).to_vec();
        wp::WebPDataClear(&mut assembled);
        Ok(out)
    }
}

/// Extract the ICCP and/or EXIF chunks from a WebP container in one
/// mux parse.
fn webp_metadata(
    srcbuf: &[u8],
    want_icc: bool,
    want_exif: bool,
) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    use libwebp_sys as wp;
    if !want_icc && !want_exif {
        return (None, None);
    }
    unsafe {
        let data = wp::WebPData {
            bytes: srcbuf.as_ptr(),
            size: srcbuf.len(),
        };
        let mux = wp::WebPMuxCreateInternal(&data, 0, wp::WEBP_MUX_ABI_VERSION as _);
        if mux.is_null() {
            return (None, None);
        }
        // The chunk data points into `srcbuf`/mux internals: copy out
        // before the mux is deleted.
        let get = |fourcc: &std::ffi::CStr| -> Option<Vec<u8>> {
            let mut chunk: wp::WebPData = std::mem::zeroed();
            let rc = wp::WebPMuxGetChunk(mux, fourcc.as_ptr(), &mut chunk);
            (rc == wp::WebPMuxError::WEBP_MUX_OK
                && !chunk.bytes.is_null()
                && chunk.size > 0
                && chunk.size <= ICC_CAP)
                .then(|| std::slice::from_raw_parts(chunk.bytes, chunk.size).to_vec())
        };
        let icc = if want_icc { get(c"ICCP") } else { None };
        let exif = if want_exif { get(c"EXIF") } else { None };
        wp::WebPMuxDelete(mux);
        (icc, exif)
    }
}

/// Encode the resized RGB(A)8 pixels in `Scratch::out8` as `target`.
/// When `target` matches the source format this calls the same encoder
/// with the same arguments as the pre-cross-format code, so same-format
/// output stays byte-identical. JPEG is the only alpha-less target;
/// RGBA input is flattened onto OXIMG_FLATTEN_BG first.
fn encode_output(
    s: &mut Scratch,
    dst_w: usize,
    dst_h: usize,
    channels: usize,
    target: ImageFormat,
    p: &Params,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    match target {
        ImageFormat::Jpeg => {
            if channels == 4 {
                flatten_alpha_in_out8(s, dst_w, dst_h);
            }
            encode_with_icc(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, p, icc)
        }
        ImageFormat::Png => encode_png(
            &s.out8[..dst_w * dst_h * channels],
            dst_w,
            dst_h,
            channels,
            icc,
        ),
        ImageFormat::Webp => encode_webp(
            &s.out8[..dst_w * dst_h * channels],
            dst_w,
            dst_h,
            channels,
            icc,
        ),
        // Fully opaque RGBA drops its alpha item inside encode_avif
        // (skipping the second SVT session entirely).
        #[cfg(feature = "avif")]
        ImageFormat::Avif => {
            let quality = avif_quality();
            let params = crate::avif::AvifParams {
                quality,
                alpha_quality: avif_alpha_quality(quality),
                speed: avif_speed(),
                ..Default::default()
            };
            crate::avif::encode_avif(
                &s.out8[..dst_w * dst_h * channels],
                dst_w,
                dst_h,
                channels,
                &params,
                icc,
            )
        }
        #[cfg(not(feature = "avif"))]
        ImageFormat::Avif => anyhow::bail!("AVIF support is not enabled in this build"),
    }
}

/// OXIMG_FLATTEN_BG: background for alpha -> JPEG flattening, as RRGGBB
/// hex; default white.
fn flatten_bg() -> [u8; 3] {
    static BG: OnceLock<[u8; 3]> = OnceLock::new();
    *BG.get_or_init(|| {
        std::env::var("OXIMG_FLATTEN_BG")
            .ok()
            .and_then(|v| {
                let v = v.trim().trim_start_matches('#');
                // is_ascii keeps the byte-offset slicing below from
                // panicking on multi-byte values; malformed input falls
                // back to white either way.
                if v.len() != 6 || !v.is_ascii() {
                    return None;
                }
                let c = |i| u8::from_str_radix(&v[i..i + 2], 16).ok();
                Some([c(0)?, c(2)?, c(4)?])
            })
            .unwrap_or([255, 255, 255])
    })
}

/// Composite the straight-alpha RGBA8 pixels in `out8` onto the
/// flatten background in linear light, compacting to RGB8 in place
/// (pixel i writes 3i..3i+3 after reading 4i..4i+4, so the forward
/// pass never clobbers unread input).
fn flatten_alpha_in_out8(s: &mut Scratch, dst_w: usize, dst_h: usize) {
    let (fwd, back) = (fwd_lut(), back_lut());
    let bg = flatten_bg();
    let bg_lin = [
        fwd[bg[0] as usize] as u32,
        fwd[bg[1] as usize] as u32,
        fwd[bg[2] as usize] as u32,
    ];
    for i in 0..dst_w * dst_h {
        let px: [u8; 4] = s.out8[i * 4..i * 4 + 4].try_into().unwrap();
        let a = px[3] as u32;
        for c in 0..3 {
            let lin = (fwd[px[c] as usize] as u32 * a + bg_lin[c] * (255 - a) + 127) / 255;
            s.out8[i * 3 + c] = back[lin as usize];
        }
    }
}

pub fn encode(rgb: &[u8], w: usize, h: usize, p: &Params) -> Result<Vec<u8>> {
    encode_with_icc(rgb, w, h, p, None)
}

/// JPEG encode with an optional ICC profile written ahead of the
/// scanlines (both encoders chunk it into the standard APP2 chain).
/// With `icc: None` this is byte-identical to the pre-ICC encoder.
fn encode_with_icc(
    rgb: &[u8],
    w: usize,
    h: usize,
    p: &Params,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    if p.encoder == Encoder::Jpegli {
        return encode_jpegli(rgb, w, h, p.quality, icc);
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
    if let Some(icc) = icc {
        for chunk in icc_app2_chunks(icc) {
            started.write_marker(mozjpeg::Marker::APP(2), &chunk);
        }
    }
    started.write_scanlines(rgb)?;
    Ok(started.finish()?)
}

/// Chunk a profile into standard APP2 `ICC_PROFILE` payloads with
/// 1-based sequence numbers. Deliberately not the mozjpeg crate's
/// `write_icc_profile`, which emits 0-based sequence numbers (as of
/// 0.10.13) that libjpeg's own `jpeg_read_icc_profile` — and therefore
/// browsers — reject wholesale.
fn icc_app2_chunks(icc: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
    const MAX_DATA: usize = 65533 - 14;
    let count = icc.len().div_ceil(MAX_DATA);
    // ICC_CAP (and SCAN_CAP on the JPEG side) keep count well under
    // the u8 chunk-count limit.
    debug_assert!(count <= 255);
    icc.chunks(MAX_DATA).enumerate().map(move |(i, part)| {
        let mut v = Vec::with_capacity(14 + part.len());
        v.extend_from_slice(b"ICC_PROFILE\0");
        v.push((i + 1) as u8);
        v.push(count as u8);
        v.extend_from_slice(part);
        v
    })
}

/// jpegli encode via its libjpeg-compatible API (symbols are
/// `jpegli_`-prefixed, so it links alongside mozjpeg without conflicts).
/// OXIMG_JPEG_PROGRESSIVE=0 selects baseline jpegli: a few percent
/// larger output, but the entropy pass at finish_compress shrinks,
/// which is the fused path's only serial tail.
fn jpegli_progressive() -> bool {
    static P: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *P.get_or_init(|| std::env::var("OXIMG_JPEG_PROGRESSIVE").as_deref() != Ok("0"))
}

fn encode_jpegli(
    rgb: &[u8],
    w: usize,
    h: usize,
    quality: f32,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut comp = jpegli::Compress::new(jpegli::ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(quality);
    // cjpegli emits progressive by default; the libjpeg-compat layer does
    // not. Progressive is worth several percent at these sizes.
    if jpegli_progressive() {
        comp.set_progressive_mode();
    }
    let mut started = comp.start_compress(Vec::with_capacity(64 * 1024))?;
    if let Some(icc) = icc {
        for chunk in icc_app2_chunks(icc) {
            started.write_marker(jpegli::Marker::APP(2), &chunk);
        }
    }
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
            output: None,
        };
        let fuse = match fuse_quality {
            Some(quality) => Fuse::Jpegli { quality },
            None => Fuse::Off,
        };
        let mut s = Scratch::default();
        let dec = Decompress::new_mem(jpeg).unwrap();
        match decode_resize(
            &mut s,
            dec,
            320,
            320,
            1,
            crate::meta::Orientation::UPRIGHT,
            fuse,
        )
        .unwrap()
        {
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
            #[cfg(feature = "avif")]
            Decoded::YuvPlanes { .. } => panic!("yuv fuse was not requested"),
        }
    }

    /// Resized RGB pixels via the given fuse mode (Off = the serial
    /// streamed kernel path, Pixels = the cross-format fused worker).
    fn run_jpeg_pixels(jpeg: &[u8], fuse: Fuse) -> Vec<u8> {
        let mut s = Scratch::default();
        let dec = Decompress::new_mem(jpeg).unwrap();
        match decode_resize(
            &mut s,
            dec,
            320,
            320,
            1,
            crate::meta::Orientation::UPRIGHT,
            fuse,
        )
        .unwrap()
        {
            Decoded::Pixels { dst_w, dst_h } => s.out8[..dst_w * dst_h * 3].to_vec(),
            Decoded::Encoded(_) => panic!("pixel run must not encode"),
            #[cfg(feature = "avif")]
            Decoded::YuvPlanes { .. } => panic!("yuv fuse was not requested"),
        }
    }

    #[cfg(feature = "avif")]
    fn test_avif_params() -> crate::avif::AvifParams {
        crate::avif::AvifParams {
            quality: 55,
            alpha_quality: 55,
            ..Default::default()
        }
    }

    /// The fused AVIF path converts rows (and creates the encoder
    /// session) during the decode overlap; its planes — and therefore
    /// the encoded bytes — must match the serial path's full-frame
    /// conversion of the same pixels exactly.
    #[cfg(feature = "avif")]
    fn run_jpeg_avif(jpeg: &[u8], yuv_fuse: bool) -> Vec<u8> {
        run_jpeg_avif_icc(jpeg, yuv_fuse, None)
    }

    #[cfg(feature = "avif")]
    fn run_jpeg_avif_icc(jpeg: &[u8], yuv_fuse: bool, icc: Option<&[u8]>) -> Vec<u8> {
        let params = test_avif_params();
        let fuse = if yuv_fuse {
            Fuse::Yuv { params }
        } else {
            Fuse::Off
        };
        let mut s = Scratch::default();
        let dec = Decompress::new_mem(jpeg).unwrap();
        match decode_resize(
            &mut s,
            dec,
            320,
            320,
            1,
            crate::meta::Orientation::UPRIGHT,
            fuse,
        )
        .unwrap()
        {
            Decoded::Pixels { dst_w, dst_h } => crate::avif::encode_avif(
                &s.out8[..dst_w * dst_h * 3],
                dst_w,
                dst_h,
                3,
                &params,
                icc,
            )
            .unwrap(),
            Decoded::YuvPlanes { session } => {
                crate::avif::encode_avif_with_session(session, &s.y16, &s.cb16, &s.cr16, icc)
                    .unwrap()
            }
            Decoded::Encoded(_) => panic!("avif run must not hit the jpegli fuse"),
        }
    }

    #[cfg(feature = "avif")]
    #[test]
    fn fused_yuv_bytes_match_serial_avif() {
        if !fuse_kernel_available() {
            return;
        }
        // Odd dimensions exercise chunk boundaries, scalar tails, and
        // the odd-height final chroma row.
        for (w, h, gray) in [(799, 601, false), (400, 300, true), (321, 243, false)] {
            let jpeg = make_test_jpeg(w, h, gray);
            assert_eq!(
                run_jpeg_avif(&jpeg, false),
                run_jpeg_avif(&jpeg, true),
                "{w}x{h} gray={gray}"
            );
        }
        // With a profile the parity must hold too — the fused session
        // path and the one-shot path splice the identical colr.
        let jpeg = make_test_jpeg(321, 243, false);
        let icc: Vec<u8> = (0..500u32).map(|i| (i % 251) as u8).collect();
        let serial = run_jpeg_avif_icc(&jpeg, false, Some(&icc));
        let fused = run_jpeg_avif_icc(&jpeg, true, Some(&icc));
        assert_eq!(serial, fused, "profiled fused/serial parity");
        assert_eq!(crate::avif::extract_icc(&fused).as_deref(), Some(&icc[..]));
    }

    #[cfg(feature = "avif")]
    #[test]
    fn fused_yuv_survives_truncated_sources() {
        let jpeg = make_test_jpeg(799, 601, false);
        let cut = &jpeg[..jpeg.len() * 3 / 5];
        let mut s = Scratch::default();
        if let Ok(dec) = Decompress::new_mem(cut) {
            let _ = decode_resize(
                &mut s,
                dec,
                320,
                320,
                1,
                crate::meta::Orientation::UPRIGHT,
                Fuse::Yuv {
                    params: test_avif_params(),
                },
            );
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

    /// The serial path streams rows through the same SIMD kernel the
    /// fused path runs on its worker thread, so the bytes must match
    /// exactly on every architecture.
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
        assert_eq!(run_jpeg(&jpeg, None), fused);
    }

    /// The cross-format fused worker writes the same rows the serial
    /// streamed path writes inline, so out8 must match byte for byte.
    #[test]
    fn fused_pixels_match_serial_pixels() {
        if !fuse_kernel_available() {
            return;
        }
        // Odd dimensions exercise chunk boundaries and scalar tails.
        for (w, h, gray) in [(799, 601, false), (400, 300, true)] {
            let jpeg = make_test_jpeg(w, h, gray);
            assert_eq!(
                run_jpeg_pixels(&jpeg, Fuse::Off),
                run_jpeg_pixels(&jpeg, Fuse::Pixels),
                "{w}x{h} gray={gray}"
            );
        }
    }

    #[test]
    fn fused_pixels_survive_truncated_sources() {
        let jpeg = make_test_jpeg(799, 601, false);
        let cut = &jpeg[..jpeg.len() * 3 / 5];
        let mut s = Scratch::default();
        if let Ok(dec) = Decompress::new_mem(cut) {
            let _ = decode_resize(
                &mut s,
                dec,
                320,
                320,
                1,
                crate::meta::Orientation::UPRIGHT,
                Fuse::Pixels,
            );
        }
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
            output: None,
        };
        let mut s = Scratch::default();
        if let Ok(dec) = Decompress::new_mem(cut) {
            let _ = decode_resize(
                &mut s,
                dec,
                320,
                320,
                1,
                crate::meta::Orientation::UPRIGHT,
                Fuse::Jpegli { quality: p.quality },
            );
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
