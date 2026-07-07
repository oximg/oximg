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
    run_jpeg_icc(jpeg, fuse_quality, None)
}

fn run_jpeg_icc(jpeg: &[u8], fuse_quality: Option<f32>, icc: Option<&[u8]>) -> Vec<u8> {
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
        icc,
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
            encode_with_icc(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, &p, icc).unwrap()
        }
        #[cfg(feature = "avif")]
        Decoded::YuvPlanes { .. } => panic!("yuv fuse was not requested"),
        #[cfg(feature = "avif")]
        Decoded::PixelsSession { .. } => panic!("preheat fuse was not requested"),
    }
}

/// The fused jpegli encoder writes the ICC chain ahead of its
/// scanlines exactly like the serial encoder does — profiled
/// same-format JPEG keeps its bytes fuse-independent.
#[test]
fn fused_jpegli_bytes_match_serial_with_icc() {
    if !fuse_kernel_available() {
        return;
    }
    let jpeg = make_test_jpeg(799, 601, false);
    let icc: Vec<u8> = (0..70_000u32).map(|i| (i % 251) as u8).collect();
    let serial = run_jpeg_icc(&jpeg, None, Some(&icc));
    let fused = run_jpeg_icc(&jpeg, Some(80.0), Some(&icc));
    assert_eq!(serial, fused, "profiled fused/serial parity");
    // 70KB spans two APP2 chunks; both must survive intact.
    let mut chunks: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut i = 2;
    while i + 4 <= fused.len() && fused[i] == 0xFF {
        let m = fused[i + 1];
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([fused[i + 2], fused[i + 3]]) as usize;
        let body = &fused[i + 4..i + 2 + len];
        if m == 0xE2 && body.starts_with(b"ICC_PROFILE\0") {
            chunks.push((body[12], body[14..].to_vec()));
        }
        i += 2 + len;
    }
    chunks.sort_by_key(|(seq, _)| *seq);
    let got: Vec<u8> = chunks.into_iter().flat_map(|(_, d)| d).collect();
    assert_eq!(got, icc, "profile reassembles from the fused output");
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
        None,
    )
    .unwrap()
    {
        Decoded::Pixels { dst_w, dst_h } => s.out8[..dst_w * dst_h * 3].to_vec(),
        Decoded::Encoded(_) => panic!("pixel run must not encode"),
        #[cfg(feature = "avif")]
        Decoded::YuvPlanes { .. } => panic!("yuv fuse was not requested"),
        #[cfg(feature = "avif")]
        Decoded::PixelsSession { .. } => panic!("preheat fuse was not requested"),
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
        None,
    )
    .unwrap()
    {
        Decoded::Pixels { dst_w, dst_h } => {
            crate::avif::encode_avif(&s.out8[..dst_w * dst_h * 3], dst_w, dst_h, 3, &params, icc)
                .unwrap()
        }
        Decoded::YuvPlanes { session } => {
            crate::avif::encode_avif_with_session(session, &s.y16, &s.cb16, &s.cr16, icc).unwrap()
        }
        Decoded::PixelsSession { .. } => panic!("preheat fuse was not requested"),
        Decoded::Encoded(_) => panic!("avif run must not hit the jpegli fuse"),
    }
}

/// Oriented AVIF targets preheat their session on the fused
/// worker; the bytes must match the fully serial path (rotate on
/// out8, then encode_avif) exactly.
#[cfg(feature = "avif")]
#[test]
fn preheated_session_bytes_match_serial_oriented_avif() {
    if !fuse_kernel_available() {
        return;
    }
    let params = test_avif_params();
    let orientation = crate::meta::Orientation::from_rot_mirror(1, None); // 90° CCW
    let jpeg = make_test_jpeg(799, 601, false);
    let run = |fuse: Fuse| -> Vec<u8> {
        let mut s = Scratch::default();
        let dec = Decompress::new_mem(&jpeg).unwrap();
        match decode_resize(&mut s, dec, 320, 320, 1, orientation, fuse, None).unwrap() {
            Decoded::PixelsSession {
                dst_w,
                dst_h,
                session,
            } => {
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
                    None,
                )
                .unwrap()
            }
            Decoded::Pixels { dst_w, dst_h } => {
                let dims = crate::meta::apply_orientation(
                    &s.out8[..dst_w * dst_h * 3],
                    dst_w,
                    dst_h,
                    3,
                    orientation,
                    &mut s.chunk8,
                );
                std::mem::swap(&mut s.out8, &mut s.chunk8);
                crate::avif::encode_avif(
                    &s.out8[..dims.0 * dims.1 * 3],
                    dims.0,
                    dims.1,
                    3,
                    &params,
                    None,
                )
                .unwrap()
            }
            _ => panic!("unexpected decode result"),
        }
    };
    let serial = run(Fuse::Off);
    let preheated = run(Fuse::PixelsPreheat { params });
    assert_eq!(serial, preheated, "preheat must not change a byte");
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
            None,
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
            None,
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
            None,
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

