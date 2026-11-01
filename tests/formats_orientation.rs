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
