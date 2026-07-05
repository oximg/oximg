//! Source metadata: EXIF orientation parsing and the pixel-space
//! orientation transforms.
//!
//! Rotation happens *after* the resize, on the small output frame:
//! Lanczos is separable and axis-symmetric, so resizing in the stored
//! orientation and then rotating is exactly equivalent to rotating
//! first and resizing with a swapped target box — which keeps every
//! streaming decode/resize path untouched and makes the transform an
//! O(output) copy instead of an O(input) one.

/// EXIF/TIFF orientation, 1..=8. 1 is upright; 5..=8 swap the axes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Orientation(u8);

impl Orientation {
    pub(crate) const UPRIGHT: Orientation = Orientation(1);

    pub(crate) fn is_upright(self) -> bool {
        self.0 == 1
    }

    /// Orientations 5..=8 exchange width and height on display.
    pub(crate) fn swaps_axes(self) -> bool {
        self.0 >= 5
    }

    /// The displayed dimensions of a `(w, h)` stored frame.
    pub(crate) fn display_dims(self, w: usize, h: usize) -> (usize, usize) {
        if self.swaps_axes() { (h, w) } else { (w, h) }
    }

    /// Parse the orientation out of a JPEG APP1 payload
    /// (`Exif\0\0` + TIFF). `None` for non-Exif APP1 segments (XMP
    /// also uses APP1) and for anything malformed — a broken tag must
    /// never fail the request, it just means "no orientation here".
    pub(crate) fn from_exif_app1(data: &[u8]) -> Option<Orientation> {
        parse_exif_orientation(data)
    }
}

