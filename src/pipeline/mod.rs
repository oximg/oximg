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

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Encoder {
    /// jpegli: trellis-class compression at roughly half the CPU of
    /// mozjpeg's trellis path. The default.
    #[default]
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

/// PNG encode effort, mirroring `OXIMG_PNG_EFFORT`'s levels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PngEffort {
    Fastest,
    Fast,
    Balanced,
    High,
}

/// Resize + re-encode parameters for [`process`] and friends.
///
/// Construct with `..Default::default()` and only set the fields you
/// care about — that keeps your build working if a future minor
/// version adds a field. (The struct keeps public fields rather than
/// `#[non_exhaustive]`, which would forbid struct-literal construction
/// across the crate boundary entirely, `..Default` included.)
///
/// The `Option` fields at the bottom are per-call overrides for knobs
/// that are otherwise process-global (`OXIMG_*` environment variables,
/// resolved once at first use): `None` keeps the env-configured
/// behavior byte-for-byte, `Some` takes precedence — so an embedder
/// can run different settings per call, in one process, without
/// touching the environment.
///
/// # Examples
///
/// Contradictory settings coexist in one process — the point of
/// per-call overrides:
///
/// ```
/// use oximg::pipeline::{ImageFormat, Params};
///
/// let thumbnail = Params {
///     max_width: 100,
///     max_height: 100,
///     output: Some(ImageFormat::Webp),
///     webp_quality: Some(40.0),
///     ..Params::default()
/// };
/// let content = Params {
///     max_width: 1600,
///     max_height: 1600,
///     webp_quality: Some(85.0),
///     ..Params::default()
/// };
/// assert_ne!(thumbnail.webp_quality, content.webp_quality);
/// ```
#[derive(Clone, Debug)]
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
    /// WebP encode quality (`OXIMG_WEBP_QUALITY`, default 75).
    pub webp_quality: Option<f32>,
    /// PNG encode effort (`OXIMG_PNG_EFFORT`). Unset, the default
    /// depends on the path: fast for lossless output, balanced when
    /// quantization is active (where effort buys ~2x the reduction).
    pub png_effort: Option<PngEffort>,
    /// Palette-quantize opaque PNG output (`OXIMG_PNG_QUANTIZE`,
    /// default off). Sources with an alpha channel always encode
    /// lossless RGBA regardless of this setting.
    pub png_quantize: Option<bool>,
    /// Quantized palette size, 2-256 (`OXIMG_PNG_QUANTIZE_COLORS`,
    /// default 256). Values outside the range are clamped.
    pub png_quantize_colors: Option<u16>,
    /// Apply EXIF/AVIF orientation (`OXIMG_AUTO_ROTATE`, default on).
    pub auto_rotate: Option<bool>,
    /// Carry the source ICC profile into the output (`OXIMG_ICC`,
    /// default on). `false` also selects the naive CMYK conversion,
    /// exactly like `OXIMG_ICC=0`.
    pub icc: Option<bool>,
    /// Background for alpha→JPEG flattening (`OXIMG_FLATTEN_BG`,
    /// default white).
    pub flatten_bg: Option<[u8; 3]>,
    /// Resize in linear light (`OXIMG_RESIZE`, default on; `false` =
    /// the srgb mode).
    pub linear_light: Option<bool>,
    /// AVIF encode quality (`OXIMG_AVIF_QUALITY`, default 55).
    #[cfg(feature = "avif")]
    pub avif_quality: Option<u8>,
}

impl Default for Params {
    /// Re-encode at the source's own size and format, jpegli q80,
    /// single-threaded, every override unset (env-configured behavior).
    /// The dimension defaults are `u32::MAX`, i.e. no downscale bound —
    /// the pipeline never upscales, so this yields the original
    /// dimensions until a caller sets a smaller box.
    fn default() -> Self {
        Params {
            max_width: u32::MAX,
            max_height: u32::MAX,
            quality: 80.0,
            encoder: Encoder::Jpegli,
            parallel: 1,
            output: None,
            webp_quality: None,
            png_effort: None,
            png_quantize: None,
            png_quantize_colors: None,
            auto_rotate: None,
            icc: None,
            flatten_bg: None,
            linear_light: None,
            #[cfg(feature = "avif")]
            avif_quality: None,
        }
    }
}

/// The largest dimension the format's own container can express.
/// WebP's is a hard 14-bit field in the VP8 bitstream; the others
/// have no limit worth enforcing here (JPEG 65535 and PNG/AVIF 2^31
/// are past the decoded-pixel caps).
fn format_max_dimension(format: ImageFormat) -> Option<u32> {
    match format {
        ImageFormat::Webp => Some(16383),
        // GIF's logical screen fields are u16. Inert while GIF is
        // input-only (a source can never exceed it), and correct the
        // moment it isn't.
        ImageFormat::Gif => Some(65535),
        _ => None,
    }
}

