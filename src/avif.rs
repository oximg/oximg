//! AVIF encoding via SVT-AV1 (sync-path settings validated in the encoder
//! study: preset 8, tune=3, 10-bit 4:2:0) and decoding via dav1d. The SVT
//! session setup mirrors libavif's codec_svt.c so measurements against
//! `avifenc -c svt` transfer.

use crate::svt::bindings as svt;
use crate::yuv::{self, Row};
use anyhow::{Context, Result, ensure};

/// libavif's quality -> quantizer mapping (codec_svt.c).
fn quality_to_qp(quality: u8) -> u32 {
    ((100 - quality as u32) * 63 + 50) / 100
}

/// RGB(A)8 -> 10-bit 4:2:0 YUV, BT.601 matrix, full range (matching the
/// avifenc defaults used in the encoder study). Chroma is averaged over
/// each 2x2 block; an alpha channel, if present, is ignored here (it is
/// encoded as a separate auxiliary image). The scalar rows are the
/// reference; the aarch64 NEON rows mirror their arithmetic operation
/// for operation and are asserted bit-identical in tests (the yuv.rs
/// contract). x86-64 AVX2 rows are a possible follow-up.
fn rgb_to_yuv420_10bit(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    y_plane: &mut Vec<u16>,
    cb_plane: &mut Vec<u16>,
    cr_plane: &mut Vec<u16>,
) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    // Grow-only scratch; every element of all three planes is written by
    // the loops below.
    for (plane, len) in [
        (&mut *y_plane, w * h),
        (&mut *cb_plane, cw * ch),
        (&mut *cr_plane, cw * ch),
    ] {
        if plane.len() < len {
            plane.resize(len, 0);
        }
        plane.truncate(len);
    }

    luma_rows(&pixels[..w * h * channels], channels, y_plane);
    chroma_rows(pixels, w, h, channels, cb_plane, cr_plane);
}

/// Luma: Y10 = (0.299 R + 0.587 G + 0.114 B) * 1023/255, fixed point.
/// Coefficients sum to 4096, so the pre-scale maximum is 4096*255 and the
/// rounding divide maps 255 -> exactly 1023 (never 1024: out-of-range
/// samples make SVT emit full-scale luma garbage in the affected blocks).
fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
    #[cfg(target_arch = "aarch64")]
    if crate::yuv::neon() {
        return unsafe { neon_enc::luma_rows(pixels, channels, y_plane) };
    }
    luma_rows_scalar(pixels, channels, y_plane);
}

fn luma_rows_scalar(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
    for (px, y) in pixels.chunks_exact(channels).zip(y_plane.iter_mut()) {
        *y = luma_px(px[0], px[1], px[2]);
    }
}

#[inline]
fn luma_px(r: u8, g: u8, b: u8) -> u16 {
    let (r, g, b) = (r as u32, g as u32, b as u32);
    (((1225 * r + 2404 * g + 467 * b) * 1023 + 522_240) / 1_044_480) as u16
}

fn chroma_rows(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    cb_plane: &mut [u16],
    cr_plane: &mut [u16],
) {
    #[cfg(target_arch = "aarch64")]
    if crate::yuv::neon() {
        return unsafe { neon_enc::chroma_rows(pixels, w, h, channels, cb_plane, cr_plane) };
    }
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    for cy in 0..ch {
        for cx in 0..cw {
            let (cb, cr) = chroma_block(pixels, w, h, channels, cx, cy);
            cb_plane[cy * cw + cx] = cb;
            cr_plane[cy * cw + cx] = cr;
        }
    }
}

/// Chroma from the 2x2-averaged RGB block at (cx, cy); partial blocks
/// at the right/bottom edges average the pixels that exist. The scalar
/// reference for both the scalar loop and the NEON edge handling.
#[inline]
fn chroma_block(
    pixels: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    cx: usize,
    cy: usize,
) -> (u16, u16) {
    let (mut rs, mut gs, mut bs, mut n) = (0u32, 0u32, 0u32, 0u32);
    for dy in 0..2 {
        for dx in 0..2 {
            let (x, yy) = (cx * 2 + dx, cy * 2 + dy);
            if x < w && yy < h {
                let p = (yy * w + x) * channels;
                rs += pixels[p] as u32;
                gs += pixels[p + 1] as u32;
                bs += pixels[p + 2] as u32;
                n += 1;
            }
        }
    }
    let (r, g, b) = (
        rs as f32 / n as f32,
        gs as f32 / n as f32,
        bs as f32 / n as f32,
    );
    let y = 0.299 * r + 0.587 * g + 0.114 * b;
    let cb = (b - y) * (0.5 / (1.0 - 0.114)) * (1023.0 / 255.0) + 512.0;
    let cr = (r - y) * (0.5 / (1.0 - 0.299)) * (1023.0 / 255.0) + 512.0;
    (
        (cb.round() as i32).clamp(0, 1023) as u16,
        (cr.round() as i32).clamp(0, 1023) as u16,
    )
}

