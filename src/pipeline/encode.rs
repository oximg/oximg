//! Output-side encoders and their knobs: JPEG (jpegli + mozjpeg
//! presets, ICC chunking), PNG, WebP (bare + ICCP re-container),
//! AVIF parameters, alpha flattening, and the target dispatch.

use super::*;

pub(super) fn encode_png(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    icc: Option<&[u8]>,
    p: &Resolved,
) -> Result<Vec<u8>> {
    // Opaque output only: quantette has no alpha-aware quantizer, and
    // approximating one (per-palette-cell alpha) would produce fringes
    // exactly where alpha matters. Alpha sources stay lossless RGBA.
    let quantized = if channels == 3 {
        p.png_quantize
            .map(|colors| quantize_rgb(pixels, w, h, colors))
    } else {
        None
    };
    let (color, palette, data, row) = match &quantized {
        Some((palette, indices)) => (png::ColorType::Indexed, Some(&palette[..]), &indices[..], w),
        None if channels == 4 => (png::ColorType::Rgba, None, pixels, w * 4),
        None => (png::ColorType::Rgb, None, pixels, w * 3),
    };
    let compression = p.png_compression(quantized.is_some());
    let stored = zlib_stored_len((row + 1) * h);
    let mut out = Vec::with_capacity(64 * 1024);
    // `write_image_data` wrote stored blocks when its fdeflate output
    // came out larger than them (noise: +37% at 2000² otherwise), and a
    // stream cannot take back what it wrote. `out` can: the rare image
    // that does not compress is encoded a second time, stored. The
    // first attempt stops, at the next band of rows, once its output
    // passes everything a stored PNG could hold, so `out` never grows
    // past one and the doomed deflate does not run to the end.
    let fdeflate = matches!(
        compression,
        png::Compression::Fastest | png::Compression::Fast
    );
    let cap = if fdeflate {
        let framing = 12 * stored.div_ceil(PNG_IDAT_CHUNK);
        let other = 1024 + icc.map_or(0, <[u8]>::len) + palette.map_or(0, <[u8]>::len);
        stored + framing + other
    } else {
        usize::MAX
    };
    let fits = write_png(&mut out, cap, w, h, icc, color, palette, data, compression)?;
    if !fits || (fdeflate && idat_len(&out) > stored) {
        out.clear();
        let none = png::Compression::NoCompression;
        write_png(&mut out, usize::MAX, w, h, icc, color, palette, data, none)?;
    }
    Ok(out)
}

/// IDAT payload per chunk: 12 bytes of chunk framing per 256 KiB, and
/// the stream writer's one buffer of this size is all it holds besides
/// three rows.
const PNG_IDAT_CHUNK: usize = 256 * 1024;

/// `out`, refusing to grow past `cap`. Past it, writes are dropped and
/// `over` is set, never failed: fdeflate unwraps the writes in its
/// `finish` (the last bit-buffer bytes and the Adler-32), so an error
/// there would panic instead of reaching the fallback. The caller stops
/// feeding rows once `over` is set.
struct Capped<'a> {
    out: &'a mut Vec<u8>,
    cap: usize,
    over: &'a std::cell::Cell<bool>,
}

impl std::io::Write for Capped<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        if self.over.get() || self.out.len() + b.len() > self.cap {
            self.over.set(true);
        } else {
            self.out.extend_from_slice(b);
        }
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Encode into `out`; `Ok(false)` when the output would pass `cap`.
#[allow(clippy::too_many_arguments)]
pub(super) fn write_png(
    out: &mut Vec<u8>,
    cap: usize,
    w: usize,
    h: usize,
    icc: Option<&[u8]>,
    color: png::ColorType,
    palette: Option<&[u8]>,
    data: &[u8],
    compression: png::Compression,
) -> Result<bool> {
    let over = std::cell::Cell::new(false);
    let r = write_png_capped(
        Capped {
            out,
            cap,
            over: &over,
        },
        &over,
        w,
        h,
        icc,
        color,
        palette,
        data,
        compression,
    );
    // A capped attempt left `out` truncated; whatever png made of that
    // (its last chunk's flush error is dropped silently), it is discarded.
    if over.get() {
        return Ok(false);
    }
    r.map(|()| true)
}

