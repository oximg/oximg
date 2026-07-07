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
        parse_tiff_orientation(data.strip_prefix(b"Exif\0\0")?)
    }

    /// Parse an EXIF payload that may or may not carry the JPEG-style
    /// `Exif\0\0` prefix: PNG `eXIf` and WebP `EXIF` chunks are raw
    /// TIFF per their specs, but writers disagree and browsers accept
    /// both, so we do too.
    pub(crate) fn from_exif_payload(data: &[u8]) -> Option<Orientation> {
        parse_tiff_orientation(data.strip_prefix(b"Exif\0\0").unwrap_or(data))
    }

    /// Compose a CCW quarter-turn rotation applied *first* (HEIF
    /// `irot` angle) with an optional mirror applied *second* (HEIF
    /// 2022 `imir` mode: 0 exchanges top/bottom, 1 exchanges
    /// left/right) into the equivalent EXIF orientation. MIAF fixes
    /// exactly this application order (rotation, then mirror). Pinned
    /// against independent transform composition in tests and against
    /// libheif's rendering of the AVIF fixtures (avifdec does not
    /// apply the transforms; libheif — ImageMagick, most viewers —
    /// does).
    // Callers live behind the avif feature; the unit tests pin the
    // table unconditionally.
    #[cfg_attr(not(feature = "avif"), allow(dead_code))]
    pub(crate) fn from_rot_mirror(angle_ccw: u8, mirror: Option<u8>) -> Orientation {
        Orientation(match (angle_ccw & 3, mirror.map(|m| m & 1)) {
            (0, None) => 1,
            (1, None) => 8,
            (2, None) => 3,
            (3, None) => 6,
            (0, Some(1)) => 2,
            (1, Some(1)) => 7,
            (2, Some(1)) => 4,
            (3, Some(1)) => 5,
            (0, Some(_)) => 4,
            (1, Some(_)) => 5,
            (2, Some(_)) => 2,
            (3, Some(_)) => 7,
            _ => unreachable!("angle is masked to 0..=3"),
        })
    }
}

