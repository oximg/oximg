//! GIF decode. Input-only: nothing here encodes GIF, so a GIF request
//! comes back as whatever `@{fmt}` asked for, defaulting to WebP
//! (`super::default_target`). GIF-to-GIF was measured and rejected —
//! best case 80.7% of the source bytes against 25.2% for animated WebP
//! at the same visual score (docs/gif-evaluation.md §3).
//!
//! A GIF is a sequence of sub-rectangles painted onto one logical
//! screen, so even a *still* result needs the compositor: the first
//! frame may be smaller than the screen, and its palette may mark one
//! index transparent. This module renders that screen and hands the
//! pixels to the shared resize/encode path.
//!
//! Two things a GIF never carries, and so are absent here: an ICC
//! profile (the palette has nowhere to put one) and an EXIF orientation
//! (hence `fit_dims` directly, not `resize_pixels_oriented`).

use super::*;
// `super::gif` is *this* module, which shadows the crate of the same
// name in the extern prelude — the leading `::` reaches past it.
use ::gif::{ColorOutput, DecodeOptions, Frame, MemoryLimit};

/// Logical screen size, without decoding a single frame:
/// `skip_frame_decoding` means nothing pixel-sized is allocated no
/// matter what the header claims, so this stays safe on untrusted input
/// ahead of any budget check.
pub(super) fn probe_gif(bytes: &[u8]) -> Result<(usize, usize)> {
    let mut opts = DecodeOptions::new();
    opts.skip_frame_decoding(true);
    let dec = opts
        .read_info(std::io::Cursor::new(bytes))
        .context("parse GIF")?;
    Ok((dec.width() as usize, dec.height() as usize))
}

pub(super) fn process_gif<R: std::io::Read>(
    s: &mut Scratch,
    mut reader: R,
    target: ImageFormat,
    p: &Resolved,
) -> Result<Vec<u8>> {
    s.srcbuf.clear();
    reader
        .read_to_end(&mut s.srcbuf)
        .context("read GIF source")?;
    // The decoder borrows the compressed source for as long as it runs,
    // while `resize_pixels_to` and `encode_output` want all of `s`.
    // Moving the buffer out for the decode keeps both borrows trivial
    // and still recycles the allocation, which is the point of scratch.
    let src = std::mem::take(&mut s.srcbuf);
    let out = process_gif_src(s, &src, target, p);
    s.srcbuf = src;
    out
}

fn process_gif_src(
    s: &mut Scratch,
    src: &[u8],
    target: ImageFormat,
    p: &Resolved,
) -> Result<Vec<u8>> {
    let timing = crate::config::config().timing;
    let t0 = std::time::Instant::now();
    let (src_w, src_h, channels) = first_frame_into_chunk8(s, src, p)?;
    let t_dec = t0.elapsed();

    let t1 = std::time::Instant::now();
    let (dst_w, dst_h) = fit_dims(src_w, src_h, p.max_width, p.max_height);
    resize_pixels_to(s, channels, src_w, src_h, dst_w, dst_h, p)?;
    let t_resize = t1.elapsed();

    let t2 = std::time::Instant::now();
    let out = encode_output(s, dst_w, dst_h, channels, target, p, None)?;
    if timing {
        eprintln!(
            "timing gif decode({src_w}x{src_h}x{channels})={:.1}ms resize={:.1}ms encode={:.1}ms",
            t_dec.as_secs_f64() * 1e3,
            t_resize.as_secs_f64() * 1e3,
            t2.elapsed().as_secs_f64() * 1e3
        );
    }
    Ok(out)
}

