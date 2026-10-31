//! Lossless metadata strip (#127): drop EXIF, XMP, IPTC, comments and
//! other non-rendering blocks from a JPEG, PNG or WebP container
//! without decoding a pixel. The compressed image data is copied byte
//! for byte, so memory is O(file size) and quality is untouched — the
//! alternative, a `0 0` re-encode, costs a generation and the
//! pipeline's largest frame buffers.
//!
//! What survives is an allowlist of what rendering needs, not a
//! denylist of known metadata: an unknown block is dropped, because a
//! strip that misses a vendor's private GPS chunk has failed at its
//! one job. Two things are written back or refused rather than lost:
//!
//! - **Orientation.** Dropping EXIF would display a rotated phone photo
//!   sideways, so a non-upright orientation is written back as an
//!   EXIF block holding that one tag ([`Orientation::minimal_exif_tiff`]),
//!   in place of the first EXIF block — the one browsers honor.
//! - **CMYK profiles.** Browsers render CMYK JPEG pixels without their
//!   profile in visibly wrong colours, so dropping a CMYK frame's ICC
//!   is refused; `resize` converts such a source to sRGB instead.

use super::{Error, ImageFormat};
use crate::meta::Orientation;
use anyhow::{Result, bail, ensure};
use std::collections::HashMap;

/// What [`strip`] may keep besides the image data.
#[derive(Clone, Copy, Debug, Default)]
pub struct StripParams {
    /// Keep the embedded ICC profile. `None` resolves like
    /// [`Params::icc`](super::Params::icc): `OXIMG_ICC`, else keep —
    /// a wide-gamut photo stripped of its profile renders desaturated.
    pub icc: Option<bool>,
}

/// The result of [`strip`].
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Stripped {
    pub bytes: Vec<u8>,
    pub format: ImageFormat,
    /// What was dropped, one entry per kind of block in the order first
    /// seen. Empty when the source carried nothing to drop.
    pub removed: Vec<Removed>,
    /// The orientation written back (2..=8), or `None` when the source
    /// was upright or carried none.
    pub orientation: Option<u8>,
}

/// One kind of dropped block: a JPEG segment (`APP1 Exif`, `XMP`,
/// `COM`, ...), a PNG chunk type, a WebP chunk FourCC, or `trailer` for
/// bytes after the end of the image.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Removed {
    pub name: String,
    pub count: usize,
    /// Bytes the blocks occupied in the source, headers included.
    pub bytes: usize,
}

/// Strip metadata from a JPEG, PNG or WebP without re-encoding.
/// Any other format, or a container too broken to walk, is an
/// [`ErrorKind::Undecodable`](super::ErrorKind::Undecodable) error, as
/// is dropping the profile of a CMYK JPEG.
///
/// # Examples
///
/// ```
/// use oximg::pipeline::{self, ImageFormat, StripParams};
///
/// let bytes = std::fs::read(concat!(
///     env!("CARGO_MANIFEST_DIR"),
///     "/tests/fixtures/photo.jpg"
/// ))?;
/// let out = pipeline::strip(&bytes, &StripParams::default())?;
/// assert_eq!(out.format, ImageFormat::Jpeg);
/// assert!(out.bytes.len() <= bytes.len());
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
pub fn strip(bytes: &[u8], p: &StripParams) -> Result<Stripped, Error> {
    let keep_icc = p
        .icc
        .unwrap_or_else(|| crate::config::config().icc_passthrough);
    strip_inner(bytes, keep_icc).map_err(|e| Error::classify(e, false))
}

fn strip_inner(bytes: &[u8], keep_icc: bool) -> Result<Stripped> {
    let mut header = [0u8; 12];
    let n = bytes.len().min(12);
    header[..n].copy_from_slice(&bytes[..n]);
    let mut log = Log::default();
    let (format, out, orientation) = match ImageFormat::sniff(&header) {
        Some(ImageFormat::Jpeg) => {
            let (out, o) = strip_jpeg(bytes, keep_icc, &mut log)?;
            (ImageFormat::Jpeg, out, o)
        }
        Some(ImageFormat::Png) => {
            let (out, o) = strip_png(bytes, keep_icc, &mut log)?;
            (ImageFormat::Png, out, o)
        }
        Some(ImageFormat::Webp) => {
            let (out, o) = strip_webp(bytes, keep_icc, &mut log)?;
            (ImageFormat::Webp, out, o)
        }
        Some(other) => bail!(
            "metadata strip supports JPEG, PNG and WebP, not {}",
            other.content_type()
        ),
        None => bail!("unsupported image format"),
    };
    Ok(Stripped {
        bytes: out,
        format,
        removed: log.removed,
        orientation: (!orientation.is_upright()).then_some(orientation.value()),
    })
}