/// Deterministic 4-component JPEG source: plain CMYK (Adobe APP14
/// transform 0) or YCCK (transform 2). The scanlines are written in
/// libjpeg's stored convention, which is Adobe-inverted (0 = full
/// ink, 255 = no ink).
fn make_cmyk_jpeg(w: usize, h: usize, ycck: bool) -> Vec<u8> {
    let mut px = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            // Smooth gradients survive DCT quantization better than
            // noise, keeping pixel-level assertions meaningful.
            px.push((x * 255 / w.max(1)) as u8);
            px.push((y * 255 / h.max(1)) as u8);
            px.push(((x + y) * 255 / (w + h)) as u8);
            px.push(255 - (y * 128 / h.max(1)) as u8);
        }
    }
    let mut comp = Compress::new(ColorSpace::JCS_CMYK);
    if ycck {
        comp.set_color_space(ColorSpace::JCS_YCCK);
    }
    comp.set_size(w, h);
    comp.set_quality(95.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_scanlines(&px).unwrap();
    started.finish().unwrap()
}

/// CMYK and YCCK sources decode to RGB via the naive composite
/// (`r = c'·k'/255` on the stored Adobe-inverted samples). The
/// reference applies the same formula to the raw CMYK planes decoded
/// by libjpeg itself, pinning oximg's plumbing (YCCK normalization,
/// in-place 4→3 compaction, the exact-size resize join)
/// byte-for-byte; independent third-party ground truth lives in
/// tests/formats_cmyk.rs against committed djpeg references.
#[test]
fn cmyk_and_ycck_sources_decode_to_naive_rgb() {
    for ycck in [false, true] {
        let jpeg = make_cmyk_jpeg(64, 48, ycck);
        let (rgb, w, h) = decode_and_resize(&jpeg, 64, 48, 1).unwrap();
        assert_eq!((w, h), (64, 48), "ycck={ycck}");
        let dec = Decompress::new_mem(&jpeg).unwrap();
        assert_eq!(
            dec.color_space(),
            if ycck {
                ColorSpace::JCS_YCCK
            } else {
                ColorSpace::JCS_CMYK
            }
        );
        let mut started = dec.to_colorspace(ColorSpace::JCS_CMYK).unwrap();
        let planes: Vec<u8> = started.read_scanlines().unwrap();
        started.finish().unwrap();
        let want: Vec<u8> = planes
            .chunks_exact(4)
            .flat_map(|px| {
                let k = px[3] as u32;
                [0, 1, 2].map(|c| ((px[c] as u32 * k + 127) / 255) as u8)
            })
            .collect();
        assert_eq!(rgb, want, "ycck={ycck}");
    }
}

/// The resized CMYK path survives both the single-thread linear
/// kernel and the band-parallel arm (`Decoded::Pixels` join).
#[test]
fn cmyk_resize_smoke_across_paths() {
    let jpeg = make_cmyk_jpeg(97, 61, true);
    for parallel in [1, 2] {
        let (rgb, w, h) = decode_and_resize(&jpeg, 48, 48, parallel).unwrap();
        assert!(w <= 48 && h <= 48 && w > 0 && h > 0, "parallel={parallel}");
        assert_eq!(rgb.len(), w * h * 3, "parallel={parallel}");
    }
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