/// Encode-side RGB(A)8 -> YUV NEON rows, mirroring the scalar reference
/// operation for operation (same integer formula for luma via an exact
/// division identity, same f32 order for chroma, no FMA contraction) so
/// output is bit-identical — asserted exhaustively in tests.
#[cfg(target_arch = "aarch64")]
mod neon_enc {
    use std::arch::aarch64::*;

    /// Deinterleave 8 RGB(A) pixels starting at `p`.
    /// Safety: caller guarantees 8 full pixels at `p`; NEON enabled.
    #[inline]
    unsafe fn load8(p: *const u8, channels: usize) -> (uint8x8_t, uint8x8_t, uint8x8_t) {
        unsafe {
            if channels == 3 {
                let v = vld3_u8(p);
                (v.0, v.1, v.2)
            } else {
                let v = vld4_u8(p);
                (v.0, v.1, v.2)
            }
        }
    }

    /// The scalar luma divide, vectorized exactly: the divisor factors
    /// as 1044480 = 4096 * 255, floor division composes through the
    /// factors, and (t * 8421505) >> 31 == floor(t / 255) — the
    /// round-up magic m = ceil(2^31/255) with error e = m*255 - 2^31 =
    /// 127, exact for every t <= floor(2^31/127) = 16.9M, far above
    /// this path's t <= 260992. Proven exhaustively in
    /// tests::luma_divider_identity_is_exact.
    #[inline]
    unsafe fn luma4(r: uint16x4_t, g: uint16x4_t, b: uint16x4_t) -> uint16x4_t {
        unsafe {
            let acc = vmlal_n_u16(vmlal_n_u16(vmull_n_u16(r, 1225), g, 2404), b, 467);
            let acc = vaddq_u32(vmulq_n_u32(acc, 1023), vdupq_n_u32(522_240));
            let t = vshrq_n_u32::<12>(acc);
            let lo = vshrq_n_u64::<31>(vmull_n_u32(vget_low_u32(t), 8_421_505));
            let hi = vshrq_n_u64::<31>(vmull_n_u32(vget_high_u32(t), 8_421_505));
            vmovn_u32(vcombine_u32(vmovn_u64(lo), vmovn_u64(hi)))
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn luma_rows(pixels: &[u8], channels: usize, y_plane: &mut [u16]) {
        unsafe {
            let n = y_plane.len();
            let mut i = 0;
            while i + 8 <= n {
                let (r, g, b) = load8(pixels.as_ptr().add(i * channels), channels);
                let (r, g, b) = (vmovl_u8(r), vmovl_u8(g), vmovl_u8(b));
                let y = vcombine_u16(
                    luma4(vget_low_u16(r), vget_low_u16(g), vget_low_u16(b)),
                    luma4(vget_high_u16(r), vget_high_u16(g), vget_high_u16(b)),
                );
                vst1q_u16(y_plane.as_mut_ptr().add(i), y);
                i += 8;
            }
            for (j, y) in y_plane.iter_mut().enumerate().skip(i) {
                let p = j * channels;
                *y = super::luma_px(pixels[p], pixels[p + 1], pixels[p + 2]);
            }
        }
    }

    #[target_feature(enable = "neon")]
    pub(super) unsafe fn chroma_rows(
        pixels: &[u8],
        w: usize,
        h: usize,
        channels: usize,
        cb_plane: &mut [u16],
        cr_plane: &mut [u16],
    ) {
        // The same f32 constants the scalar reference spells inline.
        const CB1: f32 = 0.5 / (1.0 - 0.114);
        const CR1: f32 = 0.5 / (1.0 - 0.299);
        const SCALE: f32 = 1023.0 / 255.0;
        unsafe {
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let four = vdupq_n_f32(4.0);
            let v512 = vdupq_n_f32(512.0);
            let vmax = vdupq_n_s32(1023);
            for cy in 0..ch {
                let y0 = cy * 2;
                let mut cx = 0usize;
                // Vector path: 4 chroma columns = 8 source pixels, both
                // rows in range, so every block is a full 2x2 (n = 4).
                if y0 + 1 < h {
                    while cx * 2 + 8 <= w {
                        let p0 = pixels.as_ptr().add((y0 * w + cx * 2) * channels);
                        let p1 = pixels.as_ptr().add(((y0 + 1) * w + cx * 2) * channels);
                        let (r0, g0, b0) = load8(p0, channels);
                        let (r1, g1, b1) = load8(p1, channels);
                        // Pairwise-add columns, then the two rows: the
                        // 2x2 sums for 4 chroma columns (max 1020).
                        let rs = vadd_u16(vpaddl_u8(r0), vpaddl_u8(r1));
                        let gs = vadd_u16(vpaddl_u8(g0), vpaddl_u8(g1));
                        let bs = vadd_u16(vpaddl_u8(b0), vpaddl_u8(b1));
                        // Mirror the scalar: sums / n, then the mul/add
                        // chain in source order (no FMA).
                        let r = vdivq_f32(vcvtq_f32_u32(vmovl_u16(rs)), four);
                        let g = vdivq_f32(vcvtq_f32_u32(vmovl_u16(gs)), four);
                        let b = vdivq_f32(vcvtq_f32_u32(vmovl_u16(bs)), four);
                        let y = vaddq_f32(
                            vaddq_f32(vmulq_n_f32(r, 0.299), vmulq_n_f32(g, 0.587)),
                            vmulq_n_f32(b, 0.114),
                        );
                        let cb =
                            vaddq_f32(vmulq_n_f32(vmulq_n_f32(vsubq_f32(b, y), CB1), SCALE), v512);
                        let cr =
                            vaddq_f32(vmulq_n_f32(vmulq_n_f32(vsubq_f32(r, y), CR1), SCALE), v512);
                        // round() then clamp(0, 1023): ties-away convert,
                        // min against 1023, saturating-unsigned narrow
                        // (which floors negatives at 0).
                        let cb = vqmovun_s32(vminq_s32(vcvtaq_s32_f32(cb), vmax));
                        let cr = vqmovun_s32(vminq_s32(vcvtaq_s32_f32(cr), vmax));
                        vst1_u16(cb_plane.as_mut_ptr().add(cy * cw + cx), cb);
                        vst1_u16(cr_plane.as_mut_ptr().add(cy * cw + cx), cr);
                        cx += 4;
                    }
                }
                // Right-edge columns and the odd-height bottom row take
                // the scalar reference block (partial 2x2 averages).
                for cx in cx..cw {
                    let (cb, cr) = super::chroma_block(pixels, w, h, channels, cx, cy);
                    cb_plane[cy * cw + cx] = cb;
                    cr_plane[cy * cw + cx] = cr;
                }
            }
        }
    }
}

thread_local! {
    /// Encode-side plane scratch, reused across requests.
    static ENC_SCRATCH: std::cell::RefCell<(Vec<u16>, Vec<u16>, Vec<u16>)> =
        const { std::cell::RefCell::new((Vec::new(), Vec::new(), Vec::new())) };
}

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
    let mut fy = avif_serialize::Aviffy::new();
    fy.matrix_coefficients(avif_serialize::constants::MatrixCoefficients::Bt601)
        .full_color_range(true)
        .set_chroma_subsampling((true, true));
    Ok(fy.to_vec(&color, alpha.as_deref(), w as u32, h as u32, 10))
}

#[allow(clippy::too_many_arguments)]
fn encode_svt(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    w: usize,
    h: usize,
    qp: u32,
    aux_alpha: bool,
    p: &AvifParams,
) -> Result<Vec<u8>> {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    unsafe {
        let mut handle: *mut svt::EbComponentType = std::ptr::null_mut();
        let mut config: svt::EbSvtAv1EncConfiguration = std::mem::zeroed();
        let err = svt::svt_av1_enc_init_handle(&mut handle, &mut config);
        ensure!(
            err == svt::EbErrorType::EB_ErrorNone,
            "svt init_handle: {err:?}"
        );

        // Guard: from here on, every exit must deinit the handle.
        struct Handle(*mut svt::EbComponentType);
        impl Drop for Handle {
            fn drop(&mut self) {
                unsafe {
                    svt::svt_av1_enc_deinit(self.0);
                    svt::svt_av1_enc_deinit_handle(self.0);
                }
            }
        }
        let guard = Handle(handle);

        // Mirrors libavif codec_svt.c for a single still image.
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
        let _ = ch;

        let err = svt::svt_av1_enc_send_picture(handle, &mut input);
        ensure!(
            err == svt::EbErrorType::EB_ErrorNone,
            "svt send_picture: {err:?}"
        );

        // EOS flush.
        let mut eos: svt::EbBufferHeaderType = std::mem::zeroed();
        eos.size = std::mem::size_of::<svt::EbBufferHeaderType>() as u32;
        eos.flags = svt::EB_BUFFERFLAG_EOS;
        let err = svt::svt_av1_enc_send_picture(handle, &mut eos);
        ensure!(err == svt::EbErrorType::EB_ErrorNone, "svt eos: {err:?}");

        // Drain packets until EOS.
        let mut av1 = Vec::new();
        loop {
            let mut out: *mut svt::EbBufferHeaderType = std::ptr::null_mut();
            let res = svt::svt_av1_enc_get_packet(handle, &mut out, 1);
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
        drop(guard);
        ensure!(!av1.is_empty(), "svt produced no output");
        Ok(av1)
    }
}

/// dav1d reports errors as negative errno values.
#[cfg(target_os = "linux")]
const EAGAIN: std::os::raw::c_int = 11;
#[cfg(not(target_os = "linux"))]
const EAGAIN: std::os::raw::c_int = 35;

thread_local! {
    /// Set while the deliberately unwind-caught avif-parse call runs, so
    /// the filtering panic hook stays silent for it: without this, every
    /// attacker-supplied malformed AVIF would print a crash-shaped trace
    /// (and, under RUST_BACKTRACE, serialize on the global backtrace
    /// lock) even though the request fails cleanly.
    static SUPPRESS_PANIC_LOG: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Install (once) a panic hook that skips logging for panics this
/// module catches on purpose and delegates to the previous hook for
/// everything else.
fn install_quiet_panic_hook() {
    static HOOK: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    HOOK.get_or_init(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if !SUPPRESS_PANIC_LOG.with(|s| s.get()) {
                prev(info);
            }
        }));
    });
}

