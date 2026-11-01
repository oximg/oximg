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
    // the stored-frame fit would have produced ~100x75 rotated. The
    // drift is a property of decoding at an intermediate size, so it
    // is exactly what turning the scaler off removes — the PNG
    // assertion below is the same box with no scaler and lands on 133.
    // WebP keeps its shrink by default (it is the memory it buys, not
    // the CPU), so this stays 134; `OXIMG_DCT_MARGIN` unset on the
    // *streaming* JPEG arm is what changed.
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

    // At a box larger than the source nothing resizes: the decoded
    // frame is handed straight to the output buffer (issue #129) and
    // must still rotate and mirror exactly as libheif shows it.
    for (name, (dw, dh), expected) in cases {
        let p = Params {
            output: Some(ImageFormat::Jpeg),
            ..params(4096)
        };
        let (out, _) = pipeline::process(&fixture(name), &p).unwrap();
        let (ow, oh, corners) = common::corner_classes(&out);
        assert_eq!((ow, oh), (dw * 2, dh * 2), "{name} at exact size");
        assert_eq!(corners, expected, "{name} at exact size");
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

/// Params that keep the source's size and emit PNG, so an output's
/// dimensions are the displayed frame.
fn native_png() -> Params {
    Params {
        max_width: 4096,
        max_height: 4096,
        output: Some(ImageFormat::Png),
        ..params(120)
    }
}

/// A JPEG, PNG and WebP storing `display` rotated by `o` and tagged
/// with it.
fn oriented_sources(display: &[u8], w: usize, h: usize, o: u8) -> Vec<(&'static str, Vec<u8>)> {
    let (stored, sw, sh) = common::store_for_orientation(display, w, h, o);
    let bare = {
        let p = Params {
            output: Some(ImageFormat::Webp),
            ..native_png()
        };
        let png = common::png_with_orientation(&stored, sw, sh, 1);
        pipeline::process(&png, &p).unwrap().0
    };
    let exif = common::tiff_orientation(o as u16);
    vec![
        (
            "jpeg",
            common::jpeg_with_orientation(&stored, sw, sh, Some(o as u16)),
        ),
        (
            "png",
            common::png_with_orientation(&stored, sw, sh, o as u16),
        ),
        ("webp", common::webp_with_exif(&bare, sw, sh, &exif)),
    ]
}

/// `probe_header` reports the tag, and its display size is the frame
/// `process` emits under auto-rotate: checked against the output's own
/// dimensions, not recomputed (#163).
#[test]
fn probe_header_reports_the_orientation_resize_applies() {
    let (w, h, block) = (240usize, 180usize, 60usize);
    let display = common::corner_base(w, h, block);
    for o in 1..=8u8 {
        for (label, src) in oriented_sources(&display, w, h, o) {
            let hdr = pipeline::probe_header(&src).unwrap();
            assert_eq!(hdr.orientation, o, "{label} o={o}");
            assert_eq!((hdr.width, hdr.height), dims_of(&src), "{label} o={o}");
            let (out, _) = pipeline::process(&src, &native_png()).unwrap();
            let shown = (hdr.display_width(), hdr.display_height());
            assert_eq!(shown, dims_of(&out), "{label} o={o}: what resize emits");
            assert_eq!(shown, (w, h), "{label} o={o}");
            assert_eq!(
                pipeline::probe_header_reader(&src[..]).unwrap(),
                hdr,
                "{label} o={o}: the reader agrees"
            );
        }
    }
}

/// Formats without an orientation, a missing tag and an invalid value
/// all report 1, as resize treats them.
#[test]
fn probe_header_reports_one_without_a_valid_tag() {
    for name in [
        "photo.jpg",
        "rgb.png",
        "photo.webp",
        "anim.gif",
        "still.gif",
    ] {
        let hdr = pipeline::probe_header(&common::fixture(name)).unwrap();
        assert_eq!(hdr.orientation, 1, "{name}");
        let shown = (hdr.display_width(), hdr.display_height());
        assert_eq!(shown, (hdr.width, hdr.height), "{name}");
    }
    let (w, h) = (64usize, 48usize);
    let px = vec![128u8; w * h * 3];
    let no_tag = common::jpeg_with_app1s(&px, w, h, &[&common::app1_exif_no_orientation()]);
    assert_eq!(pipeline::probe_header(&no_tag).unwrap().orientation, 1);
    let invalid = common::jpeg_with_orientation(&px, w, h, Some(9));
    assert_eq!(pipeline::probe_header(&invalid).unwrap().orientation, 1);
}

/// A reader over `bytes` that counts what it handed out.
struct Counting<'a> {
    bytes: &'a [u8],
    read: usize,
}