/// Composite the first frame onto the logical screen, leaving the
/// pixels in `chunk8`. Returns (width, height, channels) — 3 channels
/// when the result turned out opaque, which most GIFs are.
///
/// Animated sources render this first frame, like other image proxies
/// and like the animated-WebP path above.
fn first_frame_into_chunk8(
    s: &mut Scratch,
    src: &[u8],
    p: &Resolved,
) -> Result<(usize, usize, usize)> {
    let mut dec = decoder(src)?;
    let (src_w, src_h) = (dec.width() as usize, dec.height() as usize);
    anyhow::ensure!(src_w > 0 && src_h > 0, "GIF logical screen is empty");
    check_src_pixels(src_w, src_h)?;
    let (out_w, out_h) = fit_dims(src_w, src_h, p.max_width, p.max_height);
    let mut cost = DecodeCost::full_frame(src_w, src_h, 4, p);
    // Two frame-sized buffers, not one: the decoder stages its own RGBA
    // frame next to the canvas that frame composites onto. Counted at
    // the screen size for both, which over-states the staged frame
    // whenever it is a sub-rectangle — the safe direction.
    cost.staged_bytes = cost.staged_bytes.saturating_mul(2);
    check_decoded_bytes(
        cost.with_output(out_w, out_h, 4)
            .with_compressed(src.len() + s.held_source_bytes),
        "GIF",
    )?;

    // Transparent background, then paint: a first frame smaller than
    // the screen leaves the rest showing through, which is what a
    // browser renders before any later frame covers it. The fill is
    // required — scratch buffers keep the previous request's pixels.
    let canvas = scratch_u8(&mut s.chunk8, src_w * src_h * 4);
    canvas.fill(0);
    let frame = dec
        .read_next_frame()
        .context("decode GIF frame")?
        .context("GIF has no frames")?;
    draw_frame(canvas, src_w, src_h, frame);

    Ok((
        src_w,
        src_h,
        compact_if_opaque(&mut s.chunk8, src_w * src_h),
    ))
}

/// The one place decoder policy is set, so the still path and anything
/// that reads frames later cannot drift apart.
fn decoder(src: &[u8]) -> Result<::gif::Decoder<std::io::Cursor<&[u8]>>> {
    let mut opts = DecodeOptions::new();
    opts.set_color_output(ColorOutput::RGBA);
    // Frames whose rectangle escapes the logical screen are clipped by
    // `draw_frame`, not rejected: writers do emit them, and browsers
    // draw the part that lands. The memory limit below — not this
    // check — is what bounds an oversized frame.
    opts.check_frame_consistency(false);
    // A frame the configured pixel cap would refuse is not worth
    // decoding, so refuse it inside the decoder too. Its limit is
    // per-frame output bytes, which at RGBA is exactly 4 per pixel;
    // `max(1)` only because the type is NonZero.
    let cap = crate::config::config()
        .max_src_pixels
        .saturating_mul(4)
        .max(1);
    opts.set_memory_limit(MemoryLimit::Bytes(
        std::num::NonZeroU64::new(cap).expect("max(1) above"),
    ));
    opts.read_info(std::io::Cursor::new(src))
        .context("parse GIF")
}

/// GIF frame composition: opaque pixels overwrite the canvas,
/// transparent ones leave it alone. Binary alpha, no blending — a GIF
/// palette entry is either the transparent index or fully opaque.
///
/// Frame rectangles are clipped to the screen on both axes, so a
/// malformed frame that hangs off the edge draws the part that lands
/// instead of erroring. `chunks_exact` handles the other direction: a
/// truncated frame buffer yields fewer rows rather than panicking.
fn draw_frame(canvas: &mut [u8], canvas_w: usize, canvas_h: usize, f: &Frame<'_>) {
    let (fw, fh) = (f.width as usize, f.height as usize);
    let (fx, fy) = (f.left as usize, f.top as usize);
    if fw == 0 || fx >= canvas_w || fy >= canvas_h {
        return;
    }
    let cols = fw.min(canvas_w - fx);
    let rows = fh.min(canvas_h - fy);
    for (row, line) in f.buffer.chunks_exact(fw * 4).take(rows).enumerate() {
        let dst = ((fy + row) * canvas_w + fx) * 4;
        for (i, px) in line[..cols * 4].chunks_exact(4).enumerate() {
            if px[3] == 0 {
                continue;
            }
            canvas[dst + i * 4..dst + i * 4 + 4].copy_from_slice(px);
        }
    }
}

/// Drop the alpha channel when no pixel uses it, and report the channel
/// count now in `chunk8`. Worth the scan: most GIFs are fully opaque,
/// and carrying a constant-255 channel costs a third more work in both
/// the resize and the encoder, plus an alpha plane in the output.
///
/// Compaction is a forward pass — pixel i writes 3i..3i+3 after reading
/// 4i..4i+3 — so it never clobbers unread input, the same trick as
/// `flatten_alpha_in_out8`.
fn compact_if_opaque(chunk8: &mut [u8], pixels: usize) -> usize {
    if chunk8[..pixels * 4].chunks_exact(4).any(|px| px[3] != 255) {
        return 4;
    }
    for i in 0..pixels {
        chunk8.copy_within(i * 4..i * 4 + 3, i * 3);
    }
    3
}