#[allow(clippy::too_many_arguments)]
fn write_png_capped(
    out: Capped<'_>,
    over: &std::cell::Cell<bool>,
    w: usize,
    h: usize,
    icc: Option<&[u8]>,
    color: png::ColorType,
    palette: Option<&[u8]>,
    data: &[u8],
    compression: png::Compression,
) -> Result<()> {
    // The no-profile arm constructs the encoder exactly as the pre-ICC
    // code did.
    let mut enc = match icc {
        Some(icc) => {
            let mut info = png::Info::with_size(w as u32, h as u32);
            info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
            png::Encoder::with_info(out, info).context("PNG info")?
        }
        None => png::Encoder::new(out, w as u32, h as u32),
    };
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(compression);
    enc.set_color(color);
    if let Some(palette) = palette {
        enc.set_palette(palette);
    }
    let mut writer = enc.write_header().context("PNG header")?;
    // `write_image_data` deflates the whole image into its own Vec before
    // copying it into `out` as one IDAT, so both grew with the compressed
    // size (3.6 B per raw byte held on noise). The stream writer deflates
    // the same filtered rows straight into IDAT chunks of `out` (issue
    // #133). Chunk boundaries, and so the bytes, differ from the one-IDAT
    // layout; the pixels do not.
    let mut stream = writer
        .stream_writer_with_size(PNG_IDAT_CHUNK)
        .context("PNG stream")?;
    // Bands of rows, so a capped attempt stops soon after it overflows.
    // Leaving rows unwritten makes the stream's own finish (and drop)
    // refuse before fdeflate's.
    for band in data.chunks((data.len() / h.max(1)).max(1) * 64) {
        std::io::Write::write_all(&mut stream, band).context("PNG encode")?;
        if over.get() {
            anyhow::bail!("PNG output past its cap");
        }
    }
    stream.finish().context("PNG encode")?;
    writer.finish().context("PNG finish")?;
    Ok(())
}

/// Total IDAT payload of a PNG the encoder above just wrote.
fn idat_len(png: &[u8]) -> usize {
    let mut i = 8;
    let mut total = 0;
    while i + 8 <= png.len() {
        let len = u32::from_be_bytes(png[i..i + 4].try_into().unwrap()) as usize;
        if &png[i + 4..i + 8] == b"IDAT" {
            total += len;
        }
        i += 12 + len;
    }
    total
}

/// A zlib stream of `raw` bytes in stored blocks: header, 5 bytes per
/// 65535-byte block, Adler-32.
fn zlib_stored_len(raw: usize) -> usize {
    2 + raw + 5 * raw.div_ceil(65535).max(1) + 4
}

/// Wu color quantization (quantette) with Floyd-Steinberg dithering,
/// single-threaded — request concurrency is the CPU semaphore's job.
/// Returns (PLTE bytes, per-pixel indices).
fn quantize_rgb(pixels: &[u8], w: usize, h: usize, colors: u16) -> (Vec<u8>, Vec<u8>) {
    use quantette::deps::palette::Srgb;
    let px: Vec<Srgb<u8>> = pixels
        .as_chunks::<3>()
        .0
        .iter()
        .map(|&[r, g, b]| Srgb::new(r, g, b))
        .collect();
    let image =
        quantette::ImageBuf::new(w as u32, h as u32, px).expect("pixel count matches dimensions");
    let indexed = quantette::Pipeline::new()
        .palette_size(quantette::PaletteSize::try_from(colors).expect("colors clamped to 2-256"))
        .ditherer(quantette::dither::FloydSteinberg::new())
        .input_image(image.as_ref())
        .output_srgb8_indexed_image();
    let (palette, indices) = indexed.into_parts();
    let plte: Vec<u8> = palette
        .into_iter()
        .flat_map(|c| [c.red, c.green, c.blue])
        .collect();
    (plte, indices)
}

pub(super) fn encode_webp(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    icc: Option<&[u8]>,
    p: &Resolved,
) -> Result<Vec<u8>> {
    let out = encode_webp_bare(pixels, w, h, channels, p)?;
    match icc {
        Some(icc) => wrap_webp_icc(&out, icc),
        None => Ok(out),
    }
}

pub(super) fn webp_effort() -> i32 {
    crate::config::config().webp_effort
}