impl std::io::Read for Counting<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = std::io::Read::read(&mut &self.bytes[self.read..], buf)?;
        self.read += n;
        Ok(n)
    }
}

/// How many bytes `probe` reads from `bytes` through `read`.
fn bytes_read<T>(bytes: &[u8], read: impl Fn(&mut Counting<'_>) -> T) -> (T, usize) {
    let mut r = Counting { bytes, read: 0 };
    let answer = read(&mut r);
    (answer, r.read)
}

/// A WebP whose image data alone outgrows the first 64 KiB read.
fn big_bare_webp() -> (Vec<u8>, usize, usize) {
    let (w, h) = (400usize, 300usize);
    // Deterministic noise, which no encoder shrinks much.
    let mut x = 0x2545_f491u32;
    let px: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let p = Params {
        output: Some(ImageFormat::Webp),
        webp_quality: Some(100.0),
        ..native_png()
    };
    let png = common::png_with_orientation(&px, w, h, 1);
    let webp = pipeline::process(&png, &p).unwrap().0;
    assert!(webp.len() > 2 * (64 << 10), "{} bytes", webp.len());
    (webp, w, h)
}

/// A WebP's `EXIF` chunk follows the image data, so the header reader
/// reads a WebP that flags one through EOF, where `probe_reader` stops
/// at the first 64 KiB. Without the flag it stops there too: libwebp's
/// mux refuses an unflagged chunk, so resize does not rotate either,
/// and the prefix's answer (1) is the whole file's.
#[test]
fn probe_header_reader_reads_a_flagged_webp_through_its_exif() {
    let (bare, w, h) = big_bare_webp();
    let flagged = common::webp_with_exif(&bare, w, h, &common::tiff_orientation(6));
    assert_eq!(flagged[20] & 0x08, 0x08);

    let (hdr, read) = bytes_read(&flagged, |r| pipeline::probe_header_reader(r).unwrap());
    assert_eq!(hdr.orientation, 6);
    assert_eq!(read, flagged.len());
    let (_, read) = bytes_read(&flagged, |r| pipeline::probe_reader(r).unwrap());
    assert_eq!(read, 64 << 10, "probe_reader reads what it always has");

    let mut unflagged = flagged.clone();
    unflagged[20] &= !0x08;
    let (hdr, read) = bytes_read(&unflagged, |r| pipeline::probe_header_reader(r).unwrap());
    assert_eq!(hdr.orientation, 1);
    assert_eq!(read, 64 << 10);
    assert_eq!(pipeline::probe_header(&unflagged).unwrap().orientation, 1);
    let (out, _) = pipeline::process(&unflagged, &native_png()).unwrap();
    assert_eq!(dims_of(&out), (w, h), "an unflagged EXIF does not rotate");
}

/// A JPEG's Exif APP1 behind more metadata than the first read holds
/// is still found: the header is done only once the frame header is.
#[test]
fn probe_header_reader_finds_an_exif_behind_large_metadata() {
    let (w, h) = (64usize, 48usize);
    let px = vec![128u8; w * h * 3];
    let pad = vec![0u8; 0xFFFF - 2];
    let app1 = common::app1_orientation(6);
    let jpeg = common::jpeg_with_markers(&px, w, h, &[(2, &pad), (2, &pad), (1, &app1)]);
    let (hdr, read) = bytes_read(&jpeg, |r| pipeline::probe_header_reader(r).unwrap());
    assert_eq!(hdr.orientation, 6);
    assert_eq!((hdr.display_width(), hdr.display_height()), (h, w));
    assert!(read > 128 << 10 && read <= 256 << 10, "{read}");
}

/// A seekable source over `bytes` that counts the bytes it handed out.
struct SeekCounting {
    inner: std::io::Cursor<Vec<u8>>,
    read: usize,
}

impl std::io::Read for SeekCounting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read += n;
        Ok(n)
    }
}

