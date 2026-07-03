//! AVIF encoding via SVT-AV1 (sync-path settings validated in the encoder
//! study: preset 8, tune=3, 10-bit 4:2:0) and decoding via dav1d. The SVT
//! session setup mirrors libavif's codec_svt.c so measurements against
//! `avifenc -c svt` transfer.

use crate::svt::bindings as svt;
use anyhow::{Context, Result, ensure};

/// libavif's quality -> quantizer mapping (codec_svt.c).
fn quality_to_qp(quality: u8) -> u32 {
    ((100 - quality as u32) * 63 + 50) / 100
}

/// RGB8 -> 10-bit 4:2:0 YUV, BT.601 matrix, full range (matching the
/// avifenc defaults used in the encoder study). Chroma is averaged over
/// each 2x2 block.
fn rgb_to_yuv420_10bit(rgb: &[u8], w: usize, h: usize) -> (Vec<u16>, Vec<u16>, Vec<u16>) {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut y_plane = vec![0u16; w * h];
    let mut cb_plane = vec![0u16; cw * ch];
    let mut cr_plane = vec![0u16; cw * ch];

    // Luma: Y10 = (0.299 R + 0.587 G + 0.114 B) * 1023/255, fixed point.
    // Coefficients sum to 4096, so the pre-scale maximum is 4096*255 and the
    // rounding divide maps 255 -> exactly 1023 (never 1024: out-of-range
    // samples make SVT emit full-scale luma garbage in the affected blocks).
    for (i, px) in rgb.chunks_exact(3).enumerate() {
        let (r, g, b) = (px[0] as u32, px[1] as u32, px[2] as u32);
        y_plane[i] = (((1225 * r + 2404 * g + 467 * b) * 1023 + 522_240) / 1_044_480) as u16;
    }
    // Chroma from 2x2-averaged RGB.
    for cy in 0..ch {
        for cx in 0..cw {
            let (mut rs, mut gs, mut bs, mut n) = (0u32, 0u32, 0u32, 0u32);
            for dy in 0..2 {
                for dx in 0..2 {
                    let (x, yy) = (cx * 2 + dx, cy * 2 + dy);
                    if x < w && yy < h {
                        let p = (yy * w + x) * 3;
                        rs += rgb[p] as u32;
                        gs += rgb[p + 1] as u32;
                        bs += rgb[p + 2] as u32;
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
            cb_plane[cy * cw + cx] = (cb.round() as i32).clamp(0, 1023) as u16;
            cr_plane[cy * cw + cx] = (cr.round() as i32).clamp(0, 1023) as u16;
        }
    }
    (y_plane, cb_plane, cr_plane)
}

pub struct AvifParams {
    /// 0-100, libavif semantics.
    pub quality: u8,
    /// SVT preset (enc_mode); the sync-path setting is 8.
    pub speed: i8,
    /// SVT logical processors; 1 keeps the CPU-slot model honest.
    pub threads: u32,
}

impl Default for AvifParams {
    fn default() -> Self {
        AvifParams {
            quality: 60,
            speed: 8,
            threads: 1,
        }
    }
}

pub fn encode_avif(rgb: &[u8], w: usize, h: usize, p: &AvifParams) -> Result<Vec<u8>> {
    ensure!(rgb.len() >= w * h * 3, "pixel buffer too small");
    let (y_plane, cb_plane, cr_plane) = rgb_to_yuv420_10bit(rgb, w, h);
    let av1 = encode_svt(&y_plane, &cb_plane, &cr_plane, w, h, p)?;
    let mut fy = avif_serialize::Aviffy::new();
    fy.matrix_coefficients(avif_serialize::constants::MatrixCoefficients::Bt601)
        .full_color_range(true)
        .set_chroma_subsampling((true, true));
    Ok(fy.to_vec(&av1, None, w as u32, h as u32, 10))
}

fn encode_svt(
    y_plane: &[u16],
    cb_plane: &[u16],
    cr_plane: &[u16],
    w: usize,
    h: usize,
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
        config.color_primaries = 1; // BT.709
        config.transfer_characteristics = 13; // sRGB
        config.matrix_coefficients = 6; // BT.601
        config.color_range = 1; // full
        config.source_width = w as u32;
        config.source_height = h as u32;
        config.level_of_parallelism = p.threads;
        config.aq_mode = 2;
        config.rate_control_mode = 0;
        config.min_qp_allowed = 0;
        config.max_qp_allowed = 63;
        config.qp = quality_to_qp(p.quality);
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

/// Container-level probe: dimensions from the primary item's AV1
/// sequence header, no pixel decoding.
pub fn probe_avif(data: &[u8]) -> Result<(usize, usize)> {
    let avif =
        avif_parse::read_avif(&mut std::io::Cursor::new(data)).context("parse AVIF container")?;
    let meta = avif
        .primary_item_metadata()
        .context("parse AV1 sequence header")?;
    Ok((
        meta.max_frame_width.get() as usize,
        meta.max_frame_height.get() as usize,
    ))
}

/// Decode an AVIF file to RGB8 via dav1d. Handles 4:0:0/4:2:0/4:2:2/4:4:4
/// at 8/10/12 bits, identity/BT.601/BT.709/BT.2020-NCL matrices, and both
/// color ranges. Alpha AVIFs are rejected until the encode side can carry
/// the alpha item through.
pub fn decode_avif(data: &[u8]) -> Result<(Vec<u8>, usize, usize)> {
    let avif =
        avif_parse::read_avif(&mut std::io::Cursor::new(data)).context("parse AVIF container")?;
    ensure!(avif.alpha_item.is_none(), "AVIF with alpha is unsupported");
    decode_av1_to_rgb(&avif.primary_item)
}

fn decode_av1_to_rgb(av1: &[u8]) -> Result<(Vec<u8>, usize, usize)> {
    use dav1d_sys as d;
    unsafe {
        let mut settings: d::Dav1dSettings = std::mem::zeroed();
        d::dav1d_default_settings(&mut settings);
        // One request = one CPU slot, same as every other decoder here.
        settings.n_threads = 1;
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

        picture_to_rgb(&pic)
    }
}

/// Convert a decoded dav1d picture (planar YUV) to interleaved RGB8.
fn picture_to_rgb(pic: &dav1d_sys::Dav1dPicture) -> Result<(Vec<u8>, usize, usize)> {
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

    // Plane sampler: strides are in bytes; samples are u8 or little-endian
    // u16 depending on bit depth.
    let hbd = bpc > 8;
    let sample = |plane: usize, x: usize, y: usize| -> f32 {
        let (ptr, stride) = if plane == 0 {
            (pic.data[0], pic.stride[0])
        } else {
            (pic.data[plane], pic.stride[1])
        };
        unsafe {
            if hbd {
                *(ptr as *const u16).add(y * (stride as usize / 2) + x) as f32
            } else {
                *(ptr as *const u8).add(y * stride as usize + x) as f32
            }
        }
    };

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

    let mut rgb = vec![0u8; w * h * 3];
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
            for cx in 0..cw {
                if sy == 1 {
                    cb_mid[cx] = (3.0 * sample(1, cx, near) + sample(1, cx, other)) * 0.25;
                    cr_mid[cx] = (3.0 * sample(2, cx, near) + sample(2, cx, other)) * 0.25;
                } else {
                    cb_mid[cx] = sample(1, cx, near);
                    cr_mid[cx] = sample(2, cx, near);
                }
            }
            // Horizontal pass to full resolution.
            if sx == 1 {
                for x in 0..w {
                    let cx = x >> 1;
                    let other = if x & 1 == 1 {
                        (cx + 1).min(cw - 1)
                    } else {
                        cx.saturating_sub(1)
                    };
                    cb_row[x] = (3.0 * cb_mid[cx] + cb_mid[other]) * 0.25;
                    cr_row[x] = (3.0 * cr_mid[cx] + cr_mid[other]) * 0.25;
                }
            } else {
                cb_row.copy_from_slice(&cb_mid);
                cr_row.copy_from_slice(&cr_mid);
            }
        }

        let row = &mut rgb[y * w * 3..(y + 1) * w * 3];
        for (x, px) in row.chunks_exact_mut(3).enumerate() {
            let yf = (sample(0, x, y) - y_off) * y_mul;
            let (r, g, b) = if monochrome {
                (yf, yf, yf)
            } else if identity {
                // Identity: G=Y, B=U, R=V, chroma is not centered.
                (cr_row[x] * y_mul, yf, cb_row[x] * y_mul)
            } else {
                let cb = (cb_row[x] - center) * c_mul;
                let cr = (cr_row[x] - center) * c_mul;
                let r = yf + 2.0 * (1.0 - kr) * cr;
                let b = yf + 2.0 * (1.0 - kb) * cb;
                let g = (yf - kr * r - kb * b) / kg;
                (r, g, b)
            };
            px[0] = (r + 0.5).clamp(0.0, 255.0) as u8;
            px[1] = (g + 0.5).clamp(0.0, 255.0) as u8;
            px[2] = (b + 0.5).clamp(0.0, 255.0) as u8;
        }
    }
    Ok((rgb, w, h))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let out = encode_avif(&rgb, w, h, &AvifParams::default()).unwrap();
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
        let encoded = encode_avif(&rgb, w, h, &params).unwrap();
        let (decoded, dw, dh) = decode_avif(&encoded).unwrap();
        assert_eq!((dw, dh), (w, h));
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
        let encoded = encode_avif(&rgb, 96, 64, &AvifParams::default()).unwrap();
        assert_eq!(probe_avif(&encoded).unwrap(), (96, 64));
    }

    #[test]
    fn decode_rejects_garbage() {
        assert!(decode_avif(b"not an avif at all").is_err());
        assert!(decode_avif(&[]).is_err());
    }

    #[test]
    fn yuv_conversion_hits_known_anchors() {
        // white -> Y=1023, Cb=Cr=512; black -> Y=0, Cb=Cr=512
        let (y, cb, cr) = rgb_to_yuv420_10bit(
            &[255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255],
            2,
            2,
        );
        assert!(y.iter().all(|&v| v >= 1022), "{y:?}");
        assert_eq!((cb[0], cr[0]), (512, 512));
        let (y, cb, cr) = rgb_to_yuv420_10bit(&[0; 12], 2, 2);
        assert!(y.iter().all(|&v| v == 0));
        assert_eq!((cb[0], cr[0]), (512, 512));
    }
}
