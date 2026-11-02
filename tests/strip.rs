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
    StripParams {
        icc: Some(true),
        ..Default::default()
    }
}

fn drop_icc() -> StripParams {
    StripParams {
        icc: Some(false),
        ..Default::default()
    }
}

/// The decoded pixels in stored orientation, as a lossless PNG — the
/// "decoded hash" #127 asks to stay byte-identical. A PNG is decoded at
/// its native depth: the pipeline strips 16-bit samples to 8, which
/// would hide a change in a low byte.
fn pixels(b: &[u8]) -> Vec<u8> {
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        let mut d = png::Decoder::new(std::io::Cursor::new(b));
        d.set_transformations(png::Transformations::EXPAND);
        let mut r = d.read_info().unwrap();
        let mut px = vec![0; r.output_buffer_size().unwrap()];
        let info = r.next_frame(&mut px).unwrap();
        px.truncate(info.buffer_size());
        return [&[info.color_type as u8, info.bit_depth as u8][..], &px].concat();
    }
    let p = Params {
        output: Some(ImageFormat::Png),
        auto_rotate: Some(false),
        icc: Some(false),
        png_quantize: Some(false),
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
            "APP5",
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

/// A JFIF thumbnail is a second picture (possibly of what a crop cut
/// away): it goes, and the JFIF header stays with its density.
#[test]
fn jfif_thumbnail_is_dropped() {
    let photo = fixture("photo.jpg");
    let app0 = jpeg_markers(&photo)[0].clone();
    assert_eq!(app0.0, 0xE0);
    assert!(app0.1.starts_with(b"JFIF\0"));
    let header = &app0.1[..12];
    // A 2x1 thumbnail: 6 bytes of RGB after the dimensions.
    let mut body = header.to_vec();
    body.extend_from_slice(&[2, 1]);
    body.extend_from_slice(b"SECRET");
    let mut src = photo[..2].to_vec();
    src.extend_from_slice(&[0xFF, 0xE0]);
    src.extend_from_slice(&(body.len() as u16 + 2).to_be_bytes());
    src.extend_from_slice(&body);
    src.extend_from_slice(&photo[4 + app0.1.len() + 2..]);

    let out = strip_clean(&src, &keep_icc());
    assert_eq!(removed_names(&out), ["JFIF thumbnail"]);
    assert_eq!(out.removed[0].bytes, 6);
    let kept = &jpeg_markers(&out.bytes)[0];
    assert_eq!(kept.0, 0xE0);
    assert_eq!(kept.1, [header, &[0, 0]].concat());
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
fn png_16_bit_keeps_every_sample_bit() {
    let src = fixture("rgb16.png");
    strip_clean(&src, &keep_icc());

    // The comparison itself must see a low-byte-only change.
    let mut d = png::Decoder::new(std::io::Cursor::new(&src[..]));
    d.set_transformations(png::Transformations::IDENTITY);
    let mut r = d.read_info().unwrap();
    let mut px = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut px).unwrap();
    px.truncate(info.buffer_size());
    assert_eq!(info.bit_depth, png::BitDepth::Sixteen);
    px[1] ^= 1;
    let mut nudged = Vec::new();
    let mut e = png::Encoder::new(&mut nudged, info.width, info.height);
    e.set_color(info.color_type);
    e.set_depth(info.bit_depth);
    e.write_header().unwrap().write_image_data(&px).unwrap();
    assert_ne!(pixels(&nudged), pixels(&src));
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

/// Rebuild a WebP with a private chunk appended inside every animation
/// frame, where the spec allows unknown chunks after the bitstream.
fn plant_in_frames(b: &[u8]) -> Vec<u8> {
    let mut out = b[..12].to_vec();
    let mut i = 12;
    while i < b.len() {
        let size = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let end = (i + 8 + size + (size & 1)).min(b.len());
        if &b[i..i + 4] == b"ANMF" {
            let mut frame = b[i + 8..end].to_vec();
            frame.extend_from_slice(b"PRVT\x07\0\0\0SECRET!\0");
            out.extend_from_slice(b"ANMF");
            out.extend_from_slice(&(frame.len() as u32).to_le_bytes());
            out.extend_from_slice(&frame);
        } else {
            out.extend_from_slice(&b[i..end]);
        }
        i = end;
    }
    let riff = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    out
}

/// An animated WebP comes back with every frame, byte for byte, and
/// metadata hidden inside a frame goes like top-level metadata.
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

    let frames = pipeline::probe_animation(&anim).unwrap().unwrap().frames;
    let src = plant_in_frames(&anim);
    assert!(contains(&src, b"SECRET"));
    let out = strip_clean(&src, &keep_icc());
    assert_eq!(out.bytes, anim);
    assert_eq!(out.removed.len(), 1);
    assert_eq!(out.removed[0].name, "PRVT");
    assert_eq!(out.removed[0].count, frames);
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
// --- `auto_rotate` (#163): lossless DCT-domain rotation of a JPEG ---

fn rotate(icc: bool) -> StripParams {
    StripParams {
        icc: Some(icc),
        auto_rotate: true,
    }
}

/// RGB pixels as a q90 JPEG with chroma subsampled `(h, v)` (4:2:0 is
/// `(2, 2)`), baseline or progressive, carrying the APPn `markers`.
fn jpeg_coded(
    px: &[u8],
    (w, h): (usize, usize),
    sub: (u8, u8),
    progressive: bool,
    markers: &[(u8, &[u8])],
) -> Vec<u8> {
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_fastest_defaults();
    comp.set_size(w, h);
    comp.set_quality(90.0);
    comp.set_chroma_sampling_pixel_sizes(sub, sub);
    if progressive {
        comp.set_progressive_mode();
    }
    let mut started = comp.start_compress(Vec::new()).unwrap();
    for (n, p) in markers {
        started.write_marker(mozjpeg::Marker::APP(*n), p);
    }
    started.write_scanlines(px).unwrap();
    started.finish().unwrap()
}

/// The decoded samples, stored or `displayed` upright, as RGB.
fn rgb(b: &[u8], displayed: bool) -> (usize, usize, Vec<u8>) {
    let p = Params {
        output: Some(ImageFormat::Png),
        auto_rotate: Some(displayed),
        icc: Some(false),
        png_quantize: Some(false),
        ..Default::default()
    };
    let png = pipeline::process(b, &p).unwrap().0;
    let mut r = png::Decoder::new(std::io::Cursor::new(png))
        .read_info()
        .unwrap();
    let mut px = vec![0; r.output_buffer_size().unwrap()];
    let info = r.next_frame(&mut px).unwrap();
    px.truncate(info.buffer_size());
    (info.width as usize, info.height as usize, px)
}

/// The inverse of orientation `o`: 6 and 8 are opposite quarter turns,
/// every other orientation undoes itself.
fn inverse(o: u8) -> u8 {
    match o {
        6 => 8,
        8 => 6,
        o => o,
    }
}

/// `jpeg` with an Exif APP1 holding orientation `o` after the SOI.
fn tagged(jpeg: &[u8], o: u8) -> Vec<u8> {
    let app1 = common::app1_orientation(o.into());
    let mut v = jpeg[..2].to_vec();
    v.extend_from_slice(&[0xFF, 0xE1]);
    v.extend_from_slice(&(app1.len() as u16 + 2).to_be_bytes());
    v.extend_from_slice(&app1);
    v.extend_from_slice(&jpeg[2..]);
    v
}

/// Check one rotated strip of `src` (orientation `o`, displayed
/// `w`x`h`): upright, untagged, lossless and clean.
fn assert_rotated(src: &[u8], out: &Stripped, o: u8, (w, h): (usize, usize), what: &str) {
    assert_eq!(out.rotated, Some(o), "{what}");
    assert_eq!((out.orientation, &out.not_rotated), (None, &None), "{what}");
    assert!(
        !contains(&out.bytes, b"SECRET"),
        "{what}: metadata survived"
    );
    let app1s = jpeg_markers(&out.bytes)
        .iter()
        .filter(|m| m.0 == 0xE1)
        .count();
    assert_eq!(app1s, 0, "{what}: an orientation tag survived");
    // Upright as stored: the corners are where the display has them.
    assert_eq!(
        common::corner_classes(&out.bytes),
        (w, h, ['R', 'G', 'B', 'W']),
        "{what}"
    );
    // The same picture the source shows, up to the decoder's rounding
    // (a transposed block's IDCT passes run in the other order; fancy
    // upsampling is not mirror-symmetric): at most 3 measured.
    let (want, got) = (rgb(src, true), rgb(&out.bytes, false));
    assert_eq!((got.0, got.1), (want.0, want.1), "{what}");
    let diff = want.2.iter().zip(&got.2).map(|(a, b)| a.abs_diff(*b)).max();
    assert!(diff <= Some(3), "{what}: max diff {diff:?}");
    // Lossless: the inverse turn restores the stored pixels exactly,
    // which only the source's own coefficients decode to.
    let back = pipeline::strip(&tagged(&out.bytes, inverse(o)), &rotate(true)).unwrap();
    assert_eq!(back.rotated, Some(inverse(o)), "{what}");
    assert_eq!(pixels(&back.bytes), pixels(src), "{what}: not lossless");
    // Already upright: a second strip finds nothing and changes nothing.
    let again = pipeline::strip(&out.bytes, &rotate(true)).unwrap();
    assert!(
        again.removed.is_empty(),
        "{what}: residue {:?}",
        again.removed
    );
    assert_eq!(again.bytes, out.bytes, "{what}: not idempotent");
    assert_eq!(again.rotated, None, "{what}");
}

#[test]
fn auto_rotate_turns_every_orientation_upright_losslessly() {
    let (w, h) = (256, 192); // whole 16x16 iMCUs both ways
    let display = common::corner_base(w, h, 64);
    for (sub, label) in [((2, 2), "4:2:0"), ((2, 1), "4:2:2"), ((1, 1), "4:4:4")] {
        for progressive in [false, true] {
            for o in 1..=8u8 {
                let what = format!("{label} progressive={progressive} o={o}");
                let (stored, sw, sh) = common::store_for_orientation(&display, w, h, o);
                let exif = exif_app1(o.into());
                let src = jpeg_coded(&stored, (sw, sh), sub, progressive, &[(1, &exif), (1, XMP)]);
                let out = pipeline::strip(&src, &rotate(true)).unwrap();
                if o == 1 {
                    assert_eq!(out.bytes, pipeline::strip(&src, &keep_icc()).unwrap().bytes);
                    assert_eq!((out.rotated, out.not_rotated), (None, None), "{what}");
                    continue;
                }
                assert_rotated(&src, &out, o, (w, h), &what);
                // Coded as the source was: baseline stays baseline.
                let sof = |b: &[u8]| common::jpeg_scans(b).0;
                assert_eq!(sof(&out.bytes), sof(&src), "{what}");
            }
        }
    }
}

/// Grey has one 8x8 block per iMCU, so 8-aligned edges suffice.
#[test]
fn auto_rotate_turns_a_grey_jpeg_on_8px_edges() {
    let (w, h) = (248, 184);
    let display = common::corner_base(w, h, 64);
    for o in 2..=8u8 {
        let (stored, sw, sh) = common::store_for_orientation(&display, w, h, o);
        let grey: Vec<u8> = stored
            .chunks(3)
            .map(|p| ((u32::from(p[0]) + u32::from(p[1]) + u32::from(p[2])) / 3) as u8)
            .collect();
        let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_GRAYSCALE);
        comp.set_fastest_defaults();
        comp.set_size(sw, sh);
        comp.set_quality(90.0);
        let mut started = comp.start_compress(Vec::new()).unwrap();
        started.write_marker(mozjpeg::Marker::APP(1), &exif_app1(o.into()));
        started.write_scanlines(&grey).unwrap();
        let src = started.finish().unwrap();
        let out = pipeline::strip(&src, &rotate(true)).unwrap();
        assert_eq!(out.rotated, Some(o), "o={o}");
        let back = pipeline::strip(&tagged(&out.bytes, inverse(o)), &rotate(true)).unwrap();
        assert_eq!(pixels(&back.bytes), pixels(&src), "o={o}: not lossless");
        assert_eq!(rgb(&out.bytes, false).0, w, "o={o}");
    }
}

/// A partial edge iMCU cannot move: the transforms that would move one
/// leave the file exactly as a plain strip does, and say why.
#[test]
fn auto_rotate_keeps_the_tag_when_an_edge_is_partial() {
    // 248 is not a multiple of 16, 192 is: only the transforms that
    // leave the right edge in place are exact (libjpeg's own rule,
    // `jtransform_perfect_transform`).
    let (w, h) = (248, 192);
    let px = common::corner_base(w, h, 64);
    for o in 2..=8u8 {
        let exif = exif_app1(o.into());
        let src = jpeg_coded(&px, (w, h), (2, 2), false, &[(1, &exif)]);
        let out = pipeline::strip(&src, &rotate(true)).unwrap();
        let exact = (4..=6).contains(&o);
        assert_eq!(out.rotated.is_some(), exact, "o={o}");
        if exact {
            let display = if o >= 5 { (h, w) } else { (w, h) };
            assert_eq!(rgb(&out.bytes, false).0, display.0, "o={o}");
            continue;
        }
        let plain = pipeline::strip(&src, &keep_icc()).unwrap();
        assert_eq!(out.bytes, plain.bytes, "o={o}");
        assert_eq!(out.orientation, Some(o), "o={o}");
        let why = out.not_rotated.unwrap();
        assert!(
            why.contains("248x192") && why.contains("16x16"),
            "o={o}: {why}"
        );
    }
}

#[test]
fn auto_rotate_keeps_the_icc_profile_and_aspect() {
    let (w, h) = (256, 192);
    let display = common::corner_base(w, h, 64);
    let icc = common::fake_icc(70_000);
    let chunks = common::app2_icc_payloads(&icc, 60_000);
    let (stored, sw, sh) = common::store_for_orientation(&display, w, h, 6);
    let exif = exif_app1(6);
    let src = jpeg_coded(
        &stored,
        (sw, sh),
        (2, 2),
        false,
        &[(1, &exif), (2, &chunks[0]), (2, &chunks[1])],
    );
    // Non-square pixels: 72x144 dpi, which a quarter turn exchanges.
    let mut src = src;
    let at = src.windows(5).position(|w| w == b"JFIF\0").unwrap();
    src[at + 7] = 1; // dots per inch
    src[at + 8..at + 12].copy_from_slice(&[0, 72, 0, 144]);
    let out = pipeline::strip(&src, &rotate(true)).unwrap();
    assert_rotated(&src, &out, 6, (w, h), "icc");
    assert_eq!(common::jpeg_icc(&out.bytes).as_deref(), Some(&icc[..]));
    let jfif = &jpeg_markers(&out.bytes)[0];
    assert_eq!(jfif.0, 0xE0);
    assert_eq!(jfif.1[7..12], [1, 0, 144, 0, 72], "density not exchanged");
    let dropped = pipeline::strip(&src, &rotate(false)).unwrap();
    assert_eq!(dropped.rotated, Some(6));
    assert_eq!(common::jpeg_icc(&dropped.bytes), None);
}

/// A source whose EXIF is already just the orientation (a plain strip's
/// output) now loses that block too, and says so.
#[test]
fn auto_rotate_reports_the_dropped_orientation_block() {
    let (w, h) = (256, 192);
    let (stored, sw, sh) = common::store_for_orientation(&common::corner_base(w, h, 64), w, h, 8);
    let exif = exif_app1(8);
    let src = jpeg_coded(&stored, (sw, sh), (2, 2), false, &[(1, &exif)]);
    let plain = pipeline::strip(&src, &keep_icc()).unwrap();
    assert_eq!(plain.orientation, Some(8));
    let out = pipeline::strip(&plain.bytes, &rotate(true)).unwrap();
    assert_eq!(out.rotated, Some(8));
    assert_eq!(removed_names(&out), ["APP1 Exif"]);
    assert_eq!(out.removed[0].bytes, 36);
}

/// Rotation must be exact or not happen: libjpeg would "repair" a
/// truncated scan with grey, so a warning keeps the tag instead.
#[test]
fn auto_rotate_leaves_a_truncated_jpeg_tagged() {
    let (w, h) = (256, 192);
    let px = common::corner_base(w, h, 64);
    let exif = exif_app1(6);
    let src = jpeg_coded(&px, (w, h), (2, 2), false, &[(1, &exif)]);
    let cut = &src[..src.len() * 2 / 3];
    let out = pipeline::strip(cut, &rotate(true)).unwrap();
    assert_eq!((out.rotated, out.orientation), (None, Some(6)));
    assert!(out.not_rotated.unwrap().contains("truncated"));
    assert_eq!(out.bytes, pipeline::strip(cut, &keep_icc()).unwrap().bytes);
}

/// PNG and WebP have no lossless rotation: the tag is kept, as without
/// the flag, and the reason names the format.
#[test]
fn auto_rotate_keeps_png_and_webp_tagged() {
    let (w, h) = (64, 48);
    let px = common::corner_base(w, h, 16);
    let png = common::png_with_orientation(&px, w, h, 6);
    let out = pipeline::strip(&png, &rotate(true)).unwrap();
    assert_eq!(out.bytes, pipeline::strip(&png, &keep_icc()).unwrap().bytes);
    assert_eq!((out.rotated, out.orientation), (None, Some(6)));
    assert!(out.not_rotated.unwrap().contains("image/png"));
    let webp = common::webp_with_exif(
        &fixture("photo.webp"),
        200,
        150,
        &common::tiff_orientation(3),
    );
    let out = pipeline::strip(&webp, &rotate(true)).unwrap();
    assert_eq!((out.rotated, out.orientation), (None, Some(3)));
    assert!(out.not_rotated.unwrap().contains("image/webp"));
    // Upright sources have nothing to explain.
    let upright = pipeline::strip(&fixture("rgb.png"), &rotate(true)).unwrap();
    assert_eq!((upright.rotated, upright.not_rotated), (None, None));
}
