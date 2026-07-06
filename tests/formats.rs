//! Format-matrix integration tests over `pipeline::process`, driven by
//! committed fixtures (tests/fixtures/, all 200x150 unless noted).

mod common;

use oximg::pipeline::{self, Encoder, ImageFormat, Params};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn params(max: u32) -> Params {
    Params {
        max_width: max,
        max_height: max,
        quality: 80.0,
        encoder: Encoder::Jpegli,
        parallel: 1,
        output: None,
    }
}

/// Dims of any output, via the pipeline's own header probe.
fn dims_of(bytes: &[u8]) -> (usize, usize) {
    let (_, w, h) = pipeline::probe(bytes).unwrap();
    (w, h)
}

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

/// Every EXIF orientation must display upright: sources are built by
/// applying the *inverse* transform (an implementation independent of
/// src/meta.rs) to a frame with four distinct corner colors, tagging
/// them, and asserting the pipeline output shows the corners where the
/// original had them — at the orientation-corrected dimensions.
#[test]
fn every_exif_orientation_displays_upright() {
    let (w, h, block) = (240usize, 180usize, 60usize);
    let display = common::corner_base(w, h, block);
    for o in 1..=8u8 {
        let (stored, sw, sh) = common::store_for_orientation(&display, w, h, o);
        let jpeg = common::jpeg_with_orientation(&stored, sw, sh, Some(o as u16));
        let (out, fmt) = pipeline::process(&jpeg, &params(120)).unwrap();
        assert_eq!(fmt, ImageFormat::Jpeg, "o={o}");
        let (ow, oh, corners) = common::corner_classes(&out);
        assert_eq!((ow, oh), (120, 90), "o={o}: output must be display-fit");
        assert_eq!(corners, ['R', 'G', 'B', 'W'], "o={o}");
    }
}

/// An orientation-1 tag and no tag at all must produce identical
/// output bytes (the marker changes nothing but the source file).
#[test]
fn orientation_one_matches_untagged_bytes() {
    let display = common::corner_base(240, 180, 60);
    let tagged = common::jpeg_with_orientation(&display, 240, 180, Some(1));
    let untagged = common::jpeg_with_orientation(&display, 240, 180, None);
    let (a, _) = pipeline::process(&tagged, &params(120)).unwrap();
    let (b, _) = pipeline::process(&untagged, &params(120)).unwrap();
    assert_eq!(a, b);
}

/// Orientation applies before the cross-format encode, so a rotated
/// source converts with corrected dimensions in any target format.
#[test]
fn orientation_applies_to_cross_format_targets() {
    let display = common::corner_base(240, 180, 60);
    let (stored, sw, sh) = common::store_for_orientation(&display, 240, 180, 6);
    let jpeg = common::jpeg_with_orientation(&stored, sw, sh, Some(6));
    let p = Params {
        output: Some(ImageFormat::Webp),
        ..params(120)
    };
    let (out, fmt) = pipeline::process(&jpeg, &p).unwrap();
    assert_eq!(fmt, ImageFormat::Webp);
    assert_eq!(dims_of(&out), (120, 90));
}

/// The *first* Exif APP1 decides, matching Chrome and Firefox: a
/// non-Exif APP1 (XMP) before it must not mask it, and an
/// orientation-less first Exif pins the image upright even when a
/// later Exif segment carries a rotation.
#[test]
fn first_exif_app1_wins_across_multiple_app1_segments() {
    let display = common::corner_base(240, 180, 60);
    let (stored, sw, sh) = common::store_for_orientation(&display, 240, 180, 6);
    let xmp = b"http://ns.adobe.com/xap/1.0/\0<x/>".to_vec();
    let exif6 = common::app1_orientation(6);

    let jpeg = common::jpeg_with_app1s(&stored, sw, sh, &[&xmp, &exif6]);
    let (out, _) = pipeline::process(&jpeg, &params(120)).unwrap();
    let (ow, oh, corners) = common::corner_classes(&out);
    assert_eq!((ow, oh), (120, 90), "XMP before Exif must not mask it");
    assert_eq!(corners, ['R', 'G', 'B', 'W']);

    let no_tag = common::app1_exif_no_orientation();
    let jpeg = common::jpeg_with_app1s(&stored, sw, sh, &[&no_tag, &exif6]);
    let (out, _) = pipeline::process(&jpeg, &params(120)).unwrap();
    let (ow, oh, _) = common::corner_classes(&out);
    assert_eq!(
        (ow, oh),
        (90, 120),
        "orientation-less first Exif wins: stored (portrait) frame served as-is"
    );
}

