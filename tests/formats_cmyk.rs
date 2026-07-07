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

/// An embedded CMYK-class profile is honored: the conversion runs
/// through moxcms (relative colorimetric) and must land close to the
/// committed vips `icc_transform` rendering — an independent CMM
/// (lcms2) reading the same embedded profile. Tolerances follow the
/// measured CMM gap (max|Δ|=2, mean 0.55 on this profile class).
#[test]
fn embedded_cmyk_profile_is_honored() {
    let src = fixture("cmyk_icc.jpg");
    let (want, w, h) = read_ppm(&fixture("cmyk_icc.ppm"));
    let (got, gw, gh) = decode_and_resize(&src, w as u32, h as u32, 1).unwrap();
    assert_eq!((gw, gh), (w, h));
    let (mut worst, mut sum) = (0i32, 0u64);
    for (a, b) in got.iter().zip(&want) {
        let d = (*a as i32 - *b as i32).abs();
        worst = worst.max(d);
        sum += d as u64;
    }
    let mean = sum as f64 / got.len() as f64;
    assert!(worst <= 3, "max per-channel delta {worst} > 3");
    assert!(mean <= 1.0, "mean per-channel delta {mean:.3} > 1.0");

    // Divergence guard: the color-managed rendering must differ
    // loudly from the naive composite of the same samples, or a
    // silently-ignored profile would sail through the tolerances.
    let dec = mozjpeg::Decompress::new_mem(&src).unwrap();
    let mut started = dec.to_colorspace(mozjpeg::ColorSpace::JCS_CMYK).unwrap();
    let planes: Vec<u8> = started.read_scanlines().unwrap();
    started.finish().unwrap();
    let naive: Vec<u8> = planes
        .chunks_exact(4)
        .flat_map(|px| {
            let k = px[3] as u32;
            [0, 1, 2].map(|c| ((px[c] as u32 * k + 127) / 255) as u8)
        })
        .collect();
    let diverges = got
        .iter()
        .zip(&naive)
        .any(|(a, b)| (*a as i32 - *b as i32).abs() >= 30);
    assert!(diverges, "ICC rendering is indistinguishable from naive");
}

/// Splice an ICC profile as an APP2 chain right after SOI.
fn splice_app2(jpeg: &[u8], icc: &[u8]) -> Vec<u8> {
    let mut out = jpeg[..2].to_vec();
    for payload in app2_icc_payloads(icc, 500) {
        out.push(0xFF);
        out.push(0xE2);
        out.extend(((payload.len() + 2) as u16).to_be_bytes());
        out.extend(&payload);
    }
    out.extend(&jpeg[2..]);
    out
}

/// A parseable but non-CMYK-class profile on a CMYK source is bad
/// metadata, not a conversion recipe: it must never reach the
/// 4-channel transform, and the pixels must render exactly like the
/// profile-less baseline (naive). Pins the `DataColorSpace::Cmyk`
/// class guard in src/pipeline/cmyk.rs, which the unparseable
/// `fake_icc` case cannot exercise.
#[test]
fn non_cmyk_class_profile_falls_back_to_naive() {
    let src = fixture("cmyk_ycck.jpg");
    let srgb = moxcms::ColorProfile::new_srgb().encode().unwrap();
    assert!(
        moxcms::ColorProfile::new_from_slice(&srgb).is_ok(),
        "test premise: the spliced profile must be parseable"
    );
    let (with_rgb_profile, w, h) = decode_and_resize(&splice_app2(&src, &srgb), 64, 48, 1).unwrap();
    let (naive, nw, nh) = decode_and_resize(&src, 64, 48, 1).unwrap();
    assert_eq!((w, h), (nw, nh));
    assert_eq!(with_rgb_profile, naive, "RGB-class profile must be ignored");
}

/// A CMYK source's embedded profile describes ink, not the RGB the
/// pipeline emits: it must never pass through to any output target —
/// the classic interop bug that self-roundtrip suites cannot catch.
/// `cmyk_icc.jpg` carries a real (consumed) CMYK profile; the spliced
/// `fake_icc` covers the unparseable-profile fallback.
#[test]
fn cmyk_source_profile_never_passes_through() {
    let profiled = splice_app2(&fixture("cmyk_ycck.jpg"), &fake_icc(600));
    for source in [profiled, fixture("cmyk_icc.jpg")] {
        for (target, extract) in [
            (ImageFormat::Jpeg, jpeg_icc as fn(&[u8]) -> Option<Vec<u8>>),
            (ImageFormat::Png, png_icc),
            (ImageFormat::Webp, webp_icc),
        ] {
            let mut p = params(32);
            p.output = Some(target);
            let (out, _) = pipeline::process(&source, &p).unwrap();
            assert_eq!(extract(&out), None, "{target:?} must not carry the profile");
        }
        #[cfg(feature = "avif")]
        {
            let mut p = params(32);
            p.output = Some(ImageFormat::Avif);
            let (out, _) = pipeline::process(&source, &p).unwrap();
            assert_eq!(
                oximg::avif::extract_icc(&out),
                None,
                "Avif must not carry the profile"
            );
        }
    }
}