/// Walk the TIFF structure far enough to find IFD0 tag 0x0112. Every
/// read is bounds-checked; `None` on any structural problem.
fn parse_tiff_orientation(tiff: &[u8]) -> Option<Orientation> {
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

/// Scan cap: EXIF and ICC sit in the pre-frame header in practice; a
/// source whose header exceeds this is served without rotation or
/// profile rather than buffered without bound. Sized to fit the LUT
/// profiles print-workflow CMYK JPEGs actually embed (USWebCoatedSWOP
/// is ~557KB) — under a 256KB cap those sources would silently lose
/// their profile and degrade to the naive conversion.
const SCAN_CAP: usize = 1024 * 1024;

/// Metadata pulled from the JPEG header scan.
pub(crate) struct JpegMeta {
    pub(crate) orientation: Orientation,
    pub(crate) icc: Option<Vec<u8>>,
}

impl JpegMeta {
    pub(crate) const NONE: JpegMeta = JpegMeta {
        orientation: Orientation::UPRIGHT,
        icc: None,
    };
}

/// Reassembles the APP2 `ICC_PROFILE` chunk chain, enforcing the same
/// rules as libjpeg's `jpeg_read_icc_profile` (which browsers
/// ultimately follow): 1-based sequence numbers, a chunk count every
/// chunk agrees on, no duplicates, every chunk present. Any violation
/// poisons the whole chain — a broken profile must never ship, it just
/// means "no profile here".
#[derive(Default)]
struct IccAssembler {
    chunks: Vec<Option<Vec<u8>>>,
    broken: bool,
}

impl IccAssembler {
    /// Feed one APP2 payload; non-ICC APP2 segments are ignored.
    fn add(&mut self, body: &[u8]) {
        if self.broken {
            return;
        }
        let Some(rest) = body.strip_prefix(b"ICC_PROFILE\0") else {
            return;
        };
        if rest.len() < 2 {
            // libjpeg's marker_is_icc requires the full 14-byte
            // overhead before classifying a marker as ICC at all, so a
            // truncated header is "not ICC" (skip), not poison.
            return;
        }
        let (seq, count, data) = (rest[0] as usize, rest[1] as usize, &rest[2..]);
        if seq == 0 || count == 0 || seq > count {
            self.broken = true;
            return;
        }
        if self.chunks.is_empty() {
            self.chunks = vec![None; count];
        }
        if self.chunks.len() != count || self.chunks[seq - 1].is_some() {
            self.broken = true;
            return;
        }
        self.chunks[seq - 1] = Some(data.to_vec());
    }

    fn finish(self) -> Option<Vec<u8>> {
        if self.broken || self.chunks.is_empty() {
            return None;
        }
        let mut out = Vec::new();
        for c in self.chunks {
            out.extend_from_slice(&c?);
        }
        (!out.is_empty()).then_some(out)
    }
}

/// Walk the JPEG header segments of `reader`, buffering every byte
/// read into `prefix` (so the caller can re-chain it in front of the
/// remaining stream) and returning the orientation from the *first*
/// Exif APP1 — matching the first-Exif-wins behavior of Chrome and
/// Firefox, including treating an orientation-less first Exif as
/// upright — plus, when `want_icc`, the reassembled APP2 ICC profile.
///
/// The walk covers every length-framed segment up to SOS/EOI (the
/// same span libjpeg's marker saving covers); without `want_icc` it
/// short-circuits at the first Exif segment. Marker fill bytes (0xFF
/// padding, B.1.1.2) are skipped like libjpeg and Skia do. Scanning
/// stops at [`SCAN_CAP`] buffered bytes, on EOF, or on anything
/// structurally bogus — all of which mean "no metadata" and leave the
/// real error handling to the decoder, which always sees the
/// byte-identical stream (every consumed byte lands in `prefix`, even
/// on a short final read). This replaces libjpeg-side marker saving,
/// whose per-request memory scales with the number of
/// attacker-supplied APP1 segments; here the buffer is hard-capped.
pub(crate) fn scan_jpeg_meta<R: std::io::BufRead>(
    reader: &mut R,
    prefix: &mut Vec<u8>,
    want_icc: bool,
) -> JpegMeta {
    prefix.clear();
    let mut meta = JpegMeta::NONE;
    let mut exif_seen = false;
    let mut icc = IccAssembler::default();
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
    'walk: {
        // SOI
        let Some(soi) = take(prefix, 2) else {
            break 'walk;
        };
        if prefix[soi..] != [0xFF, 0xD8] {
            break 'walk;
        }
        loop {
            let Some(m) = take(prefix, 2) else {
                break 'walk;
            };
            if prefix[m] != 0xFF {
                break 'walk;
            }
            // Markers may be padded with any number of 0xFF fill bytes
            // (B.1.1.2); libjpeg and Skia skip them, so the scan must too.
            let mut marker = prefix[m + 1];
            while marker == 0xFF {
                let Some(b) = take(prefix, 1) else {
                    break 'walk;
                };
                marker = prefix[b];
            }
            // SOS/EOI end the header. Stray TEM/RSTn are standalone
            // (no length field); libjpeg's read_markers skips them and
            // keeps scanning, so the walk must too — otherwise a
            // profile behind one would be stripped from a file the
            // decoder accepts.
            if marker == 0xDA || marker == 0xD9 {
                break 'walk;
            }
            if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
                continue;
            }
            let Some(l) = take(prefix, 2) else {
                break 'walk;
            };
            let len = u16::from_be_bytes([prefix[l], prefix[l + 1]]) as usize;
            if len < 2 {
                break 'walk;
            }
            let Some(body) = take(prefix, len - 2) else {
                break 'walk;
            };
            if marker == 0xE1 && !exif_seen && prefix[body..].starts_with(b"Exif\0\0") {
                // First Exif segment decides, orientation tag or not.
                exif_seen = true;
                meta.orientation =
                    Orientation::from_exif_app1(&prefix[body..]).unwrap_or(Orientation::UPRIGHT);
                if !want_icc {
                    // Nothing left to look for.
                    break 'walk;
                }
            } else if want_icc && marker == 0xE2 {
                icc.add(&prefix[body..]);
            }
        }
    }
    if want_icc {
        meta.icc = icc.finish();
    }
    meta
}

