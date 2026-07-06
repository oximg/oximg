//! The streaming JPEG source path: header pre-scan, DCT
//! shrink-on-load decode, fuse selection, and the post-resize
//! orientation/ICC handling.

use super::*;

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
        match decode_resize(s, dec, max_w, max_h, parallel, orientation, Fuse::Off, None)? {
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
            #[cfg(feature = "avif")]
            Decoded::PixelsSession { .. } => unreachable!("no fuse was requested"),
            Decoded::Encoded(_) => unreachable!("no fuse quality was requested"),
        }
    })
}

/// Result of the JPEG decode stage: either resized pixels left in
/// `Scratch::out8` for a separate encode, or — on the fused path — the
/// finished JPEG bytes (decode overlapped with resize+encode).
pub(super) enum Decoded {
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
    /// Resized pixels in `Scratch::out8` plus a color session created
    /// during the decode overlap, sized for the displayed frame — the
    /// oriented-AVIF preheat path: rotate, convert, encode.
    #[cfg(feature = "avif")]
    PixelsSession {
        dst_w: usize,
        dst_h: usize,
        session: crate::avif::SvtSession,
    },
}

/// How the JPEG decode overlaps with downstream work (all variants
/// produce identical pixels/bytes; see overlap_gate).
#[derive(Clone, Copy)]
pub(super) enum Fuse {
    /// Serial: decode, then resize inline on the same thread.
    Off,
    /// Decode ∥ resize + incremental jpegli encode on a worker thread —
    /// the same-format JPEG fast path; yields `Decoded::Encoded`.
    Jpegli { quality: f32 },
    /// Decode ∥ resize into `Scratch::out8` on a worker thread; the
    /// (one-shot) target encoder runs after. Used for cross-format
    /// targets, hiding the resize behind the decode wall.
    Pixels,
    /// `Pixels`, plus the worker creates the AVIF color session for
    /// the *displayed* frame during the decode — the oriented-AVIF
    /// path, where rotation forbids streaming the YUV conversion but
    /// the ~1ms session setup still hides behind the decode wall.
    #[cfg(feature = "avif")]
    PixelsPreheat { params: crate::avif::AvifParams },
    /// Decode ∥ resize + RGB→YUV conversion straight into the 10-bit
    /// planes on the worker thread, which also creates the SVT session
    /// while the decode runs — AVIF targets, hiding the resize, the
    /// conversion, and the encoder setup behind the decode wall.
    #[cfg(feature = "avif")]
    Yuv { params: crate::avif::AvifParams },
}