/// Removals aggregated by name in first-seen order. Names carry up to
/// 24 bytes of the source (a JPEG APP identifier), so a hostile file
/// can make every block a new name: the index keeps that linear.
#[derive(Default)]
struct Log {
    removed: Vec<Removed>,
    index: HashMap<String, usize>,
}

impl Log {
    fn add(&mut self, name: &str, bytes: usize) {
        match self.index.get(name) {
            Some(&at) => {
                let r = &mut self.removed[at];
                r.count += 1;
                r.bytes += bytes;
            }
            None => {
                self.index.insert(name.to_string(), self.removed.len());
                self.removed.push(Removed {
                    name: name.to_string(),
                    count: 1,
                    bytes,
                });
            }
        }
    }
}

/// Walk every marker segment of the JPEG, between scans too (APPn and
/// COM are legal there), copying entropy-coded data through untouched.
/// Kept: APP0 JFIF, APP2 ICC_PROFILE (when `keep_icc`), APP14 Adobe —
/// its transform flag is what tells a decoder CMYK from YCCK — and
/// every non-APP, non-COM segment (frame, tables, scans). Bytes after
/// EOI are dropped: that is where MPF secondary images (a second full
/// picture with its own EXIF) live. A stream that ends without EOI is
/// copied as far as it goes, as decoders and browsers accept it.
fn strip_jpeg(b: &[u8], keep_icc: bool, log: &mut Log) -> Result<(Vec<u8>, Orientation)> {
    ensure!(b.starts_with(&[0xFF, 0xD8]), "not a JPEG");
    let mut out = Vec::with_capacity(b.len());
    out.extend_from_slice(&b[..2]);
    let mut i = 2;
    let mut exif_seen = false;
    let mut orientation = Orientation::UPRIGHT;
    let mut components = None;
    let mut icc_dropped = false;
    loop {
        ensure!(i < b.len(), "truncated JPEG header at offset {i}");
        ensure!(b[i] == 0xFF, "JPEG marker expected at offset {i}");
        // Fill bytes (B.1.1.2) before the marker code are not copied.
        while i < b.len() && b[i] == 0xFF {
            i += 1;
        }
        ensure!(i < b.len(), "truncated JPEG marker");
        let marker = b[i];
        i += 1;
        if marker == 0xD9 {
            out.extend_from_slice(&[0xFF, 0xD9]);
            if i < b.len() {
                log.add("trailer", b.len() - i);
            }
            break;
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            out.extend_from_slice(&[0xFF, marker]);
            continue;
        }
        ensure!(i + 2 <= b.len(), "truncated JPEG segment length");
        let len = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
        ensure!(len >= 2 && i + len <= b.len(), "truncated JPEG segment");
        let body = &b[i + 2..i + len];
        let seg = (i - 2)..(i + len);
        i += len;
        // Set when a dropped block is replaced by identical bytes (an
        // already-stripped orientation): nothing was removed.
        let mut rewrote = false;
        let keep = match marker {
            0xE0 => body.starts_with(b"JFIF\0"),
            0xE1 => {
                if !exif_seen && body.starts_with(b"Exif\0\0") {
                    // The first Exif segment decides, as in the decode
                    // path (`meta::scan_jpeg_meta`) and browsers.
                    exif_seen = true;
                    orientation = Orientation::from_exif_app1(body).unwrap_or(Orientation::UPRIGHT);
                    if !orientation.is_upright() {
                        let tiff = orientation.minimal_exif_tiff();
                        rewrote = body.strip_prefix(b"Exif\0\0") == Some(&tiff[..]);
                        out.extend_from_slice(&[0xFF, 0xE1]);
                        out.extend_from_slice(&(2 + 6 + tiff.len() as u16).to_be_bytes());
                        out.extend_from_slice(b"Exif\0\0");
                        out.extend_from_slice(&tiff);
                    }
                }
                false
            }
            0xE2 if body.starts_with(b"ICC_PROFILE\0") => {
                icc_dropped |= !keep_icc;
                keep_icc
            }
            0xEE => body.starts_with(b"Adobe"),
            0xE2..=0xEF | 0xFE => false,
            // SOFn; C4 (DHT), C8 (JPG) and CC (DAC) share the range.
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                components = body.get(5).copied();
                true
            }
            _ => true,
        };
        if keep {
            out.extend_from_slice(&[0xFF, marker]);
            out.extend_from_slice(&b[seg.start + 2..seg.end]);
        } else if !rewrote {
            log.add(&jpeg_segment_name(marker, body), seg.len());
        }
        if marker == 0xDA {
            // Entropy-coded data runs to the next marker that is not a
            // stuffed 0x00, a restart marker, or fill.
            let start = i;
            loop {
                match b[i..].iter().position(|&x| x == 0xFF) {
                    None => {
                        i = b.len();
                        break;
                    }
                    Some(k) => i += k,
                }
                match b.get(i + 1) {
                    None => {
                        i = b.len();
                        break;
                    }
                    Some(0x00 | 0xD0..=0xD7) => i += 2,
                    Some(0xFF) => i += 1,
                    Some(_) => break,
                }
            }
            out.extend_from_slice(&b[start..i]);
            if i == b.len() {
                break;
            }
        }
    }
    if icc_dropped && components == Some(4) {
        bail!(
            "refusing to drop the ICC profile of a CMYK JPEG: browsers would render \
             it in the wrong colours (keep the profile, or re-encode with resize, \
             which converts to sRGB)"
        );
    }
    Ok((out, orientation))
}

