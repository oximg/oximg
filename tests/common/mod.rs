//! Shared helpers for the EXIF-orientation integration tests.
//!
//! The pixel transforms here are written independently from
//! `src/meta.rs` (simple source-major primitives composed per
//! orientation) so the tests cross-check the pipeline's inverse-mapping
//! implementation rather than mirroring it.
#![allow(dead_code)]

use oximg::pipeline::{self, Encoder, Params};

/// Read a committed fixture by name.
pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Square-box params at the jpegli default (q80, single-threaded,
/// source format).
pub fn params(max: u32) -> Params {
    Params {
        max_width: max,
        max_height: max,
        encoder: Encoder::Jpegli,
        ..Default::default()
    }
}

/// Output dimensions via the pipeline's own header probe.
pub fn dims_of(bytes: &[u8]) -> (usize, usize) {
    let (_, w, h) = pipeline::probe(bytes).unwrap();
    (w, h)
}


/// Minimal little-endian Exif APP1 payload carrying one orientation tag.
pub fn app1_orientation(o: u16) -> Vec<u8> {
    let mut v = b"Exif\0\0".to_vec();
    v.extend(*b"II");
    v.extend(42u16.to_le_bytes());
    v.extend(8u32.to_le_bytes()); // IFD0 offset
    v.extend(1u16.to_le_bytes()); // entry count
    v.extend(0x0112u16.to_le_bytes());
    v.extend(3u16.to_le_bytes()); // SHORT
    v.extend(1u32.to_le_bytes()); // count
    v.extend(o.to_le_bytes());
    v.extend(0u16.to_le_bytes()); // padding
    v.extend(0u32.to_le_bytes()); // next-IFD terminator
    v
}

/// RGB test frame with four solid, saturated corner blocks
/// (TL=red, TR=green, BL=blue, BR=white) on a gray field.
pub fn corner_base(w: usize, h: usize, block: usize) -> Vec<u8> {
    let mut px = vec![128u8; w * h * 3];
    let mut fill = |x0: usize, y0: usize, rgb: [u8; 3]| {
        for y in y0..y0 + block {
            for x in x0..x0 + block {
                px[(y * w + x) * 3..][..3].copy_from_slice(&rgb);
            }
        }
    };
    fill(0, 0, [255, 0, 0]);
    fill(w - block, 0, [0, 255, 0]);
    fill(0, h - block, [0, 0, 255]);
    fill(w - block, h - block, [255, 255, 255]);
    px
}

fn rot90cw(px: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let mut d = vec![0u8; px.len()];
    for y in 0..h {
        for x in 0..w {
            let (dx, dy) = (h - 1 - y, x);
            d[(dy * h + dx) * 3..][..3].copy_from_slice(&px[(y * w + x) * 3..][..3]);
        }
    }
    (d, h, w)
}

fn flip_h(px: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let mut d = vec![0u8; px.len()];
    for y in 0..h {
        for x in 0..w {
            d[(y * w + (w - 1 - x)) * 3..][..3].copy_from_slice(&px[(y * w + x) * 3..][..3]);
        }
    }
    (d, w, h)
}

fn transpose(px: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let mut d = vec![0u8; px.len()];
    for y in 0..h {
        for x in 0..w {
            d[(x * h + y) * 3..][..3].copy_from_slice(&px[(y * w + x) * 3..][..3]);
        }
    }
    (d, h, w)
}

fn rot180(px: &[u8], w: usize, h: usize) -> (Vec<u8>, usize, usize) {
    let (a, aw, ah) = rot90cw(px, w, h);
    rot90cw(&a, aw, ah)
}

/// Produce the *stored* pixels whose EXIF-oriented display equals
/// `display`: apply the inverse of orientation `o`. Orientations
/// 1-5 and 7 are self-inverse; 6 and 8 invert to each other.
pub fn store_for_orientation(display: &[u8], w: usize, h: usize, o: u8) -> (Vec<u8>, usize, usize) {
    match o {
        1 => (display.to_vec(), w, h),
        2 => flip_h(display, w, h),
        3 => rot180(display, w, h),
        4 => {
            // flipV = flipH ∘ rot180
            let (a, aw, ah) = rot180(display, w, h);
            flip_h(&a, aw, ah)
        }
        5 => transpose(display, w, h),
        6 => {
            // display = rot90cw(stored) → stored = rot90cw³(display)
            let (a, aw, ah) = rot180(display, w, h);
            rot90cw(&a, aw, ah)
        }
        7 => {
            // transverse = rot180 ∘ transpose
            let (a, aw, ah) = transpose(display, w, h);
            rot180(&a, aw, ah)
        }
        8 => rot90cw(display, w, h), // display = rot90ccw(stored)
        _ => panic!("orientation {o}"),
    }
}

/// Valid little-endian Exif APP1 payload whose IFD0 has no
/// orientation tag (one XResolution entry instead).
pub fn app1_exif_no_orientation() -> Vec<u8> {
    let mut v = b"Exif\0\0".to_vec();
    v.extend(*b"II");
    v.extend(42u16.to_le_bytes());
    v.extend(8u32.to_le_bytes()); // IFD0 offset
    v.extend(1u16.to_le_bytes()); // entry count
    v.extend(0x011au16.to_le_bytes()); // XResolution
    v.extend(5u16.to_le_bytes()); // RATIONAL
    v.extend(1u32.to_le_bytes()); // count
    v.extend(26u32.to_le_bytes()); // value offset
    v.extend(0u32.to_le_bytes()); // next-IFD terminator
    v.extend(72u32.to_le_bytes()); // 72/1 dpi
    v.extend(1u32.to_le_bytes());
    v
}

/// Encode RGB pixels as q95 JPEG carrying the given APPn payloads in
/// order.
pub fn jpeg_with_markers(px: &[u8], w: usize, h: usize, markers: &[(u8, &[u8])]) -> Vec<u8> {
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(95.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    for (n, p) in markers {
        started.write_marker(mozjpeg::Marker::APP(*n), p);
    }
    started.write_scanlines(px).unwrap();
    started.finish().unwrap()
}

/// Encode RGB pixels as q95 JPEG carrying the given APP1 payloads in
/// order.
pub fn jpeg_with_app1s(px: &[u8], w: usize, h: usize, payloads: &[&[u8]]) -> Vec<u8> {
    let markers: Vec<(u8, &[u8])> = payloads.iter().map(|p| (1u8, *p)).collect();
    jpeg_with_markers(px, w, h, &markers)
}

/// Deterministic pseudo-profile; pass-through treats ICC bytes as
/// opaque, so the tests only need a recognizable byte pattern.
pub fn fake_icc(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 131 + 7) % 251) as u8).collect()
}