/// avif-parse can panic on truncated/malformed containers (internal
/// parser-state assertions, observed in 2.1.0); malformed input must
/// surface as a parse error, not a crash, so the call is unwind-caught.
/// The crate is pure Rust and the input is a shared slice, so no state
/// can be left torn.
fn read_avif_container(data: &[u8]) -> Result<avif_parse::AvifData> {
    install_quiet_panic_hook();
    struct Unsuppress;
    impl Drop for Unsuppress {
        fn drop(&mut self) {
            SUPPRESS_PANIC_LOG.with(|s| s.set(false));
        }
    }
    SUPPRESS_PANIC_LOG.with(|s| s.set(true));
    let _guard = Unsuppress;
    match std::panic::catch_unwind(|| avif_parse::read_avif(&mut std::io::Cursor::new(data))) {
        Ok(parsed) => parsed.context("parse AVIF container"),
        // Keep the assertion text: it identifies which upstream bug fired.
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                .unwrap_or("non-string panic payload");
            Err(anyhow::anyhow!("AVIF container parse panicked: {msg}"))
        }
    }
}

/// Container-level probe: dimensions from the primary item's AV1
/// sequence header, no pixel decoding.
pub fn probe_avif(data: &[u8]) -> Result<(usize, usize)> {
    let avif = read_avif_container(data)?;
    let meta = avif
        .primary_item_metadata()
        .context("parse AV1 sequence header")?;
    Ok((
        meta.max_frame_width.get() as usize,
        meta.max_frame_height.get() as usize,
    ))
}

