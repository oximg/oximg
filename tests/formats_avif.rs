//! AVIF-specific format tests (decode, alpha, truncation, opaque-alpha
//! drop, and the no-feature clean error).

#![cfg(feature = "avif")]

mod common;

use common::{dims_of, fixture, params};
use oximg::pipeline::{self, ImageFormat, Params};


#[cfg(feature = "avif")]
#[test]
fn avif_resizes_in_avif() {
    let (out, fmt) = pipeline::process(&fixture("photo.avif"), &params(100)).unwrap();
    assert_eq!(fmt, ImageFormat::Avif);
    assert!(!out.is_empty());
    assert_eq!(dims_of(&out), (100, 75));
}


#[cfg(feature = "avif")]
#[test]
fn avif_alpha_survives_and_does_not_bleed() {
    // alpha.avif is avifenc-encoded from rgba.png: alpha 0 at the left
    // edge, ~255 at the right edge.
    let (out, fmt) = pipeline::process(&fixture("alpha.avif"), &params(50)).unwrap();
    assert_eq!(fmt, ImageFormat::Avif);
    let (rgba, w, h, channels) = oximg::avif::decode_avif(&out).unwrap();
    assert_eq!(channels, 4, "alpha must survive the round trip");
    let mid = h / 2;
    let px = |x: usize, y: usize| -> &[u8] { &rgba[(y * w + x) * 4..(y * w + x) * 4 + 4] };
    assert!(px(1, mid)[3] < 24, "left edge should stay transparent");
    assert!(px(w - 2, mid)[3] > 230, "right edge should stay opaque");
}


#[cfg(feature = "avif")]
#[test]
fn avif_garbage_and_truncation_error_instead_of_panicking() {
    let full = fixture("photo.avif");
    for cut in [16, 64, full.len() / 2] {
        assert!(
            pipeline::process(&full[..cut], &params(100)).is_err(),
            "avif truncated at {cut} should error"
        );
    }
}


/// Fully opaque RGBA must not pay for an AVIF alpha item: the output
/// drops to 3 channels and is byte-identical to encoding the same
/// pixels as plain RGB (the color path ignores the alpha byte).
#[cfg(feature = "avif")]
#[test]
fn opaque_rgba_avif_drops_the_alpha_item() {
    let (w, h) = (97, 61);
    let mut rgba = Vec::with_capacity(w * h * 4);
    let mut rgb = Vec::with_capacity(w * h * 3);
    for i in 0..w * h {
        let px = [(i % 251) as u8, (i % 241) as u8, (i % 239) as u8];
        rgb.extend_from_slice(&px);
        rgba.extend_from_slice(&px);
        rgba.push(255);
    }
    let params = oximg::avif::AvifParams {
        quality: 55,
        alpha_quality: 55,
        ..Default::default()
    };
    let from_rgba = oximg::avif::encode_avif(&rgba, w, h, 4, &params, None).unwrap();
    let from_rgb = oximg::avif::encode_avif(&rgb, w, h, 3, &params, None).unwrap();
    assert_eq!(from_rgba, from_rgb, "opaque RGBA must match the RGB encode");
    let (_, _, _, channels) = oximg::avif::decode_avif(&from_rgba).unwrap();
    assert_eq!(channels, 3, "no alpha item expected");

    // One transparent pixel is enough to keep the alpha item.
    rgba[3] = 254;
    let with_alpha = oximg::avif::encode_avif(&rgba, w, h, 4, &params, None).unwrap();
    let (_, _, _, channels) = oximg::avif::decode_avif(&with_alpha).unwrap();
    assert_eq!(channels, 4, "alpha item must survive");
}


#[cfg(feature = "avif")]
#[test]
fn avif_alpha_survives_cross_format_to_png() {
    let p = Params {
        output: Some(ImageFormat::Png),
        ..params(50)
    };
    let (out, fmt) = pipeline::process(&fixture("alpha.avif"), &p).unwrap();
    assert_eq!(fmt, ImageFormat::Png);
    let r = png::Decoder::new(std::io::Cursor::new(&out))
        .read_info()
        .unwrap();
    assert_eq!(r.info().color_type, png::ColorType::Rgba);
}


/// Animated AVIF sources render their first frame (the still primary
/// item MIAF requires them to carry), like other image proxies —
/// previously a clean rejection. Metadata composes: orientation and
/// ICC on an animated source apply to that first frame.
#[cfg(feature = "avif")]
#[test]
fn animated_avif_renders_first_frame() {
    // anim.avif: three 120x90 solid frames (red, blue, green), 2fps.
    let src = fixture("anim.avif");
    assert_eq!(
        pipeline::probe(&src).unwrap(),
        (ImageFormat::Avif, 120, 90),
        "probe falls back to the still item's ispe"
    );
    for target in [ImageFormat::Jpeg, ImageFormat::Webp, ImageFormat::Avif] {
        let p = Params {
            output: Some(target),
            ..params(100)
        };
        let (out, fmt) = pipeline::process(&src, &p).unwrap();
        assert_eq!(fmt, target);
        assert_eq!(dims_of(&out), (100, 75), "{target:?}");
    }
    // First frame is solid red: check via the JPEG target.
    let p = Params {
        output: Some(ImageFormat::Jpeg),
        ..params(100)
    };
    let (out, _) = pipeline::process(&src, &p).unwrap();
    let (w, h, corners) = common::corner_classes(&out);
    assert_eq!((w, h), (100, 75));
    assert_eq!(corners, ['R', 'R', 'R', 'R'], "first frame is the red one");

    // anim_meta.avif: same frames with --irot 1 and an ICC profile.
    let src = fixture("anim_meta.avif");
    let (out, _) = pipeline::process(
        &src,
        &Params {
            output: Some(ImageFormat::Jpeg),
            ..params(100)
        },
    )
    .unwrap();
    assert_eq!(
        dims_of(&out),
        (75, 100),
        "irot applies to the animated source's first frame"
    );
    assert_eq!(
        common::jpeg_icc(&out).as_deref(),
        Some(&common::fake_icc(900)[..]),
        "profile passes through from the animated source"
    );
}
