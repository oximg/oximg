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
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(p.png_compression(quantized.is_some()));
    match &quantized {
        Some((palette, indices)) => {
            enc.set_color(png::ColorType::Indexed);
            enc.set_palette(palette.as_slice());
            let mut writer = enc.write_header().context("PNG header")?;
            writer.write_image_data(indices).context("PNG encode")?;
            writer.finish().context("PNG finish")?;
        }
        None => {
            enc.set_color(if channels == 4 {
                png::ColorType::Rgba
            } else {
                png::ColorType::Rgb
            });
            let mut writer = enc.write_header().context("PNG header")?;
            writer.write_image_data(pixels).context("PNG encode")?;
            writer.finish().context("PNG finish")?;
        }
    }
    Ok(out)
}

/// Wu color quantization (quantette) with Floyd-Steinberg dithering,
/// single-threaded — request concurrency is the CPU semaphore's job.
/// Returns (PLTE bytes, per-pixel indices).
fn quantize_rgb(pixels: &[u8], w: usize, h: usize, colors: u16) -> (Vec<u8>, Vec<u8>) {
    use quantette::deps::palette::Srgb;
    let px: Vec<Srgb<u8>> = pixels
        .chunks_exact(3)
        .map(|c| Srgb::new(c[0], c[1], c[2]))
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
        const ANIMATION_FLAG: u32 = 0x02;
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
/// OXIMG_JPEG_PROGRESSIVE=0 selects baseline jpegli: a few percent
/// larger output, but the entropy pass at finish_compress shrinks,
/// which is the fused path's only serial tail.
pub(super) fn jpegli_progressive() -> bool {
    crate::config::config().jpegli_progressive
}

pub(super) fn encode_jpegli(
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