/// Tighten a request's fit box to what the output format can hold.
/// Capping the box (rather than the fitted output) keeps `fit_dims`'
/// proportional scaling intact: a 2000x19708 source asked for
/// width=1920 as WebP comes out 1663x16383, the largest WebP that
/// still has the source's shape. Sources already inside the ceiling
/// are untouched — the box only ever shrinks, and never enlarges.
fn clamp_to_format(mut p: Resolved, target: ImageFormat) -> Resolved {
    let Some(cap) = format_max_dimension(target) else {
        return p;
    };
    // Not counted anywhere: the box is `u32::MAX` on an unconstrained
    // axis, so "the box was tightened" fires on almost every WebP
    // request while "the output was actually reduced" is only knowable
    // where the source dimensions are (four fit_dims call sites down).
    // The returned dimensions describe themselves; the README says so.
    p.max_width = p.max_width.min(cap);
    p.max_height = p.max_height.min(cap);
    p
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
/// ceil(dim * num / 8). `None` — the default — decodes at full size.
///
/// Shrink-on-load only ever costs quality, so it is off unless asked
/// for. The old default (1.7) was chosen believing that ~2x of headroom
/// let Lanczos recover what the DCT truncation dropped; a sweep of
/// every reachable numerator over the quality corpus
/// (bench/quality/dct_sweep.py) says otherwise. Full decode is the best
/// cell or within 0.04 of it at every ratio measured, the penalty is
/// erratic rather than graded — 3/8 measured 13.4 SSIMULACRA2 points
/// below full decode at 5.3x while 5/8 on the same image was optimal —
/// and no single margin dodges the bad scales everywhere: 3.0 fixes
/// 5.3x and is worse than 1.7 at 4x. ImageMagick reproduces the same
/// dips through its own jpeg:size hint, so this is libjpeg's reduced
/// IDCT rather than anything here.
/// The margin the *buffered* decode paths keep when the knob is unset.
///
/// Where a whole frame is staged at the decode size — CMYK/YCCK JPEG,
/// and WebP — shrink-on-load is not paying for quality, it is paying
/// for memory, and the exchange rate is nothing like the streaming
/// arm's. Measured on a 4000x2667 source into 750px: CMYK peaks at
/// 23.7 MB with this margin against 111.5 MB decoding full size, and
/// WebP at 21.7 MB against 133.9 MB. The quality those buy back (5.1
/// SSIMULACRA2 points for WebP) is not worth 6x the resident set in a
/// service whose whole point is fitting in a small container. The
/// streaming arm, where the decode size never materializes, pays no
/// such price and so defaults to no shrink at all.
pub(crate) const BUFFERED_DCT_MARGIN: f64 = 1.7;

fn dct_scale_num(
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    margin: Option<f64>,
) -> u8 {
    let Some(margin) = margin else {
        return 8;
    };
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

fn dct_margin() -> Option<f64> {
    crate::config::config().dct_margin
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ImageFormat {
    Jpeg,
    Png,
    Webp,
    Avif,
    /// Decode-only. No GIF encoder ships here — a GIF source with no
    /// requested output becomes WebP (see [`default_target`]) — but the
    /// variant is a real format everywhere else: `sniff` returns it,
    /// `probe` reports it, and `content_type` names it.
    Gif,
}

impl ImageFormat {
    pub fn content_type(self) -> &'static str {
        match self {
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
            ImageFormat::Webp => "image/webp",
            ImageFormat::Avif => "image/avif",
            ImageFormat::Gif => "image/gif",
        }
    }

    /// Parse an output-format token (the URL's `@{fmt}` suffix and the
    /// OXIMG_AUTO_FORMAT list). Unlike source extensions — which are
    /// never trusted — these name the *requested* output format.
    /// Returns Avif even in non-avif builds; availability is the
    /// caller's check (HTTP rejects before spending a CPU slot).
    ///
    /// Deliberately no `"gif"`: there is no GIF encoder here, so every
    /// caller that turns a token into an output format — `@{fmt}`,
    /// `format=`, OXIMG_AUTO_FORMAT, the CLI's output extension — is
    /// better served by rejecting it than by silently emitting
    /// something else under a `.gif` name.
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
        } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
            Some(ImageFormat::Gif)
        } else if &header[4..8] == b"ftyp"
            && (&header[8..12] == b"avif" || &header[8..12] == b"avis")
        {
            Some(ImageFormat::Avif)
        } else {
            None
        }
    }
}

/// What a source becomes when the request names no output format.
/// Normally the source's own format — the pipeline is a resizer first
/// and a transcoder second — except for the decode-only ones, which
/// have to land somewhere they can actually be encoded. GIF goes to
/// WebP: it is the only target that can carry both an animation and an
/// alpha channel, and at matched visual scores it measured ~3x smaller
/// than the best GIF-to-GIF variant on a real-world corpus — where
/// gifsicle `-O3` saved nothing at all on 9 of 15 files
/// (docs/gif-evaluation.md §3).
fn default_target(format: ImageFormat) -> ImageFormat {
    match format {
        ImageFormat::Gif => ImageFormat::Webp,
        other => other,
    }
}

/// Cheap header probe: format + *stored* source dimensions without
/// decoding pixels. EXIF orientation is not consulted — with
/// auto-rotation on (the default), `process` fits and emits the
/// *displayed* frame, so for orientations 5-8 its output axes are
/// swapped relative to these dimensions.
///
/// # Examples
///
/// ```
/// use oximg::pipeline::{self, ImageFormat};
///
/// let bytes = std::fs::read(concat!(
///     env!("CARGO_MANIFEST_DIR"),
///     "/tests/fixtures/photo.jpg"
/// ))?;
/// let (format, w, h) = pipeline::probe(&bytes)?;
/// assert_eq!(format, ImageFormat::Jpeg);
/// assert_eq!((w, h), (200, 150));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn probe(bytes: &[u8]) -> Result<(ImageFormat, usize, usize), Error> {
    probe_inner(bytes).map_err(|e| Error::classify(e, false))
}

