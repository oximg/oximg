//! ICC profile pass-through across formats and directions.

mod common;

use common::{dims_of, fixture, params};
use oximg::pipeline::{self, Encoder, ImageFormat, Params};


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
