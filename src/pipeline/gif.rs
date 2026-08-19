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
//! Two paths share that compositor:
//!
//! * **animated** — every frame composited, resized, and streamed into
//!   libwebp's animation encoder, so an animated GIF comes back as
//!   animated WebP. Only for a WebP target, and only inside the
//!   `OXIMG_MAX_ANIM_*` budgets, because one such request can cost as
//!   much CPU as hundreds of still ones (docs/gif-evaluation.md §5).
//! * **still** — the first frame alone: for every other target, for
//!   single-frame sources, and as the degradation whenever a budget says
//!   no. A budget never fails a request; it serves less.
//!
//! Two things a GIF never carries, and so are absent here: an ICC
//! profile (the palette has nowhere to put one) and an EXIF orientation
//! (hence `fit_dims` directly, not `resize_pixels_oriented`).

use super::*;
// `super::gif` is *this* module, which shadows the crate of the same
// name in the extern prelude — the leading `::` reaches past it.
use ::gif::{ColorOutput, DecodeOptions, DisposalMethod, Frame, MemoryLimit, Repeat};

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
    // WebP is the only target here that can carry an animation at all;
    // for the others the still path below is not a degradation, it is
    // the only thing the format can say.
    if target == ImageFormat::Webp
        && let Some(out) = try_animated(s, src, p)?
    {
        return Ok(out);
    }
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

/// Header-only summary of a GIF's animation: everything needed to price
/// the animated path, and to answer `probe_animation`, without decoding
/// a pixel.
#[derive(Default)]
pub(super) struct GifScan {
    pub width: usize,
    pub height: usize,
    /// Frames whose *metadata* parsed. The LZW payloads are skipped
    /// here, so a frame counted may still fail to decode; the encode
    /// loop stops at that point and keeps what it has.
    pub frames: usize,
    /// Sum of the normalized frame delays — see [`delay_ms`], which is
    /// also what the encoder emits, so this is the output's duration and
    /// not merely the source's.
    pub duration_ms: u64,
    /// Whether any frame asks for [`DisposalMethod::Previous`], the one
    /// disposal that needs a second canvas.
    pub needs_previous: bool,
    /// Plays before stopping, in WebP's spelling: 0 = forever.
    pub loop_count: u32,
}

/// Walk every frame header. Cheap by construction: `skip_frame_decoding`
/// discards the compressed payloads instead of expanding them, so this
/// costs one linear pass over the source and allocates nothing
/// pixel-sized — safe to run on untrusted input ahead of any budget.
///
/// A parse error part-way through ends the walk rather than failing:
/// what a truncated GIF has is however many complete frames arrived, and
/// that is exactly the number the budgets should be told about.
pub(super) fn scan_gif(src: &[u8]) -> Result<GifScan> {
    let mut opts = DecodeOptions::new();
    opts.skip_frame_decoding(true);
    let mut dec = opts
        .read_info(std::io::Cursor::new(src))
        .context("parse GIF")?;
    let mut scan = GifScan {
        width: dec.width() as usize,
        height: dec.height() as usize,
        ..Default::default()
    };
    while let Ok(Some(f)) = dec.next_frame_info() {
        scan.frames += 1;
        scan.duration_ms += u64::from(delay_ms(f.delay));
        scan.needs_previous |= f.dispose == DisposalMethod::Previous;
    }
    // Read after the walk, not before: the loop-count extension is an
    // application block that may sit anywhere, and by now the whole file
    // has been parsed.
    scan.loop_count = match dec.repeat() {
        Repeat::Infinite => 0,
        // GIF's count is plays, and a file with no loop extension
        // reports Finite(0) — meaning "show it once", which in WebP is
        // 1, since 0 there means forever.
        Repeat::Finite(n) => u32::from(n).max(1),
    };
    Ok(scan)
}

/// GIF delays are centiseconds, and a great many files store 0 or 1,
/// which means "as fast as the player can" rather than a real duration.
/// oximg substitutes 10 cs: emitted literally, a 0 ms frame asks the
/// player for an unbounded frame rate, and WebP has no way to spell "as
/// fast as possible". This is the one place the output's timing is
/// deliberately not the source's.
fn delay_ms(delay_cs: u16) -> u32 {
    if delay_cs <= 1 {
        100
    } else {
        u32::from(delay_cs) * 10
    }
}