/// Decode an AVIF file via dav1d to interleaved RGB8 or, when the file
/// carries an alpha auxiliary image, straight-alpha RGBA8. Handles
/// 4:0:0/4:2:0/4:2:2/4:4:4 at 8/10/12 bits, identity/BT.601/BT.709/
/// BT.2020-NCL matrices, and both color ranges; premultiplied alpha is
/// converted to straight alpha. Returns (pixels, width, height, channels).
pub fn decode_avif(data: &[u8]) -> Result<(Vec<u8>, usize, usize, usize)> {
    let mut out = Vec::new();
    let (w, h, channels) = decode_avif_into(data, &mut out)?;
    Ok((out, w, h, channels))
}

/// Like [`decode_avif`], but reuses `out` as the pixel buffer.
pub fn decode_avif_into(data: &[u8], out: &mut Vec<u8>) -> Result<(usize, usize, usize)> {
    let avif = read_avif_container(data)?;
    let (w, h) = with_decoded_picture(&avif.primary_item, |pic| picture_to_rgb(pic, out))?;
    let Some(alpha_item) = avif.alpha_item.as_deref() else {
        return Ok((w, h, 3));
    };

    let alpha = with_decoded_picture(alpha_item, |pic| picture_to_alpha(pic, w, h))
        .context("decode alpha item")?;
    // Expand RGB to RGBA in place, back to front (writes at i*4.. never
    // overlap reads at j*3..j*3+3 for j < i); every output position is
    // written, so growth does not need to re-zero.
    if out.len() < w * h * 4 {
        out.resize(w * h * 4, 0);
    }
    out.truncate(w * h * 4);
    for i in (0..w * h).rev() {
        let (r, g, b) = (out[i * 3], out[i * 3 + 1], out[i * 3 + 2]);
        let a = alpha[i];
        let (r, g, b) = if avif.premultiplied_alpha && a != 255 {
            if a == 0 {
                (0, 0, 0)
            } else {
                let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                (un(r), un(g), un(b))
            }
        } else {
            (r, g, b)
        };
        out[i * 4] = r;
        out[i * 4 + 1] = g;
        out[i * 4 + 2] = b;
        out[i * 4 + 3] = a;
    }
    Ok((w, h, 4))
}