/// Split a profile into standard APP2 `ICC_PROFILE` chunk payloads.
pub fn app2_icc_payloads(icc: &[u8], chunk: usize) -> Vec<Vec<u8>> {
    let count = icc.len().div_ceil(chunk);
    icc.chunks(chunk)
        .enumerate()
        .map(|(i, part)| {
            let mut v = b"ICC_PROFILE\0".to_vec();
            v.push((i + 1) as u8);
            v.push(count as u8);
            v.extend(part);
            v
        })
        .collect()
}

/// Reassemble the APP2 ICC chain from a JPEG — an independent walker
/// enforcing libjpeg's `jpeg_read_icc_profile` rules (1-based
/// sequence, agreed count, no duplicates, all present), so an
/// encoder-side chunking bug that real consumers reject fails the
/// tests here too.
pub fn jpeg_icc(b: &[u8]) -> Option<Vec<u8>> {
    let mut chunks: Vec<Option<Vec<u8>>> = Vec::new();
    let mut i = 2;
    while i + 4 <= b.len() {
        if b[i] != 0xFF {
            break;
        }
        let m = b[i + 1];
        if m == 0xDA || m == 0xD9 {
            break;
        }
        let len = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
        let body = b.get(i + 4..i + 2 + len)?;
        if m == 0xE2 && body.starts_with(b"ICC_PROFILE\0") && body.len() >= 14 {
            let (seq, count) = (body[12] as usize, body[13] as usize);
            if seq == 0 || count == 0 || seq > count {
                return None;
            }
            if chunks.is_empty() {
                chunks = vec![None; count];
            }
            if chunks.len() != count || chunks[seq - 1].is_some() {
                return None;
            }
            chunks[seq - 1] = Some(body[14..].to_vec());
        }
        i += 2 + len;
    }
    if chunks.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for c in chunks {
        out.extend_from_slice(&c?);
    }
    (!out.is_empty()).then_some(out)
}