/// Animated GIF to animated WebP, or `None` to leave the request on the
/// still path.
///
/// Every "no" here is a degradation, never an error — including the
/// global memory cap, which a still of the same source would pass. Input
/// errors are also a `None`, so that the still path stays the single
/// place a broken GIF is reported and there is one message per failure
/// mode rather than two.
///
/// Memory stays O(canvas) however many frames arrive: frames are
/// composited, resized and handed to the encoder one at a time.
fn try_animated(s: &mut Scratch, src: &[u8], p: &Resolved) -> Result<Option<Vec<u8>>> {
    let cfg = crate::config::config();
    if !cfg.gif_animation {
        return Ok(None);
    }
    let Ok(scan) = scan_gif(src) else {
        return Ok(None);
    };
    let (cw, ch) = (scan.width, scan.height);
    let step = cfg.anim_frame_step.max(1);
    // An upper bound on frames encoded, not a prediction: identical
    // adjacent frames are merged during the loop below, which can only
    // lower it. Budgeting the bound keeps every allocation and every
    // decision ahead of the first decoded pixel.
    let encoded = scan.frames.div_ceil(step);
    // A single frame *is* the still path, and wrapping it in an
    // animation container would only add bytes.
    if encoded < 2 || cw == 0 || ch == 0 {
        return Ok(None);
    }
    // Two budgets for the two halves of the work: the frame count bounds
    // decode+composite, which no output size reduces, and the
    // frames x area product bounds the encode, which dominates (§5).
    if scan.frames > cfg.max_anim_frames {
        return Ok(None);
    }
    if check_src_pixels(cw, ch).is_err() {
        return Ok(None);
    }
    let (out_w, out_h) = fit_dims(cw, ch, p.max_width, p.max_height);
    let work = (encoded as u64).saturating_mul((out_w as u64).saturating_mul(out_h as u64));
    if work > cfg.max_anim_work {
        return Ok(None);
    }

    let mut cost = DecodeCost::full_frame(cw, ch, 4, p);
    // The canvas, the decoder's staged frame, and — only when a source
    // asks for it — the snapshot the Previous disposal restores from.
    // Counted at the screen size each, which over-states any frame that
    // is a sub-rectangle: the safe direction.
    cost.staged_bytes = cost
        .staged_bytes
        .saturating_mul(if scan.needs_previous { 3 } else { 2 });
    let mut cost = cost
        .with_output(out_w, out_h, 4)
        .with_compressed(src.len() + s.held_source_bytes);
    // libwebp retains every frame it has compressed until Assemble.
    // Modeled at one byte per encoded pixel: ~9x the 0.11 B/px measured
    // across the whole corpus in docs/gif-evaluation.md §5, so the
    // estimate errs high against the only figures we have.
    cost.output_bytes = cost.output_bytes.saturating_add(work);
    // Over the global cap this animation would be a 413 — but the same
    // GIF as a still fits comfortably, so treat the cap like the
    // animation budgets and serve less instead of refusing. The still
    // path runs its own check, so nothing escapes the cap.
    if cfg.max_decoded_bytes.is_some_and(|cap| cost.bytes() > cap) {
        return Ok(None);
    }
    check_decoded_bytes(cost, "animated GIF")?;

    let mut dec = decoder(src)?;
    let canvas_len = cw * ch * 4;
    // Transparent background, as in the still path: whatever no frame
    // covers shows through, and scratch holds the previous request's
    // pixels until told otherwise.
    scratch_u8(&mut s.chunk8, canvas_len).fill(0);
    let mut enc = AnimEncoder::new(out_w, out_h, scan.loop_count, p).context(ServerFault)?;
    let mut ts = 0i32;
    let (mut index, mut emitted) = (0usize, 0usize);
    // The first frame always emits; after that only frames that changed
    // something, so a source that repeats a frame pays for it once and
    // the previous frame's duration absorbs the delay.
    let mut dirty = true;
    let (mut t_frames, mut t_encode) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    loop {
        let t = std::time::Instant::now();
        // A frame whose payload fails to decode ends the animation. The
        // scan above counted it from metadata alone, so this is where
        // truncation and corruption actually surface, and "show what
        // arrived" is the same rule the still path applies.
        let Ok(Some(frame)) = dec.read_next_frame() else {
            break;
        };
        let (delay, dispose) = (frame.delay, frame.dispose);
        let rect = (frame.left, frame.top, frame.width, frame.height);
        if dispose == DisposalMethod::Previous {
            let (canvas, prev) = (&s.chunk8[..canvas_len], &mut s.anim_prev);
            scratch_u8(prev, canvas_len).copy_from_slice(canvas);
        }
        dirty |= draw_frame(scratch_u8(&mut s.chunk8, canvas_len), cw, ch, frame);
        t_frames += t.elapsed();

        if emitted == 0 || (dirty && index % step == 0) {
            let t = std::time::Instant::now();
            resize_pixels_to(s, 4, cw, ch, out_w, out_h, p)?;
            t_frames += t.elapsed();
            let t = std::time::Instant::now();
            enc.add(&s.out8[..out_w * out_h * 4], ts)
                .context(ServerFault)?;
            t_encode += t.elapsed();
            emitted += 1;
            dirty = false;
        }
        // Timestamps are start times, so this advances *after* the add:
        // a frame that was skipped still moves the clock, which is how
        // the previous frame's duration absorbs it.
        ts = ts.saturating_add(delay_ms(delay) as i32);

        let t = std::time::Instant::now();
        // Disposal happens after the frame has been shown, and prepares
        // the canvas the next frame draws onto.
        match dispose {
            DisposalMethod::Background => {
                dirty |= clear_rect(scratch_u8(&mut s.chunk8, canvas_len), cw, ch, rect)
            }
            DisposalMethod::Previous => {
                let (canvas, prev) = (&mut s.chunk8, &s.anim_prev);
                dirty |= restore(&mut canvas[..canvas_len], &prev[..canvas_len]);
            }
            DisposalMethod::Any | DisposalMethod::Keep => {}
        }
        t_frames += t.elapsed();
        index += 1;
    }
    // Fewer than two frames actually made it — a truncated source, or one
    // whose frames all repeat the first. One frame *is* the still path, so
    // hand the request back: a plain WebP is smaller than a one-frame
    // animation container, and if nothing decoded at all the still path's
    // error message for this source is the canonical one.
    if emitted < 2 {
        return Ok(None);
    }
    let t = std::time::Instant::now();
    let out = enc.finish(ts).context(ServerFault)?;
    t_encode += t.elapsed();
    if cfg.timing {
        eprintln!(
            "timing gif-anim {cw}x{ch}->{out_w}x{out_h} frames={emitted}/{} \
             decode+composite+resize={:.1}ms encode={:.1}ms bytes={}",
            scan.frames,
            t_frames.as_secs_f64() * 1e3,
            t_encode.as_secs_f64() * 1e3,
            out.len()
        );
    }
    Ok(Some(out))
}