fn probe_inner(bytes: &[u8]) -> Result<(ImageFormat, usize, usize)> {
    let mut header = [0u8; 12];
    anyhow::ensure!(bytes.len() >= 12, "source too short");
    header.copy_from_slice(&bytes[..12]);
    let format = ImageFormat::sniff(&header).context("unsupported image format")?;
    match format {
        ImageFormat::Jpeg => {
            // mozjpeg reports fatal libjpeg errors by unwinding out of
            // its C error handler, so a malformed header is a panic,
            // not an Err — caught here so it classifies as undecodable
            // input (422) instead of taking the request, or under
            // panic=abort the process, down. Found by fuzzing: a
            // 4-component Adobe-marked JPEG whose SOF0 disagrees with
            // its component table panics inside jpeg_read_header.
            let (w, h) = crate::panic_guard::catch_unwind_as_error("JPEG header parse", || {
                Decompress::new_mem(bytes).map(|dec| dec.size())
            })?
            .context("parse JPEG")?;
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
        ImageFormat::Gif => {
            let (w, h) = probe_gif(bytes)?;
            Ok((format, w, h))
        }
        #[cfg(feature = "avif")]
        ImageFormat::Avif => {
            let (w, h) = crate::avif::probe_avif(bytes)?;
            Ok((format, w, h))
        }
        #[cfg(not(feature = "avif"))]
        ImageFormat::Avif => anyhow::bail!("AVIF support is not enabled in this build"),
    }
}

/// Resize + re-encode a source held in memory: sniff the format by
/// magic bytes, fit within the [`Params`] box (never enlarging), and
/// encode in `p.output` (defaulting to the source's own format).
///
/// # Examples
///
/// ```
/// use oximg::pipeline::{self, ImageFormat, Params};
///
/// let src = std::fs::read(concat!(
///     env!("CARGO_MANIFEST_DIR"),
///     "/tests/fixtures/photo.jpg"
/// ))?;
/// // Fit a 200x150 JPEG within 64x64 and transcode to WebP.
/// let p = Params {
///     max_width: 64,
///     max_height: 64,
///     output: Some(ImageFormat::Webp),
///     ..Params::default()
/// };
/// let (bytes, format) = pipeline::process(&src, &p)?;
/// assert_eq!(format, ImageFormat::Webp);
/// let (_, w, h) = pipeline::probe(&bytes)?;
/// assert_eq!((w, h), (64, 48), "aspect preserved inside the box");
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn process(bytes: &[u8], p: &Params) -> Result<(Vec<u8>, ImageFormat), Error> {
    process_reader(std::io::Cursor::new(bytes), p, bytes.len())
        .map_err(|e| Error::classify(e, false))
}

/// Sniff the source format, then resize + re-encode in the target
/// format (`p.output`, defaulting to the source's own). JPEG keeps its
/// fully streaming decode path; PNG streams through the png crate; WebP
/// requires the whole compressed source in memory (libwebp has no
/// incremental one-shot API). Decode-side optimizations (DCT
/// shrink-on-load, WebP decode-scaler) are per-source and stay active
/// for every target.
///
/// `held_source_bytes` is the compressed source the *caller* keeps
/// resident for the duration of the call — `bytes.len()` for the
/// in-memory entry point, zero for the streaming ones. It feeds the
/// decoded-bytes estimate: a buffered remote source is exactly as
/// resident as `srcbuf`, and omitting it would under-estimate in the
/// direction that gets a container OOM-killed (issue #22).
fn process_reader<R: std::io::Read>(
    mut reader: R,
    p: &Params,
    held_source_bytes: usize,
) -> Result<(Vec<u8>, ImageFormat)> {
    let mut header = [0u8; 12];
    std::io::Read::read_exact(&mut reader, &mut header).context("source too short")?;
    let format = ImageFormat::sniff(&header).context("unsupported image format")?;
    let target = p.output.unwrap_or_else(|| default_target(format));
    // Every knob resolves here, once (override > env > default); the
    // stages below read plain data and never consult the environment.
    // The output format's own dimension ceiling is one more constraint
    // on the same fit box, so fold it in before any decode work: the
    // resize then lands inside it in one pass, with the aspect ratio
    // preserved. Without this, a tall source encoded to WebP failed at
    // the encoder (issue #14) — a 500 for a request the format simply
    // cannot express at the asked-for size.
    let p = &clamp_to_format(Resolved::new(p), target);
    // Fail before decode work; the HTTP layer rejects earlier still,
    // this covers library callers.
    #[cfg(not(feature = "avif"))]
    anyhow::ensure!(
        target != ImageFormat::Avif,
        "AVIF support is not enabled in this build"
    );
    // Same treatment for the format that decodes but never encodes:
    // `ImageFormat::Gif` is public, so a library caller can name it as
    // an output even though no URL token can. Refusing here makes it a
    // clean 422 instead of a 500 from the encoder's backstop arm.
    anyhow::ensure!(
        target != ImageFormat::Gif,
        "GIF output is not supported (GIF is a decode-only format here)"
    );
    let reader = std::io::BufReader::new(std::io::Read::chain(&header[..], reader));

    let _active = ActiveGuard::enter();
    SCRATCH.with(|s| {
        let s = &mut *s.borrow_mut();
        s.held_source_bytes = held_source_bytes;
        let out = match format {
            // The whole JPEG decode is unwind-guarded, not just the
            // header parse: libjpeg signals fatal errors (bogus
            // Huffman tables, corrupt scan data) by unwinding out of
            // mozjpeg's C error handler at whichever stage hits them,
            // and every one of them is undecodable client input.
            ImageFormat::Jpeg => crate::panic_guard::catch_unwind_as_error("JPEG decode", || {
                jpeg::process_jpeg(s, reader, target, p)
            })??,
            ImageFormat::Png => process_png(s, reader, target, p)?,
            ImageFormat::Webp => process_webp(s, reader, target, p)?,
            ImageFormat::Gif => process_gif(s, reader, target, p)?,
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
pub fn process_path(path: &std::path::Path, p: &Params) -> Result<(Vec<u8>, ImageFormat), Error> {
    let inner = || -> Result<(Vec<u8>, ImageFormat)> {
        let file = std::fs::File::open(path).context("open source")?;
        process_reader(file, p, 0)
    };
    inner().map_err(|e| Error::classify(e, false))
}

#[cfg(feature = "server")]
fn max_source_bytes() -> u64 {
    crate::config::config().max_source_bytes
}

/// Lifetime count of upstream fetch retries. A plain counter the
/// server's /metrics page reads — the library keeps no other metrics
/// state, and a retry that goes unobserved hides exactly the flakiness
/// an operator needs to see (issue #11: the pre-retry version of that
/// flakiness was a production rollback).
#[cfg(feature = "server")]
static UPSTREAM_RETRIES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[cfg(feature = "server")]
pub fn upstream_retry_count() -> u64 {
    UPSTREAM_RETRIES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Marker attached (as anyhow context) to failures that are the
/// server's fault — encoding, worker infrastructure — as opposed to
/// undecodable client input. Consumed by [`Error::classify`] into
/// [`ErrorKind::Internal`]; not part of the public surface.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ServerFault;

impl std::fmt::Display for ServerFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("internal image-processing error")
    }
}

/// Marker for a source key the origin can never serve: past the
/// store's key-length limit, or refused as a malformed request
/// (400/414). Consumed by [`Error::classify`] into
/// [`ErrorKind::SourceRejected`]; not part of the public surface.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SourceRejected;

impl std::fmt::Display for SourceRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source key rejected")
    }
}

/// Marker for remote-origin failures (transport errors, non-404 error
/// statuses): the client's request was fine, the upstream wasn't.
/// Consumed by [`Error::classify`] into [`ErrorKind::Upstream`]; not
/// part of the public surface. (Produced on the remote-source path,
/// but classification must see the type on every build, so it is not
/// feature-gated.)
#[derive(Debug, Clone, Copy)]
pub(crate) struct UpstreamFault;

impl std::fmt::Display for UpstreamFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("upstream image fetch failed")
    }
}

