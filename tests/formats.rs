//! Core format matrix: same-format and cross-format resize/transcode
//! over the committed fixtures. Orientation, ICC, and AVIF-specific
//! cases live in the sibling `formats_*` test files.

mod common;

use common::{dims_of, fixture, params};
use oximg::pipeline::{self, Encoder, ImageFormat, Params};

#[test]
fn every_source_shape_resizes_in_its_own_format() {
    for (name, format) in [
        ("rgb.png", ImageFormat::Png),
        ("rgba.png", ImageFormat::Png),
        ("gray.png", ImageFormat::Png),
        ("graya.png", ImageFormat::Png),
        ("palette.png", ImageFormat::Png),
        ("gray16.png", ImageFormat::Png),
        ("rgb16.png", ImageFormat::Png),
        ("interlaced.png", ImageFormat::Png),
        ("photo.jpg", ImageFormat::Jpeg),
        ("photo.webp", ImageFormat::Webp),
        ("alpha.webp", ImageFormat::Webp),
    ] {
        let (out, fmt) = pipeline::process(&fixture(name), &params(100))
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert_eq!(fmt, format, "{name}");
        assert!(!out.is_empty(), "{name}: empty output");
        if format == ImageFormat::Jpeg {
            assert_eq!(dims_of(&out), (100, 75), "{name}");
        }
    }
}

#[test]
fn png_and_webp_outputs_have_expected_dims() {
    for name in ["rgb.png", "rgba.png", "photo.webp", "alpha.webp"] {
        let (out, _) = pipeline::process(&fixture(name), &params(100)).unwrap();
        assert_eq!(dims_of(&out), (100, 75), "{name}");
    }
}

#[test]
fn never_upscales() {
    let (out, _) = pipeline::process(&fixture("tiny.jpg"), &params(500)).unwrap();
    assert_eq!(dims_of(&out), (40, 30));
}

#[test]
fn alpha_survives_and_does_not_bleed() {
    // rgba.png has alpha 0 at the left edge and ~255 at the right edge.
    let (out, _) = pipeline::process(&fixture("rgba.png"), &params(50)).unwrap();
    let mut r = png::Decoder::new(std::io::Cursor::new(&out))
        .read_info()
        .unwrap();
    let info = r.info();
    assert_eq!(info.color_type, png::ColorType::Rgba);
    let (w, h) = (info.width as usize, info.height as usize);
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        buf[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };
    let mid = h / 2;
    assert!(px(1, mid)[3] < 24, "left edge should stay transparent");
    assert!(px(w - 2, mid)[3] > 230, "right edge should stay opaque");
}

/// Animated WebP sources render their first frame (full-canvas first
/// frames only — a partial one would need compositing and still gets
/// the clean animation error).
#[test]
fn animated_webp_renders_first_frame() {
    // animated.webp: two full-canvas 64x48 frames.
    let (out, fmt) = pipeline::process(&fixture("animated.webp"), &params(100)).unwrap();
    assert_eq!(fmt, ImageFormat::Webp);
    assert_eq!(dims_of(&out), (64, 48), "no upscale past the source");
    let p = Params {
        output: Some(ImageFormat::Jpeg),
        ..params(32)
    };
    let (out, fmt) = pipeline::process(&fixture("animated.webp"), &p).unwrap();
    assert_eq!(fmt, ImageFormat::Jpeg);
    assert_eq!(dims_of(&out), (32, 24), "cross-format first frame");
}

#[test]
fn garbage_and_truncation_error_instead_of_panicking() {
    assert!(pipeline::process(b"GIF89a not supported here", &params(100)).is_err());
    assert!(pipeline::process(b"", &params(100)).is_err());
    for name in ["photo.jpg", "rgb.png", "photo.webp"] {
        let full = fixture(name);
        // Header-level truncation must always error.
        for cut in [16, 64] {
            assert!(
                pipeline::process(&full[..cut], &params(100)).is_err(),
                "{name} truncated at {cut} should error"
            );
        }
        // Mid-stream truncation: PNG and WebP error; JPEG follows the
        // libjpeg-family convention of completing with filler blocks (the
        // behavior browsers and other proxies share), so it must at least
        // not panic and still produce a decodable image.
        let half = pipeline::process(&full[..full.len() / 2], &params(100));
        if name == "photo.jpg" {
            let (out, _) = half.expect("truncated jpeg should degrade gracefully");
            assert_eq!(dims_of(&out), (100, 75));
        } else {
            assert!(half.is_err(), "{name} truncated at half should error");
        }
    }
}

/// All 200x150 fixtures and their source formats.
fn all_fixtures() -> Vec<(&'static str, ImageFormat)> {
    let mut v = vec![
        ("rgb.png", ImageFormat::Png),
        ("rgba.png", ImageFormat::Png),
        ("gray.png", ImageFormat::Png),
        ("graya.png", ImageFormat::Png),
        ("palette.png", ImageFormat::Png),
        ("gray16.png", ImageFormat::Png),
        ("rgb16.png", ImageFormat::Png),
        ("interlaced.png", ImageFormat::Png),
        ("photo.jpg", ImageFormat::Jpeg),
        ("photo.webp", ImageFormat::Webp),
        ("alpha.webp", ImageFormat::Webp),
    ];
    if cfg!(feature = "avif") {
        v.push(("photo.avif", ImageFormat::Avif));
        v.push(("alpha.avif", ImageFormat::Avif));
    }
    v
}