/// `APP1 Exif`, `XMP`, `APP13 Photoshop 3.0`, `COM`, ...: the marker
/// plus the segment's identifier string where it has one.
fn jpeg_segment_name(marker: u8, body: &[u8]) -> String {
    if marker == 0xFE {
        return "COM".into();
    }
    if body.starts_with(b"http://ns.adobe.com/xap/1.0/\0") {
        return "XMP".into();
    }
    if body.starts_with(b"http://ns.adobe.com/xmp/extension/\0") {
        return "XMP extension".into();
    }
    let ident: String = body
        .iter()
        .take_while(|&&c| c != 0 && c.is_ascii_graphic() || c == b' ')
        .take(24)
        .map(|&c| c as char)
        .collect();
    let app = format!("APP{}", marker - 0xE0);
    match ident.trim() {
        "" => app,
        id => format!("{app} {id}"),
    }
}

/// Ancillary PNG chunks that change what is displayed. Critical chunks
/// (uppercase first letter) are always kept; `iCCP` follows the ICC
/// choice; everything else — `tEXt`/`zTXt`/`iTXt`, `eXIf`, `tIME`, and
/// any private chunk — is dropped.
const PNG_RENDERING: &[&[u8; 4]] = &[
    b"tRNS", b"gAMA", b"cHRM", b"sRGB", b"sBIT", b"bKGD", b"pHYs", b"hIST", b"cICP", b"mDCV",
    b"cLLI", b"acTL", b"fcTL", b"fdAT",
];