/// Read the iCCP profile from a PNG via the png crate.
pub fn png_icc(b: &[u8]) -> Option<Vec<u8>> {
    let r = png::Decoder::new(std::io::Cursor::new(b))
        .read_info()
        .ok()?;
    r.info().icc_profile.as_ref().map(|c| c.to_vec())
}

/// Find the ICCP chunk in a WebP container by walking the RIFF chunks.
pub fn webp_icc(b: &[u8]) -> Option<Vec<u8>> {
    if b.len() < 12 || &b[0..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return None;
    }
    let mut i = 12;
    while i + 8 <= b.len() {
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().ok()?) as usize;
        if &b[i..i + 4] == b"ICCP" {
            return b.get(i + 8..i + 8 + len).map(|s| s.to_vec());
        }
        i += 8 + len + (len & 1);
    }
    None
}

/// Encode RGB(A) pixels as a PNG carrying an iCCP profile.
pub fn png_with_icc_color(
    px: &[u8],
    w: usize,
    h: usize,
    icc: &[u8],
    color: png::ColorType,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut info = png::Info::with_size(w as u32, h as u32);
    info.icc_profile = Some(std::borrow::Cow::Borrowed(icc));
    let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
    enc.set_color(color);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(px).unwrap();
    writer.finish().unwrap();
    out
}

/// Encode RGB pixels as a PNG carrying an iCCP profile.
pub fn png_with_icc(px: &[u8], w: usize, h: usize, icc: &[u8]) -> Vec<u8> {
    png_with_icc_color(px, w, h, icc, png::ColorType::Rgb)
}

/// Raw little-endian TIFF payload with one orientation entry — what
/// PNG `eXIf` and (per spec) WebP `EXIF` chunks carry.
pub fn tiff_orientation(o: u16) -> Vec<u8> {
    app1_orientation(o)[6..].to_vec()
}

/// Encode RGB pixels as a PNG carrying an `eXIf` orientation chunk.
pub fn png_with_orientation(px: &[u8], w: usize, h: usize, o: u16) -> Vec<u8> {
    let mut out = Vec::new();
    let mut info = png::Info::with_size(w as u32, h as u32);
    info.exif_metadata = Some(std::borrow::Cow::Owned(tiff_orientation(o)));
    let mut enc = png::Encoder::with_info(&mut out, info).unwrap();
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(px).unwrap();
    writer.finish().unwrap();
    out
}

/// Wrap a bare (VP8/VP8L) WebP of known canvas size in a VP8X
/// container carrying an `EXIF` chunk (placed after the image data,
/// where the spec puts it) — RIFF surgery independent of libwebp's
/// mux.
pub fn webp_with_exif(webp: &[u8], w: usize, h: usize, exif: &[u8]) -> Vec<u8> {
    assert_eq!(&webp[0..4], b"RIFF");
    assert_eq!(&webp[8..12], b"WEBP");
    assert!(
        &webp[12..16] == b"VP8 " || &webp[12..16] == b"VP8L",
        "helper expects a bare container"
    );
    let push_chunk = |chunks: &mut Vec<u8>, four: &[u8; 4], body: &[u8]| {
        chunks.extend_from_slice(four);
        chunks.extend((body.len() as u32).to_le_bytes());
        chunks.extend_from_slice(body);
        if body.len() % 2 == 1 {
            chunks.push(0);
        }
    };
    let mut chunks = Vec::new();
    let mut vp8x = vec![0x08u8, 0, 0, 0]; // EXIF flag
    vp8x.extend(&((w - 1) as u32).to_le_bytes()[..3]);
    vp8x.extend(&((h - 1) as u32).to_le_bytes()[..3]);
    push_chunk(&mut chunks, b"VP8X", &vp8x);
    chunks.extend_from_slice(&webp[12..]); // image data
    push_chunk(&mut chunks, b"EXIF", exif);
    let mut out = b"RIFF".to_vec();
    out.extend(((chunks.len() + 4) as u32).to_le_bytes());
    out.extend(b"WEBP");
    out.extend(chunks);
    out
}