/// The refactor pin: requesting the source's own format must reproduce
/// the default (sniff-and-match) output byte for byte — including
/// through the fused JPEG gate, whose condition gained a target check.
#[test]
fn explicit_same_format_is_byte_identical() {
    for (name, format) in all_fixtures() {
        let src = fixture(name);
        let implicit = pipeline::process(&src, &params(100)).unwrap();
        let explicit = pipeline::process(
            &src,
            &Params {
                output: Some(format),
                ..params(100)
            },
        )
        .unwrap();
        assert_eq!(implicit.1, explicit.1, "{name}");
        assert_eq!(implicit.0, explicit.0, "{name}: bytes must be identical");
    }
}

#[test]
fn cross_format_matrix() {
    let mut targets = vec![ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::Webp];
    if cfg!(feature = "avif") {
        targets.push(ImageFormat::Avif);
    }
    for (name, _) in all_fixtures() {
        for &target in &targets {
            let p = Params {
                output: Some(target),
                ..params(100)
            };
            let (out, fmt) = pipeline::process(&fixture(name), &p)
                .unwrap_or_else(|e| panic!("{name}->{target:?}: {e:#}"));
            assert_eq!(fmt, target, "{name}->{target:?}");
            let (sniffed, w, h) = pipeline::probe(&out).unwrap();
            assert_eq!(sniffed, target, "{name}->{target:?}: output magic bytes");
            assert_eq!((w, h), (100, 75), "{name}->{target:?}");
        }
    }
}

/// rgba.png is transparent at the left edge; flattening onto the
/// default white background must leave that region near-white in the
/// JPEG (which has no alpha channel to hide behind).
#[test]
fn rgba_to_jpeg_flattens_onto_white() {
    let p = Params {
        output: Some(ImageFormat::Jpeg),
        ..params(50)
    };
    let (out, fmt) = pipeline::process(&fixture("rgba.png"), &p).unwrap();
    assert_eq!(fmt, ImageFormat::Jpeg);
    let mut dec = mozjpeg::Decompress::new_mem(&out).unwrap().rgb().unwrap();
    let (w, h) = (dec.width(), dec.height());
    let px: Vec<[u8; 3]> = dec.read_scanlines().unwrap();
    let mid = px[(h / 2) * w + 1];
    for c in mid {
        assert!(
            c > 230,
            "transparent edge should flatten to white, got {mid:?}"
        );
    }
}

/// Cross-format must route alpha through targets that support it:
/// WebP-with-alpha -> PNG keeps the transparent/opaque edges intact.
#[test]
fn cross_format_alpha_survives_webp_to_png() {
    let p = Params {
        output: Some(ImageFormat::Png),
        ..params(50)
    };
    let (out, fmt) = pipeline::process(&fixture("alpha.webp"), &p).unwrap();
    assert_eq!(fmt, ImageFormat::Png);
    let mut r = png::Decoder::new(std::io::Cursor::new(&out))
        .read_info()
        .unwrap();
    assert_eq!(r.info().color_type, png::ColorType::Rgba);
    let (w, h) = (r.info().width as usize, r.info().height as usize);
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    let alpha = |x: usize| buf[((h / 2) * w + x) * 4 + 3];
    assert!(alpha(1) < 24, "left edge should stay transparent");
    assert!(alpha(w - 2) > 230, "right edge should stay opaque");
}

#[test]
fn rgba_to_webp_keeps_alpha_channel() {
    let p = Params {
        output: Some(ImageFormat::Webp),
        ..params(50)
    };
    let (out, fmt) = pipeline::process(&fixture("rgba.png"), &p).unwrap();
    assert_eq!(fmt, ImageFormat::Webp);
    unsafe {
        let mut features: libwebp_sys::WebPBitstreamFeatures = std::mem::zeroed();
        let status = libwebp_sys::WebPGetFeatures(out.as_ptr(), out.len(), &mut features);
        assert_eq!(status, libwebp_sys::VP8StatusCode::VP8_STATUS_OK);
        assert_eq!(features.has_alpha, 1, "alpha must survive PNG->WebP");
    }
}

#[test]
fn cross_format_never_upscales() {
    let p = Params {
        output: Some(ImageFormat::Webp),
        ..params(500)
    };
    let (out, _) = pipeline::process(&fixture("tiny.jpg"), &p).unwrap();
    assert_eq!(dims_of(&out), (40, 30));
}

/// Mid-stream JPEG truncation degrades gracefully on the same-format
/// path (libjpeg fills the tail); the cross-format dispatch must keep
/// that behavior instead of turning it into an error or panic.
#[test]
fn truncated_jpeg_to_webp_degrades_gracefully() {
    let full = fixture("photo.jpg");
    let p = Params {
        output: Some(ImageFormat::Webp),
        ..params(100)
    };
    let (out, fmt) = pipeline::process(&full[..full.len() / 2], &p)
        .expect("truncated jpeg should degrade gracefully");
    assert_eq!(fmt, ImageFormat::Webp);
    assert_eq!(dims_of(&out), (100, 75));
}

#[cfg(not(feature = "avif"))]
#[test]
fn avif_output_errors_cleanly_without_the_feature() {
    let p = Params {
        output: Some(ImageFormat::Avif),
        ..params(100)
    };
    let err = pipeline::process(&fixture("rgb.png"), &p).unwrap_err();
    assert!(format!("{err:#}").contains("not enabled"), "got: {err:#}");
}

#[test]
fn jpeg_presets_all_produce_decodable_output() {
    for encoder in [Encoder::Jpegli, Encoder::MozFast, Encoder::MozSmall] {
        let p = Params {
            encoder,
            ..params(100)
        };
        let (out, fmt) = pipeline::process(&fixture("photo.jpg"), &p).unwrap();
        assert_eq!(fmt, ImageFormat::Jpeg);
        assert_eq!(dims_of(&out), (100, 75), "{encoder:?}");
    }
}