/// Copy kept chunks verbatim (their CRCs stay valid) up to IEND. A
/// pre-IDAT `eXIf` with a rotation is replaced by a minimal one; a
/// post-IDAT one is dropped outright, as browsers ignore it anyway
/// (`formats.rs`, the PNG decode).
fn strip_png(b: &[u8], keep_icc: bool, log: &mut Log) -> Result<(Vec<u8>, Orientation)> {
    ensure!(b.starts_with(b"\x89PNG\r\n\x1a\n"), "not a PNG");
    let mut out = Vec::with_capacity(b.len());
    out.extend_from_slice(&b[..8]);
    let mut i = 8;
    let mut seen_idat = false;
    let mut exif_seen = false;
    let mut orientation = Orientation::UPRIGHT;
    loop {
        ensure!(i + 8 <= b.len(), "truncated PNG: no IEND");
        let len = u32::from_be_bytes(b[i..i + 4].try_into().unwrap()) as usize;
        let ty: [u8; 4] = b[i + 4..i + 8].try_into().unwrap();
        let end = i
            .checked_add(12)
            .and_then(|e| e.checked_add(len))
            .filter(|&e| e <= b.len());
        let Some(end) = end else {
            bail!("truncated PNG chunk {}", String::from_utf8_lossy(&ty));
        };
        let data = &b[i + 8..i + 8 + len];
        let mut rewrote = false;
        let keep = match &ty {
            b"eXIf" => {
                if !seen_idat && !exif_seen {
                    exif_seen = true;
                    orientation =
                        Orientation::from_exif_payload(data).unwrap_or(Orientation::UPRIGHT);
                    if !orientation.is_upright() {
                        let tiff = orientation.minimal_exif_tiff();
                        rewrote = data == tiff;
                        write_png_chunk(&mut out, b"eXIf", &tiff);
                    }
                }
                false
            }
            b"iCCP" => keep_icc,
            t if t[0].is_ascii_uppercase() => true,
            t => PNG_RENDERING.contains(&t),
        };
        if keep {
            out.extend_from_slice(&b[i..end]);
        } else if !rewrote {
            log.add(&String::from_utf8_lossy(&ty), end - i);
        }
        seen_idat |= &ty == b"IDAT";
        i = end;
        if &ty == b"IEND" {
            if i < b.len() {
                log.add("trailer", b.len() - i);
            }
            break;
        }
    }
    Ok((out, orientation))
}

fn write_png_chunk(out: &mut Vec<u8>, ty: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(ty);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (ISO-HDLC, the PNG one), bitwise: it only ever covers the
/// 30-byte chunk written back, so a table would be wasted.
fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &byte in data {
        c ^= u32::from(byte);
        for _ in 0..8 {
            c = if c & 1 != 0 {
                (c >> 1) ^ 0xEDB8_8320
            } else {
                c >> 1
            };
        }
    }
    !c
}

/// WebP chunks that make up the image: kept, `ICCP` aside. `EXIF`,
/// `XMP ` and unknown chunks are dropped.
const WEBP_IMAGE: &[&[u8; 4]] = &[b"VP8X", b"VP8 ", b"VP8L", b"ALPH", b"ANIM"];

/// VP8X feature flags (WebP container spec, "Extended File Format").
const VP8X_ICC: u8 = 0x20;
const VP8X_EXIF: u8 = 0x08;
const VP8X_XMP: u8 = 0x04;