/// The library decode API (qcli's path) honors rotation exactly like
/// the server: display-fit dims and upright pixels.
#[test]
fn decode_and_resize_honors_orientation() {
    let display = common::corner_base(240, 180, 60);
    let (stored, sw, sh) = common::store_for_orientation(&display, 240, 180, 6);
    let jpeg = common::jpeg_with_orientation(&stored, sw, sh, Some(6));
    let (px, w, h) = pipeline::decode_and_resize(&jpeg, 120, 120, 1).unwrap();
    assert_eq!((w, h), (120, 90), "box must fit the displayed frame");
    let at = |x: usize, y: usize| common::classify(&px[(y * w + x) * 3..]);
    let (ix, iy) = (w / 8, h / 8);
    assert_eq!(
        [
            at(ix, iy),
            at(w - 1 - ix, iy),
            at(ix, h - 1 - iy),
            at(w - 1 - ix, h - 1 - iy)
        ],
        ['R', 'G', 'B', 'W']
    );
}

/// A JPEG source's ICC profile survives into every profile-capable
/// target, byte for byte — including a profile large enough to span
/// multiple APP2 chunks, and under every JPEG encoder.
#[test]
fn jpeg_icc_roundtrips_to_every_icc_target() {
    let px = common::corner_base(240, 180, 60);
    for icc_len in [600usize, 70_000] {
        let icc = common::fake_icc(icc_len);
        let payloads = common::app2_icc_payloads(&icc, 60_000);
        let markers: Vec<(u8, &[u8])> = payloads.iter().map(|p| (2u8, &p[..])).collect();
        let jpeg = common::jpeg_with_markers(&px, 240, 180, &markers);

        for encoder in [Encoder::Jpegli, Encoder::MozFast, Encoder::MozSmall] {
            let p = Params {
                encoder,
                ..params(120)
            };
            let (out, _) = pipeline::process(&jpeg, &p).unwrap();
            assert_eq!(
                common::jpeg_icc(&out).as_deref(),
                Some(&icc[..]),
                "jpeg target, {encoder:?}, {icc_len}B profile"
            );
        }
        for (target, extract) in [
            (
                ImageFormat::Png,
                common::png_icc as fn(&[u8]) -> Option<Vec<u8>>,
            ),
            (
                ImageFormat::Webp,
                common::webp_icc as fn(&[u8]) -> Option<Vec<u8>>,
            ),
        ] {
            let p = Params {
                output: Some(target),
                ..params(120)
            };
            let (out, fmt) = pipeline::process(&jpeg, &p).unwrap();
            assert_eq!(fmt, target);
            assert_eq!(
                extract(&out).as_deref(),
                Some(&icc[..]),
                "{target:?} target, {icc_len}B profile"
            );
            assert_eq!(dims_of(&out), (120, 90));
        }
    }
}