/// GIF's "restore to background" disposal, which in practice means
/// "clear to transparent": the frame's own rectangle is wiped before the
/// next frame draws. Returns whether anything changed.
fn clear_rect(
    canvas: &mut [u8],
    canvas_w: usize,
    canvas_h: usize,
    rect: (u16, u16, u16, u16),
) -> bool {
    let (fx, fy, fw, fh) = (
        rect.0 as usize,
        rect.1 as usize,
        rect.2 as usize,
        rect.3 as usize,
    );
    if fw == 0 || fx >= canvas_w || fy >= canvas_h {
        return false;
    }
    let cols = fw.min(canvas_w - fx);
    let mut changed = false;
    for row in 0..fh.min(canvas_h - fy) {
        let at = ((fy + row) * canvas_w + fx) * 4;
        let line = &mut canvas[at..at + cols * 4];
        changed |= line.iter().any(|&b| b != 0);
        line.fill(0);
    }
    changed
}

/// GIF's "restore to previous" disposal. Compared before copying so that
/// a restore which changes nothing does not force a duplicate frame into
/// the output.
fn restore(canvas: &mut [u8], prev: &[u8]) -> bool {
    if canvas == prev {
        return false;
    }
    canvas.copy_from_slice(prev);
    true
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
///
/// Returns whether any canvas pixel actually changed, which is how the
/// animated path recognizes a frame it does not need to encode.
fn draw_frame(canvas: &mut [u8], canvas_w: usize, canvas_h: usize, f: &Frame<'_>) -> bool {
    let (fw, fh) = (f.width as usize, f.height as usize);
    let (fx, fy) = (f.left as usize, f.top as usize);
    if fw == 0 || fx >= canvas_w || fy >= canvas_h {
        return false;
    }
    let cols = fw.min(canvas_w - fx);
    let rows = fh.min(canvas_h - fy);
    let mut changed = false;
    for (row, line) in f.buffer.chunks_exact(fw * 4).take(rows).enumerate() {
        let dst = ((fy + row) * canvas_w + fx) * 4;
        for (i, px) in line[..cols * 4].chunks_exact(4).enumerate() {
            if px[3] == 0 {
                continue;
            }
            let at = dst + i * 4;
            changed |= canvas[at..at + 4] != *px;
            canvas[at..at + 4].copy_from_slice(px);
        }
    }
    changed
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