/// Filter the top-level RIFF chunks, then make the VP8X flags agree
/// with what is left and fix the RIFF size. Animation frames (`ANMF`)
/// are filtered too ([`strip_anmf`]) and rewritten with their new size.
fn strip_webp(b: &[u8], keep_icc: bool, log: &mut Log) -> Result<(Vec<u8>, Orientation)> {
    ensure!(
        b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP",
        "not a WebP"
    );
    let riff_end = (u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize)
        .checked_add(8)
        .filter(|&e| e <= b.len() && e >= 12);
    let Some(riff_end) = riff_end else {
        bail!("truncated WebP: RIFF size past end of file");
    };
    let mut out = Vec::with_capacity(riff_end);
    out.extend_from_slice(&b[..12]);
    let mut i = 12;
    let mut vp8x_flags = None;
    let mut exif_seen = false;
    let mut icc_kept = false;
    let mut orientation = Orientation::UPRIGHT;
    while i < riff_end {
        ensure!(i + 8 <= riff_end, "truncated WebP chunk header");
        let fourcc: [u8; 4] = b[i..i + 4].try_into().unwrap();
        let size = u32::from_le_bytes(b[i + 4..i + 8].try_into().unwrap()) as usize;
        let data_end = (i + 8).checked_add(size).filter(|&e| e <= riff_end);
        let Some(data_end) = data_end else {
            bail!("truncated WebP chunk {}", String::from_utf8_lossy(&fourcc));
        };
        // Odd-sized chunks are padded to even; a writer that left the
        // final pad byte out is tolerated, and the pad restored.
        let end = (data_end + (size & 1)).min(riff_end);
        let data = &b[i + 8..data_end];
        let mut rewrote = false;
        let keep = match &fourcc {
            b"ICCP" => {
                icc_kept |= keep_icc;
                keep_icc
            }
            b"EXIF" => {
                if !exif_seen {
                    exif_seen = true;
                    orientation =
                        Orientation::from_exif_payload(data).unwrap_or(Orientation::UPRIGHT);
                    // A simple-format file (no VP8X) cannot flag EXIF,
                    // so nothing would read a written-back chunk.
                    if !orientation.is_upright() && vp8x_flags.is_some() {
                        let tiff = orientation.minimal_exif_tiff();
                        rewrote = data == tiff;
                        out.extend_from_slice(b"EXIF");
                        out.extend_from_slice(&(tiff.len() as u32).to_le_bytes());
                        out.extend_from_slice(&tiff);
                    } else {
                        orientation = Orientation::UPRIGHT;
                    }
                }
                false
            }
            b"ANMF" => {
                let frame = strip_anmf(data, log)?;
                out.extend_from_slice(b"ANMF");
                out.extend_from_slice(&u32::try_from(frame.len())?.to_le_bytes());
                out.extend_from_slice(&frame);
                if frame.len() & 1 == 1 {
                    out.push(0);
                }
                // Written above; what it dropped is logged per subchunk.
                rewrote = true;
                false
            }
            t => WEBP_IMAGE.contains(&t),
        };
        if keep {
            if &fourcc == b"VP8X" {
                ensure!(size >= 10, "truncated WebP VP8X chunk");
                vp8x_flags = Some(out.len() + 8);
            }
            out.extend_from_slice(&b[i..end]);
            if (end - i) & 1 == 1 {
                out.push(0);
            }
        } else if !rewrote {
            log.add(&String::from_utf8_lossy(&fourcc), end - i);
        }
        i = end;
    }
    if riff_end < b.len() {
        log.add("trailer", b.len() - riff_end);
    }
    if let Some(at) = vp8x_flags {
        let mut flags = out[at] & !(VP8X_EXIF | VP8X_XMP | VP8X_ICC);
        if icc_kept {
            flags |= VP8X_ICC;
        }
        if !orientation.is_upright() {
            flags |= VP8X_EXIF;
        }
        out[at] = flags;
    }
    let riff_size = u32::try_from(out.len() - 8)?;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Ok((out, orientation))
}

/// An `ANMF` payload is a 16-byte frame header, then subchunks: `ALPH`
/// and `VP8 `/`VP8L` are the frame, and the spec lets a writer append
/// unknown chunks after them, so private data can hide in a frame as
/// well as at the top level.
fn strip_anmf(data: &[u8], log: &mut Log) -> Result<Vec<u8>> {
    ensure!(data.len() >= 16, "truncated WebP ANMF chunk");
    let mut out = Vec::with_capacity(data.len());
    out.extend_from_slice(&data[..16]);
    let mut i = 16;
    while i < data.len() {
        ensure!(i + 8 <= data.len(), "truncated WebP frame chunk header");
        let fourcc: [u8; 4] = data[i..i + 4].try_into().unwrap();
        let size = u32::from_le_bytes(data[i + 4..i + 8].try_into().unwrap()) as usize;
        let data_end = (i + 8).checked_add(size).filter(|&e| e <= data.len());
        let Some(data_end) = data_end else {
            bail!(
                "truncated WebP frame chunk {}",
                String::from_utf8_lossy(&fourcc)
            );
        };
        let end = (data_end + (size & 1)).min(data.len());
        if matches!(&fourcc, b"ALPH" | b"VP8 " | b"VP8L") {
            out.extend_from_slice(&data[i..end]);
            if (end - i) & 1 == 1 {
                out.push(0);
            }
        } else {
            log.add(&String::from_utf8_lossy(&fourcc), end - i);
        }
        i = end;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_the_png_check_value() {
        // The CRC-32/ISO-HDLC check value, and IEND's well-known CRC.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b"IEND"), 0xAE42_6082);
    }

    #[test]
    fn segment_names_carry_the_identifier() {
        assert_eq!(jpeg_segment_name(0xE1, b"Exif\0\0MM"), "APP1 Exif");
        assert_eq!(
            jpeg_segment_name(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x/>"),
            "XMP"
        );
        assert_eq!(
            jpeg_segment_name(0xED, b"Photoshop 3.0\08BIM"),
            "APP13 Photoshop 3.0"
        );
        assert_eq!(jpeg_segment_name(0xFE, b"hello"), "COM");
        assert_eq!(jpeg_segment_name(0xE5, b"\x01\x02"), "APP5");
    }
}
