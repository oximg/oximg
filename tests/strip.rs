//! Lossless metadata strip (#127): `pipeline::strip` drops EXIF, XMP,
//! IPTC, comments and unknown blocks from JPEG, PNG and WebP, keeps the
//! decoded pixels identical, writes a non-upright orientation back, and
//! refuses to leave a CMYK JPEG without its profile.
//!
//! Sources are built here from committed fixtures plus spliced blocks,
//! each carrying a `SECRET` marker string: a strip passes only if no
//! such string survives anywhere in the output.

mod common;

use common::{fixture, params};
use oximg::pipeline::{self, ErrorKind, ImageFormat, Params, StripParams, Stripped};

fn keep_icc() -> StripParams {
    StripParams { icc: Some(true) }
}

fn drop_icc() -> StripParams {
    StripParams { icc: Some(false) }
}

/// The decoded pixels in stored orientation, as a lossless PNG — the
/// "decoded hash" #127 asks to stay byte-identical.
fn pixels(b: &[u8]) -> Vec<u8> {
    let p = Params {
        output: Some(ImageFormat::Png),
        auto_rotate: Some(false),
        icc: Some(false),
        ..Default::default()
    };
    pipeline::process(b, &p).unwrap().0
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn removed_names(s: &Stripped) -> Vec<&str> {
    s.removed.iter().map(|r| r.name.as_str()).collect()
}

/// Strip, check nothing secret survives and the pixels match, and that
/// a second strip finds nothing left and changes nothing.
fn strip_clean(src: &[u8], p: &StripParams) -> Stripped {
    let out = pipeline::strip(src, p).unwrap();
    assert!(!contains(&out.bytes, b"SECRET"), "metadata survived");
    assert_eq!(pixels(&out.bytes), pixels(src), "pixels changed");
    let again = pipeline::strip(&out.bytes, p).unwrap();
    assert!(again.removed.is_empty(), "residue: {:?}", again.removed);
    assert_eq!(again.bytes, out.bytes, "strip is not idempotent");
    assert_eq!(again.orientation, out.orientation);
    out
}

/// An Exif payload whose IFD0 holds orientation `o`, followed by bytes
/// standing in for the GPS, camera and timestamp tags a phone writes.
fn exif_app1(o: u16) -> Vec<u8> {
    let mut v = common::app1_orientation(o);
    v.extend_from_slice(b"SECRET GPS 25.0330N 121.5654E Pixel 9 2026:10:30");
    v
}

fn exif_tiff(o: u16) -> Vec<u8> {
    exif_app1(o)[6..].to_vec()
}

const XMP: &[u8] = b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>SECRET-XMP</x:xmpmeta>";

/// The marker codes of a JPEG's header segments up to the first SOS,
/// walked independently of the code under test.
fn jpeg_markers(b: &[u8]) -> Vec<(u8, Vec<u8>)> {
    let mut v = Vec::new();
    let mut i = 2;
    while i + 4 <= b.len() && b[i] == 0xFF && b[i + 1] != 0xDA {
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        v.push((b[i + 1], b[i + 4..i + 2 + len].to_vec()));
        i += 2 + len;
    }
    v
}

#[test]
fn jpeg_drops_every_metadata_segment_and_keeps_pixels() {
    let px = common::corner_base(240, 180, 60);
    let icc = common::fake_icc(70_000);
    let icc_chunks = common::app2_icc_payloads(&icc, 60_000);
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(240, 180);
    comp.set_quality(90.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    started.write_marker(mozjpeg::Marker::APP(1), &exif_app1(1));
    started.write_marker(mozjpeg::Marker::APP(1), XMP);
    started.write_marker(mozjpeg::Marker::APP(13), b"Photoshop 3.0\08BIM SECRET-IPTC");
    for c in &icc_chunks {
        started.write_marker(mozjpeg::Marker::APP(2), c);
    }
    started.write_marker(mozjpeg::Marker::APP(2), b"MPF\0SECRET-MPF");
    started.write_marker(mozjpeg::Marker::APP(5), b"SECRET-APP5");
    started.write_marker(mozjpeg::Marker::COM, b"SECRET comment");
    started.write_scanlines(&px).unwrap();
    let mut src = started.finish().unwrap();
    // An MPF secondary image lives after EOI.
    src.extend_from_slice(b"\xFF\xD8SECRET-SECONDARY\xFF\xD9");

    let out = strip_clean(&src, &keep_icc());
    assert_eq!(out.format, ImageFormat::Jpeg);
    assert_eq!(out.orientation, None, "upright: no EXIF written back");
    assert_eq!(
        removed_names(&out),
        [
            "APP1 Exif",
            "XMP",
            "APP13 Photoshop 3.0",
            "APP2 MPF",
            "APP5 SECRET-APP5",
            "COM",
            "trailer"
        ]
    );
    assert_eq!(common::jpeg_icc(&out.bytes).as_deref(), Some(&icc[..]));
    let markers: Vec<u8> = jpeg_markers(&out.bytes).iter().map(|m| m.0).collect();
    assert_eq!(markers[0], 0xE0, "JFIF APP0 stays first");
    assert!(!markers.contains(&0xE1) && !markers.contains(&0xFE));

    let out = strip_clean(&src, &drop_icc());
    assert_eq!(common::jpeg_icc(&out.bytes), None);
    assert!(removed_names(&out).contains(&"APP2 ICC_PROFILE"));
    let icc_removed = out
        .removed
        .iter()
        .find(|r| r.name == "APP2 ICC_PROFILE")
        .unwrap();
    assert_eq!(icc_removed.count, 2, "both chunks of the chain");
}

/// A progressive JPEG carries tables between its scans; the walk must
/// copy every scan through, not stop at the first SOS.
#[test]
fn jpeg_progressive_scans_copy_through() {
    let src = common::jpeg_with_app1s(&common::corner_base(240, 180, 60), 240, 180, &[]);
    let (_, w, h) = pipeline::probe(&src).unwrap();
    let prog = pipeline::process(
        &src,
        &Params {
            max_width: w as u32 - 1,
            max_height: h as u32,
            ..params(0)
        },
    )
    .unwrap()
    .0;
    assert!(common::jpeg_scans(&prog).1.len() > 1, "not multi-scan");
    let out = strip_clean(&prog, &keep_icc());
    assert_eq!(out.bytes, prog, "nothing to strip: bytes unchanged");
}

#[test]
fn rotated_photos_keep_only_their_orientation() {
    let (w, h, block) = (240usize, 180usize, 60usize);
    let display = common::corner_base(w, h, block);
    for o in 1..=8u8 {
        let (stored, sw, sh) = common::store_for_orientation(&display, w, h, o);
        let src = common::jpeg_with_app1s(&stored, sw, sh, &[&exif_app1(o as u16)]);
        let out = strip_clean(&src, &keep_icc());
        assert_eq!(out.orientation, (o != 1).then_some(o), "o={o}");
        assert_eq!(
            pipeline::probe_display_with(&out.bytes, true).unwrap(),
            pipeline::probe_display_with(&src, true).unwrap(),
            "o={o}"
        );
        // Displayed upright: the resize path, honoring EXIF the way
        // browsers do, sees the corners where they belong.
        let (shown, _) = pipeline::process(&out.bytes, &params(120)).unwrap();
        assert_eq!(
            common::corner_classes(&shown),
            (120, 90, ['R', 'G', 'B', 'W']),
            "o={o}"
        );
        let app1s = jpeg_markers(&out.bytes)
            .into_iter()
            .filter(|m| m.0 == 0xE1)
            .count();
        assert_eq!(app1s, usize::from(o != 1), "o={o}");
    }
}

/// The first Exif segment decides orientation, as in the decode path;
/// a later one is metadata like any other.
#[test]
fn first_exif_wins() {
    let px = common::corner_base(240, 180, 60);
    let src = common::jpeg_with_app1s(&px, 240, 180, &[&exif_app1(1), &exif_app1(6)]);
    let out = strip_clean(&src, &keep_icc());
    assert_eq!(out.orientation, None);
    assert_eq!(out.removed[0].count, 2);
}

#[test]
fn cmyk_profiles_are_never_dropped() {
    let src = fixture("cmyk_icc.jpg");
    let e = pipeline::strip(&src, &drop_icc()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Undecodable);
    assert!(e.to_string().contains("CMYK"), "{e}");

    let out = strip_clean(&src, &keep_icc());
    assert_eq!(common::jpeg_icc(&out.bytes), common::jpeg_icc(&src));
    // Adobe APP14 carries the transform flag CMYK/YCCK decoding needs.
    for name in ["cmyk_icc.jpg", "cmyk_ycck.jpg"] {
        let src = fixture(name);
        let out = strip_clean(&src, &keep_icc());
        assert!(
            jpeg_markers(&out.bytes)
                .iter()
                .any(|(m, b)| *m == 0xEE && b.starts_with(b"Adobe")),
            "{name}"
        );
    }
    // Without a profile there is nothing to drop, so --icc strip is fine.
    let src = fixture("cmyk_ycck.jpg");
    assert!(common::jpeg_icc(&src).is_none());
    strip_clean(&src, &drop_icc());
}

/// A stream that ends before EOI is copied as far as it goes, like
/// decoders accept it; one that ends inside a header is an error.
#[test]
fn truncated_jpegs() {
    let src = common::jpeg_with_app1s(&common::corner_base(240, 180, 60), 240, 180, &[XMP]);
    let cut = &src[..src.len() - 2];
    let out = pipeline::strip(cut, &keep_icc()).unwrap();
    assert!(!contains(&out.bytes, b"SECRET"));
    assert!(cut.ends_with(&out.bytes[out.bytes.len() - 64..]));

    let e = pipeline::strip(&src[..30], &keep_icc()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Undecodable);
}

/// Bitwise CRC-32, independent of the strip's own.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
    }
    !c
}