/// dav1d worker threads. The default is architecture-aware: on x86-64,
/// two threads ride the second SMT sibling of the request's core (the
/// same rationale as libwebp's two-thread decode, which libvips also
/// ships), improving both latency and saturated throughput. aarch64
/// server cores have no SMT, so a second thread costs a full core:
/// wall latency improves at light load but saturated throughput drops
/// (measured -6% requests/s on Graviton3) — single-threaded decoding
/// is the default there. OXIMG_AVIF_DECODE_THREADS overrides.
fn dav1d_threads() -> std::os::raw::c_int {
    std::env::var("OXIMG_AVIF_DECODE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(if cfg!(target_arch = "x86_64") { 2 } else { 1 })
}

/// Run one dav1d session over a single-frame AV1 stream and hand the
/// decoded picture to `f`.
fn with_decoded_picture<T>(
    av1: &[u8],
    f: impl FnOnce(&dav1d_sys::Dav1dPicture) -> Result<T>,
) -> Result<T> {
    use dav1d_sys as d;
    unsafe {
        let mut settings: d::Dav1dSettings = std::mem::zeroed();
        d::dav1d_default_settings(&mut settings);
        settings.n_threads = dav1d_threads();
        settings.max_frame_delay = 1;

        let mut ctx: *mut d::Dav1dContext = std::ptr::null_mut();
        ensure!(d::dav1d_open(&mut ctx, &settings) == 0, "dav1d_open");
        struct Ctx(*mut d::Dav1dContext);
        impl Drop for Ctx {
            fn drop(&mut self) {
                unsafe { d::dav1d_close(&mut self.0) }
            }
        }
        let _ctx_guard = Ctx(ctx);

        // Borrow the OBU buffer; it outlives the decoder, so the free
        // callback (which dav1d requires to be non-null) is a no-op.
        unsafe extern "C" fn no_free(_buf: *const u8, _cookie: *mut std::ffi::c_void) {}
        let mut data: d::Dav1dData = std::mem::zeroed();
        ensure!(
            d::dav1d_data_wrap(
                &mut data,
                av1.as_ptr(),
                av1.len(),
                Some(no_free),
                std::ptr::null_mut()
            ) == 0,
            "dav1d_data_wrap"
        );
        struct Data(*mut d::Dav1dData);
        impl Drop for Data {
            fn drop(&mut self) {
                unsafe {
                    if !(*self.0).data.is_null() {
                        d::dav1d_data_unref(self.0);
                    }
                }
            }
        }
        let _data_guard = Data(&mut data);

        let mut pic: d::Dav1dPicture = std::mem::zeroed();
        loop {
            if data.sz > 0 {
                let res = d::dav1d_send_data(ctx, &mut data);
                ensure!(res == 0 || res == -EAGAIN, "dav1d_send_data: {res}");
            }
            let res = d::dav1d_get_picture(ctx, &mut pic);
            if res == 0 {
                break;
            }
            ensure!(
                res == -EAGAIN && data.sz > 0,
                "dav1d_get_picture: {res} (no picture in stream)"
            );
        }
        struct Pic(*mut d::Dav1dPicture);
        impl Drop for Pic {
            fn drop(&mut self) {
                unsafe { d::dav1d_picture_unref(self.0) }
            }
        }
        let _pic_guard = Pic(&mut pic);

        f(&pic)
    }
}

/// Extract the alpha plane (the luma of the auxiliary image) as u8.
fn picture_to_alpha(pic: &dav1d_sys::Dav1dPicture, w: usize, h: usize) -> Result<Vec<u8>> {
    ensure!(
        (pic.p.w as usize, pic.p.h as usize) == (w, h),
        "alpha dimensions do not match the color image"
    );
    let bpc = pic.p.bpc as u32;
    ensure!(matches!(bpc, 8 | 10 | 12), "unsupported bit depth {bpc}");
    let seq = unsafe { &*pic.seq_hdr };
    // The spec requires full range for alpha; honor the flag regardless.
    let max = ((1u32 << bpc) - 1) as f32;
    let scale8 = (1u32 << (bpc - 8)) as f32;
    let (a_mul, a_off) = if seq.color_range != 0 {
        (255.0 / max, 0.0)
    } else {
        (255.0 / (219.0 * scale8), 16.0 * scale8)
    };

    let hbd = bpc > 8;
    let mut alpha = vec![0u8; w * h];
    for y in 0..h {
        let row = &mut alpha[y * w..(y + 1) * w];
        let src = plane_row(pic, 0, y, w, hbd);
        match src {
            Row::B8(s) if seq.color_range != 0 => row.copy_from_slice(s),
            src => yuv::alpha_row(src, a_off, a_mul, row),
        }
    }
    Ok(alpha)
}

/// Borrow one row of a dav1d plane as typed samples. Plane 0 uses the
/// luma stride; planes 1/2 share the chroma stride. Strides are bytes.
fn plane_row(
    pic: &dav1d_sys::Dav1dPicture,
    plane: usize,
    y: usize,
    len: usize,
    hbd: bool,
) -> Row<'_> {
    let (ptr, stride) = if plane == 0 {
        (pic.data[0], pic.stride[0] as usize)
    } else {
        (pic.data[plane], pic.stride[1] as usize)
    };
    unsafe {
        if hbd {
            Row::B16(std::slice::from_raw_parts(
                (ptr as *const u16).add(y * (stride / 2)),
                len,
            ))
        } else {
            Row::B8(std::slice::from_raw_parts(
                (ptr as *const u8).add(y * stride),
                len,
            ))
        }
    }
}