/// PNG and WebP sources carry their profiles too (same-format, to
/// JPEG, and — with the avif feature — into a spliced AVIF `colr`).
#[test]
fn png_and_webp_icc_sources_roundtrip() {
    let icc = common::fake_icc(1200);
    let px = common::corner_base(240, 180, 60);
    let png = common::png_with_icc(&px, 240, 180, &icc);
    for (target, extract) in [
        (
            ImageFormat::Png,
            common::png_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
        (
            ImageFormat::Jpeg,
            common::jpeg_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
    ] {
        let p = Params {
            output: Some(target),
            ..params(120)
        };
        let (out, _) = pipeline::process(&png, &p).unwrap();
        assert_eq!(extract(&out).as_deref(), Some(&icc[..]), "png → {target:?}");
    }

    let webp = common::webp_with_icc(&fixture("photo.webp"), 200, 150, &icc);
    for (target, extract) in [
        (
            ImageFormat::Webp,
            common::webp_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
        (
            ImageFormat::Jpeg,
            common::jpeg_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
    ] {
        let p = Params {
            output: Some(target),
            ..params(100)
        };
        let (out, _) = pipeline::process(&webp, &p).unwrap();
        assert_eq!(
            extract(&out).as_deref(),
            Some(&icc[..]),
            "webp → {target:?}"
        );
    }

    // AVIF target: the profile is spliced into the container as a
    // `colr` (prof) property.
    #[cfg(feature = "avif")]
    {
        let p = Params {
            output: Some(ImageFormat::Avif),
            ..params(100)
        };
        let (out, fmt) = pipeline::process(&png, &p).unwrap();
        assert_eq!(fmt, ImageFormat::Avif);
        assert_eq!(dims_of(&out), (100, 75));
        assert_eq!(
            oximg::avif::extract_icc(&out).as_deref(),
            Some(&icc[..]),
            "png → avif carries the profile"
        );
    }
}

/// A libavif-authored (avifenc --icc) fixture: our container walk must
/// read a third-party writer's property layout, not just its own.
#[cfg(feature = "avif")]
#[test]
fn libavif_authored_icc_is_extracted() {
    let src = fixture("icc.avif");
    let icc = common::fake_icc(900); // the exact blob avifenc embedded
    assert_eq!(
        oximg::avif::extract_icc(&src).as_deref(),
        Some(&icc[..]),
        "extractor must read libavif's layout"
    );
    let p = Params {
        output: Some(ImageFormat::Jpeg),
        ..params(100)
    };
    let (out, _) = pipeline::process(&src, &p).unwrap();
    assert_eq!(
        common::jpeg_icc(&out).as_deref(),
        Some(&icc[..]),
        "libavif source → jpeg carries the profile"
    );
}

/// AVIF is profile-capable in both directions: a profiled JPEG
/// converts to a profiled AVIF (through the fused YUV path — ICC does
/// not force the pixel fuse for AVIF targets), and an ICC-bearing AVIF
/// source carries its profile to every target, including back into
/// AVIF.
#[cfg(feature = "avif")]
#[test]
fn avif_icc_both_directions() {
    let icc = common::fake_icc(1100);
    let px = common::corner_base(240, 180, 60);
    let app2 = common::app2_icc_payloads(&icc, 60_000).remove(0);
    let jpeg = common::jpeg_with_markers(&px, 240, 180, &[(2, &app2)]);
    let p = Params {
        output: Some(ImageFormat::Avif),
        ..params(120)
    };
    let (avif_out, fmt) = pipeline::process(&jpeg, &p).unwrap();
    assert_eq!(fmt, ImageFormat::Avif);
    assert_eq!(dims_of(&avif_out), (120, 90));
    assert_eq!(
        oximg::avif::extract_icc(&avif_out).as_deref(),
        Some(&icc[..]),
        "jpeg → avif carries the profile"
    );

    // Round two: that AVIF as a *source*.
    for (target, check) in [
        (
            ImageFormat::Avif,
            oximg::avif::extract_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
        (
            ImageFormat::Jpeg,
            common::jpeg_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
        (
            ImageFormat::Png,
            common::png_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
        (
            ImageFormat::Webp,
            common::webp_icc as fn(&[u8]) -> Option<Vec<u8>>,
        ),
    ] {
        let p = Params {
            output: Some(target),
            ..params(100)
        };
        let (out, fmt) = pipeline::process(&avif_out, &p).unwrap();
        assert_eq!(fmt, target);
        assert_eq!(
            check(&out).as_deref(),
            Some(&icc[..]),
            "avif source → {target:?}"
        );
    }
}

/// The RGBA PNG decode path (no fused fast path) carries the profile
/// too, and a profile past ICC_CAP is dropped, not copied into every
/// output.
#[test]
fn png_general_arm_and_icc_cap() {
    let icc = common::fake_icc(1500);
    let (w, h) = (240usize, 180usize);
    let rgba: Vec<u8> = common::corner_base(w, h, 60)
        .chunks(3)
        .flat_map(|px| [px[0], px[1], px[2], 255])
        .collect();
    let png = common::png_with_icc_color(&rgba, w, h, &icc, png::ColorType::Rgba);
    let (out, fmt) = pipeline::process(&png, &params(120)).unwrap();
    assert_eq!(fmt, ImageFormat::Png);
    assert_eq!(
        common::png_icc(&out).as_deref(),
        Some(&icc[..]),
        "RGBA (general-arm) PNG keeps its profile"
    );

    // ICC_CAP: a 4MB+ "profile" in a WebP container is dropped.
    let oversized = common::fake_icc(4 * 1024 * 1024 + 1);
    let webp = common::webp_with_icc(&fixture("photo.webp"), 200, 150, &oversized);
    let p = Params {
        output: Some(ImageFormat::Webp),
        ..params(100)
    };
    let (out, _) = pipeline::process(&webp, &p).unwrap();
    assert_eq!(
        common::webp_icc(&out),
        None,
        "oversized profile must be dropped, not amplified"
    );
}

/// Profile-less sources must not grow profiles, whatever the target.
#[test]
fn icc_less_sources_stay_profile_free() {
    for (name, src_fmt) in [
        ("photo.jpg", ImageFormat::Jpeg),
        ("rgb.png", ImageFormat::Png),
        ("photo.webp", ImageFormat::Webp),
    ] {
        for target in [ImageFormat::Jpeg, ImageFormat::Png, ImageFormat::Webp] {
            let p = Params {
                output: Some(target),
                ..params(100)
            };
            let (out, _) = pipeline::process(&fixture(name), &p).unwrap();
            let found = match target {
                ImageFormat::Jpeg => common::jpeg_icc(&out),
                ImageFormat::Png => common::png_icc(&out),
                _ => common::webp_icc(&out),
            };
            assert_eq!(found, None, "{src_fmt:?} → {target:?}");
        }
    }
}

/// Rotation and profile pass-through compose: an oriented, profiled
/// source comes out upright with the profile intact.
#[test]
fn icc_and_orientation_compose() {
    let display = common::corner_base(240, 180, 60);
    let (stored, sw, sh) = common::store_for_orientation(&display, 240, 180, 6);
    let icc = common::fake_icc(900);
    let app1 = common::app1_orientation(6);
    let app2 = common::app2_icc_payloads(&icc, 60_000).remove(0);
    let jpeg = common::jpeg_with_markers(&stored, sw, sh, &[(1, &app1), (2, &app2)]);
    let (out, _) = pipeline::process(&jpeg, &params(120)).unwrap();
    let (ow, oh, corners) = common::corner_classes(&out);
    assert_eq!((ow, oh), (120, 90));
    assert_eq!(corners, ['R', 'G', 'B', 'W']);
    assert_eq!(common::jpeg_icc(&out).as_deref(), Some(&icc[..]));
}

/// PNG `eXIf` and WebP `EXIF` orientations display upright, for every
/// orientation value, through the same corner cross-check as JPEG.
#[test]
fn png_and_webp_orientations_display_upright() {
    let (w, h, block) = (240usize, 180usize, 60usize);
    let display = common::corner_base(w, h, block);
    let corners_of_png = |png_bytes: &[u8]| {
        let mut r = png::Decoder::new(std::io::Cursor::new(png_bytes))
            .read_info()
            .unwrap();
        let info = r.info();
        let (ow, oh) = (info.width as usize, info.height as usize);
        let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
        r.next_frame(&mut buf).unwrap();
        let at = |x: usize, y: usize| common::classify(&buf[(y * ow + x) * 3..]);
        let (ix, iy) = (ow / 8, oh / 8);
        (
            ow,
            oh,
            [
                at(ix, iy),
                at(ow - 1 - ix, iy),
                at(ix, oh - 1 - iy),
                at(ow - 1 - ix, oh - 1 - iy),
            ],
        )
    };
    for o in 1..=8u8 {
        let (stored, sw, sh) = common::store_for_orientation(&display, w, h, o);

        // PNG source → PNG output.
        let png_src = common::png_with_orientation(&stored, sw, sh, o as u16);
        let (out, fmt) = pipeline::process(&png_src, &params(120)).unwrap();
        assert_eq!(fmt, ImageFormat::Png, "png o={o}");
        let (ow, oh, corners) = corners_of_png(&out);
        assert_eq!((ow, oh), (120, 90), "png o={o}");
        assert_eq!(corners, ['R', 'G', 'B', 'W'], "png o={o}");

        // WebP source (EXIF chunk) → PNG output for the corner check.
        // The source is built by encoding the stored pixels as a bare
        // WebP through the pipeline, then wrapping in the EXIF chunk.
        let bare = {
            let p = Params {
                output: Some(ImageFormat::Webp),
                max_width: 4096,
                max_height: 4096,
                ..params(120)
            };
            let stored_png = common::png_with_orientation(&stored, sw, sh, 1);
            pipeline::process(&stored_png, &p).unwrap().0
        };
        let webp_src = common::webp_with_exif(&bare, sw, sh, &common::tiff_orientation(o as u16));
        let p = Params {
            output: Some(ImageFormat::Png),
            ..params(120)
        };
        let (out, _) = pipeline::process(&webp_src, &p).unwrap();
        let (ow, oh, corners) = corners_of_png(&out);
        assert_eq!((ow, oh), (120, 90), "webp o={o}");
        assert_eq!(corners, ['R', 'G', 'B', 'W'], "webp o={o}");
    }

    // A JPEG-style "Exif\0\0" prefix on the chunk payload must parse
    // too (writers disagree; browsers accept both).
    let (stored, sw, sh) = common::store_for_orientation(&display, w, h, 6);
    let mut prefixed = b"Exif\0\0".to_vec();
    prefixed.extend(common::tiff_orientation(6));
    let bare = {
        let p = Params {
            output: Some(ImageFormat::Webp),
            max_width: 4096,
            max_height: 4096,
            ..params(120)
        };
        let stored_png = common::png_with_orientation(&stored, sw, sh, 1);
        pipeline::process(&stored_png, &p).unwrap().0
    };
    let webp_src = common::webp_with_exif(&bare, sw, sh, &prefixed);
    let (out, _) = pipeline::process(&webp_src, &params(120)).unwrap();
    assert_eq!(dims_of(&out), (120, 90), "prefixed EXIF payload");
}

/// Orientation coverage beyond plain RGB and square boxes: grayscale
/// PNG (channel expansion shares the rotation scratch buffer), RGBA
/// (4-channel rotation), a WebP big-box request (no decode scaler, no
/// resize — the copy path must still rotate), and a non-square box
/// (where fitting the *displayed* frame gives different dims than
/// fitting the stored one — square boxes cannot tell them apart).
#[test]
fn orientation_edge_paths() {
    // Grayscale + eXIf: corners at four distinct luminance levels.
    let (w, h, block) = (240usize, 180usize, 60usize);
    let mut gray = vec![120u8; w * h];
    let mut fill = |x0: usize, y0: usize, v: u8| {
        for y in y0..y0 + block {
            for x in x0..x0 + block {
                gray[y * w + x] = v;
            }
        }
    };
    fill(0, 0, 0);
    fill(w - block, 0, 70);
    fill(0, h - block, 180);
    fill(w - block, h - block, 255);
    let display_levels = [0u8, 70, 180, 255];
    let (stored, sw, sh) = {
        // reuse the RGB inverse-transform helper by expanding, then
        // taking one channel back out
        let rgb: Vec<u8> = gray.iter().flat_map(|&g| [g, g, g]).collect();
        let (srgb, sw, sh) = common::store_for_orientation(&rgb, w, h, 6);
        let sg: Vec<u8> = srgb.chunks(3).map(|p| p[0]).collect();
        (sg, sw, sh)
    };
    let png_src = {
        let mut out = Vec::new();
        let mut info = png::Info::with_size(sw as u32, sh as u32);
        info.exif_metadata = Some(std::borrow::Cow::Owned(common::tiff_orientation(6)));
        let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
        enc.set_color(png::ColorType::Grayscale);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&stored).unwrap();
        wr.finish().unwrap();
        out
    };
    let (out, _) = pipeline::process(&png_src, &params(120)).unwrap();
    let mut r = png::Decoder::new(std::io::Cursor::new(&out))
        .read_info()
        .unwrap();
    let (ow, oh) = {
        let i = r.info();
        (i.width as usize, i.height as usize)
    };
    assert_eq!((ow, oh), (120, 90), "gray eXIf: display-fit dims");
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    let (ix, iy) = (ow / 8, oh / 8);
    let lum = |x: usize, y: usize| buf[(y * ow + x) * 3];
    let got = [
        lum(ix, iy),
        lum(ow - 1 - ix, iy),
        lum(ix, oh - 1 - iy),
        lum(ow - 1 - ix, oh - 1 - iy),
    ];
    for (g, want) in got.iter().zip(display_levels) {
        assert!(
            g.abs_diff(want) < 25,
            "gray eXIf corners: got {got:?}, want ~{display_levels:?}"
        );
    }

    // RGBA + eXIf: 4-channel rotation, alpha rides along.
    let display = common::corner_base(w, h, block);
    let (stored, sw, sh) = common::store_for_orientation(&display, w, h, 6);
    let rgba: Vec<u8> = stored
        .chunks(3)
        .enumerate()
        .flat_map(|(i, px)| {
            let x = i % sw;
            let y = i / sw;
            // transparent stored top-left block: displays top-right
            let a = if x < block && y < block { 40 } else { 255 };
            [px[0], px[1], px[2], a]
        })
        .collect();
    let png_src = {
        let mut out = Vec::new();
        let mut info = png::Info::with_size(sw as u32, sh as u32);
        info.exif_metadata = Some(std::borrow::Cow::Owned(common::tiff_orientation(6)));
        let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut wr = enc.write_header().unwrap();
        wr.write_image_data(&rgba).unwrap();
        wr.finish().unwrap();
        out
    };
    let (out, _) = pipeline::process(&png_src, &params(120)).unwrap();
    let mut r = png::Decoder::new(std::io::Cursor::new(&out))
        .read_info()
        .unwrap();
    let (ow, oh) = {
        let i = r.info();
        assert_eq!(i.color_type, png::ColorType::Rgba);
        (i.width as usize, i.height as usize)
    };
    assert_eq!((ow, oh), (120, 90), "rgba eXIf: display-fit dims");
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    // Stored top-left lands top-right under orientation 6
    // (display(x, y) = stored(y, h-1-x)).
    let alpha_at = |x: usize, y: usize| buf[(y * ow + x) * 4 + 3];
    assert!(
        alpha_at(ow - 1 - ow / 8, oh / 8) < 90,
        "transparent block must land top-right after rotation"
    );
    assert!(alpha_at(ow / 8, oh - 1 - oh / 8) > 200);

    // WebP big-box request: no decode scaler, no resize; the plain
    // copy path must still rotate.
    let (stored, sw, sh) = common::store_for_orientation(&display, w, h, 6);
    let bare = {
        let p = Params {
            output: Some(ImageFormat::Webp),
            max_width: 4096,
            max_height: 4096,
            ..params(120)
        };
        let stored_png = common::png_with_orientation(&stored, sw, sh, 1);
        pipeline::process(&stored_png, &p).unwrap().0
    };
    let webp_src = common::webp_with_exif(&bare, sw, sh, &common::tiff_orientation(6));
    let p = Params {
        max_width: 4096,
        max_height: 4096,
        ..params(120)
    };
    let (out, _) = pipeline::process(&webp_src, &p).unwrap();
    assert_eq!(dims_of(&out), (240, 180), "big box: rotated at source size");

    // Non-square box: fitting the displayed 240x180 frame into 200x100
    // gives 133x100; fitting the stored 180x240 frame would give
    // 75x100 rotated — square boxes cannot distinguish the two.
    let p = Params {
        max_width: 200,
        max_height: 100,
        ..params(120)
    };
    let (out, _) = pipeline::process(&webp_src, &p).unwrap();
    // 134 not 133: WebP re-fits from the decode-scaler's intermediate
    // dims (pre-existing ±1 rounding drift, orientation-independent);
    // the stored-frame fit would have produced ~100x75 rotated.
    assert_eq!(
        dims_of(&out),
        (134, 100),
        "non-square box fits the displayed frame"
    );
    let png_src = common::png_with_orientation(&stored, sw, sh, 6);
    let (out, _) = pipeline::process(&png_src, &p).unwrap();
    assert_eq!(dims_of(&out), (133, 100), "png too (no scaler, exact)");
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

/// avifenc-authored irot/imir fixtures must display exactly as
/// libheif renders them (ground truth captured via ImageMagick's heic
/// delegate, which applies transformative properties): the corner
/// arrangements below are libheif's own output for these files.
#[cfg(feature = "avif")]
#[test]
fn avif_irot_imir_match_libheif_rendering() {
    // (fixture, displayed dims at fit 120, libheif corner arrangement)
    let cases: [(&str, (usize, usize), [char; 4]); 4] = [
        ("orient_irot1.avif", (90, 120), ['G', 'W', 'R', 'B']),
        ("orient_imir0.avif", (120, 90), ['B', 'W', 'R', 'G']),
        ("orient_imir1.avif", (120, 90), ['G', 'R', 'W', 'B']),
        ("orient_irot1_imir1.avif", (90, 120), ['W', 'G', 'B', 'R']),
    ];
    for (name, dims, expected) in cases {
        let p = Params {
            output: Some(ImageFormat::Jpeg),
            ..params(120)
        };
        let (out, _) = pipeline::process(&fixture(name), &p).unwrap();
        let (ow, oh, corners) = common::corner_classes(&out);
        assert_eq!((ow, oh), dims, "{name}");
        assert_eq!(corners, expected, "{name}");
    }

    // Rotation and ICC compose on AVIF sources too: irot=3 displays as
    // a 90° CW turn and the profile still passes through.
    let p = Params {
        output: Some(ImageFormat::Jpeg),
        ..params(120)
    };
    let (out, _) = pipeline::process(&fixture("orient_irot3_icc.avif"), &p).unwrap();
    let (ow, oh, corners) = common::corner_classes(&out);
    assert_eq!((ow, oh), (90, 120), "irot3");
    assert_eq!(corners, ['B', 'R', 'W', 'G'], "irot3 = 90° CW");
    assert_eq!(
        common::jpeg_icc(&out).as_deref(),
        Some(&common::fake_icc(900)[..]),
        "profile survives alongside the rotation"
    );
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