impl std::io::Seek for SeekCounting {
    fn seek(&mut self, to: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(to)
    }
}

/// `probe_header_seek`'s outcome on `bytes` and the bytes it read; an
/// error by kind and message, so it compares with `probe_header`'s.
fn seek_probe(bytes: &[u8]) -> (Result<pipeline::Header, String>, usize) {
    let mut r = SeekCounting {
        inner: std::io::Cursor::new(bytes.to_vec()),
        read: 0,
    };
    let got = pipeline::probe_header_seek(&mut r).map_err(|e| format!("{:?}: {e:#}", e.kind()));
    (got, r.read)
}

fn whole_probe(bytes: &[u8]) -> Result<pipeline::Header, String> {
    pipeline::probe_header(bytes).map_err(|e| format!("{:?}: {e:#}", e.kind()))
}

/// A RIFF chunk: its FourCC and its body.
type Chunk = ([u8; 4], Vec<u8>);

/// The chunks of a WebP after its 12-byte RIFF header.
fn webp_chunks(webp: &[u8]) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut p = 12;
    while p + 8 <= webp.len() {
        let size = u32::from_le_bytes(webp[p + 4..p + 8].try_into().unwrap()) as usize;
        out.push((
            webp[p..p + 4].try_into().unwrap(),
            webp[p + 8..p + 8 + size].to_vec(),
        ));
        p += 8 + size + (size & 1);
    }
    out
}

