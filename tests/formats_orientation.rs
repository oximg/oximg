//! Orientation tests across every source format (EXIF, PNG eXIf,
//! WebP EXIF, AVIF irot/imir).

mod common;

#[cfg(feature = "avif")]
use common::fixture;
use common::{dims_of, params};
use oximg::pipeline::{self, ImageFormat, Params};

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
