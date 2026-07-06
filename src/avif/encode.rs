//! SVT-AV1 still-image encoding: session lifecycle, the fused-path
//! entry points, and container assembly.

use super::*;

thread_local! {
    /// Encode-side plane scratch, reused across requests.
    static ENC_SCRATCH: std::cell::RefCell<(Vec<u16>, Vec<u16>, Vec<u16>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
}

#[derive(Clone, Copy)]
pub struct AvifParams {
    /// 0-100, libavif semantics.
    pub quality: u8,
    /// 0-100, quality for the alpha auxiliary image.
    pub alpha_quality: u8,
    /// SVT preset (enc_mode); the sync-path setting is 8.
    pub speed: i8,
    /// SVT logical processors; 1 keeps the CPU-slot model honest.
    pub threads: u32,
}

impl Default for AvifParams {
    fn default() -> Self {
        AvifParams {
            quality: 60,
            alpha_quality: 60,
            speed: 8,
            threads: 1,
        }
    }
}

/// Encode interleaved RGB8 (`channels == 3`) or straight-alpha RGBA8
/// (`channels == 4`) as AVIF. Alpha is carried as an auxiliary AV1 image:
/// SVT-AV1 cannot encode 4:0:0, so — like libavif's codec_svt.c — the
/// alpha plane is encoded as the luma of a 4:2:0 image with zeroed
/// placeholder chroma, which flat-codes to almost nothing.
pub fn encode_avif(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    p: &AvifParams,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    ensure!(
        channels == 3 || channels == 4,
        "unsupported channel count {channels}"
    );
    ensure!(pixels.len() >= w * h * channels, "pixel buffer too small");
    let color = ENC_SCRATCH.with(|s| {
        let (y_plane, cb_plane, cr_plane) = &mut *s.borrow_mut();
        rgb_to_yuv420_10bit(pixels, w, h, channels, y_plane, cb_plane, cr_plane);
        encode_svt(
            y_plane,
            cb_plane,
            cr_plane,
            w,
            h,
            quality_to_qp(p.quality),
            false,
            p,
        )
    })?;
    // A fully opaque alpha plane would still cost a whole second SVT
    // session; the scan below is one early-exit pass (first transparent
    // pixel aborts it), so alpha-bearing images pay ~nothing and opaque
    // RGBA drops to the 3-channel output — byte-identical to encoding
    // the same pixels as RGB, since the color path ignores px[3].
    let has_alpha = channels == 4 && pixels[..w * h * 4].chunks_exact(4).any(|px| px[3] != 255);
    let alpha = if has_alpha {
        let a_plane: Vec<u16> = pixels
            .chunks_exact(4)
            .map(|px| ((px[3] as u32 * 1023 + 128) / 255) as u16)
            .collect();
        // One zeroed buffer serves as both placeholder chroma planes.
        let uv = vec![0u16; w.div_ceil(2) * h.div_ceil(2)];
        Some(encode_svt(
            &a_plane,
            &uv,
            &uv,
            w,
            h,
            quality_to_qp(p.alpha_quality),
            true,
            p,
        )?)
    } else {
        None
    };
    Ok(finish_avif(&color, alpha.as_deref(), w, h, icc))
}

/// Start a color (non-alpha) encoder session for the pipeline's fused
/// AVIF path — created on the fused worker while the JPEG decode is
/// still running, so its ~1ms setup hides behind the decode wall.
pub(crate) fn start_color_session(w: usize, h: usize, p: &AvifParams) -> Result<SvtSession> {
    SvtSession::create(w, h, quality_to_qp(p.quality), false, p)
}

/// Encode pre-filled 10-bit 4:2:0 planes (no alpha) on an
/// already-created session and assemble the container. The planes must
/// come from the row conversion API at the session's dimensions, so
/// output is byte-identical to [`encode_avif`] on the same pixels.
pub(crate) fn encode_avif_with_session(
    session: SvtSession,
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let (w, h) = (session.w, session.h);
    let color = session.encode(y_plane, cb_plane, cr_plane)?;
    Ok(finish_avif(&color, None, w, h, icc))
}

/// Encode interleaved RGB8 with an already-created color session —
/// the oriented-target preheat path. Same conversion scratch, same
/// encode as [`encode_avif`] on 3-channel input, so the output is
/// byte-identical to the serial path (asserted in tests).
pub(crate) fn encode_avif_rgb_with_session(
    session: SvtSession,
    pixels: &[u8],
    w: usize,
    h: usize,
    icc: Option<&[u8]>,
) -> Result<Vec<u8>> {
    ensure!(pixels.len() >= w * h * 3, "pixel buffer too small");
    ensure!(
        (session.w, session.h) == (w, h),
        "preheated session dims mismatch"
    );
    let color = ENC_SCRATCH.with(|s| {
        let (y_plane, cb_plane, cr_plane) = &mut *s.borrow_mut();
        rgb_to_yuv420_10bit(pixels, w, h, 3, y_plane, cb_plane, cr_plane);
        session.encode(y_plane, cb_plane, cr_plane)
    })?;
    Ok(finish_avif(&color, None, w, h, icc))
}

/// Assemble the AVIF container around the encoded AV1 item(s); with a
/// profile, splice the `colr` (`prof`) property in afterwards
/// (avif-serialize speaks CICP only). The nclx `colr` stays alongside
/// it — matrix coefficients still describe the YUV→RGB step, while
/// the ICC profile governs the resulting RGB, exactly as in JPEG.
pub(super) fn finish_avif(
    color: &[u8],
    alpha: Option<&[u8]>,
    w: usize,
    h: usize,
    icc: Option<&[u8]>,
) -> Vec<u8> {
    let mut fy = avif_serialize::Aviffy::new();
    fy.matrix_coefficients(avif_serialize::constants::MatrixCoefficients::Bt601)
        .full_color_range(true)
        .set_chroma_subsampling((true, true));
    let out = fy.to_vec(color, alpha, w as u32, h as u32, 10);
    if let Some(patched) = icc.and_then(|icc| embed_icc(&out, icc)) {
        return patched;
    }
    out
}

#[allow(clippy::too_many_arguments)]
pub(super) fn encode_svt(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    w: usize,
    h: usize,
    qp: u32,
    aux_alpha: bool,
    p: &AvifParams,
) -> Result<Vec<u8>> {
    let session = SvtSession::create(w, h, qp, aux_alpha, p)?;
    session.encode(y_plane, cb_plane, cr_plane)
}

/// An initialized SVT-AV1 still-image encoder session. Creation costs
/// ~1ms of wall (SVT spawns its internal pipeline threads there) — a
/// measurable share of a thumbnail encode — so the fused AVIF path
/// creates the session on its worker thread while the JPEG decode is
/// still running and moves it back for the encode.
pub(crate) struct SvtSession {
    handle: *mut svt::EbComponentType,
    w: usize,
    h: usize,
    aux_alpha: bool,
}

// Safety: the handle is only ever used by one thread at a time (the
// session moves by ownership); SVT itself is internally threaded and
// makes no thread-affinity assumptions about its API caller.
unsafe impl Send for SvtSession {}

impl Drop for SvtSession {
    fn drop(&mut self) {
        unsafe {
            svt::svt_av1_enc_deinit(self.handle);
            svt::svt_av1_enc_deinit_handle(self.handle);
        }
    }
}

impl SvtSession {
    /// init_handle + parameters + enc_init, mirroring libavif's
    /// codec_svt.c for a single still image.
    pub(crate) fn create(
        w: usize,
        h: usize,
        qp: u32,
        aux_alpha: bool,
        p: &AvifParams,
    ) -> Result<SvtSession> {
        let timing = crate::config::config().timing;
        let t0 = std::time::Instant::now();
        unsafe {
            let mut handle: *mut svt::EbComponentType = std::ptr::null_mut();
            let mut config: svt::EbSvtAv1EncConfiguration = std::mem::zeroed();
            let err = svt::svt_av1_enc_init_handle(&mut handle, &mut config);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt init_handle: {err:?}"
            );
            // Guard: every exit below must deinit the handle.
            let session = SvtSession {
                handle,
                w,
                h,
                aux_alpha,
            };

            config.encoder_color_format = svt::EbColorFormat::EB_YUV420;
            config.encoder_bit_depth = 10;
            if aux_alpha {
                // CICP does not apply to the alpha auxiliary image
                // (AV1-AVIF spec section 4); its color range shall be full.
                config.color_primaries = 2; // unspecified
                config.transfer_characteristics = 2; // unspecified
                config.matrix_coefficients = 2; // unspecified
            } else {
                config.color_primaries = 1; // BT.709
                config.transfer_characteristics = 13; // sRGB
                config.matrix_coefficients = 6; // BT.601
            }
            config.color_range = 1; // full
            config.source_width = w as u32;
            config.source_height = h as u32;
            config.level_of_parallelism = p.threads;
            config.aq_mode = 2;
            config.rate_control_mode = 0;
            config.min_qp_allowed = 0;
            config.max_qp_allowed = 63;
            config.qp = qp;
            config.enc_mode = p.speed;
            config.force_key_frames = true;
            config.avif = true;
            let tune = std::ffi::CString::new("tune").unwrap();
            let three = std::ffi::CString::new("3").unwrap();
            ensure!(
                svt::svt_av1_enc_parse_parameter(&mut config, tune.as_ptr(), three.as_ptr())
                    == svt::EbErrorType::EB_ErrorNone,
                "svt tune=3"
            );

            let err = svt::svt_av1_enc_set_parameter(handle, &mut config);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt set_parameter: {err:?}"
            );
            let err = svt::svt_av1_enc_init(handle);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt enc_init: {err:?}"
            );
            if timing {
                eprintln!(
                    "timing svt-init({w}x{h}{}) {:.1}ms",
                    if aux_alpha { " alpha" } else { "" },
                    t0.elapsed().as_secs_f64() * 1e3,
                );
            }
            Ok(session)
        }
    }

    /// Send the planes, flush, and drain the AV1 payload; the session
    /// is consumed (deinit on drop).
    pub(crate) fn encode(
        self,
        y_plane: &[u16],
        cb_plane: &[u16],
        cr_plane: &[u16],
    ) -> Result<Vec<u8>> {
        let (w, h) = (self.w, self.h);
        let cw = w.div_ceil(2);
        let timing = crate::config::config().timing;
        let t0 = std::time::Instant::now();
        unsafe {
            let mut io: svt::EbSvtIOFormat = std::mem::zeroed();
            io.luma = y_plane.as_ptr() as *mut u8;
            io.cb = cb_plane.as_ptr() as *mut u8;
            io.cr = cr_plane.as_ptr() as *mut u8;
            io.y_stride = w as u32;
            io.cb_stride = cw as u32;
            io.cr_stride = cw as u32;

            let mut input: svt::EbBufferHeaderType = std::mem::zeroed();
            input.size = std::mem::size_of::<svt::EbBufferHeaderType>() as u32;
            input.p_buffer = (&mut io) as *mut svt::EbSvtIOFormat as *mut u8;
            input.n_filled_len = (y_plane.len() * 2 + (cb_plane.len() + cr_plane.len()) * 2) as u32;
            input.pic_type = svt::EbAv1PictureType::EB_AV1_KEY_PICTURE;
            input.pts = 0;

            let err = svt::svt_av1_enc_send_picture(self.handle, &mut input);
            ensure!(
                err == svt::EbErrorType::EB_ErrorNone,
                "svt send_picture: {err:?}"
            );

            // EOS flush.
            let mut eos: svt::EbBufferHeaderType = std::mem::zeroed();
            eos.size = std::mem::size_of::<svt::EbBufferHeaderType>() as u32;
            eos.flags = svt::EB_BUFFERFLAG_EOS;
            let err = svt::svt_av1_enc_send_picture(self.handle, &mut eos);
            ensure!(err == svt::EbErrorType::EB_ErrorNone, "svt eos: {err:?}");

            // Drain packets until EOS.
            let mut av1 = Vec::new();
            loop {
                let mut out: *mut svt::EbBufferHeaderType = std::ptr::null_mut();
                let res = svt::svt_av1_enc_get_packet(self.handle, &mut out, 1);
                if !out.is_null() {
                    let ob = &*out;
                    if !ob.p_buffer.is_null() && ob.n_filled_len > 0 {
                        av1.extend_from_slice(std::slice::from_raw_parts(
                            ob.p_buffer,
                            ob.n_filled_len as usize,
                        ));
                    }
                    let at_eos = ob.flags & svt::EB_BUFFERFLAG_EOS != 0;
                    svt::svt_av1_enc_release_out_buffer(&mut out);
                    if at_eos {
                        break;
                    }
                }
                ensure!(
                    res == svt::EbErrorType::EB_ErrorNone,
                    "svt get_packet: {res:?}"
                );
            }
            if timing {
                eprintln!(
                    "timing svt-enc({w}x{h}{}) {:.1}ms",
                    if self.aux_alpha { " alpha" } else { "" },
                    t0.elapsed().as_secs_f64() * 1e3,
                );
            }
            ensure!(!av1.is_empty(), "svt produced no output");
            Ok(av1)
        }
    }
}