/// The shared async HTTP client — one client, and therefore one
/// connection pool, for the async server paths and the sync embedder
/// wrappers alike. reqwest over rustls with h2 enabled: against an
/// h2-speaking origin (GCS, most CDNs) every concurrent fetch
/// multiplexes over a single connection, which retires the
/// connection-churn cost the permit-lab churn cell measured; against
/// HTTP/1.1 origins the pool keeps idle connections per host without
/// ureq 3.3's 3-per-host ceiling (whose idle-age check was also a no-op).
/// no-op: `age()` computed `now - now`).
#[cfg(feature = "server")]
fn fetch_client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        let cfg = crate::config::config();
        reqwest::Client::builder()
            // The whole-fetch deadline — connect through the last body
            // byte — bounds how long a stalled origin can hold a fetch
            // slot (and its buffer); the connect timeout separates
            // "origin unreachable" from "origin slow" without waiting
            // out the full budget.
            .timeout(std::time::Duration::from_secs(cfg.upstream_timeout))
            .connect_timeout(std::time::Duration::from_secs(cfg.upstream_connect_timeout))
            // No redirects: the operator points OXIMG_SOURCE_BASE_URL
            // at the right place, and an origin that can be induced to
            // redirect (e.g. object-store website endpoints honoring
            // user-settable redirect metadata) must not turn this
            // fetcher into an SSRF proxy.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("construct the HTTP client")
    })
}

/// Timeouts get their own io shape so [`Error::classify`] files them
/// as [`ErrorKind::UpstreamTimeout`]; everything else at send time is
/// the upstream's fault (statuses never appear here — with redirects
/// off, every response returns Ok and is judged by `refuse_status`).
#[cfg(feature = "server")]
fn map_send_err(e: reqwest::Error) -> anyhow::Error {
    if e.is_timeout() {
        anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, e))
    } else {
        anyhow::Error::new(e)
            .context("fetch source")
            .context(UpstreamFault)
    }
}

/// The status table, unchanged from the ureq implementation: 404
/// passes through, 400/414 is the origin refusing the request itself
/// (the requester's fault — issue #13), redirects are refused, and
/// any other non-success indicts the origin.
#[cfg(feature = "server")]
fn refuse_status(resp: reqwest::Response) -> Result<reqwest::Response> {
    let status = resp.status();
    if status.as_u16() == 404 {
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "source returned 404",
        )));
    }
    if matches!(status.as_u16(), 400 | 414) {
        return Err(
            anyhow::anyhow!("origin rejected the request ({status})").context(SourceRejected)
        );
    }
    if status.is_redirection() {
        return Err(
            anyhow::anyhow!("origin answered {status} (redirects are not followed)")
                .context(UpstreamFault),
        );
    }
    if !status.is_success() {
        return Err(anyhow::anyhow!("origin answered {status}")
            .context("fetch source")
            .context(UpstreamFault));
    }
    Ok(resp)
}