#[allow(clippy::too_many_arguments)]
pub(super) fn decode_resize<R: std::io::BufRead>(
    s: &mut Scratch,
    mut dec: Decompress<R>,
    max_w: u32,
    max_h: u32,
    parallel: usize,
    orientation: crate::meta::Orientation,
    fuse: Fuse,
    // Written ahead of the scanlines by the fused jpegli encoder;
    // the other fuse variants embed via their one-shot encoders.
    icc: Option<&[u8]>,
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
            fused_resize_encode(&mut started, dec_w, dec_h, dst_w, dst_h, quality, icc)?
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
        if let Some((decode_ms, ())) = fused_resize_pixels(
            &mut started,
            dec_w,
            dec_h,
            dst_w,
            dst_h,
            &mut s.out8[..dst_w * dst_h * 3],
            2,
            || Ok(()),
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
    if let Fuse::PixelsPreheat { params } = fuse
        && linear
    {
        // The session encodes the *displayed* (rotated) frame. Session
        // creation is non-fatal: SVT resource pressure downgrades to
        // the serial encode of the same (still good) resized pixels
        // instead of failing a request the serial path would serve —
        // the same philosophy as the worker-spawn fallback.
        let (disp_w, disp_h) = orientation.display_dims(dst_w, dst_h);
        scratch_u8(&mut s.out8, dst_w * dst_h * 3);
        if let Some((decode_ms, session)) = fused_resize_pixels(
            &mut started,
            dec_w,
            dec_h,
            dst_w,
            dst_h,
            &mut s.out8[..dst_w * dst_h * 3],
            4,
            || Ok(crate::avif::start_color_session(disp_w, disp_h, &params).ok()),
        )? {
            if timing {
                let total = t0.elapsed().as_secs_f64() * 1e3;
                eprintln!(
                    "timing fused-px+session({dec_w}x{dec_h}->{dst_w}x{dst_h}) decode={decode_ms:.1}ms tail={:.1}ms total={total:.1}ms",
                    total - decode_ms
                );
            }
            started.finish().context("decode finish failed")?;
            return Ok(match session {
                Some(session) => Decoded::PixelsSession {
                    dst_w,
                    dst_h,
                    session,
                },
                None => Decoded::Pixels { dst_w, dst_h },
            });
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

/// The streaming JPEG source path: header pre-scan (orientation +
/// ICC), DCT shrink-on-load decode, fuse selection, and the
/// post-resize orientation/ICC application. Split out of the format
/// dispatch, where the other formats are one-line calls.
pub(super) fn process_jpeg<R: std::io::BufRead>(
    s: &mut Scratch,
    reader: R,
    target: ImageFormat,
    p: &Params,
) -> Result<Vec<u8>> {
    Ok({
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
        // Mirrors encode_output's AVIF arm (the tuned operating
        // point); the session the fused workers create from
        // these is what encodes the frame.
        #[cfg(feature = "avif")]
        let avif_params = || {
            let quality = avif_quality();
            crate::avif::AvifParams {
                quality,
                alpha_quality: avif_alpha_quality(quality),
                speed: avif_speed(),
                ..Default::default()
            }
        };
        let cross_fuse = || -> Fuse {
            #[cfg(feature = "avif")]
            if target == ImageFormat::Avif {
                return Fuse::Yuv {
                    params: avif_params(),
                };
            }
            Fuse::Pixels
        };
        let fuse = if p.parallel > 1
            || !overlap_gate()
            || std::env::var("OXIMG_RESIZE_BACKEND").as_deref() == Ok("fir")
        {
            Fuse::Off
        } else if !orientation.is_upright() {
            // Rotation happens on the resized frame before the
            // one-shot encode — incompatible with streaming rows
            // into jpegli or into the YUV planes, so oriented
            // sources take the pixel fuse (decode ∥ resize kept)
            // and rotate after. AVIF targets additionally
            // preheat their encoder session on the worker,
            // erasing most of the remaining oriented penalty
            // (measured ~1ms of ~1.2ms). The rest is closed by
            // measurement, not TODO: streaming the jpegli
            // encode only works for flip-h (rows stay in
            // order) — real-world mirrored images are too rare
            // to carry the complexity — and streaming the YUV
            // conversion under 90° rotation would pair chroma
            // across resized columns, a correctness minefield
            // for ~0.2ms.
            #[cfg(feature = "avif")]
            if target == ImageFormat::Avif {
                Fuse::PixelsPreheat {
                    params: avif_params(),
                }
            } else {
                Fuse::Pixels
            }
            #[cfg(not(feature = "avif"))]
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
            icc.as_deref(),
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
            #[cfg(feature = "avif")]
            Decoded::PixelsSession {
                dst_w,
                dst_h,
                session,
            } => {
                // Rotate onto the displayed frame the preheated
                // session was sized for, then convert + encode.
                let dims = crate::meta::apply_orientation(
                    &s.out8[..dst_w * dst_h * 3],
                    dst_w,
                    dst_h,
                    3,
                    orientation,
                    &mut s.chunk8,
                );
                std::mem::swap(&mut s.out8, &mut s.chunk8);
                crate::avif::encode_avif_rgb_with_session(
                    session,
                    &s.out8[..dims.0 * dims.1 * 3],
                    dims.0,
                    dims.1,
                    icc.as_deref(),
                )?
            }
        }
    })
}