/// A RIFF WebP of these chunks, each padded to an even length.
fn riff(chunks: &[Chunk]) -> Vec<u8> {
    let mut body = b"WEBP".to_vec();
    for (four, data) in chunks {
        body.extend_from_slice(four);
        body.extend((data.len() as u32).to_le_bytes());
        body.extend_from_slice(data);
        if data.len() % 2 == 1 {
            body.push(0);
        }
    }
    let mut out = b"RIFF".to_vec();
    out.extend((body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

/// A `VP8X` chunk: `flags` (ICC 0x20, alpha 0x10, EXIF 0x08, XMP 0x04,
/// animation 0x02) over a `w` x `h` canvas.
fn vp8x(flags: u8, w: usize, h: usize) -> Chunk {
    let mut v = vec![flags, 0, 0, 0];
    v.extend(&((w - 1) as u32).to_le_bytes()[..3]);
    v.extend(&((h - 1) as u32).to_le_bytes()[..3]);
    (*b"VP8X", v)
}

/// A lossy WebP with an alpha channel (`ALPH` + `VP8 `) of noise, large
/// enough that its image data outgrows the first 64 KiB read.
fn big_alpha_webp() -> (Vec<u8>, usize, usize) {
    let (w, h) = (400usize, 300usize);
    let mut x = 0x9e37_79b9u32;
    let px: Vec<u8> = (0..w * h * 4)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let mut png = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png, w as u32, h as u32);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(&px).unwrap();
    }
    let p = Params {
        output: Some(ImageFormat::Webp),
        webp_quality: Some(100.0),
        ..native_png()
    };
    let webp = pipeline::process(&png, &p).unwrap().0;
    assert!(webp.len() > 2 * (64 << 10), "{} bytes", webp.len());
    (webp, w, h)
}

/// A lossless WebP (`VP8L`) of noise, large enough that its image data
/// outgrows the first 64 KiB read: the layout of the 2.6 MB WebP behind
/// the sparse read's measured gain, whose size the mux reads through
/// `VP8LGetInfo` rather than `VP8GetInfo`. Encoded with libwebp
/// directly, since the pipeline writes lossless WebP only for alpha.
fn big_lossless_webp() -> (Vec<u8>, usize, usize) {
    let (w, h) = (400usize, 300usize);
    let mut x = 0x85eb_ca6bu32;
    let px: Vec<u8> = (0..w * h * 3)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect();
    let mut out: *mut u8 = std::ptr::null_mut();
    // SAFETY: `px` holds w*h RGB pixels at stride 3*w; libwebp writes a
    // buffer it allocates to `out` and returns its length (0 on failure),
    // which is copied out before WebPFree releases it.
    let webp = unsafe {
        let n = libwebp_sys::WebPEncodeLosslessRGB(
            px.as_ptr(),
            w as i32,
            h as i32,
            (w * 3) as i32,
            &mut out,
        );
        assert!(n > 0 && !out.is_null(), "lossless encode failed");
        let v = std::slice::from_raw_parts(out, n).to_vec();
        libwebp_sys::WebPFree(out.cast());
        v
    };
    assert_eq!(&webp[12..16], b"VP8L");
    assert!(webp.len() > 2 * (64 << 10), "{} bytes", webp.len());
    (webp, w, h)
}

/// Every WebP shape the sparse read must answer exactly as the whole
/// file does: around a lossy, a lossless, an alpha and an animated
/// image, the EXIF
/// flagged, unflagged, doubled, odd-sized, missing, or cut off; the
/// file padded, truncated, or its sizes lying. `probe_header_seek`
/// gives `probe_header`'s answer (or error) on every one.
#[test]
fn probe_header_seek_answers_as_the_whole_webp_does() {
    let exif = |o: u16| (*b"EXIF", common::tiff_orientation(o));
    let mut cases: Vec<(String, Vec<u8>)> = Vec::new();
    let (lossy, w, h) = big_bare_webp();
    let (lossless, lw, lh) = big_lossless_webp();
    let (alpha, aw, ah) = big_alpha_webp();
    let alpha_chunks: Vec<_> = webp_chunks(&alpha)
        .into_iter()
        .filter(|(f, _)| f != b"VP8X")
        .collect();
    let images: Vec<(&str, Vec<Chunk>, u8, usize, usize)> = vec![
        ("lossy", webp_chunks(&lossy), 0, w, h),
        ("lossless", webp_chunks(&lossless), 0, lw, lh),
        ("alpha", alpha_chunks, 0x10, aw, ah),
    ];
    for (name, image, base, w, h) in &images {
        let with = |flags: u8, before: &[Chunk], after: &[Chunk]| {
            let mut c = vec![vp8x(base | flags, *w, *h)];
            c.extend_from_slice(before);
            c.extend(image.iter().cloned());
            c.extend_from_slice(after);
            riff(&c)
        };
        let icc = (*b"ICCP", common::fake_icc(900));
        let big_icc = (*b"ICCP", common::fake_icc(100 << 10));
        let xmp = (*b"XMP ", b"<x:xmpmeta/>".to_vec());
        let mut odd = common::tiff_orientation(8);
        odd.push(0);
        let plain = with(0x08, &[], &[exif(6)]);
        cases.push((format!("{name}: exif"), plain.clone()));
        cases.push((format!("{name}: unflagged exif"), with(0, &[], &[exif(6)])));
        cases.push((format!("{name}: flag, no chunk"), with(0x08, &[], &[])));
        cases.push((
            format!("{name}: icc+exif+xmp"),
            with(0x2c, &[icc], &[exif(3), xmp]),
        ));
        cases.push((
            format!("{name}: 100 KiB icc ahead of the image"),
            with(0x28, &[big_icc], &[exif(5)]),
        ));
        cases.push((
            format!("{name}: odd-sized exif"),
            with(0x08, &[], &[(*b"EXIF", odd)]),
        ));
        cases.push((
            format!("{name}: two exif"),
            with(0x08, &[], &[exif(6), exif(3)]),
        ));
        cases.push((
            format!("{name}: exif without orientation"),
            with(
                0x08,
                &[],
                &[(*b"EXIF", common::app1_exif_no_orientation()[6..].to_vec())],
            ),
        ));
        let mut trailing = plain.clone();
        trailing.extend(std::iter::repeat_n(0xAB, 1000));
        cases.push((format!("{name}: bytes after the RIFF"), trailing));
        cases.push((
            format!("{name}: cut in the image data"),
            plain[..plain.len() / 2].to_vec(),
        ));
        cases.push((
            format!("{name}: cut in the exif"),
            plain[..plain.len() - 6].to_vec(),
        ));
        let mut short_riff = plain.clone();
        let declared = (plain.len() - 8 - 40) as u32;
        short_riff[4..8].copy_from_slice(&declared.to_le_bytes());
        cases.push((format!("{name}: RIFF size short of the exif"), short_riff));
        let mut long_riff = plain.clone();
        long_riff[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push((format!("{name}: RIFF size past the end"), long_riff));
        let mut huge_chunk = plain.clone();
        let at = huge_chunk.len() - 8 - common::tiff_orientation(6).len();
        huge_chunk[at + 4..at + 8].copy_from_slice(&u32::MAX.to_le_bytes());
        cases.push((format!("{name}: exif size past the end"), huge_chunk));
    }
    // Animated: the frames are read whole, the answer still exact.
    let anim = common::fixture("animated.webp");
    let mut anim_chunks = webp_chunks(&anim);
    anim_chunks[0].1[0] |= 0x08;
    anim_chunks.push(exif(6));
    cases.push(("animated: exif".into(), riff(&anim_chunks)));
    for name in ["photo.webp", "alpha.webp", "animated.webp"] {
        cases.push((name.into(), common::fixture(name)));
    }

    let mut rotated = 0;
    for (label, bytes) in &cases {
        let (got, _) = seek_probe(bytes);
        let want = whole_probe(bytes);
        assert_eq!(got, want, "{label}");
        rotated += usize::from(matches!(want, Ok(h) if h.orientation != 1));
    }
    // Not all refusals or 1s: per image, the plain, icc+xmp, big-ICC,
    // odd-sized and trailing-bytes cases carry an orientation (the mux
    // takes none from two EXIF chunks or a cut file), and so does the
    // animation.
    assert_eq!(rotated, 3 * 5 + 1, "cases that reported an orientation");

    // And the point: past the first read, only chunk headers, the EXIF
    // and a little of each image chunk are read.
    for (_, image, base, w, h) in &images {
        let mut c = vec![vp8x(base | 0x08, *w, *h)];
        c.extend(image.iter().cloned());
        c.push(exif(6));
        let bytes = riff(&c);
        let (got, read) = seek_probe(&bytes);
        assert_eq!(got.unwrap().orientation, 6);
        assert!(
            read <= (64 << 10) + 3 * (4 << 10),
            "read {read} of {}",
            bytes.len()
        );
    }
}

/// Every fixture probes the same through the seekable reader.
#[test]
fn probe_header_seek_agrees_with_probe_header_on_every_fixture() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    let mut seen = 0;
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if !matches!(ext, "jpg" | "png" | "webp" | "gif" | "avif") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            seek_probe(&bytes).0,
            whole_probe(&bytes),
            "{}",
            path.display()
        );
        // A real file too, from where its reader stands.
        let mut file = std::fs::File::open(&path).unwrap();
        let from_file =
            pipeline::probe_header_seek(&mut file).map_err(|e| format!("{:?}: {e:#}", e.kind()));
        assert_eq!(from_file, whole_probe(&bytes), "{}", path.display());
        seen += 1;
    }
    assert!(seen >= 30, "only {seen} fixtures probed");
}