/// Wrap a bare (VP8/VP8L) WebP of known canvas size in a VP8X
/// container carrying an ICCP chunk — RIFF surgery independent of
/// libwebp's mux.
pub fn webp_with_icc(webp: &[u8], w: usize, h: usize, icc: &[u8]) -> Vec<u8> {
    assert_eq!(&webp[0..4], b"RIFF");
    assert_eq!(&webp[8..12], b"WEBP");
    assert!(
        &webp[12..16] == b"VP8 " || &webp[12..16] == b"VP8L",
        "helper expects a bare container"
    );
    let mut chunks = Vec::new();
    let push_chunk = |chunks: &mut Vec<u8>, four: &[u8; 4], body: &[u8]| {
        chunks.extend_from_slice(four);
        chunks.extend((body.len() as u32).to_le_bytes());
        chunks.extend_from_slice(body);
        if body.len() % 2 == 1 {
            chunks.push(0);
        }
    };
    let mut vp8x = vec![0x20u8, 0, 0, 0]; // ICC flag
    vp8x.extend(&((w - 1) as u32).to_le_bytes()[..3]);
    vp8x.extend(&((h - 1) as u32).to_le_bytes()[..3]);
    push_chunk(&mut chunks, b"VP8X", &vp8x);
    push_chunk(&mut chunks, b"ICCP", icc);
    chunks.extend_from_slice(&webp[12..]);
    let mut out = b"RIFF".to_vec();
    out.extend(((chunks.len() + 4) as u32).to_le_bytes());
    out.extend(b"WEBP");
    out.extend(chunks);
    out
}

/// Encode RGB pixels as q95 JPEG, optionally with an Exif APP1.
pub fn jpeg_with_orientation(px: &[u8], w: usize, h: usize, o: Option<u16>) -> Vec<u8> {
    match o {
        Some(o) => jpeg_with_app1s(px, w, h, &[&app1_orientation(o)]),
        None => jpeg_with_app1s(px, w, h, &[]),
    }
}

/// Classify a pixel as one of the four corner colors (or gray).
pub fn classify(px: &[u8]) -> char {
    let (r, g, b) = (px[0] as i32, px[1] as i32, px[2] as i32);
    if r > 180 && g > 180 && b > 180 {
        'W'
    } else if r > g + 60 && r > b + 60 {
        'R'
    } else if g > r + 60 && g > b + 60 {
        'G'
    } else if b > r + 60 && b > g + 60 {
        'B'
    } else {
        '?'
    }
}

/// Decode a JPEG and classify the four corner blocks (sampled inset).
pub fn corner_classes(jpeg: &[u8]) -> (usize, usize, [char; 4]) {
    let dec = mozjpeg::Decompress::new_mem(jpeg).unwrap();
    let mut rgb = dec.rgb().unwrap();
    let (w, h) = (rgb.width(), rgb.height());
    let px: Vec<[u8; 3]> = rgb.read_scanlines().unwrap();
    rgb.finish().unwrap();
    let at = |x: usize, y: usize| classify(&px[y * w + x]);
    let (ix, iy) = (w / 8, h / 8); // inset well inside the corner blocks
    (
        w,
        h,
        [
            at(ix, iy),
            at(w - 1 - ix, iy),
            at(ix, h - 1 - iy),
            at(w - 1 - ix, h - 1 - iy),
        ],
    )
}