/// One retry on connection-level transients: a GET is idempotent and
/// no response bytes have been consumed yet, so retrying here is safe
/// — and it is the difference between "a network blip" and "a broken
/// image at the CDN" (issue #11, a production rollback). Timeouts are
/// excluded (the deadline budget is already spent, and doubling the
/// hold is worse than failing); origin answers are never errors here,
/// so a status can never be retried by accident.
#[cfg(feature = "server")]
async fn fetch_head_async(url: &str) -> Result<reqwest::Response> {
    let resp = match fetch_client().get(url).send().await {
        Ok(r) => r,
        Err(e) if !e.is_timeout() && (e.is_connect() || e.is_request()) => {
            UPSTREAM_RETRIES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            fetch_client().get(url).send().await.map_err(map_send_err)?
        }
        Err(e) => return Err(map_send_err(e)),
    };
    refuse_status(resp)
}

/// The buffered tail: refuse an over-cap Content-Length before
/// reading a byte, then accumulate the body under the same cap (the
/// count is what catches chunked or lying origins — exactly-cap-sized
/// sources are fine). Mid-body timeouts keep their io shape for
/// classification; other body failures indict the origin.
#[cfg(feature = "server")]
async fn buffer_body_async(mut resp: reqwest::Response) -> Result<Vec<u8>> {
    let cap = max_source_bytes();
    if let Some(len) = resp.content_length()
        && len > cap
    {
        return Err(anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("source is {len} bytes, over the {cap}-byte limit"),
        )));
    }
    let map_body_err = |e: reqwest::Error| {
        if e.is_timeout() {
            anyhow::Error::new(std::io::Error::new(std::io::ErrorKind::TimedOut, e))
        } else {
            anyhow::Error::new(e)
                .context("read source body")
                .context(UpstreamFault)
        }
    };
    // The declared size is already checked against the cap, so a
    // well-behaved origin costs one allocation; chunked origins grow.
    let mut buf =
        Vec::with_capacity(usize::try_from(resp.content_length().unwrap_or(0)).unwrap_or(0));
    while let Some(chunk) = resp.chunk().await.map_err(map_body_err)? {
        if (buf.len() as u64).saturating_add(chunk.len() as u64) > cap {
            return Err(anyhow::Error::new(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "source exceeds OXIMG_MAX_SOURCE_BYTES",
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    Ok(buf)
}

/// Async buffered fetch: [`fetch_url`] for callers already inside a
/// runtime — the server awaits this directly, so a fetch occupies no
/// thread at all while it waits. Requires the `server` feature.
#[cfg(feature = "server")]
pub async fn fetch_url_async(url: &str) -> Result<Vec<u8>, Error> {
    let inner = async { buffer_body_async(fetch_head_async(url).await?).await };
    inner.await.map_err(|e| Error::classify(e, true))
}

/// Run a fetch future to completion from sync code, from any thread —
/// including a tokio worker, where `Runtime::block_on` would panic:
/// the future runs on one dedicated background runtime thread and the
/// caller blocks on a channel. The dedicated runtime drives I/O only
/// (fetches are network-bound); CPU work never lands here.
#[cfg(feature = "server")]
fn block_on_fetch<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    static HANDLE: OnceLock<tokio::runtime::Handle> = OnceLock::new();
    let handle = HANDLE.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("oximg-fetch".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("build the fetch runtime");
                let _ = tx.send(rt.handle().clone());
                rt.block_on(std::future::pending::<()>());
            })
            .expect("spawn the fetch runtime thread");
        rx.recv().expect("fetch runtime failed to start")
    });
    let (tx, rx) = std::sync::mpsc::channel();
    handle.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv().expect("fetch task dropped without a result")
}

/// Remote-source variant: fetch `url` whole (bounded by
/// `OXIMG_MAX_SOURCE_BYTES`), then decode from the buffer — the same
/// two steps as [`fetch_url`] + [`process`], packaged for callers that
/// want one call. Requires the `server` feature (the HTTP client
/// stack).
#[cfg(feature = "server")]
pub fn process_url(url: &str, p: &Params) -> Result<(Vec<u8>, ImageFormat), Error> {
    let bytes = fetch_url(url)?;
    process(&bytes, p)
}

/// GCS-source variant (`gs://` mode): fetch `key` from `bucket` with
/// GCP-attached credentials into a bounded buffer, then decode — same
/// contract as [`process_url`]. `key` must already be percent-encoded
/// segment-wise. Requires the `server` feature.
#[cfg(feature = "server")]
pub fn process_gcs(bucket: &str, key: &str, p: &Params) -> Result<(Vec<u8>, ImageFormat), Error> {
    let bytes = fetch_gcs(bucket, key)?;
    process(&bytes, p)
}

/// Async buffered GCS fetch: [`fetch_gcs`] for callers already inside
/// a runtime — the server awaits this directly. Requires the `server`
/// feature.
#[cfg(feature = "server")]
pub async fn fetch_gcs_async(bucket: &str, key: &str) -> Result<Vec<u8>, Error> {
    let inner = async { buffer_body_async(gcs::fetch(bucket, key).await?).await };
    inner.await.map_err(|e| Error::classify(e, true))
}

