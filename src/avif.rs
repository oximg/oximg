//! AVIF encoding via SVT-AV1 (sync-path settings validated in the encoder
//! study: preset 8, tune=3, 10-bit 4:2:0). The SVT session setup mirrors
//! libavif's codec_svt.c so measurements against `avifenc -c svt` transfer.

use crate::svt::bindings as svt;
use anyhow::{Result, ensure};

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