/// Apply the orientation to interleaved `channels`-byte pixels,
/// writing the displayed frame into `dst` (resized as needed) and
/// returning the displayed dimensions. `dst` is fully overwritten.
///
/// Loops are destination-major (sequential writes, strided reads); the
/// whole frame is at most the fitted output (≤ ~0.8MB at 512²x3), so
/// this is a fast pass: the flip family reduces to (reversed) row
/// copies and the transpose family runs cache-blocked, all
/// monomorphized per channel count — measured 5-10x over the previous
/// generic per-pixel loop at output sizes. The generic loop survives
/// as [`apply_orientation_reference`], the oracle the specializations
/// are pinned against byte-for-byte.
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
    match channels {
        3 => orient_pixels::<3>(src, w, h, o, dst),
        4 => orient_pixels::<4>(src, w, h, o, dst),
        _ => apply_orientation_reference(src, w, h, channels, o, dst),
    }
    (dw, dh)
}

/// Channel-monomorphized orientation pass. `dst` is already sized to
/// the displayed frame.
fn orient_pixels<const C: usize>(src: &[u8], w: usize, h: usize, o: Orientation, dst: &mut [u8]) {
    match o.0 {
        1 => dst.copy_from_slice(&src[..w * h * C]),
        // Flip family: whole (reversed) row copies.
        2 => {
            for (drow, srow) in dst
                .chunks_exact_mut(w * C)
                .zip(src[..w * h * C].chunks_exact(w * C))
            {
                for (d, s) in drow.chunks_exact_mut(C).zip(srow.chunks_exact(C).rev()) {
                    d.copy_from_slice(s);
                }
            }
        }
        3 => {
            for (d, s) in dst
                .chunks_exact_mut(C)
                .zip(src[..w * h * C].chunks_exact(C).rev())
            {
                d.copy_from_slice(s);
            }
        }
        4 => {
            for (drow, srow) in dst
                .chunks_exact_mut(w * C)
                .zip(src[..w * h * C].chunks_exact(w * C).rev())
            {
                drow.copy_from_slice(srow);
            }
        }
        // Transpose family: cache-blocked tiles with the coordinate map
        // inlined per arm (a closure generic, not a fn pointer).
        5 => orient_tiles::<C>(src, w, h, h, w, dst, |x, y, _w, _h| (y, x)),
        6 => orient_tiles::<C>(src, w, h, h, w, dst, |x, y, _w, h| (y, h - 1 - x)),
        7 => orient_tiles::<C>(src, w, h, h, w, dst, |x, y, w, h| (w - 1 - y, h - 1 - x)),
        _ => orient_tiles::<C>(src, w, h, h, w, dst, |x, y, w, _h| (w - 1 - y, x)),
    }
}

/// Tiled destination-major copy; `f(x, y, w, h)` maps a display
/// coordinate to its stored source coordinate and inlines per call
/// site.
#[inline]
fn orient_tiles<const C: usize>(
    src: &[u8],
    w: usize,
    h: usize,
    dw: usize,
    dh: usize,
    dst: &mut [u8],
    f: impl Fn(usize, usize, usize, usize) -> (usize, usize) + Copy,
) {
    // 64px tiles keep both the strided reads and the sequential writes
    // inside L1 for 3-4 byte pixels.
    const B: usize = 64;
    for ty in (0..dh).step_by(B) {
        let y_end = (ty + B).min(dh);
        for tx in (0..dw).step_by(B) {
            let x_end = (tx + B).min(dw);
            for y in ty..y_end {
                let row = &mut dst[(y * dw + tx) * C..(y * dw + x_end) * C];
                for (px, x) in row.chunks_exact_mut(C).zip(tx..x_end) {
                    let (sx, sy) = f(x, y, w, h);
                    let s = (sy * w + sx) * C;
                    px.copy_from_slice(&src[s..s + C]);
                }
            }
        }
    }
}