/// Startup credential probe for the `gs://` mode — the server calls
/// this at boot so a missing metadata server refuses to start instead
/// of failing on the first cache miss.
#[cfg(feature = "server")]
pub fn gcs_startup() -> Result<(), String> {
    gcs::startup()
}

/// Buffered remote fetch: download `url` whole, bounded by
/// `OXIMG_MAX_SOURCE_BYTES`, and return the bytes without decoding
/// anything. The split from [`process`] exists so a caller can put
/// the network wait and the CPU work under different concurrency
/// bounds (issue #22: the server buffers first, then takes a CPU
/// permit). Sync bridge over [`fetch_url_async`]; the whole fetch is
/// recorded for [`last_fetch_seconds`]. Requires the `server` feature.
#[cfg(feature = "server")]
pub fn fetch_url(url: &str) -> Result<Vec<u8>, Error> {
    clear_fetch_time();
    let t0 = std::time::Instant::now();
    let owned = url.to_string();
    let result = block_on_fetch(async move { fetch_url_async(&owned).await });
    record_fetch_time(t0.elapsed().as_secs_f64());
    result
}

/// Buffered GCS fetch: [`fetch_url`]'s contract for the `gs://` mode —
/// authenticated via GCP-attached credentials, same caps and
/// classification as [`process_gcs`]. `key` must already be
/// percent-encoded segment-wise. Sync bridge over
/// [`fetch_gcs_async`]. Requires the `server` feature.
#[cfg(feature = "server")]
pub fn fetch_gcs(bucket: &str, key: &str) -> Result<Vec<u8>, Error> {
    clear_fetch_time();
    let t0 = std::time::Instant::now();
    let (bucket, key) = (bucket.to_string(), key.to_string());
    let result = block_on_fetch(async move { fetch_gcs_async(&bucket, &key).await });
    record_fetch_time(t0.elapsed().as_secs_f64());
    result
}

thread_local! {
    // Per-blocking-pool-thread reusable work buffers: at 600 RPS this
    // removes ~4GB/s of malloc/free traffic (decode buffer, u16
    // intermediate image, Resizer internal temporaries).
    static SCRATCH: std::cell::RefCell<Scratch> = std::cell::RefCell::new(Scratch::default());
}