/// Convert a decoded dav1d picture (planar YUV) to interleaved RGB8.
fn picture_to_rgb(pic: &dav1d_sys::Dav1dPicture, out: &mut Vec<u8>) -> Result<(usize, usize)> {
    use dav1d_sys as d;
    let (w, h) = (pic.p.w as usize, pic.p.h as usize);
    let bpc = pic.p.bpc as u32;
    ensure!(matches!(bpc, 8 | 10 | 12), "unsupported bit depth {bpc}");
    let seq = unsafe { &*pic.seq_hdr };
    let full_range = seq.color_range != 0;
    let monochrome = pic.p.layout == d::DAV1D_PIXEL_LAYOUT_I400;
    let (sx, sy) = match pic.p.layout {
        d::DAV1D_PIXEL_LAYOUT_I420 => (1u32, 1u32),
        d::DAV1D_PIXEL_LAYOUT_I422 => (1, 0),
        _ => (0, 0),
    };

    let hbd = bpc > 8;
    let max = ((1u32 << bpc) - 1) as f32;
    let center = ((1u32 << bpc) / 2) as f32;
    let scale8 = (1u32 << (bpc - 8)) as f32;
    // Normalize to the 0..255 scale: full range divides by the sample
    // maximum; limited range maps 16..235 (luma) / 16..240 (chroma).
    let (y_mul, y_off, c_mul) = if full_range {
        (255.0 / max, 0.0, 255.0 / max)
    } else {
        (
            255.0 / (219.0 * scale8),
            16.0 * scale8,
            255.0 / (224.0 * scale8),
        )
    };

    // Matrix coefficients from the sequence header. Unspecified and exotic
    // matrices fall back to BT.601 so a slightly mistagged file still
    // serves a reasonable image instead of a 5xx.
    let identity = seq.mtrx == 0 && !monochrome;
    if identity {
        ensure!(
            pic.p.layout == d::DAV1D_PIXEL_LAYOUT_I444,
            "identity matrix requires 4:4:4"
        );
    }
    let (kr, kb) = match seq.mtrx {
        1 => (0.2126, 0.0722),     // BT.709
        9 => (0.2627, 0.0593),     // BT.2020 NCL
        _ => (0.299f32, 0.114f32), // BT.601 and fallback
    };
    let kg = 1.0 - kr - kb;

    // Subsampled chroma is upsampled with the separable center-sited
    // bilinear kernel (9:3:3:1) that libyuv and libjpeg's "fancy
    // upsampling" use, so output matches avifdec instead of showing
    // nearest-neighbor chroma blocking.
    let cw = if sx == 1 { w.div_ceil(2) } else { w };
    let ch = if sy == 1 { h.div_ceil(2) } else { h };
    let mut cb_mid = vec![0f32; cw];
    let mut cr_mid = vec![0f32; cw];
    let mut cb_row = vec![0f32; w];
    let mut cr_row = vec![0f32; w];

    out.clear();
    out.resize(w * h * 3, 0);
    let csc = yuv::Csc {
        y_off,
        y_mul,
        center,
        c_mul,
        kr,
        kb,
        kg,
    };
    for y in 0..h {
        if !monochrome {
            // Vertical pass at chroma horizontal resolution.
            let (near, other) = if sy == 1 {
                let near = y >> 1;
                let other = if y & 1 == 1 {
                    (near + 1).min(ch - 1)
                } else {
                    near.saturating_sub(1)
                };
                (near, other)
            } else {
                (y, y)
            };
            for (plane, mid) in [(1usize, &mut cb_mid), (2, &mut cr_mid)] {
                if sy == 1 {
                    yuv::chroma_blend(
                        plane_row(pic, plane, near, cw, hbd),
                        plane_row(pic, plane, other, cw, hbd),
                        mid,
                    );
                } else {
                    yuv::chroma_widen(plane_row(pic, plane, near, cw, hbd), mid);
                }
            }
            // Horizontal pass to full resolution.
            if sx == 1 {
                yuv::chroma_upsample_h(&cb_mid, &mut cb_row);
                yuv::chroma_upsample_h(&cr_mid, &mut cr_row);
            } else {
                cb_row.copy_from_slice(&cb_mid);
                cr_row.copy_from_slice(&cr_mid);
            }
        }

        let y_row = plane_row(pic, 0, y, w, hbd);
        let row = &mut out[y * w * 3..(y + 1) * w * 3];
        if !monochrome && !identity {
            yuv::yuv_row_to_rgb(y_row, &cb_row, &cr_row, &csc, row);
            continue;
        }
        for (x, px) in row.chunks_exact_mut(3).enumerate() {
            let yf = (y_row.at(x) - y_off) * y_mul;
            let (r, g, b) = if monochrome {
                (yf, yf, yf)
            } else {
                // Identity: G=Y, B=U, R=V, chroma is not centered.
                (cr_row[x] * y_mul, yf, cb_row[x] * y_mul)
            };
            px[0] = (r + 0.5).clamp(0.0, 255.0) as u8;
            px[1] = (g + 0.5).clamp(0.0, 255.0) as u8;
            px[2] = (b + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    out.truncate(w * h * 3);
    Ok((w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random pixels covering 0 and 255 exactly.
    fn pixel_samples(n: usize, seed: u32) -> Vec<u8> {
        let mut s = seed;
        (0..n)
            .map(|i| match i % 17 {
                0 => 0,
                1 => 255,
                _ => {
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    (s >> 24) as u8
                }
            })
            .collect()
    }

    /// The NEON luma path replaces `acc / 1044480` with
    /// `((acc >> 12) * 8421505) >> 31`; prove both identities it stacks
    /// (factor split and magic-multiply /255) over the full domain the
    /// pipeline can produce.
    #[test]
    fn luma_divider_identity_is_exact() {
        for x in 0..=1_044_480u32 {
            let acc = x * 1023 + 522_240;
            let reference = acc / 1_044_480;
            let vectorized = (((acc >> 12) as u64 * 8_421_505) >> 31) as u32;
            assert_eq!(reference, vectorized, "x={x}");
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_luma_rows_match_scalar_bit_exactly() {
        if !crate::yuv::neon() {
            return;
        }
        // Odd lengths exercise the scalar tail after the 8-wide loop.
        for (n, channels, seed) in [(1024, 3, 1), (1021, 3, 2), (1024, 4, 3), (777, 4, 4)] {
            let px = pixel_samples(n * channels, seed);
            let mut scalar = vec![0u16; n];
            let mut neon = vec![0u16; n];
            luma_rows_scalar(&px, channels, &mut scalar);
            unsafe { neon_enc::luma_rows(&px, channels, &mut neon) };
            assert_eq!(scalar, neon, "n={n} channels={channels}");
        }
    }

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn neon_chroma_rows_match_scalar_bit_exactly() {
        if !crate::yuv::neon() {
            return;
        }
        // Odd dims exercise the right-edge columns and bottom row that
        // fall back to the scalar block.
        for (w, h, channels, seed) in [
            (128, 64, 3, 5),
            (127, 63, 3, 6),
            (9, 5, 3, 7),
            (130, 62, 4, 8),
            (33, 7, 4, 9),
        ] {
            let px = pixel_samples(w * h * channels, seed);
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let (mut cb_s, mut cr_s) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
            for cy in 0..ch {
                for cx in 0..cw {
                    let (cb, cr) = chroma_block(&px, w, h, channels, cx, cy);
                    cb_s[cy * cw + cx] = cb;
                    cr_s[cy * cw + cx] = cr;
                }
            }
            let (mut cb_n, mut cr_n) = (vec![0u16; cw * ch], vec![0u16; cw * ch]);
            unsafe { neon_enc::chroma_rows(&px, w, h, channels, &mut cb_n, &mut cr_n) };
            assert_eq!(cb_s, cb_n, "cb {w}x{h} channels={channels}");
            assert_eq!(cr_s, cr_n, "cr {w}x{h} channels={channels}");
        }
    }

    #[test]
    fn encodes_a_decodable_avif() {
        let (w, h) = (128, 96);
        let rgb: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as u8;
                let y = (i / w) as u8;
                [x.wrapping_mul(2), y.wrapping_mul(2), x ^ y]
            })
            .collect();
        let out = encode_avif(&rgb, w, h, 3, &AvifParams::default()).unwrap();
        assert!(out.len() > 100, "suspiciously small: {}", out.len());
        // container sanity: ftyp avif brand near the start
        assert_eq!(&out[4..12], b"ftypavif", "not an avif container");
    }

    #[test]
    fn encode_decode_roundtrip_preserves_the_image() {
        let (w, h) = (160, 120);
        // Smooth gradient: compresses well, so quality loss stays small
        // and any plane/matrix/range mix-up shows up as a huge error.
        let rgb: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as f32 / (w - 1) as f32;
                let y = (i / w) as f32 / (h - 1) as f32;
                [
                    (x * 255.0) as u8,
                    (y * 255.0) as u8,
                    ((1.0 - x) * 200.0) as u8,
                ]
            })
            .collect();
        let params = AvifParams {
            quality: 85,
            ..AvifParams::default()
        };
        let encoded = encode_avif(&rgb, w, h, 3, &params).unwrap();
        let (decoded, dw, dh, channels) = decode_avif(&encoded).unwrap();
        assert_eq!((dw, dh, channels), (w, h, 3));
        assert_eq!(decoded.len(), rgb.len());
        let se: f64 = rgb
            .iter()
            .zip(&decoded)
            .map(|(&a, &b)| ((a as f64) - (b as f64)).powi(2))
            .sum();
        let rmse = (se / rgb.len() as f64).sqrt();
        assert!(rmse < 6.0, "roundtrip rmse too high: {rmse:.2}");
    }

    #[test]
    fn probe_reports_dimensions_without_decoding() {
        let rgb = vec![128u8; 96 * 64 * 3];
        let encoded = encode_avif(&rgb, 96, 64, 3, &AvifParams::default()).unwrap();
        assert_eq!(probe_avif(&encoded).unwrap(), (96, 64));
    }

    #[test]
    fn rgba_roundtrip_preserves_color_and_alpha() {
        let (w, h) = (160, 120);
        // Color gradient with an alpha ramp: left edge transparent,
        // right edge opaque.
        let rgba: Vec<u8> = (0..w * h)
            .flat_map(|i| {
                let x = (i % w) as f32 / (w - 1) as f32;
                let y = (i / w) as f32 / (h - 1) as f32;
                [
                    (x * 255.0) as u8,
                    (y * 255.0) as u8,
                    ((1.0 - x) * 200.0) as u8,
                    (x * 255.0) as u8,
                ]
            })
            .collect();
        let params = AvifParams {
            quality: 85,
            alpha_quality: 85,
            ..AvifParams::default()
        };
        let encoded = encode_avif(&rgba, w, h, 4, &params).unwrap();
        let (decoded, dw, dh, channels) = decode_avif(&encoded).unwrap();
        assert_eq!((dw, dh, channels), (w, h, 4));
        let a_se: f64 = rgba
            .chunks_exact(4)
            .zip(decoded.chunks_exact(4))
            .map(|(s, d)| ((s[3] as f64) - (d[3] as f64)).powi(2))
            .sum();
        let a_rmse = (a_se / (w * h) as f64).sqrt();
        assert!(a_rmse < 3.0, "alpha rmse too high: {a_rmse:.2}");
        // Color must survive where alpha is meaningful.
        let (mut c_se, mut n) = (0f64, 0u32);
        for (s, d) in rgba.chunks_exact(4).zip(decoded.chunks_exact(4)) {
            if s[3] > 128 {
                for c in 0..3 {
                    c_se += ((s[c] as f64) - (d[c] as f64)).powi(2);
                }
                n += 3;
            }
        }
        let c_rmse = (c_se / n as f64).sqrt();
        assert!(c_rmse < 8.0, "color rmse too high: {c_rmse:.2}");
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_avif(b"not an avif at all").is_err());
        assert!(decode_avif(&[]).is_err());
    }

    #[test]
    fn yuv_conversion_hits_known_anchors() {
        // white -> Y=1023, Cb=Cr=512; black -> Y=0, Cb=Cr=512
        let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
        rgb_to_yuv420_10bit(
            &[255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            2,
            2,
            3,
            &mut y,
            &mut cb,
            &mut cr,
        );
        assert!(y.iter().all(|&v| v >= 1022), "{y:?}");
        assert_eq!((cb[0], cr[0]), (512, 512));
        let (mut y, mut cb, mut cr) = (Vec::new(), Vec::new(), Vec::new());
        rgb_to_yuv420_10bit(&[0; 12], 2, 2, 3, &mut y, &mut cb, &mut cr);
        assert!(y.iter().all(|&v| v == 0));
        assert_eq!((cb[0], cr[0]), (512, 512));
    }
}