/// The original per-pixel generic loop: the reference oracle for the
/// specializations above (and the fallback for unusual channel
/// counts). Source coordinates for display coordinate (x, y) derive
/// from the TIFF 6.0 orientation definitions and are pinned by the
/// corner anchors in tests::every_orientation_matches_its_anchor.
fn apply_orientation_reference(
    src: &[u8],
    w: usize,
    h: usize,
    channels: usize,
    o: Orientation,
    dst: &mut [u8],
) {
    let (dw, dh) = o.display_dims(w, h);
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
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
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
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
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
        while jpeg.len() <= SCAN_CAP {
            jpeg.extend(seg(0xFE, &vec![0xAB; 60_000]));
        }
        jpeg.extend(seg(0xE1, &app1(6, false)));
        jpeg.extend(b"tail");
        let mut r = &jpeg[..];
        let mut prefix = Vec::new();
        assert_eq!(
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
            Orientation::UPRIGHT
        );
        assert!(prefix.len() <= SCAN_CAP);
        assert_rechains(&prefix, r, &jpeg);

        // Not a JPEG at all: two sniffed bytes, re-chained intact.
        let png = b"\x89PNG\r\n\x1a\n....".to_vec();
        let mut r = &png[..];
        assert_eq!(
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &png);

        // Bogus segment length (< 2 can't frame its own field).
        let jpeg = b"\xFF\xD8\xFF\xE1\x00\x01junk".to_vec();
        let mut r = &jpeg[..];
        assert_eq!(
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &jpeg);

        // Truncated mid-segment: upright, and even the partial final
        // read stays buffered — lossless through EOF too.
        let jpeg = b"\xFF\xD8\xFF\xE1\x00\x30Exif".to_vec();
        let mut r = &jpeg[..];
        assert_eq!(
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
            Orientation::UPRIGHT
        );
        assert_rechains(&prefix, r, &jpeg);
    }

    /// APP2 payload for one ICC chunk.
    fn app2_icc(seq: u8, count: u8, data: &[u8]) -> Vec<u8> {
        let mut v = b"ICC_PROFILE\0".to_vec();
        v.push(seq);
        v.push(count);
        v.extend(data);
        v
    }

    /// Wrap segments into a JPEG-shaped stream ending in a DQT-like
    /// table and SOS so the header walk has a realistic footer.
    fn jpeg_stream(segments: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut jpeg = vec![0xFF, 0xD8];
        for (marker, body) in segments {
            jpeg.extend(seg(*marker, body));
        }
        jpeg.extend(seg(0xDB, &[0u8; 4])); // table segment
        jpeg.extend(b"\xFF\xDAentropy...");
        jpeg
    }

    fn scan_icc(jpeg: &[u8]) -> (Orientation, Option<Vec<u8>>) {
        let mut r = jpeg;
        let mut prefix = Vec::new();
        let m = scan_jpeg_meta(&mut r, &mut prefix, true);
        assert_rechains(&prefix, r, jpeg);
        (m.orientation, m.icc)
    }

    #[test]
    fn icc_chain_reassembles_in_sequence_order() {
        let profile: Vec<u8> = (0..600u32).map(|i| (i % 251) as u8).collect();
        // single chunk
        let jpeg = jpeg_stream(&[(0xE2, app2_icc(1, 1, &profile))]);
        assert_eq!(scan_icc(&jpeg).1.as_deref(), Some(&profile[..]));
        // two chunks, delivered out of order, Exif in between
        let jpeg = jpeg_stream(&[
            (0xE2, app2_icc(2, 2, &profile[300..])),
            (0xE1, app1(6, false)),
            (0xE2, app2_icc(1, 2, &profile[..300])),
        ]);
        let (o, icc) = scan_icc(&jpeg);
        assert_eq!(o, Orientation(6), "orientation and ICC coexist");
        assert_eq!(icc.as_deref(), Some(&profile[..]));
        // chunks may follow tables (the walk spans the whole header)
        let mut jpeg = vec![0xFF, 0xD8];
        jpeg.extend(seg(0xDB, &[0u8; 4]));
        jpeg.extend(seg(0xE2, &app2_icc(1, 1, &profile)));
        jpeg.extend(b"\xFF\xDAentropy");
        assert_eq!(scan_icc(&jpeg).1.as_deref(), Some(&profile[..]));
    }

    /// A USWebCoatedSWOP-sized profile (~557KB, the profile real
    /// print-workflow CMYK JPEGs actually embed) spans nine APP2
    /// chunks and must survive the scan cap — under a 256KB cap it
    /// silently degraded to no profile, which for a CMYK source means
    /// a silently naive conversion.
    #[test]
    fn half_megabyte_icc_chain_reassembles() {
        let profile: Vec<u8> = (0..570_000u32).map(|i| (i % 251) as u8).collect();
        let chunks: Vec<&[u8]> = profile.chunks(65_000).collect();
        let count = chunks.len() as u8;
        let segments: Vec<(u8, Vec<u8>)> = chunks
            .iter()
            .enumerate()
            .map(|(i, c)| (0xE2, app2_icc(i as u8 + 1, count, c)))
            .collect();
        let jpeg = jpeg_stream(&segments);
        assert_eq!(scan_icc(&jpeg).1.as_deref(), Some(&profile[..]));
    }

    #[test]
    fn broken_icc_chains_yield_no_profile() {
        let d = [7u8; 40];
        let cases: Vec<Vec<(u8, Vec<u8>)>> = vec![
            vec![(0xE2, app2_icc(1, 2, &d))], // missing chunk 2
            vec![(0xE2, app2_icc(1, 1, &d)), (0xE2, app2_icc(1, 1, &d))], // duplicate
            vec![(0xE2, app2_icc(1, 1, &d)), (0xE2, app2_icc(2, 2, &d))], // count mismatch
            vec![(0xE2, app2_icc(0, 1, &d))], // zero-based seq
            vec![(0xE2, app2_icc(2, 1, &d))], // seq > count
            vec![(0xE2, app2_icc(1, 1, &[]))], // empty profile
            vec![(0xE2, b"ICC_PROFILE\0".to_vec())], // truncated header
        ];
        for (i, segs) in cases.iter().enumerate() {
            assert_eq!(scan_icc(&jpeg_stream(segs)).1, None, "case {i}");
        }
        // non-ICC APP2 segments are ignored, not poison
        let profile = [9u8; 64];
        let jpeg = jpeg_stream(&[
            (0xE2, b"FPXR\0not-icc".to_vec()),
            (0xE2, app2_icc(1, 1, &profile)),
        ]);
        assert_eq!(scan_icc(&jpeg).1.as_deref(), Some(&profile[..]));
        // ...and so is a truncated "ICC_PROFILE\0" header: libjpeg's
        // marker_is_icc never classifies it as ICC, so it must not
        // poison a valid chain next to it.
        let jpeg = jpeg_stream(&[
            (0xE2, b"ICC_PROFILE\0".to_vec()),
            (0xE2, app2_icc(1, 1, &profile)),
        ]);
        assert_eq!(scan_icc(&jpeg).1.as_deref(), Some(&profile[..]));
    }

    /// Stray standalone markers (TEM, RSTn) are skipped exactly like
    /// libjpeg's read_markers skips them: metadata behind one must
    /// still be found, since the decoder accepts such files.
    #[test]
    fn stray_standalone_markers_do_not_end_the_walk() {
        let profile = [3u8; 48];
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xD0, 0xFF, 0x01]; // RST0, TEM
        jpeg.extend(seg(0xE1, &app1(6, false)));
        jpeg.extend(seg(0xE2, &app2_icc(1, 1, &profile)));
        jpeg.extend(b"\xFF\xDAtail");
        let mut r = &jpeg[..];
        let mut prefix = Vec::new();
        let m = scan_jpeg_meta(&mut r, &mut prefix, true);
        assert_eq!(m.orientation, Orientation(6));
        assert_eq!(m.icc.as_deref(), Some(&profile[..]));
        assert_rechains(&prefix, r, &jpeg);
    }

    #[test]
    fn icc_is_not_collected_when_unwanted() {
        let jpeg = jpeg_stream(&[(0xE2, app2_icc(1, 1, &[5u8; 16]))]);
        let mut r = &jpeg[..];
        let mut prefix = Vec::new();
        let m = scan_jpeg_meta(&mut r, &mut prefix, false);
        assert_eq!(m.icc, None);
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
        assert_eq!(
            scan_jpeg_meta(&mut r, &mut prefix, false).orientation,
            Orientation(6)
        );
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

    /// `from_rot_mirror` must equal composing the rotation and the
    /// mirror as two independent `apply_orientation` passes, for every
    /// combination — the table cannot drift from the transform code.
    #[test]
    fn rot_mirror_table_matches_composition() {
        let (w, h) = (4usize, 3usize);
        let src: Vec<u8> = (0..(w * h) as u8).collect();
        // irot angle as a pure-rotation orientation, applied first.
        let rot = [1u8, 8, 3, 6];
        // imir mode as a pure-mirror orientation, applied second:
        // 0 = top/bottom (flip vertical), 1 = left/right (flip horizontal).
        for angle in 0..4u8 {
            for mirror in [None, Some(0u8), Some(1u8)] {
                let (mut a, mut b) = (Vec::new(), Vec::new());
                let (rw, rh) =
                    apply_orientation(&src, w, h, 1, Orientation(rot[angle as usize]), &mut a);
                let (want, ww, wh) = match mirror {
                    None => (a.clone(), rw, rh),
                    Some(m) => {
                        let flip = if m == 1 { 2 } else { 4 };
                        let (fw, fh) = apply_orientation(&a, rw, rh, 1, Orientation(flip), &mut b);
                        (b.clone(), fw, fh)
                    }
                };
                let composed = Orientation::from_rot_mirror(angle, mirror);
                let mut got = Vec::new();
                let (gw, gh) = apply_orientation(&src, w, h, 1, composed, &mut got);
                assert_eq!((gw, gh), (ww, wh), "angle={angle} mirror={mirror:?}");
                assert_eq!(got, want, "angle={angle} mirror={mirror:?}");
            }
        }
    }

    /// Every specialization must match the reference oracle byte for
    /// byte, across orientations, channel counts, and odd/even/tiny
    /// dimensions (including tile-boundary sizes).
    #[test]
    fn specialized_orientation_matches_reference() {
        for &(w, h) in &[
            (1usize, 1usize),
            (2, 3),
            (5, 4),
            (31, 17),
            (64, 64),
            (65, 63),
            (129, 64),
        ] {
            for &channels in &[1usize, 3, 4] {
                let src: Vec<u8> = (0..w * h * channels)
                    .map(|i| (i * 89 % 251) as u8)
                    .collect();
                for o in 1..=8u8 {
                    let mut got = Vec::new();
                    let (dw, dh) =
                        apply_orientation(&src, w, h, channels, Orientation(o), &mut got);
                    let mut want = vec![0u8; dw * dh * channels];
                    apply_orientation_reference(&src, w, h, channels, Orientation(o), &mut want);
                    assert_eq!(got, want, "o={o} {w}x{h} c={channels}");
                }
            }
        }
    }

    /// Manual micro-benchmark for the rotation pass at output sizes.
    /// cargo test --release --features avif bench_orient -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_orient() {
        for (w, h) in [(512usize, 340usize), (340, 512), (512, 512), (120, 90)] {
            let src: Vec<u8> = (0..w * h * 3).map(|i| (i % 251) as u8).collect();
            let mut dst = Vec::new();
            for o in [2u8, 3, 6, 8] {
                // warm
                for _ in 0..3 {
                    apply_orientation(&src, w, h, 3, Orientation(o), &mut dst);
                }
                let n = 200;
                let t = std::time::Instant::now();
                for _ in 0..n {
                    apply_orientation(&src, w, h, 3, Orientation(o), &mut dst);
                }
                let us = t.elapsed().as_secs_f64() * 1e6 / n as f64;
                println!("orient o={o} {w}x{h}: {us:.1}µs");
            }
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
