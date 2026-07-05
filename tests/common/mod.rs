//! Shared helpers for the EXIF-orientation integration tests.
//!
//! The pixel transforms here are written independently from
//! `src/meta.rs` (simple source-major primitives composed per
//! orientation) so the tests cross-check the pipeline's inverse-mapping
//! implementation rather than mirroring it.
#![allow(dead_code)]

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

/// Encode RGB pixels as q95 JPEG carrying the given APP1 payloads in
/// order.
pub fn jpeg_with_app1s(px: &[u8], w: usize, h: usize, payloads: &[&[u8]]) -> Vec<u8> {
    let mut comp = mozjpeg::Compress::new(mozjpeg::ColorSpace::JCS_RGB);
    comp.set_size(w, h);
    comp.set_quality(95.0);
    let mut started = comp.start_compress(Vec::new()).unwrap();
    for p in payloads {
        started.write_marker(mozjpeg::Marker::APP(1), p);
    }
    started.write_scanlines(px).unwrap();
    started.finish().unwrap()
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