// SAFETY: zeroed config/pic/writer are libwebp's documented pre-init states,
// with both Init ABI checks enforced. The import reads h rows of w*channels
// bytes; callers pass exactly w*h*channels (sliced in encode_output). `writer`
// outlives WebPEncode via custom_ptr, mem[..size] is live until the final
// WebPMemoryWriterClear, and pic/writer are each freed exactly once per path.
pub(super) fn encode_webp_bare(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    p: &Resolved,
) -> Result<Vec<u8>> {
    use libwebp_sys as wp;
    unsafe {
        let mut config: wp::WebPConfig = std::mem::zeroed();
        anyhow::ensure!(wp::WebPInitConfig(&mut config), "libwebp ABI mismatch");
        config.quality = p.webp_quality;
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
            // Split the request's fault from ours: BAD_DIMENSION means
            // the asked-for output exceeds what the format can express
            // (16383 px a side) — the fit box is capped upstream so
            // this should be unreachable, but if it fires it is still
            // a statement about the request, not about the service.
            // Everything else (allocation failures, partition
            // overflows, encoder bugs) is ours to own as a 500.
            if pic.error_code == wp::WebPEncodingError::VP8_ENC_ERROR_BAD_DIMENSION {
                return Err(anyhow::anyhow!(
                    "output dimensions exceed the WebP limit of 16383 px a side"
                )
                .context(super::SourceRejected));
            }
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
// SAFETY: the WebPData views borrow `webp` and `icc`, both outliving every mux
// call (`icc` is copied in via copy_data=1; the mux borrowing `webp` is deleted
// before return on every path). On WEBP_MUX_OK, assembled.bytes[..size] is a
// mux-allocated buffer copied out before the single WebPDataClear frees it.
pub(super) fn wrap_webp_icc(webp: &[u8], icc: &[u8]) -> Result<Vec<u8>> {
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

/// `WEBP_FF_FORMAT_FLAGS` bit that says the container carries an `ANIM`
/// chunk, i.e. a frame sequence rather than a single still.
const ANIMATION_FLAG: u32 = 0x02;

/// One animation's worth of libwebp `WebPAnimEncoder`: the handle, the
/// per-frame encoder config, and the canvas size every frame must match,
/// freed on drop. The GIF path streams frames through this, so nothing
/// larger than one frame is ever staged on our side (the encoder itself
/// retains the frames it has already compressed — priced into the
/// `DecodeCost` at the call site).
///
/// Timestamps must be non-decreasing, because libwebp derives a frame's
/// duration from the *next* frame's timestamp. That is also why
/// [`AnimEncoder::finish`] takes the animation's total duration: the
/// documented final `Add(NULL, ts)` exists to give the last real frame
/// its own.
pub(super) struct AnimEncoder {
    enc: *mut libwebp_sys::WebPAnimEncoder,
    config: libwebp_sys::WebPConfig,
    w: usize,
    h: usize,
}

impl AnimEncoder {
    /// `loop_count` is WebP's spelling: 0 = forever.
    // SAFETY: both Internal entry points are called with WEBP_MUX_ABI_VERSION and
    // their return values checked, so the zeroed options/config structs are only
    // read after libwebp itself initialized them. On the config-init failure path
    // the already-created encoder is deleted before returning, and on every other
    // path it is owned by the returned value and freed exactly once in `drop`.
    pub(super) fn new(w: usize, h: usize, loop_count: u32, p: &Resolved) -> Result<Self> {
        use libwebp_sys as wp;
        unsafe {
            let mut opts: wp::WebPAnimEncoderOptions = std::mem::zeroed();
            anyhow::ensure!(
                wp::WebPAnimEncoderOptionsInitInternal(&mut opts, wp::WEBP_MUX_ABI_VERSION as _)
                    != 0,
                "libwebp mux ABI mismatch"
            );
            opts.anim_params.loop_count = loop_count as std::os::raw::c_int;
            // libwebp defaults this to 0xffffffff — opaque white — which
            // would contradict the transparent canvas the GIF compositor
            // builds: anything no frame covers, and anything a Background
            // disposal clears, is transparent for us, and a player that
            // honors the ANIM background would paint white there instead.
            // Fully transparent, to say the same thing twice.
            opts.anim_params.bgcolor = 0;
            // Both of these buy bytes with encoder passes, and CPU is
            // this path's binding constraint (docs/gif-evaluation.md
            // §5): minimize_size re-encodes to search key-frame
            // placement, allow_mixed encodes every frame twice to pick
            // lossy or lossless per frame. Off, explicitly.
            opts.minimize_size = 0;
            opts.allow_mixed = 0;
            let enc = wp::WebPAnimEncoderNewInternal(
                w as i32,
                h as i32,
                &opts,
                wp::WEBP_MUX_ABI_VERSION as _,
            );
            anyhow::ensure!(!enc.is_null(), "webp animation encoder");
            let mut config: wp::WebPConfig = std::mem::zeroed();
            if !wp::WebPInitConfig(&mut config) {
                wp::WebPAnimEncoderDelete(enc);
                anyhow::bail!("libwebp ABI mismatch");
            }
            // The same quality and effort knobs as a still WebP: an
            // animation is not a different product, just more frames.
            config.quality = p.webp_quality;
            config.method = webp_effort().clamp(0, 6);
            Ok(AnimEncoder { enc, config, w, h })
        }
    }

    /// Add one full-canvas RGBA frame, starting at `ts_ms`.
    // SAFETY: a zeroed WebPPicture is libwebp's documented pre-init state and the
    // Init ABI check is enforced. The import reads exactly h rows of w*4 bytes,
    // which the length assertion pins to the caller's slice; `pic` is freed on
    // both the import-failure and the normal path, exactly once. `self.enc` is
    // non-null for the lifetime of `self`, and the config is only read by Add.
    pub(super) fn add(&mut self, rgba: &[u8], ts_ms: i32) -> Result<()> {
        use libwebp_sys as wp;
        assert_eq!(rgba.len(), self.w * self.h * 4, "frame is not the canvas");
        unsafe {
            let mut pic: wp::WebPPicture = std::mem::zeroed();
            anyhow::ensure!(wp::WebPPictureInit(&mut pic), "libwebp ABI mismatch");
            pic.width = self.w as i32;
            pic.height = self.h as i32;
            // ARGB rather than the default YUV import: the encoder diffs
            // each frame against its canvas in ARGB, and converts a YUV
            // frame back with a loss its own header calls out.
            pic.use_argb = 1;
            let imported = wp::WebPPictureImportRGBA(&mut pic, rgba.as_ptr(), (self.w * 4) as i32);
            if imported == 0 {
                wp::WebPPictureFree(&mut pic);
                anyhow::bail!("webp picture import");
            }
            let ok = wp::WebPAnimEncoderAdd(self.enc, &mut pic, ts_ms, &self.config);
            wp::WebPPictureFree(&mut pic);
            anyhow::ensure!(ok != 0, "webp animation frame: {}", self.error());
            Ok(())
        }
    }

    /// Close the animation at `total_ms` — the last frame's end — and
    /// assemble the container.
    // SAFETY: the NULL frame is the documented terminator, not a missing argument.
    // A zeroed WebPData is a valid out-param; on success data.bytes[..size] is
    // libwebp-allocated and copied out before the single WebPDataClear frees it,
    // and Clear on the zeroed/failed struct is a no-op. `self` is dropped (and the
    // encoder deleted) on every path out of here.
    pub(super) fn finish(self, total_ms: i32) -> Result<Vec<u8>> {
        use libwebp_sys as wp;
        unsafe {
            anyhow::ensure!(
                wp::WebPAnimEncoderAdd(self.enc, std::ptr::null_mut(), total_ms, std::ptr::null())
                    != 0,
                "webp animation flush: {}",
                self.error()
            );
            let mut data: wp::WebPData = std::mem::zeroed();
            if wp::WebPAnimEncoderAssemble(self.enc, &mut data) == 0 {
                wp::WebPDataClear(&mut data);
                anyhow::bail!("webp animation assemble: {}", self.error());
            }
            let out = std::slice::from_raw_parts(data.bytes, data.size).to_vec();
            wp::WebPDataClear(&mut data);
            Ok(out)
        }
    }

    /// libwebp's own message for the last failure, which is the only
    /// place it says *why* (the return codes are plain booleans).
    // SAFETY: GetError returns either NULL or a NUL-terminated string owned by
    // the encoder, valid as long as `self` is; it is copied into a String here.
    fn error(&self) -> String {
        unsafe {
            let msg = libwebp_sys::WebPAnimEncoderGetError(self.enc);
            if msg.is_null() {
                "unknown error".to_string()
            } else {
                std::ffi::CStr::from_ptr(msg).to_string_lossy().into_owned()
            }
        }
    }
}

impl Drop for AnimEncoder {
    // SAFETY: `enc` came from WebPAnimEncoderNewInternal, was null-checked, is
    // never handed out or copied, and this is the only place it is deleted.
    fn drop(&mut self) {
        unsafe { libwebp_sys::WebPAnimEncoderDelete(self.enc) }
    }
}

/// Animation summary of a WebP container: (frames, total duration in ms,
/// loop count) — demux only, no frame is decoded. `None` when the file
/// carries no `ANIM` chunk (or does not parse).
// SAFETY: the demuxer borrows `srcbuf` (copy_data=0) and is deleted exactly once,
// after every read of it. A zeroed WebPIterator is the documented pre-GetFrame
// state, and each successfully fetched iterator is released before the next.
pub(super) fn webp_animation(srcbuf: &[u8]) -> Option<(usize, u64, u32)> {
    use libwebp_sys as wp;
    unsafe {
        let data = wp::WebPData {
            bytes: srcbuf.as_ptr(),
            size: srcbuf.len(),
        };
        let dmux = wp::WebPDemuxInternal(
            &data,
            0,
            std::ptr::null_mut(),
            wp::WEBP_DEMUX_ABI_VERSION as _,
        );
        if dmux.is_null() {
            return None;
        }
        let flags = wp::WebPDemuxGetI(dmux, wp::WebPFormatFeature::WEBP_FF_FORMAT_FLAGS);
        let out = (flags & ANIMATION_FLAG != 0).then(|| {
            let frames = wp::WebPDemuxGetI(dmux, wp::WebPFormatFeature::WEBP_FF_FRAME_COUNT);
            let loops = wp::WebPDemuxGetI(dmux, wp::WebPFormatFeature::WEBP_FF_LOOP_COUNT);
            // Durations live per frame, so the total is a walk over the
            // frame table — still no pixel work.
            let mut duration = 0u64;
            for i in 1..=frames {
                let mut iter: wp::WebPIterator = std::mem::zeroed();
                if wp::WebPDemuxGetFrame(dmux, i as i32, &mut iter) != 0 {
                    duration += iter.duration.max(0) as u64;
                    wp::WebPDemuxReleaseIterator(&mut iter);
                }
            }
            (frames as usize, duration, loops)
        });
        wp::WebPDemuxDelete(dmux);
        out
    }
}

/// First frame of an animated WebP as a standalone-decodable
/// bitstream — only when it covers the full canvas at zero offset
/// (true of virtually every real file; a partial first frame would
/// need canvas compositing, so those stay rejected). `None` for
/// non-animated input.
// SAFETY: the demuxer borrows `srcbuf` (no copy) and is deleted exactly once
// on every path before return, so its internal pointers stay valid while read.
// A zeroed WebPIterator is the documented pre-GetFrame state; fragment.bytes
// is null/size-checked and copied out while the demuxer is still alive.
pub(super) fn webp_first_frame(srcbuf: &[u8]) -> Option<Vec<u8>> {
    use libwebp_sys as wp;
    unsafe {
        let data = wp::WebPData {
            bytes: srcbuf.as_ptr(),
            size: srcbuf.len(),
        };
        let dmux = wp::WebPDemuxInternal(
            &data,
            0,
            std::ptr::null_mut(),
            wp::WEBP_DEMUX_ABI_VERSION as _,
        );
        if dmux.is_null() {
            return None;
        }
        let flags = wp::WebPDemuxGetI(dmux, wp::WebPFormatFeature::WEBP_FF_FORMAT_FLAGS);
        if flags & ANIMATION_FLAG == 0 {
            wp::WebPDemuxDelete(dmux);
            return None;
        }
        let cw = wp::WebPDemuxGetI(dmux, wp::WebPFormatFeature::WEBP_FF_CANVAS_WIDTH);
        let ch = wp::WebPDemuxGetI(dmux, wp::WebPFormatFeature::WEBP_FF_CANVAS_HEIGHT);
        let mut iter: wp::WebPIterator = std::mem::zeroed();
        let ok = wp::WebPDemuxGetFrame(dmux, 1, &mut iter);
        let out = (ok != 0
            && iter.complete != 0
            && iter.x_offset == 0
            && iter.y_offset == 0
            && iter.width as u32 == cw
            && iter.height as u32 == ch
            && !iter.fragment.bytes.is_null()
            && iter.fragment.size > 0)
            .then(|| std::slice::from_raw_parts(iter.fragment.bytes, iter.fragment.size).to_vec());
        if ok != 0 {
            wp::WebPDemuxReleaseIterator(&mut iter);
        }
        wp::WebPDemuxDelete(dmux);
        out
    }
}

/// Extract the ICCP and/or EXIF chunks from a WebP container in one
/// mux parse.
// SAFETY: the mux borrows `srcbuf` (copy_data=0) and lives until the single
// WebPMuxDelete at the end. A zeroed WebPData is a valid out-param; chunk
// data points into mux/srcbuf memory and is null/size-checked and copied to
// an owned Vec while the mux is still alive.
pub(super) fn webp_metadata(
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

/// What the encoder for `target` allocates on top of `out8` for a
/// `w x h` output with `channels` channels: the decoded-bytes
/// estimate's encode term (issue #130).
///
/// Per output pixel, at the worst case measured (`/usr/bin/time -l`,
/// M2 Max, exact-size re-encodes of 2000² and 4000² noise, the other
/// buffers subtracted) plus 10-25%. Noise is the worst case for every
/// encoder here, and the one a hostile upload can choose: a photo
/// costs a fraction of it (jpegli sequential at q80: 1.0 B/px on a
/// photo, 8.1 on noise), which the header cannot tell apart. Quality is
/// known, and moves every lossy encoder, so it is priced in.
pub(super) fn encode_cost(
    target: ImageFormat,
    w: usize,
    h: usize,
    channels: u64,
    p: &Resolved,
) -> u64 {
    let px = (w as u64).saturating_mul(h as u64);
    let per_px = match target {
        // Progressive output also holds whole-image coefficients before
        // the first scan is written: 4:4:4 int16, 6 B/px, measured as a
        // constant 6.0-6.2 over sequential at every quality.
        ImageFormat::Jpeg => {
            let tokens = jpeg_token_cost(p.quality);
            match p.encoder {
                Encoder::Jpegli => match jpegli_mode(w, h) {
                    JpegMode::Progressive => tokens + 7,
                    JpegMode::Sequential => tokens,
                    JpegMode::Streaming => return jpeg_streaming_cost(w, px, p.quality),
                },
                // The fastest profile: 6.2-8.5 B/px at any quality.
                Encoder::MozFast => 10,
                // Trellis + progressive: 6.3-18.9.
                Encoder::MozSmall => tokens + 4,
            }
        }
        // The stream writer holds only its output, which `encode_png`
        // caps at about the stored size, the raw frame, and which can
        // touch twice that while its Vec doubles. Measured 1.1 B per raw
        // byte on noise (2048², M2 Max; 3.6 before #133 streamed it),
        // 0.3 on a photo. Quantization adds quantette's working set
        // (17.6-18.2 B/px, RGB only).
        // Plus the writer's chunk buffer and three rows.
        ImageFormat::Png => {
            let per_px = if p.png_quantize.is_some() && channels == 3 {
                20
            } else {
                2 * channels
            };
            let fixed = PNG_IDAT_CHUNK as u64 + 3 * w as u64 * channels;
            return px.saturating_mul(per_px).saturating_add(fixed);
        }
        ImageFormat::Webp => {
            let method = webp_effort().clamp(0, 6);
            return px
                .saturating_mul(webp_cost_per_px(channels, p.webp_quality, method))
                .saturating_add(webp_fixed_cost(channels, method));
        }
        #[cfg(feature = "avif")]
        ImageFormat::Avif => return avif_encode_cost(px, channels, &p.avif_params()),
        // Never encoded: no AVIF in this build, and no GIF encoder.
        _ => 0,
    };
    px.saturating_mul(per_px)
}

/// jpegli optimizes its Huffman tables in both modes, so it holds the
/// whole image's tokens until the tables are built; mozjpeg's trellis
/// path does the same. On noise, by quality (sequential jpegli, B/px):
/// 1.0 at q1, 3.0 at q30, 4.7 at q50, 6.8 at q70, 8.1 at q80, 10.4 at
/// q90, 12.2 at q95, 15.5 at q100.
pub(super) fn jpeg_token_cost(quality: f32) -> u64 {
    match quality as u32 {
        0..=30 => 4,
        31..=50 => 6,
        51..=70 => 8,
        71..=80 => 10,
        81..=90 => 12,
        91..=95 => 15,
        _ => 18,
    }
}

/// libwebp's working set per output pixel at `quality` and `method`
/// (OXIMG_WEBP_EFFORT), on noise (B/px, 2000², M2 Max), priced ~15-20%
/// above it. No method bounds it to the content: at the default 2 a
/// photo holds 2.7 (RGB) and 17.5 (RGBA) against noise's 5.8 and 36.
///
/// - RGB, methods 0-2: 4.1-5.5 at q50, 4.9-5.8 at q75, 5.5-8.6 at q90,
///   13.3 at q100. From method 3 libwebp keeps more analysis per
///   macroblock: 16.4, 18.9, 25.4 and 33.4.
/// - RGBA codes alpha lossless (VP8L), which dominates: 28-37 at
///   methods 0-2 at any quality; 37-38 at 3-5 up to q75, 43.4 at q90,
///   49 at q100; and at method 6 VP8L's exhaustive search, which does
///   not scale evenly: 104-106 at 1024² and 1448², 75 at 2000².
///
/// Part of it does not scale at all (fits over 512² to 2000²): 2-7 MB
/// at methods 0-5, ~27 MB of method 6's search on alpha. [`webp_fixed_cost`]
/// prices that.
pub(super) fn webp_cost_per_px(channels: u64, quality: f32, method: i32) -> u64 {
    let q = quality as u32;
    match (channels == 4, method) {
        (true, 0..=2) => 44,
        (true, 3..=5) => match q {
            0..=75 => 44,
            76..=90 => 50,
            _ => 56,
        },
        (true, _) => 110,
        (false, 0..=2) => match q {
            0..=75 => 8,
            76..=90 => 10,
            _ => 15,
        },
        (false, _) => match q {
            0..=75 => 22,
            76..=90 => 29,
            _ => 38,
        },
    }
}

/// The part of libwebp's working set that does not scale with the
/// image (see [`webp_cost_per_px`]).
pub(super) fn webp_fixed_cost(channels: u64, method: i32) -> u64 {
    if channels == 4 && method >= 6 {
        32 << 20
    } else {
        4 << 20
    }
}

/// [`JpegMode::Streaming`] holds its output and a few rows. The output
/// is the one part that still follows content; on noise, by quality
/// (B/px, 8000², M2 Max): 0.48 at q30, 0.68 at q50, 1.06 at q70, 1.35 at
/// q80, 1.87 at q90, 2.41 at q95, 4.31 at q100, against 0.09-0.73 on a
/// photo. `Dest` grows its `Vec` by doubling, and the last growth
/// touches the old buffer and its copy at once, so the output can hold
/// up to twice its size (macOS remaps large reallocations instead, ~1x
/// at 8000²; the 2048² cell in tests/decoded_bytes.rs held ~1.6x).
/// Priced at twice noise plus 10%, in tenths of a byte. The rows are
/// three iMCU rows of float planes, ~600 B per column, priced at 1 KiB.
pub(super) fn jpeg_streaming_cost(w: usize, px: u64, quality: f32) -> u64 {
    let tenths: u64 = match quality as u32 {
        0..=30 => 11,
        31..=50 => 15,
        51..=70 => 24,
        71..=80 => 30,
        81..=90 => 42,
        91..=95 => 53,
        _ => 95,
    };
    (px.saturating_mul(tenths) / 10).saturating_add((w as u64).saturating_mul(1024))
}

/// SVT-AV1's working set: frame buffers at its internal bit depth,
/// lookahead, and reconstruction, which the preset sizes. Measured per
/// output pixel (RGB, 2000²): presets 7-13 29.5, 4-6 39.5, 1-3 50
/// (preset 0 measured 39, priced with 1-3); the alpha session
/// multiplies that by ~1.45. Above quality 90 the encoder holds more
/// again (noise, preset 8: 34.9 B/px at q90, 40.2 at q100). Plus ~13 MB
/// per encode that does not scale with the frame.
#[cfg(feature = "avif")]
pub(super) fn avif_encode_cost(px: u64, channels: u64, a: &crate::avif::AvifParams) -> u64 {
    let mut rgb: u64 = match a.speed {
        7.. => 32,
        4..=6 => 44,
        _ => 56,
    };
    let quality = if channels == 4 {
        a.quality.max(a.alpha_quality)
    } else {
        a.quality
    };
    if quality > 90 {
        rgb += rgb / 3;
    }
    let per_px = if channels == 4 { rgb * 3 / 2 } else { rgb };
    px.saturating_mul(per_px).saturating_add(16 << 20)
}

/// Encode the resized RGB(A)8 pixels in `Scratch::out8` as `target`.
/// When `target` matches the source format this calls the same encoder
/// with the same arguments as the pre-cross-format code, so same-format
/// output stays byte-identical. JPEG is the only alpha-less target;
/// RGBA input is flattened onto OXIMG_FLATTEN_BG first.
pub(super) fn encode_output(
    s: &mut Scratch,
    dst_w: usize,
    dst_h: usize,
    channels: usize,
    target: ImageFormat,
    p: &Resolved,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    // By this point the input decoded fine: anything that fails now is
    // the server's problem, not the client's.
    encode_output_inner(s, dst_w, dst_h, channels, target, p, icc).context(ServerFault)
}

#[allow(clippy::too_many_arguments)]
fn encode_output_inner(
    s: &mut Scratch,
    dst_w: usize,
    dst_h: usize,
    channels: usize,
    target: ImageFormat,
    p: &Resolved,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    match target {
        ImageFormat::Jpeg => {
            if channels == 4 {
                flatten_alpha_in_out8(s, dst_w, dst_h, p);
            }
            encode_with_icc(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, p, icc)
        }
        ImageFormat::Png => encode_png(
            &s.out8[..dst_w * dst_h * channels],
            dst_w,
            dst_h,
            channels,
            icc,
            p,
        ),
        ImageFormat::Webp => encode_webp(
            &s.out8[..dst_w * dst_h * channels],
            dst_w,
            dst_h,
            channels,
            icc,
            p,
        ),
        // Fully opaque RGBA drops its alpha item inside encode_avif
        // (skipping the second SVT session entirely).
        #[cfg(feature = "avif")]
        ImageFormat::Avif => {
            let params = p.avif_params();
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
        // Unreachable through any request: `from_token` has no "gif", so
        // nothing can ask for it, and `default_target` sends GIF sources
        // to WebP. A hard error rather than a quiet fallback, so a
        // future encoder cannot slip through as mislabeled bytes.
        ImageFormat::Gif => anyhow::bail!("GIF output is not supported"),
    }
}

/// Composite the straight-alpha RGBA8 pixels in `out8` onto the
/// flatten background in linear light, compacting to RGB8 in place
/// (pixel i writes 3i..3i+3 after reading 4i..4i+4, so the forward
/// pass never clobbers unread input).
pub(super) fn flatten_alpha_in_out8(s: &mut Scratch, dst_w: usize, dst_h: usize, p: &Resolved) {
    let (fwd, back) = (fwd_lut(), back_lut());
    let bg = p.flatten_bg;
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

pub fn encode(rgb: &[u8], w: usize, h: usize, p: &Params) -> Result<Vec<u8>, super::Error> {
    encode_with_icc(rgb, w, h, &Resolved::new(p), None)
        .map_err(|e| super::Error::classify(e, false))
}

/// JPEG encode with an optional ICC profile written ahead of the
/// scanlines (both encoders chunk it into the standard APP2 chain).
/// With `icc: None` this is byte-identical to the pre-ICC encoder.
pub(super) fn encode_with_icc(
    rgb: &[u8],
    w: usize,
    h: usize,
    p: &Resolved,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let mut sink = JpegSink::start(w, h, p, icc)?;
    sink.write_rows(rgb)?;
    sink.finish()
}

/// A started JPEG encoder for `p`'s preset, its profile already
/// written: RGB rows go in, in as many calls as the caller likes, and
/// [`Self::finish`] returns the file. Both libraries consume scanlines
/// one row group at a time whatever the call boundaries, so a frame fed
/// in bands encodes to the same bytes as one fed whole — which is what
/// lets the exact-size JPEG path stream from the decoder with no frame
/// buffer (see `stream_exact_jpeg`).
pub(super) enum JpegSink {
    Jpegli(JpegliEncoder),
    // Boxed: mozjpeg's cinfo is inline (~540 bytes) against jpegli's
    // already-boxed one.
    Moz(Box<mozjpeg::compress::CompressStarted<Vec<u8>>>),
}

impl JpegSink {
    pub(super) fn start(w: usize, h: usize, p: &Resolved, icc: Option<&[u8]>) -> Result<JpegSink> {
        if p.encoder == Encoder::Jpegli {
            return Ok(JpegSink::Jpegli(start_jpegli(w, h, p.quality, icc)));
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
        Ok(JpegSink::Moz(Box::new(started)))
    }

    /// Whole RGB rows, continuing where the last call stopped.
    pub(super) fn write_rows(&mut self, rows: &[u8]) -> Result<()> {
        match self {
            JpegSink::Jpegli(enc) => enc.write_scanlines(rows)?,
            JpegSink::Moz(started) => started.write_scanlines(rows)?,
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<Vec<u8>> {
        Ok(match self {
            JpegSink::Jpegli(enc) => enc.finish(),
            JpegSink::Moz(started) => started.finish()?,
        })
    }
}

/// Chunk a profile into standard APP2 `ICC_PROFILE` payloads with
/// 1-based sequence numbers. Deliberately not the mozjpeg crate's
/// `write_icc_profile`, which emits 0-based sequence numbers (as of
/// 0.10.13) that libjpeg's own `jpeg_read_icc_profile` — and therefore
/// browsers — reject wholesale.
pub(super) fn icc_app2_chunks(icc: &[u8]) -> impl Iterator<Item = Vec<u8>> + '_ {
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
/// OXIMG_JPEG_PROGRESSIVE=0 selects sequential jpegli (SOF1, extended
/// sequential, which is how jpegli marks it; not baseline SOF0) with
/// optimized Huffman tables: a few percent larger output at 512 px and
/// up, for about a quarter less encoder CPU than the scan script.
///
/// Below OXIMG_JPEG_SEQUENTIAL_PIXELS output pixels the default is
/// sequential too: the identical pixels in fewer bytes and less CPU.
/// Above OXIMG_JPEG_STREAMING_PIXELS either mode gives way to
/// [`JpegMode::Streaming`], which holds rows instead of the image.
pub(super) fn jpegli_mode(w: usize, h: usize) -> JpegMode {
    let c = crate::config::config();
    let px = (w as u64) * (h as u64);
    if px > c.jpegli_streaming_pixels {
        JpegMode::Streaming
    } else if c.jpegli_progressive && px > c.jpegli_sequential_pixels {
        JpegMode::Progressive
    } else {
        JpegMode::Sequential
    }
}

/// A jpegli encoder for a `w` x `h` frame with its profile written:
/// what [`JpegSink`] and the fused path's encoder both start from.
pub(super) fn start_jpegli(w: usize, h: usize, quality: f32, icc: Option<&[u8]>) -> JpegliEncoder {
    // cjpegli emits progressive by default; the libjpeg-compat layer does
    // not. Progressive is worth several percent at these sizes; the scan
    // script is oximg's own (see jpegli_enc.rs), not jpegli's level 2.
    let mut enc = JpegliEncoder::new(w, h, quality, jpegli_mode(w, h));
    if let Some(icc) = icc {
        for chunk in icc_app2_chunks(icc) {
            enc.write_marker(JPEG_APP2, &chunk);
        }
    }
    enc
}
