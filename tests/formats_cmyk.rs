//! CMYK/YCCK JPEG sources: naive conversion pinned against committed
//! third-party references (djpeg — byte-exact with ImageMagick's
//! non-color-managed rendering and the browsers' formula), plus
//! cross-target serving, orientation composition, and the guarantee
//! that a CMYK-source profile never passes through to RGB output.

mod common;

use common::*;
use oximg::pipeline::{self, ImageFormat, decode_and_resize};

/// Minimal strict P6 reader for the committed 8-bit references.
fn read_ppm(bytes: &[u8]) -> (Vec<u8>, usize, usize) {
    let txt = |b: &[u8]| String::from_utf8(b.to_vec()).unwrap();
    let mut fields = Vec::new();
    let mut i = 0;
    while fields.len() < 4 {
        while bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        fields.push(txt(&bytes[start..i]));
    }
    assert_eq!(fields[0], "P6");
    assert_eq!(fields[3], "255");
    let (w, h): (usize, usize) = (fields[1].parse().unwrap(), fields[2].parse().unwrap());
    let px = bytes[i + 1..].to_vec();
    assert_eq!(px.len(), w * h * 3);
    (px, w, h)
}

/// Full-frame comparison of oximg's pre-encode RGB against the djpeg
/// rendering of the same fixture. mozjpeg and djpeg share the libjpeg
/// CMYK plane decode bit-for-bit and the +127 rounding matches djpeg's
/// composite, so the tolerance of 1 only covers codec build drift.
#[test]
fn naive_conversion_matches_djpeg_reference() {
    for (jpg, ppm) in [
        ("cmyk_ycck.jpg", "cmyk_ycck.ppm"),
        ("cmyk_t0.jpg", "cmyk_t0.ppm"),
        // Progressive re-encode of cmyk_ycck.jpg: exact coefficient
        // twin, so it shares the baseline's reference.
        ("cmyk_prog.jpg", "cmyk_ycck.ppm"),
        // APP14-stripped twin of cmyk_t0.jpg: 4 components without an
        // Adobe marker are classified CMYK, same stored convention.
        ("cmyk_noadobe.jpg", "cmyk_t0.ppm"),
        ("cmyk_sub.jpg", "cmyk_sub.ppm"),
    ] {
        let (want, w, h) = read_ppm(&fixture(ppm));
        let (got, gw, gh) = decode_and_resize(&fixture(jpg), w as u32, h as u32, 1).unwrap();
        assert_eq!((gw, gh), (w, h), "{jpg}");
        let worst = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (*a as i32 - *b as i32).abs())
            .max()
            .unwrap();
        assert!(worst <= 1, "{jpg}: max per-channel delta {worst} > 1");
    }
}

/// CMYK sources serve every output format, and the resized frame
/// keeps the corner layout (TL=R TR=G BL=B BR=W on gray) the naive
/// conversion reconstructs from ImageMagick's RGB->CMYK separation.
#[test]
fn cmyk_source_serves_every_target() {
    let src = fixture("cmyk_ycck.jpg");
    for target in [
        None,
        Some(ImageFormat::Jpeg),
        Some(ImageFormat::Png),
        Some(ImageFormat::Webp),
        #[cfg(feature = "avif")]
        Some(ImageFormat::Avif),
    ] {
        let mut p = params(32);
        p.output = target;
        let (out, format) = pipeline::process(&src, &p).unwrap();
        assert_eq!(format, target.unwrap_or(ImageFormat::Jpeg), "{target:?}");
        let (fmt, w, h) = pipeline::probe(&out).unwrap();
        assert_eq!(fmt, format, "{target:?}");
        assert_eq!((w, h), (32, 24), "{target:?}");
    }
    let (w, h, classes) = corner_classes(&pipeline::process(&src, &params(48)).unwrap().0);
    assert_eq!((w, h), (48, 36));
    assert_eq!(classes, ['R', 'G', 'B', 'W']);
}

/// EXIF orientation composes with the CMYK path: the rotation runs on
/// the converted RGB frame, after the resize, like every other source.
#[test]
fn cmyk_source_honors_exif_orientation() {
    let src = fixture("cmyk_ycck.jpg");
    // Splice a rotate-90 Exif APP1 right after SOI.
    let app1 = app1_orientation(6);
    let mut oriented = src[..2].to_vec();
    oriented.push(0xFF);
    oriented.push(0xE1);
    oriented.extend(((app1.len() + 2) as u16).to_be_bytes());
    oriented.extend(&app1);
    oriented.extend(&src[2..]);
    let (out, _) = pipeline::process(&oriented, &params(64)).unwrap();
    // 64x48 rotated 90 degrees displays as 48x64.
    assert_eq!(dims_of(&out), (48, 64));
}

/// A CMYK source's embedded profile describes ink, not the RGB the
/// pipeline emits: it must never pass through to any output target —
/// the classic interop bug that self-roundtrip suites cannot catch.
#[test]
fn cmyk_source_profile_never_passes_through() {
    let src = fixture("cmyk_ycck.jpg");
    let icc = fake_icc(600);
    let mut profiled = src[..2].to_vec();
    for payload in app2_icc_payloads(&icc, 500) {
        profiled.push(0xFF);
        profiled.push(0xE2);
        profiled.extend(((payload.len() + 2) as u16).to_be_bytes());
        profiled.extend(&payload);
    }
    profiled.extend(&src[2..]);
    for (target, extract) in [
        (ImageFormat::Jpeg, jpeg_icc as fn(&[u8]) -> Option<Vec<u8>>),
        (ImageFormat::Png, png_icc),
        (ImageFormat::Webp, webp_icc),
    ] {
        let mut p = params(32);
        p.output = Some(target);
        let (out, _) = pipeline::process(&profiled, &p).unwrap();
        assert_eq!(extract(&out), None, "{target:?} must not carry the profile");
    }
}