fn png_chunk(ty: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut v = (data.len() as u32).to_be_bytes().to_vec();
    v.extend_from_slice(ty);
    v.extend_from_slice(data);
    let crc = crc32(&v[4..]);
    v.extend(crc.to_be_bytes());
    v
}

/// Insert chunks right after IHDR (`pre`) and right before IEND (`post`).
fn png_splice(png: &[u8], pre: &[Vec<u8>], post: &[Vec<u8>]) -> Vec<u8> {
    let ihdr_end = 8 + 12 + 13;
    let iend = png.len() - 12;
    assert_eq!(&png[iend + 4..iend + 8], b"IEND");
    let mut v = png[..ihdr_end].to_vec();
    pre.iter().for_each(|c| v.extend(c));
    v.extend_from_slice(&png[ihdr_end..iend]);
    post.iter().for_each(|c| v.extend(c));
    v.extend_from_slice(&png[iend..]);
    v
}

fn png_chunk_types(b: &[u8]) -> Vec<String> {
    let mut v = Vec::new();
    let mut i = 8;
    while i + 8 <= b.len() {
        let len = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        v.push(String::from_utf8_lossy(&b[i + 4..i + 8]).into_owned());
        i += 12 + len;
    }
    v
}

#[test]
fn png_drops_text_time_exif_and_private_chunks() {
    let (w, h) = (240usize, 180usize);
    let display = common::corner_base(w, h, 60);
    for o in [1u8, 6, 8] {
        let (stored, sw, sh) = common::store_for_orientation(&display, w, h, o);
        let icc = common::fake_icc(600);
        let base = common::png_with_icc(&stored, sw, sh, &icc);
        let src = png_splice(
            &base,
            &[
                png_chunk(b"eXIf", &exif_tiff(o as u16)),
                png_chunk(b"tEXt", b"Author\0SECRET-author"),
                png_chunk(b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0SECRET-XMP"),
                png_chunk(b"tIME", &[0x07, 0xEA, 10, 30, 12, 0, 0]),
                png_chunk(b"pHYs", &[0, 0, 11, 19, 0, 0, 11, 19, 1]),
            ],
            &[
                png_chunk(b"zTXt", b"Comment\0\0SECRET-not-really-deflated"),
                png_chunk(b"prVt", b"SECRET vendor GPS"),
            ],
        );
        let out = strip_clean(&src, &keep_icc());
        assert_eq!(out.format, ImageFormat::Png);
        assert_eq!(out.orientation, (o != 1).then_some(o));
        let exif = if o == 1 {
            vec![]
        } else {
            vec!["eXIf".to_string()]
        };
        let expect: Vec<String> = ["IHDR"]
            .iter()
            .map(|s| s.to_string())
            .chain(exif)
            .chain(
                ["pHYs", "iCCP", "IDAT", "IEND"]
                    .iter()
                    .map(|s| s.to_string()),
            )
            .collect();
        assert_eq!(png_chunk_types(&out.bytes), expect, "o={o}");
        assert_eq!(common::png_icc(&out.bytes).as_deref(), Some(&icc[..]));
        assert_eq!(
            pipeline::probe_display_with(&out.bytes, true).unwrap(),
            pipeline::probe_display_with(&src, true).unwrap(),
            "o={o}"
        );

        let out = strip_clean(&src, &drop_icc());
        assert_eq!(common::png_icc(&out.bytes), None);
        assert!(removed_names(&out).contains(&"iCCP"));
    }
}

/// Browsers honor only a pre-IDAT eXIf; one after the image data is
/// dropped without a write-back.
#[test]
fn png_post_idat_exif_is_just_metadata() {
    let src = png_splice(
        &common::png_with_icc(&common::corner_base(240, 180, 60), 240, 180, b"x"),
        &[],
        &[png_chunk(b"eXIf", &exif_tiff(6))],
    );
    let out = strip_clean(&src, &keep_icc());
    assert_eq!(out.orientation, None);
}

/// Append chunks to a VP8X WebP, OR `flags` into its VP8X byte, and
/// fix the RIFF size.
fn webp_append(webp: &[u8], flags: u8, chunks: &[(&[u8; 4], &[u8])]) -> Vec<u8> {
    assert_eq!(&webp[12..16], b"VP8X");
    let mut v = webp.to_vec();
    v[20] |= flags;
    for (four, body) in chunks {
        v.extend_from_slice(*four);
        v.extend((body.len() as u32).to_le_bytes());
        v.extend_from_slice(body);
        if body.len() % 2 == 1 {
            v.push(0);
        }
    }
    let size = (v.len() - 8) as u32;
    v[4..8].copy_from_slice(&size.to_le_bytes());
    v
}

#[test]
fn webp_drops_exif_xmp_and_unknown_chunks() {
    let bare = fixture("photo.webp");
    let (_, w, h) = pipeline::probe(&bare).unwrap();
    for o in [1u16, 6] {
        let with_exif = common::webp_with_exif(&bare, w, h, &exif_tiff(o));
        let src = webp_append(&with_exif, 0x04, &[(b"XMP ", XMP), (b"PRVT", b"SECRET!")]);
        let out = strip_clean(&src, &keep_icc());
        assert_eq!(out.format, ImageFormat::Webp);
        assert_eq!(removed_names(&out), ["EXIF", "XMP ", "PRVT"]);
        let flags = out.bytes[20];
        if o == 1 {
            assert_eq!(flags, 0, "no EXIF, XMP or ICC left to flag");
            assert_eq!(out.orientation, None);
        } else {
            assert_eq!(flags, 0x08, "EXIF flag for the written-back orientation");
            assert_eq!(out.orientation, Some(6));
        }
        assert_eq!(
            pipeline::probe_display_with(&out.bytes, true).unwrap(),
            pipeline::probe_display_with(&src, true).unwrap(),
            "o={o}"
        );
        let riff = u32::from_le_bytes(out.bytes[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff + 8, out.bytes.len());
    }

    let icc = common::fake_icc(601); // odd: exercises chunk padding
    let src = common::webp_with_icc(&bare, w, h, &icc);
    let out = strip_clean(&src, &keep_icc());
    assert_eq!(out.bytes, src, "only an ICC to keep: bytes unchanged");
    let out = strip_clean(&src, &drop_icc());
    assert_eq!(common::webp_icc(&out.bytes), None);
    assert_eq!(out.bytes[20] & 0x20, 0, "ICC flag cleared");
}

/// Chunk filtering is frame-agnostic: an animated WebP comes back with
/// every frame, byte for byte.
#[test]
fn animated_webp_keeps_every_frame() {
    let anim = fixture("animated.webp");
    let src = webp_append(
        &anim,
        0x08 | 0x04,
        &[(b"EXIF", &exif_tiff(1)), (b"XMP ", XMP)],
    );
    let out = pipeline::strip(&src, &keep_icc()).unwrap();
    assert!(!contains(&out.bytes, b"SECRET"));
    assert_eq!(out.bytes, anim);
    assert_eq!(
        pipeline::probe_animation(&out.bytes)
            .unwrap()
            .unwrap()
            .frames,
        pipeline::probe_animation(&anim).unwrap().unwrap().frames
    );
}

#[test]
fn other_formats_are_refused() {
    for name in ["still.gif", "photo.avif"] {
        let e = pipeline::strip(&fixture(name), &keep_icc()).unwrap_err();
        assert_eq!(e.kind(), ErrorKind::Undecodable, "{name}");
        assert!(e.to_string().contains("JPEG, PNG and WebP"), "{name}: {e}");
    }
    let e = pipeline::strip(b"not an image", &keep_icc()).unwrap_err();
    assert_eq!(e.kind(), ErrorKind::Undecodable);
}