/// Walk the TIFF structure far enough to find IFD0 tag 0x0112. Every
/// read is bounds-checked; `None` on any structural problem.
fn parse_exif_orientation(data: &[u8]) -> Option<Orientation> {
    let tiff = data.strip_prefix(b"Exif\0\0")?;
    let big_endian = match tiff.get(0..2)? {
        b"II" => false,
        b"MM" => true,
        _ => return None,
    };
    let u16_at = |off: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(off..off.checked_add(2)?)?.try_into().ok()?;
        Some(if big_endian {
            u16::from_be_bytes(b)
        } else {
            u16::from_le_bytes(b)
        })
    };
    let u32_at = |off: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(off..off.checked_add(4)?)?.try_into().ok()?;
        Some(if big_endian {
            u32::from_be_bytes(b)
        } else {
            u32::from_le_bytes(b)
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd0 = u32_at(4)? as usize;
    let entries = u16_at(ifd0)? as usize;
    // IFD entries are 12 bytes: tag u16, type u16, count u32, value u32.
    // 512 caps the walk against absurd declared counts; TIFF mandates
    // ascending tag order, so 0x0112 sits well inside any real IFD0.
    for i in 0..entries.min(512) {
        let e = ifd0.checked_add(2)?.checked_add(i * 12)?;
        if u16_at(e)? == 0x0112 {
            // Type SHORT (3), count 1: the value lives in the first two
            // bytes of the inline value field. Deliberately strict —
            // Chrome (Skia SkExif) and Firefox enforce exactly this, so
            // quirky writers (LONG-typed or count>1 entries, which
            // exiftool tolerates) render the same way our output does.
            if u16_at(e + 2)? != 3 || u32_at(e + 4)? != 1 {
                return None;
            }
            let v = u16_at(e + 8)?;
            return (1..=8).contains(&v).then_some(Orientation(v as u8));
        }
    }
    None
}

/// Scan cap: EXIF sits right after SOI in practice; a source whose
/// APP/COM preamble exceeds this is served unrotated rather than
/// buffered without bound.
const SCAN_CAP: usize = 256 * 1024;

/// Walk the leading JPEG segments of `reader`, buffering every byte
/// read into `prefix` (so the caller can re-chain it in front of the
/// remaining stream), and return the orientation from the *first* Exif
/// APP1 — matching the first-Exif-wins behavior of Chrome and Firefox,
/// including treating an orientation-less first Exif as upright.
///
/// Marker fill bytes (0xFF padding, B.1.1.2) are skipped like libjpeg
/// and Skia do. Scanning stops at the first non-APPn/COM marker
/// (tables/frame data follow), at [`SCAN_CAP`] buffered bytes, on EOF,
/// or on anything structurally bogus — all of which mean "no rotation"
/// and leave the real error handling to the decoder, which always sees
/// the byte-identical stream (every consumed byte lands in `prefix`,
/// even on a short final read). This replaces libjpeg-side marker
/// saving, whose per-request memory scales with the number of
/// attacker-supplied APP1 segments; here the buffer is hard-capped.
pub(crate) fn scan_jpeg_orientation<R: std::io::BufRead>(
    reader: &mut R,
    prefix: &mut Vec<u8>,
) -> Orientation {
    prefix.clear();
    // Read exactly `n` bytes into `prefix`, returning their start
    // offset. fill_buf/consume instead of read_exact so a short read
    // (EOF, I/O error) keeps whatever *was* consumed in `prefix` —
    // the re-chain is lossless unconditionally, not just when reads
    // fail atomically.
    let mut take = |prefix: &mut Vec<u8>, n: usize| -> Option<usize> {
        let start = prefix.len();
        if start + n > SCAN_CAP {
            return None;
        }
        let mut remaining = n;
        while remaining > 0 {
            let chunk = match reader.fill_buf() {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return None,
            };
            if chunk.is_empty() {
                return None; // EOF mid-read; partial bytes stay buffered
            }
            let k = chunk.len().min(remaining);
            prefix.extend_from_slice(&chunk[..k]);
            reader.consume(k);
            remaining -= k;
        }
        Some(start)
    };
    // SOI
    let Some(soi) = take(prefix, 2) else {
        return Orientation::UPRIGHT;
    };
    if prefix[soi..] != [0xFF, 0xD8] {
        return Orientation::UPRIGHT;
    }
    loop {
        let Some(m) = take(prefix, 2) else {
            return Orientation::UPRIGHT;
        };
        if prefix[m] != 0xFF {
            return Orientation::UPRIGHT;
        }
        // Markers may be padded with any number of 0xFF fill bytes
        // (B.1.1.2); libjpeg and Skia skip them, so the scan must too.
        let mut marker = prefix[m + 1];
        while marker == 0xFF {
            let Some(b) = take(prefix, 1) else {
                return Orientation::UPRIGHT;
            };
            marker = prefix[b];
        }
        // APPn (0xE0..=0xEF) and COM (0xFE) may precede the frame;
        // anything else ends the metadata preamble.
        if !((0xE0..=0xEF).contains(&marker) || marker == 0xFE) {
            return Orientation::UPRIGHT;
        }
        let Some(l) = take(prefix, 2) else {
            return Orientation::UPRIGHT;
        };
        let len = u16::from_be_bytes([prefix[l], prefix[l + 1]]) as usize;
        if len < 2 {
            return Orientation::UPRIGHT;
        }
        let Some(body) = take(prefix, len - 2) else {
            return Orientation::UPRIGHT;
        };
        if marker == 0xE1 && prefix[body..].starts_with(b"Exif\0\0") {
            // First Exif segment decides, orientation tag or not.
            return Orientation::from_exif_app1(&prefix[body..]).unwrap_or(Orientation::UPRIGHT);
        }
    }
}

/// Apply the orientation to interleaved `channels`-byte pixels,
/// writing the displayed frame into `dst` (resized as needed) and
/// returning the displayed dimensions. `dst` is fully overwritten.
///
/// Loops are destination-major (sequential writes, strided reads); the
/// whole frame is at most the fitted output (≤ ~0.8MB at 512²x3), so
/// this is a sub-millisecond pass.
/// TODO(simd-orient): the 90°/270° cases are strided-read transposes
/// that would benefit from cache-blocked/SIMD kernels if profiles ever
/// show them; at output sizes the scalar pass has not been worth it.
pub(crate) fn apply_orientation(
    src: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    o: Orientation,
    dst: &mut Vec<u8>,
) -> (usize, usize) {
    debug_assert!(src.len() >= w * h * channels);
    let (dw, dh) = o.display_dims(w, h);
    if dst.len() < dw * dh * channels {
        dst.resize(dw * dh * channels, 0);
    }
    dst.truncate(dw * dh * channels);
    // Source coordinates for display coordinate (x, y), derived from
    // the TIFF 6.0 orientation definitions and pinned by the corner
    // anchors in tests::every_orientation_matches_its_anchor.
    let src_xy: fn(usize, usize, usize, usize) -> (usize, usize) = match o.0 {
        1 => |x, y, _w, _h| (x, y),
        2 => |x, y, w, _h| (w - 1 - x, y),
        3 => |x, y, w, h| (w - 1 - x, h - 1 - y),
        4 => |x, y, _w, h| (x, h - 1 - y),
        5 => |x, y, _w, _h| (y, x),
        6 => |x, y, _w, h| (y, h - 1 - x),
        7 => |x, y, w, h| (w - 1 - y, h - 1 - x),
        _ => |x, y, w, _h| (w - 1 - y, x),
    };
    for y in 0..dh {
        let row = &mut dst[y * dw * channels..(y + 1) * dw * channels];
        for (x, px) in row.chunks_exact_mut(channels).enumerate() {
            let (sx, sy) = src_xy(x, y, w, h);
            let s = (sy * w + sx) * channels;
            px.copy_from_slice(&src[s..s + channels]);
        }
    }
    (dw, dh)
}

#[cfg(test)]
mod tests {
    use super::*;

    type U16Bytes = fn(u16) -> [u8; 2];
    type U32Bytes = fn(u32) -> [u8; 4];

    /// Minimal Exif APP1 payload with just an orientation tag.
    fn app1(orientation: u16, big_endian: bool) -> Vec<u8> {
        let mut v = b"Exif\0\0".to_vec();
        let (u16b, u32b): (U16Bytes, U32Bytes) = if big_endian {
            (u16::to_be_bytes, u32::to_be_bytes)
        } else {
            (u16::to_le_bytes, u32::to_le_bytes)
        };
        v.extend(if big_endian { *b"MM" } else { *b"II" });
        v.extend(u16b(42));
        v.extend(u32b(8)); // IFD0 right after the header
        v.extend(u16b(1)); // one entry
        v.extend(u16b(0x0112)); // orientation
        v.extend(u16b(3)); // SHORT
        v.extend(u32b(1)); // count
        v.extend(u16b(orientation));
        v.extend(u16b(0)); // value padding
        v.extend(u32b(0)); // next-IFD terminator
        v
    }

    #[test]
    fn parses_both_endiannesses_and_all_values() {
        for be in [false, true] {
            for o in 1..=8u16 {
                assert_eq!(
                    Orientation::from_exif_app1(&app1(o, be)),
                    Some(Orientation(o as u8)),
                    "o={o} be={be}"
                );
            }
        }
    }

    #[test]
    fn malformed_exif_is_none_not_an_error() {
        let good = app1(6, false);
        let cases: Vec<Vec<u8>> = vec![
            b"".to_vec(),
            b"Exif\0\0".to_vec(),
            b"Exif\0\0XX".to_vec(),
            good[..good.len() - 13].to_vec(), // truncated mid-entry
            app1(0, false),                   // out-of-range value
            app1(9, true),
            b"http://ns.adobe.com/xap/1.0/\0<x/>".to_vec(), // XMP APP1
            {
                let mut v = app1(6, false);
                v[12] = 0xFF; // absurd entry count with no data behind it
                v
            },
            // IFD0 offset near usize::MAX: `off + 2` must not overflow
            // (would panic on 32-bit targets without checked_add).
            b"Exif\0\0II\x2a\x00\xff\xff\xff\xff".to_vec(),
        ];
        for (i, c) in cases.iter().enumerate() {
            assert_eq!(Orientation::from_exif_app1(c), None, "case {i}");
        }
    }

    /// Deliberate browser parity (Chrome/Skia and Firefox reject
    /// these): an orientation entry that is not exactly SHORT/count==1
    /// must not rotate, however tolerant exiftool is of such writers.
    #[test]
    fn quirky_typed_orientation_entries_are_rejected() {
        let mut long_typed = app1(6, false);
        long_typed[18] = 4; // field type LONG instead of SHORT
        assert_eq!(Orientation::from_exif_app1(&long_typed), None);
        let mut multi_count = app1(6, false);
        multi_count[20] = 2; // count 2 instead of 1
        assert_eq!(Orientation::from_exif_app1(&multi_count), None);
    }

    /// Framed JPEG segment: marker byte + big-endian length + body.
    fn seg(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut v = vec![0xFF, marker];
        v.extend(((body.len() + 2) as u16).to_be_bytes());
        v.extend(body);
        v
    }

    /// Re-chaining `prefix` in front of the un-consumed remainder must
    /// reproduce the original stream byte for byte.
    fn assert_rechains(prefix: &[u8], rest: &[u8], original: &[u8]) {
        let mut rejoined = prefix.to_vec();
        rejoined.extend_from_slice(rest);
        assert_eq!(rejoined, original);
    }

    #[test]
    fn scanner_skips_xmp_and_first_exif_wins() {
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(seg(0xE0, b"JFIF\0"));
        jpeg.extend(seg(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x/>"));
        jpeg.extend(seg(0xE1, &app1(6, false)));
        jpeg.extend(seg(0xFE, b"comment"));
        jpeg.extend(b"\xFF\xDBrest of the stream");
        let mut r = &jpeg[..];
        let mut prefix = Vec::new();
        assert_eq!(
            scan_jpeg_orientation(&mut r, &mut prefix),
            Orientation(6),
            "XMP APP1 before the Exif must not mask it"
        );
        assert_rechains(&prefix, r, &jpeg);

        // An orientation-less first Exif pins upright; the later
        // oriented Exif must not override it (Chrome/Firefox parity).
        let no_tag = {
            let mut v = b"Exif\0\0II".to_vec();
            v.extend(42u16.to_le_bytes());
            v.extend(8u32.to_le_bytes());
            v.extend(0u16.to_le_bytes()); // zero IFD0 entries
            v.extend(0u32.to_le_bytes()); // next-IFD terminator
            v
        };
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(seg(0xE1, &no_tag));
        jpeg.extend(seg(0xE1, &app1(3, false)));
        jpeg.extend(b"\xFF\xDAtail");
        let mut r = &jpeg[..];
        assert_eq!(
            scan_jpeg_orientation(&mut r, &mut prefix),
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &jpeg);
    }

    /// Give-up paths must stay lossless: whatever was scanned is in
    /// `prefix` and the remainder is still in the reader, so the
    /// decoder sees the identical stream.
    #[test]
    fn scanner_gives_up_losslessly() {
        // Preamble past SCAN_CAP: served unrotated, nothing dropped.
        let mut jpeg = vec![0xFF, 0xD8];
        for _ in 0..5 {
            jpeg.extend(seg(0xFE, &vec![0xAB; 60_000]));
        }
        jpeg.extend(seg(0xE1, &app1(6, false)));
        jpeg.extend(b"tail");
        let mut r = &jpeg[..];
        let mut prefix = Vec::new();
        assert_eq!(
            scan_jpeg_orientation(&mut r, &mut prefix),
            Orientation::UPRIGHT
        );
        assert!(prefix.len() <= SCAN_CAP);
        assert_rechains(&prefix, r, &jpeg);

        // Not a JPEG at all: two sniffed bytes, re-chained intact.
        let png = b"\x89PNG\r\n\x1a\n....".to_vec();
        let mut r = &png[..];
        assert_eq!(
            scan_jpeg_orientation(&mut r, &mut prefix),
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &png);

        // Bogus segment length (< 2 can't frame its own field).
        let jpeg = b"\xFF\xD8\xFF\xE1\x00\x01junk".to_vec();
        let mut r = &jpeg[..];
        assert_eq!(
            scan_jpeg_orientation(&mut r, &mut prefix),
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &jpeg);

        // Truncated mid-segment: upright, and even the partial final
        // read stays buffered — lossless through EOF too.
        let jpeg = b"\xFF\xD8\xFF\xE1\x00\x30Exif".to_vec();
        let mut r = &jpeg[..];
        assert_eq!(
            scan_jpeg_orientation(&mut r, &mut prefix),
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &jpeg);
    }

    /// Spec-legal 0xFF fill bytes before a marker (B.1.1.2) must not
    /// hide the Exif segment: libjpeg and Skia skip them, so skipping
    /// them here keeps rotation in agreement with browsers.
    #[test]
    fn scanner_skips_marker_fill_bytes() {
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xFF]; // two fill bytes
        jpeg.extend(seg(0xE1, &app1(6, false)));
        jpeg.extend(b"\xFF\xDAtail");
        let mut r = &jpeg[..];
        let mut prefix = Vec::new();
        assert_eq!(scan_jpeg_orientation(&mut r, &mut prefix), Orientation(6));
        assert_rechains(&prefix, r, &jpeg);
    }

    /// 3x2 frame with unique pixel values; every orientation checked
    /// against a hand-derived anchor grid.
    #[test]
    fn every_orientation_matches_its_anchor() {
        let (w, h) = (3usize, 2usize);
        // pixel value = index, single channel keeps anchors readable
        let src: Vec<u8> = (0..(w * h) as u8).collect();
        // stored frame:
        //   0 1 2
        //   3 4 5
        let expect: [(u8, Vec<u8>); 8] = [
            (1, vec![0, 1, 2, 3, 4, 5]),
            (2, vec![2, 1, 0, 5, 4, 3]), // mirror horizontal
            (3, vec![5, 4, 3, 2, 1, 0]), // rotate 180
            (4, vec![3, 4, 5, 0, 1, 2]), // mirror vertical
            (5, vec![0, 3, 1, 4, 2, 5]), // transpose
            (6, vec![3, 0, 4, 1, 5, 2]), // rotate 90 CW
            (7, vec![5, 2, 4, 1, 3, 0]), // transverse
            (8, vec![2, 5, 1, 4, 0, 3]), // rotate 90 CCW
        ];
        for (o, want) in expect {
            let mut dst = Vec::new();
            let (dw, dh) = apply_orientation(&src, w, h, 1, Orientation(o), &mut dst);
            let swap = o >= 5;
            assert_eq!((dw, dh), if swap { (h, w) } else { (w, h) }, "o={o}");
            assert_eq!(dst, want, "o={o}");
        }
    }

    /// Multichannel pixels stay intact and the transforms compose the
    /// way the group structure says they should.
    #[test]
    fn transforms_compose_and_preserve_pixels() {
        let (w, h) = (5usize, 4usize);
        let src: Vec<u8> = (0..w * h * 3).map(|i| (i * 37 % 251) as u8).collect();
        // rot180 == flipH then flipV
        let (mut a, mut b, mut c) = (Vec::new(), Vec::new(), Vec::new());
        apply_orientation(&src, w, h, 3, Orientation(3), &mut a);
        apply_orientation(&src, w, h, 3, Orientation(2), &mut b);
        apply_orientation(&b, w, h, 3, Orientation(4), &mut c);
        assert_eq!(a, c, "rot180 == flipH∘flipV");
        // rot90CW then rot90CCW is the identity
        apply_orientation(&src, w, h, 3, Orientation(6), &mut a);
        apply_orientation(&a, h, w, 3, Orientation(8), &mut b);
        assert_eq!(b, src, "rot90 then rot270 is identity");
        // transpose twice is the identity
        apply_orientation(&src, w, h, 3, Orientation(5), &mut a);
        apply_orientation(&a, h, w, 3, Orientation(5), &mut b);
        assert_eq!(b, src, "transpose is an involution");
    }
}