#[derive(Default)]
struct Scratch {
    /// Set by the JPEG paths from the header scan before decode_resize
    /// runs: whether libjpeg will buffer whole-image coefficients for
    /// this source. Carried here rather than as a ninth parameter, and
    /// reset per request by its setters (both JPEG entry points assign
    /// it unconditionally).
    jpeg_progressive: bool,
    /// Compressed source bytes the *caller* holds resident for this
    /// request (a buffered remote source, or any `process(&bytes, ..)`
    /// input); zero on the streaming entry points. Carried here like
    /// `jpeg_progressive` — set unconditionally by `process_reader` —
    /// so the per-format cost sites can count it without a new
    /// parameter on every decode path.
    held_source_bytes: usize,
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

/// Re-exposed for `bench/tools/resolve_bench.rs` only: one request's
/// full [`Resolved`] snapshot. Guards against resolution creep as
/// knobs are added — the baseline it was measured against (the old
/// per-stage resolver chain, 2.7-2.8ns) is recorded in this commit's
/// history.
#[cfg(feature = "bench-internals")]
#[doc(hidden)]
pub fn bench_resolve(p: &Params) -> impl Sized {
    Resolved::new(p)
}

/// Targets that can carry an ICC profile — all of them when the avif
/// feature is on (AVIF embeds via the container splice in
/// `avif::embed_icc`).
fn target_supports_icc(target: ImageFormat) -> bool {
    match target {
        ImageFormat::Jpeg | ImageFormat::Png | ImageFormat::Webp => true,
        // Never a target (no encoder), and GIF's palette has nowhere to
        // put a profile anyway.
        ImageFormat::Gif => false,
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
/// What a decode is about to allocate, in bytes — the unit an operator
/// actually has a limit in.
///
/// Source pixels cannot be mapped to memory in this pipeline, because
/// the cost per pixel varies by more than an order of magnitude with
/// the source encoding (issue #17 measured 16x at *equal* pixel
/// counts, and the cheapest source in that corpus was the one a pixel
/// cap rejected first):
///
/// - Baseline JPEG decodes through DCT shrink-on-load, so its cost
///   tracks the **output** size — the cheapest path by far.
/// - Progressive JPEG forces libjpeg to buffer whole-image
///   coefficients at full source resolution, so its cost tracks the
///   **source** and barely moves with the requested output.
/// - CMYK/YCCK stages four channels instead of three.
/// - PNG and AVIF have no shrink-on-load: full frame, always.
///
/// The model follows the buffers the code actually holds at once:
/// the decoder's output frame, the linear-light resize input (the same
/// frame as u16), the output-side `dst16`+`out8`, progressive JPEG's
/// coefficient arrays, and the compressed source where a format needs
/// it whole. Field validation (issue #17 follow-up) put it 1.2-1.8x
/// above measured peaks across four real sources — deliberately on the
/// conservative side, because under-estimating is the direction that
/// gets a container OOM-killed while the cap reports itself satisfied.
/// Encode-side buffers are still excluded, which is why it rounds up
/// elsewhere.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct DecodeCost {
    /// The decoder's own output buffer: pixels it materializes (after
    /// any shrink-on-load) times bytes per staged pixel. Zero for the
    /// streaming JPEG path, which only ever holds row bands.
    pub staged_bytes: u64,
    /// The linear-light resize input for full-frame paths: the same
    /// frame again as u16 (`src16`). Zero when the path streams or
    /// when linear light is off.
    pub resize_input_bytes: u64,
    /// Output-side buffers every path allocates: `dst16` (u16) plus
    /// `out8`, i.e. 3x the output frame per channel.
    pub output_bytes: u64,
    /// Whole-source buffering no output size reduces — progressive
    /// JPEG's coefficient arrays.
    pub whole_source_bytes: u64,
    /// The compressed source held whole for the request: `srcbuf` for
    /// the buffered formats, plus any caller-held input buffer (a
    /// buffered remote source, or `process(&bytes, ..)`). The JPEG
    /// decoder itself streams, so on that path only the caller's
    /// buffer — if any — contributes.
    pub compressed_bytes: u64,
}

impl DecodeCost {
    pub fn bytes(&self) -> u64 {
        self.staged_bytes
            .saturating_add(self.resize_input_bytes)
            .saturating_add(self.output_bytes)
            .saturating_add(self.whole_source_bytes)
            .saturating_add(self.compressed_bytes)
    }

    /// A path that materializes the whole `w x h` frame at `channels`
    /// bytes per pixel and feeds the full-frame resize: `chunk8` plus,
    /// under linear light (the default), the same frame as u16.
    pub fn full_frame(w: usize, h: usize, channels: u64, p: &Resolved) -> Self {
        let px = (w as u64).saturating_mul(h as u64);
        let staged = px.saturating_mul(channels);
        DecodeCost {
            staged_bytes: staged,
            resize_input_bytes: if p.linear_light { staged * 2 } else { 0 },
            ..Default::default()
        }
    }

    /// A streaming path: no full frame is ever resident, only the
    /// output-side buffers (added by `with_output`).
    pub fn streaming() -> Self {
        DecodeCost::default()
    }

    /// `dst16` + `out8` for the resize target. Every path pays this,
    /// and on the streaming paths it is the dominant term — measured
    /// 5.1 B per output pixel against the 9 modeled here (issue #17
    /// follow-up), i.e. conservative.
    pub fn with_output(mut self, out_w: usize, out_h: usize, channels: u64) -> Self {
        let px = (out_w as u64).saturating_mul(out_h as u64);
        self.output_bytes = px.saturating_mul(channels).saturating_mul(3);
        self
    }

    /// Progressive JPEG's whole-image coefficient arrays: full source
    /// resolution, one `JCOEF` (2 bytes) per sample per component.
    /// `MAX_SRC_PIXELS` explicitly could not see these, and field
    /// measurement confirmed they dominate such sources.
    pub fn with_progressive_coefficients(mut self, src_w: usize, src_h: usize, comps: u64) -> Self {
        self.whole_source_bytes = (src_w as u64)
            .saturating_mul(src_h as u64)
            .saturating_mul(comps)
            .saturating_mul(2);
        self
    }

    /// The compressed source held whole for this request — `srcbuf`
    /// and/or the caller's own input buffer (`Scratch::
    /// held_source_bytes`); callers sum the copies that are actually
    /// resident.
    pub fn with_compressed(mut self, bytes: usize) -> Self {
        self.compressed_bytes = bytes as u64;
        self
    }
}

/// Histogram-able record of every decode's estimated cost, plus the
/// enforcement of `OXIMG_MAX_DECODED_BYTES`. Observability comes first
/// on purpose: an operator who cannot see the figure can only guess a
/// cap and learn from user reports (issue #17 — set three times, wrong
/// three times).
impl DecodeCost {
    /// The per-term breakdown, in the one place it is formatted. The
    /// terms are the point, not the total: they tell an operator
    /// whether a request is bounded by the source side (buy memory) or
    /// the output side (cap the requested width).
    fn report(&self, what: &str) -> String {
        format!(
            "{what} decode needs about {} bytes (staged {}, resize input {}, \
             output {}, whole-source {}, compressed {})",
            self.bytes(),
            self.staged_bytes,
            self.resize_input_bytes,
            self.output_bytes,
            self.whole_source_bytes,
            self.compressed_bytes,
        )
    }
}

thread_local! {
    /// The last decode's cost on this thread, so the caller — which is
    /// the only place that knows *which source* this was — can report
    /// it. Set on every request; formatted only if someone asks.
    static LAST_COST: std::cell::Cell<Option<(DecodeCost, &'static str)>> =
        const { std::cell::Cell::new(None) };
}

/// The last decode's per-term report, if its estimate exceeded
/// `OXIMG_LOG_DECODED_BYTES_ABOVE`. `None` when the knob is unset, when
/// the estimate is under it, or when no decode has run on this thread.
///
/// This exists because the cap can only ever name what it *rejects*
/// (issue #19): the histogram counts an expensive request without
/// identifying it, and a cap high enough to be safe names nothing at
/// all. Reporting without refusing is how a deployment learns its own
/// corpus before choosing a limit.
pub fn decode_report_above_threshold() -> Option<String> {
    let threshold = crate::config::config().log_decoded_bytes_above?;
    let (cost, what) = LAST_COST.get()?;
    (cost.bytes() > threshold).then(|| cost.report(what))
}

pub(crate) fn check_decoded_bytes(cost: DecodeCost, what: &'static str) -> Result<()> {
    let bytes = cost.bytes();
    record_decoded_bytes(bytes);
    LAST_COST.set(Some((cost, what)));
    let Some(cap) = crate::config::config().max_decoded_bytes else {
        return Ok(());
    };
    if bytes > cap {
        // FileTooLarge like the other caps, so this classifies as
        // ErrorKind::SourceTooLarge (HTTP 413) on the same terms.
        anyhow::bail!(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!(
                "{}, over the OXIMG_MAX_DECODED_BYTES limit ({cap})",
                cost.report(what)
            ),
        ));
    }
    Ok(())
}

#[cfg(feature = "server")]
thread_local! {
    /// Seconds spent getting a remote source's response *head* on this
    /// thread — connect, request, TTFB, plus any retry wait or token
    /// refresh. Set by the remote paths, cleared by them on entry so a
    /// local-file request can never read a stale value.
    ///
    /// The body read is deliberately excluded: streaming decode
    /// interleaves it with CPU work, so it cannot be attributed. What
    /// this measures is the part of a CPU permit's hold time during
    /// which not one byte could be decoded — the quantity that decides
    /// whether bounding requests instead of CPU work is costing
    /// anything (bench/permit-lab).
    static FETCH_SECS: std::cell::Cell<Option<f64>> = const { std::cell::Cell::new(None) };
}

#[cfg(feature = "server")]
pub(crate) fn clear_fetch_time() {
    FETCH_SECS.set(None);
}

#[cfg(feature = "server")]
pub(crate) fn record_fetch_time(seconds: f64) {
    FETCH_SECS.set(Some(FETCH_SECS.get().unwrap_or(0.0) + seconds));
}

/// Seconds the last remote fetch spent before its first decodable byte,
/// or `None` if this thread's last source was local. Requires the
/// `server` feature, like the remote source paths themselves.
#[cfg(feature = "server")]
pub fn last_fetch_seconds() -> Option<f64> {
    FETCH_SECS.get()
}

/// Whether an `OXIMG_MAX_DECODED_BYTES` cap is configured. The JPEG
/// header scan is normally skipped when neither rotation nor ICC needs
/// it; it also carries the progressive flag, and omitting that flag
/// under-estimates by the whole coefficient term — the dangerous
/// direction for a limit — so the scan runs whenever the cap is on.
pub(crate) fn decoded_bytes_cap_set() -> bool {
    crate::config::config().max_decoded_bytes.is_some()
}

/// Bucketed lifetime counts of the decoded-bytes estimate, exposed as
/// a histogram on the server's /metrics page so a cap can be derived
/// from a real corpus instead of guessed. Bounds are powers of two
/// from 1 MiB to 4 GiB.
pub const DECODED_BYTES_BOUNDS: [u64; 13] = [
    1 << 20,
    1 << 21,
    1 << 22,
    1 << 23,
    1 << 24,
    1 << 25,
    1 << 26,
    1 << 27,
    1 << 28,
    1 << 29,
    1 << 30,
    1 << 31,
    1 << 32,
];

static DECODED_BYTES_BUCKETS: [std::sync::atomic::AtomicU64; DECODED_BYTES_BOUNDS.len() + 1] =
    [const { std::sync::atomic::AtomicU64::new(0) }; DECODED_BYTES_BOUNDS.len() + 1];
static DECODED_BYTES_SUM: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn record_decoded_bytes(bytes: u64) {
    let slot = DECODED_BYTES_BOUNDS
        .iter()
        .position(|b| bytes <= *b)
        .unwrap_or(DECODED_BYTES_BOUNDS.len());
    DECODED_BYTES_BUCKETS[slot].fetch_add(1, Ordering::Relaxed);
    DECODED_BYTES_SUM.fetch_add(bytes, Ordering::Relaxed);
}

/// `(per-bound counts including the +Inf overflow slot, summed bytes)`.
pub fn decoded_bytes_histogram() -> ([u64; DECODED_BYTES_BOUNDS.len() + 1], u64) {
    let mut counts = [0u64; DECODED_BYTES_BOUNDS.len() + 1];
    for (dst, src) in counts.iter_mut().zip(DECODED_BYTES_BUCKETS.iter()) {
        *dst = src.load(Ordering::Relaxed);
    }
    (counts, DECODED_BYTES_SUM.load(Ordering::Relaxed))
}

pub(crate) fn check_src_pixels(w: usize, h: usize) -> Result<()> {
    let cap = crate::config::config().max_src_pixels;
    let px = (w as u64).saturating_mul(h as u64);
    if px > cap {
        // FileTooLarge, like the byte cap, so both limits classify as
        // ErrorKind::SourceTooLarge (HTTP 413).
        anyhow::bail!(std::io::Error::new(
            std::io::ErrorKind::FileTooLarge,
            format!("source is {w}x{h} ({px} pixels), over the OXIMG_MAX_SRC_PIXELS limit ({cap})"),
        ));
    }
    Ok(())
}

mod cmyk;
mod encode;
mod error;
mod formats;
mod fuse;
#[cfg(feature = "server")]
mod gcs;
mod gif;
mod jpeg;
mod resolved;
#[cfg(test)]
mod tests;

use cmyk::*;
pub use encode::encode;
use encode::*;
pub use error::{Error, ErrorKind};
use formats::*;
use fuse::*;
// This module, not the `gif` crate — a `mod gif` shadows the extern
// prelude here. gif.rs reaches the crate as `::gif`.
use gif::*;
pub use jpeg::decode_and_resize;
#[cfg_attr(not(test), allow(unused_imports))] // tests.rs reaches these via super::*
use jpeg::*;
use resolved::Resolved;
