//! GIF sources: composition, transparency, animation (both the animated
//! WebP output and the degradations back to a still), and the fact that
//! GIF is never an output format.
//!
//! Most sources here are assembled byte-exactly rather than read from a
//! fixture. A GIF's interesting structure — frame sub-rectangles, the
//! transparent palette index, rectangles that hang off the logical
//! screen — is exactly what a well-behaved encoder like ImageMagick
//! will not emit, so the committed `*.gif` fixtures can only pin "a
//! real writer's output decodes"; everything else is built here. The
//! writer is the `gif` crate's encoder, which shares no code with the
//! decode path under test (`src/pipeline/gif.rs` never encodes).
//!
//! Pixels are observed through PNG output at the source's native size:
//! the resize is then an exact copy, so an assertion failure is a
//! composition bug and not a resampling artifact. Animated output is
//! observed the same way, through libwebp's *decoder* — a different
//! implementation from the encoder under test, and the only thing that
//! sees the composited canvas a player would actually show.

mod common;

use common::{dims_of, fixture, params};
use oximg::pipeline::{self, ErrorKind, ImageFormat, Params};

/// Palette shared by the hand-built sources: index 0 doubles as the
/// transparent index where a frame declares one, 1 = red, 2 = green.
const PAL: &[u8] = &[0, 0, 0, 255, 0, 0, 0, 255, 0];
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// Assemble a GIF around frames the caller has fully specified.
fn build_gif(sw: u16, sh: u16, frames: Vec<gif::Frame<'static>>) -> Vec<u8> {
    build_gif_repeat(sw, sh, gif::Repeat::Infinite, frames)
}

fn build_gif_repeat(
    sw: u16,
    sh: u16,
    repeat: gif::Repeat,
    frames: Vec<gif::Frame<'static>>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = gif::Encoder::new(&mut out, sw, sh, PAL).unwrap();
    enc.set_repeat(repeat).unwrap();
    for f in &frames {
        enc.write_frame(f).unwrap();
    }
    drop(enc);
    out
}

/// One frame at an explicit rectangle over indexed pixels.
fn frame(
    left: u16,
    top: u16,
    w: u16,
    h: u16,
    idx: &[u8],
    transparent: Option<u8>,
) -> gif::Frame<'static> {
    let mut f = gif::Frame::from_indexed_pixels(w, h, idx.to_vec(), transparent);
    f.left = left;
    f.top = top;
    f
}

/// A full-screen animation frame at an explicit delay (centiseconds)
/// and disposal method — the two fields that make an animated GIF an
/// animation rather than a pile of images.
fn anim_frame(
    w: u16,
    h: u16,
    idx: &[u8],
    delay_cs: u16,
    dispose: gif::DisposalMethod,
) -> gif::Frame<'static> {
    let mut f = frame(0, 0, w, h, idx, None);
    f.delay = delay_cs;
    f.dispose = dispose;
    f
}

/// Decode PNG output to straight RGBA plus its stored color type — the
/// color type is load-bearing: an opaque GIF must not carry an alpha
/// channel through the pipeline.
fn png_rgba(bytes: &[u8]) -> (usize, usize, png::ColorType, Vec<u8>) {
    let mut r = png::Decoder::new(std::io::Cursor::new(bytes))
        .read_info()
        .unwrap();
    let (w, h, color) = {
        let i = r.info();
        (i.width as usize, i.height as usize, i.color_type)
    };
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    let rgba = match color {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        other => panic!("unexpected color type {other:?}"),
    };
    (w, h, color, rgba)
}

/// Render a source to PNG at its native size (no resampling) and return
/// the pixels alongside the stored color type.
fn render_native(src: &[u8], max: u32) -> (usize, usize, png::ColorType, Vec<u8>) {
    let p = Params {
        output: Some(ImageFormat::Png),
        ..params(max)
    };
    let (out, fmt) = pipeline::process(src, &p).unwrap_or_else(|e| panic!("{e:#}"));
    assert_eq!(fmt, ImageFormat::Png);
    png_rgba(&out)
}

/// Decode an animated WebP with libwebp's own animation decoder:
/// canvas size, loop count, and per frame the composited RGBA canvas
/// with the frame's **end** timestamp in milliseconds (what
/// `WebPAnimDecoderGetNext` reports), so a frame's duration is the
/// difference between consecutive values.
fn webp_frames(bytes: &[u8]) -> (u32, u32, u32, Vec<(i32, Vec<u8>)>) {
    // SAFETY: `data` borrows `bytes`, which outlives the decoder created
    // and deleted here. Each frame buffer is owned by the decoder and
    // copied out before the next `GetNext` invalidates it; the canvas
    // size the length comes from is the decoder's own.
    unsafe {
        let data = libwebp_sys::WebPData {
            bytes: bytes.as_ptr(),
            size: bytes.len(),
        };
        let mut opts = std::mem::zeroed::<libwebp_sys::WebPAnimDecoderOptions>();
        assert_eq!(libwebp_sys::WebPAnimDecoderOptionsInit(&mut opts), 1);
        let dec = libwebp_sys::WebPAnimDecoderNew(&data, &opts);
        assert!(!dec.is_null(), "not a decodable WebP animation");
        let mut info = libwebp_sys::WebPAnimInfo::default();
        assert_eq!(libwebp_sys::WebPAnimDecoderGetInfo(dec, &mut info), 1);
        let len = info.canvas_width as usize * info.canvas_height as usize * 4;
        let mut frames = Vec::new();
        while libwebp_sys::WebPAnimDecoderHasMoreFrames(dec) > 0 {
            let (mut buf, mut ts) = (std::ptr::null_mut(), 0);
            assert_eq!(
                libwebp_sys::WebPAnimDecoderGetNext(dec, &mut buf, &mut ts),
                1,
                "frame {} failed to decode",
                frames.len()
            );
            frames.push((ts, std::slice::from_raw_parts(buf, len).to_vec()));
        }
        libwebp_sys::WebPAnimDecoderDelete(dec);
        (
            info.canvas_width,
            info.canvas_height,
            info.loop_count,
            frames,
        )
    }
}

/// Params for the hand-built animations: WebP quality pinned high so a
/// 4x4 canvas of hard-edged flat blocks — the worst case for a lossy
/// codec, and nothing like the photographs the default 75 is tuned for —
/// comes back close enough to assert exact colors on.
fn anim_params(max: u32) -> Params {
    Params {
        webp_quality: Some(100.0),
        ..params(max)
    }
}

/// One pixel of an RGBA buffer of the given width.
fn at(rgba: &[u8], w: usize, x: usize, y: usize) -> [u8; 4] {
    rgba[(y * w + x) * 4..][..4].try_into().unwrap()
}

/// Animated output is lossy WebP, so its flat fields come back close
/// rather than exact. Fully transparent pixels carry no meaningful
/// color, so only their alpha is compared.
fn assert_near(got: [u8; 4], want: [u8; 4], what: &str) {
    if want[3] == 0 {
        assert_eq!(got[3], 0, "{what}: expected transparent, got {got:?}");
        return;
    }
    let close = (0..4).all(|c| got[c].abs_diff(want[c]) <= 12);
    assert!(close, "{what}: {got:?} is not {want:?}");
}

fn assert_pixels(rgba: &[u8], w: usize, expect: &[[u8; 4]]) {
    for (i, want) in expect.iter().enumerate() {
        let got: [u8; 4] = rgba[i * 4..i * 4 + 4].try_into().unwrap();
        assert_eq!(got, *want, "pixel ({}, {}) mismatch", i % w, i / w);
    }
}

/// A GIF frame is a sub-rectangle of the logical screen, and the
/// palette's transparent index shows the canvas through. Both have to
/// be honored even for a still result — the first frame is not
/// necessarily the whole image.
#[test]
fn first_frame_composites_onto_the_logical_screen() {
    // 4x4 screen, 2x2 frame at (1,1): red, transparent, transparent, green.
    let src = build_gif(4, 4, vec![frame(1, 1, 2, 2, &[1, 0, 0, 2], Some(0))]);
    let (w, h, color, px) = render_native(&src, 4);
    assert_eq!((w, h), (4, 4), "the screen, not the frame rectangle");
    assert_eq!(color, png::ColorType::Rgba, "the canvas shows through");
    #[rustfmt::skip]
    let expect = [
        CLEAR, CLEAR, CLEAR, CLEAR,
        CLEAR, RED,   CLEAR, CLEAR,
        CLEAR, CLEAR, GREEN, CLEAR,
        CLEAR, CLEAR, CLEAR, CLEAR,
    ];
    assert_pixels(&px, w, &expect);
}

/// `probe` reports the logical screen — the size the image displays at
/// — never the first frame's rectangle.
#[test]
fn probe_reports_the_logical_screen() {
    let src = build_gif(8, 6, vec![frame(1, 1, 2, 2, &[1, 1, 1, 1], None)]);
    let (fmt, w, h) = pipeline::probe(&src).unwrap();
    assert_eq!(fmt, ImageFormat::Gif);
    assert_eq!((w, h), (8, 6));
}

/// A GIF with no transparent pixel loses its alpha channel: carrying a
/// constant 255 through the resize and into the encoder costs a third
/// more work and an alpha plane in the output for nothing.
#[test]
fn opaque_sources_drop_their_alpha_channel() {
    let opaque = build_gif(2, 2, vec![frame(0, 0, 2, 2, &[1, 2, 2, 1], None)]);
    let (w, _, color, px) = render_native(&opaque, 2);
    assert_eq!(color, png::ColorType::Rgb);
    assert_pixels(&px, w, &[RED, GREEN, GREEN, RED]);

    // The same pixels, but with one index declared transparent and used:
    // now the alpha channel is load-bearing and must survive.
    let holed = build_gif(2, 2, vec![frame(0, 0, 2, 2, &[1, 0, 2, 1], Some(0))]);
    let (w, _, color, px) = render_native(&holed, 2);
    assert_eq!(color, png::ColorType::Rgba);
    assert_pixels(&px, w, &[RED, CLEAR, GREEN, RED]);
}

/// Rendered as a still — the target here is PNG, which cannot carry an
/// animation — an animated GIF is its first frame. This is also the
/// fallback the WebP target degrades to whenever animation is off or
/// over budget, so it stays load-bearing for both.
#[test]
fn animated_gif_renders_first_frame() {
    let src = build_gif(
        2,
        2,
        vec![
            frame(0, 0, 2, 2, &[1, 1, 1, 1], None),
            frame(0, 0, 2, 2, &[2, 2, 2, 2], None),
        ],
    );
    let (w, _, _, px) = render_native(&src, 2);
    assert_pixels(&px, w, &[RED, RED, RED, RED]);

    // And on a third-party writer's animation: anim.gif's frames are
    // red, then blue, then green.
    let (w, h, _, px) = render_native(&fixture("anim.gif"), 120);
    assert_eq!((w, h), (120, 90));
    for (i, chunk) in px.chunks_exact(4).enumerate() {
        assert_eq!(chunk, RED, "pixel {i} should be the first frame's red");
    }
}

/// An animated GIF into the default (WebP) target comes back as an
/// animated WebP: every frame, in order, at the source's timing, and
/// with the frames resized like any other output.
#[test]
fn animated_gif_becomes_animated_webp() {
    let src = fixture("anim.gif");
    let (out, fmt) = pipeline::process(&src, &params(120)).unwrap();
    assert_eq!(fmt, ImageFormat::Webp);
    assert_eq!(dims_of(&out), (120, 90));

    // Our own header reader and libwebp's demuxer must agree on what
    // was written, and both on what the source said.
    let a = pipeline::probe_animation(&out).unwrap().expect("animated");
    assert_eq!((a.frames, a.duration_ms, a.loop_count), (3, 1500, 0));
    assert_eq!(pipeline::probe_animation(&src).unwrap(), Some(a));

    let (w, h, loops, frames) = webp_frames(&out);
    assert_eq!(((w, h), loops, frames.len()), ((120, 90), 0, 3));
    // anim.gif is three 50cs frames: red, then blue, then green.
    for (i, (want, (ts, px))) in [RED, BLUE, GREEN].iter().zip(&frames).enumerate() {
        assert_eq!(*ts, 500 * (i as i32 + 1), "frame {i} end timestamp");
        assert_near(at(px, 120, 60, 45), *want, &format!("frame {i} center"));
    }

    // Resized, the frames follow the canvas — libwebp requires every
    // frame to be exactly the canvas, so a wrong resize is a hard
    // encoder error rather than a subtle one.
    let (small, _) = pipeline::process(&src, &params(40)).unwrap();
    let (w, h, _, frames) = webp_frames(&small);
    assert_eq!(((w, h), frames.len()), ((40, 30), 3));
    assert!(small.len() < out.len(), "a smaller box means fewer bytes");
}

/// WebP is the only target that can carry an animation, so every other
/// one answers with the still first frame rather than failing.
#[test]
fn animation_is_only_ever_a_webp_target() {
    let src = fixture("anim.gif");
    let mut targets = vec![ImageFormat::Png, ImageFormat::Jpeg];
    if cfg!(feature = "avif") {
        targets.push(ImageFormat::Avif);
    }
    for target in targets {
        let p = Params {
            output: Some(target),
            ..params(120)
        };
        let (out, fmt) = pipeline::process(&src, &p).unwrap();
        assert_eq!(fmt, target);
        assert_eq!(dims_of(&out), (120, 90), "{target:?}");
        assert!(
            pipeline::probe_animation(&out).unwrap().is_none(),
            "{target:?} cannot animate"
        );
    }
}

/// A frame that paints nothing new is not encoded again: its delay
/// extends the frame it repeated, so the output has fewer frames and
/// exactly the same total play time.
#[test]
fn duplicate_frames_merge_and_keep_the_total_duration() {
    let keep = gif::DisposalMethod::Keep;
    let src = build_gif(
        4,
        4,
        vec![
            anim_frame(4, 4, &[1; 16], 50, keep),
            anim_frame(4, 4, &[1; 16], 50, keep), // the same red again
            anim_frame(4, 4, &[2; 16], 50, keep),
        ],
    );
    // The header scan counts what the source stores — it is an upper
    // bound on what gets encoded, which is what the budgets want.
    let a = pipeline::probe_animation(&src).unwrap().unwrap();
    assert_eq!((a.frames, a.duration_ms), (3, 1500));

    let (out, _) = pipeline::process(&src, &anim_params(4)).unwrap();
    let (_, _, _, frames) = webp_frames(&out);
    let ends: Vec<i32> = frames.iter().map(|f| f.0).collect();
    assert_eq!(ends, vec![1000, 1500], "two frames, 1000ms then 500ms");
    assert_near(at(&frames[0].1, 4, 1, 1), RED, "first");
    assert_near(at(&frames[1].1, 4, 1, 1), GREEN, "second");
}

/// "Restore to background" clears the frame's own rectangle before the
/// next frame draws, so what follows composites onto emptiness — and the
/// emptiness has to survive into the output, which means the animation
/// carries alpha even though its first frame is opaque.
#[test]
fn background_disposal_clears_the_rectangle() {
    let mut second = frame(0, 0, 2, 2, &[2; 4], Some(0));
    second.delay = 10;
    let src = build_gif(
        4,
        4,
        vec![
            anim_frame(4, 4, &[1; 16], 10, gif::DisposalMethod::Background),
            second,
        ],
    );
    let (out, _) = pipeline::process(&src, &anim_params(4)).unwrap();
    let (_, _, _, frames) = webp_frames(&out);
    assert_eq!(frames.len(), 2);
    assert_near(
        at(&frames[0].1, 4, 3, 3),
        RED,
        "frame 0 is the whole screen",
    );
    assert_near(at(&frames[1].1, 4, 0, 0), GREEN, "frame 1's corner");
    assert_near(at(&frames[1].1, 4, 3, 3), CLEAR, "the rest was disposed");
}

/// Clipping applies to disposal too, not just to drawing: a frame whose
/// rectangle hangs off the logical screen must clear only the part that
/// landed. The point of the test is equally that the row arithmetic does
/// not panic on the overhang.
#[test]
fn disposal_of_a_clipped_rectangle_stays_on_the_canvas() {
    // 32x32 rather than the 4x4 the other cases use: VP8 codes a whole
    // macroblock at a time, so a color assertion needs a block of pixels
    // to be about, not a lone corner one.
    let mut corner = frame(24, 24, 24, 24, &[2; 24 * 24], None);
    corner.delay = 10;
    corner.dispose = gif::DisposalMethod::Background;
    let mut last = frame(0, 0, 8, 8, &[2; 64], None);
    last.delay = 10;
    let src = build_gif(
        32,
        32,
        vec![
            anim_frame(32, 32, &[1; 32 * 32], 10, gif::DisposalMethod::Keep),
            corner,
            last,
        ],
    );
    let (out, _) = pipeline::process(&src, &anim_params(32)).unwrap();
    let (_, _, _, frames) = webp_frames(&out);
    assert_eq!(frames.len(), 3);
    assert_near(at(&frames[1].1, 32, 28, 28), GREEN, "the part that lands");
    assert_near(
        at(&frames[2].1, 32, 28, 28),
        CLEAR,
        "cleared by the disposal",
    );
    assert_near(at(&frames[2].1, 32, 4, 4), GREEN, "the last frame's corner");
    assert_near(
        at(&frames[2].1, 32, 16, 4),
        RED,
        "everything else is frame 0",
    );
}

/// "Restore to previous" is the disposal that needs a second canvas: the
/// third frame must composite onto what frame 1 showed, not onto frame
/// 2's overpaint.
#[test]
fn previous_disposal_reverts_the_canvas() {
    let mut second = frame(0, 0, 2, 2, &[2; 4], None);
    second.delay = 10;
    second.dispose = gif::DisposalMethod::Previous;
    let mut third = frame(2, 2, 2, 2, &[2; 4], None);
    third.delay = 10;
    let src = build_gif(
        4,
        4,
        vec![
            anim_frame(4, 4, &[1; 16], 10, gif::DisposalMethod::Keep),
            second,
            third,
        ],
    );
    let (out, _) = pipeline::process(&src, &anim_params(4)).unwrap();
    let (_, _, _, frames) = webp_frames(&out);
    assert_eq!(frames.len(), 3);
    assert_near(at(&frames[1].1, 4, 0, 0), GREEN, "frame 1 paints a corner");
    assert_near(at(&frames[1].1, 4, 3, 3), RED, "frame 1 keeps the rest");
    assert_near(at(&frames[2].1, 4, 0, 0), RED, "frame 1's corner is undone");
    assert_near(at(&frames[2].1, 4, 3, 3), GREEN, "frame 2 paints its own");
}

/// GIF's 0 and 1 centisecond delays mean "as fast as possible", which
/// WebP cannot spell: they become 100ms, in the scan as well as in the
/// output, so what `probe` reports is what a player will do.
#[test]
fn implausible_delays_are_normalized() {
    let keep = gif::DisposalMethod::Keep;
    let src = build_gif(
        4,
        4,
        vec![
            anim_frame(4, 4, &[1; 16], 0, keep),
            anim_frame(4, 4, &[2; 16], 1, keep),
        ],
    );
    assert_eq!(
        pipeline::probe_animation(&src)
            .unwrap()
            .unwrap()
            .duration_ms,
        200
    );
    let (out, _) = pipeline::process(&src, &anim_params(4)).unwrap();
    let (_, _, _, frames) = webp_frames(&out);
    assert_eq!(
        frames.iter().map(|f| f.0).collect::<Vec<_>>(),
        vec![100, 200]
    );
}

/// A GIF that plays a fixed number of times keeps that count, and the
/// two formats' spellings of "forever" differ (GIF's Netscape 0 against
/// WebP's loop_count 0), so the translation is worth pinning.
#[test]
fn loop_counts_survive_the_transcode() {
    let keep = gif::DisposalMethod::Keep;
    let two = || {
        vec![
            anim_frame(4, 4, &[1; 16], 10, keep),
            anim_frame(4, 4, &[2; 16], 10, keep),
        ]
    };
    for (repeat, want) in [
        (gif::Repeat::Finite(3), 3),
        (gif::Repeat::Finite(1), 1),
        (gif::Repeat::Infinite, 0),
    ] {
        let src = build_gif_repeat(4, 4, repeat, two());
        let a = pipeline::probe_animation(&src).unwrap().unwrap();
        assert_eq!(a.loop_count, want, "{repeat:?} in the source");
        let (out, _) = pipeline::process(&src, &anim_params(4)).unwrap();
        assert_eq!(webp_frames(&out).2, want, "{repeat:?} in the output");
        assert_eq!(
            pipeline::probe_animation(&out).unwrap().unwrap().loop_count,
            want,
            "{repeat:?} read back"
        );
    }
}

/// `probe_animation` answers for the formats that have an animation and
/// says nothing for the rest — and a single-frame GIF is a still, not a
/// one-frame animation, in the probe and in the output alike.
#[test]
fn probe_animation_reports_only_real_animations() {
    for name in ["still.gif", "alpha.gif", "rgb.png", "photo.jpg"] {
        assert_eq!(
            pipeline::probe_animation(&fixture(name)).unwrap(),
            None,
            "{name}"
        );
    }
    let one = build_gif(4, 4, vec![frame(0, 0, 4, 4, &[1; 16], None)]);
    assert_eq!(pipeline::probe_animation(&one).unwrap(), None);
    let (out, _) = pipeline::process(&one, &anim_params(4)).unwrap();
    assert_eq!(
        pipeline::probe_animation(&out).unwrap(),
        None,
        "a still must not be wrapped in an animation container"
    );

    // Not an image at all: the same client-side error `probe` gives.
    let err = pipeline::probe_animation(b"not an image at all").unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Undecodable);
    assert_eq!(
        pipeline::probe_animation(b"short").unwrap_err().kind(),
        ErrorKind::Undecodable
    );
}

/// Truncation shows what arrived, as on the still path: whichever frames
/// are complete are kept, never more than the source claimed, and a cut
/// that leaves only the first frame degrades to a still rather than
/// emitting a one-frame animation. Every cut either serves or is a 422 —
/// none may panic.
#[test]
fn truncated_animations_keep_the_frames_that_arrived() {
    let keep = gif::DisposalMethod::Keep;
    let src = build_gif(
        4,
        4,
        vec![
            anim_frame(4, 4, &[1; 16], 10, keep),
            anim_frame(4, 4, &[2; 16], 10, keep),
            anim_frame(
                4,
                4,
                &[1, 2, 1, 2, 2, 1, 2, 1, 1, 2, 1, 2, 2, 1, 2, 1],
                10,
                keep,
            ),
        ],
    );
    let full = pipeline::probe_animation(&src).unwrap().unwrap().frames;
    assert_eq!(full, 3);
    let mut counts = std::collections::BTreeSet::new();
    for cut in 1..src.len() {
        match pipeline::process(&src[..cut], &anim_params(4)) {
            // A still counts as one frame: probe_animation says nothing
            // about a plain WebP.
            Ok((out, _)) => {
                let n = pipeline::probe_animation(&out)
                    .unwrap()
                    .map_or(1, |a| a.frames);
                assert!(n <= full, "cut at {cut}: {n} frames of {full}");
                counts.insert(n);
            }
            Err(e) => assert_eq!(e.kind(), ErrorKind::Undecodable, "cut at {cut}: {e:#}"),
        }
    }
    assert_eq!(
        counts,
        [1, 2, 3].into_iter().collect(),
        "cuts should land inside every frame in turn"
    );
}

/// Frame rectangles that fall outside the logical screen are clipped,
/// not rejected: the format permits them, browsers draw the part that
/// lands, and rejecting would fail requests other proxies serve. The
/// point of the test is equally that none of these panic.
#[test]
fn frame_rectangles_outside_the_screen_are_clipped() {
    // Bottom-right corner: a 4x4 frame at (3,3) lands one pixel.
    let src = build_gif(4, 4, vec![frame(3, 3, 4, 4, &[1; 16], Some(0))]);
    let (w, _, _, px) = render_native(&src, 4);
    let mut expect = [CLEAR; 16];
    expect[15] = RED;
    assert_pixels(&px, w, &expect);

    // Entirely off-screen: nothing is drawn, and the canvas stays as it
    // began — fully transparent.
    let src = build_gif(4, 4, vec![frame(10, 10, 2, 2, &[1; 4], Some(0))]);
    let (_, _, _, px) = render_native(&src, 4);
    assert!(
        px.chunks_exact(4).all(|p| p[3] == 0),
        "an off-screen frame must not paint anything"
    );

    // Larger than the screen at the origin: the top-left 2x2 of a 4x4
    // frame whose columns alternate red/green.
    let big: Vec<u8> = (0..16).map(|i| if i % 2 == 0 { 1 } else { 2 }).collect();
    let src = build_gif(2, 2, vec![frame(0, 0, 4, 4, &big, None)]);
    let (w, _, _, px) = render_native(&src, 2);
    assert_pixels(&px, w, &[RED, GREEN, RED, GREEN]);
}

/// GIF87a is still in the wild; a reader that only knew GIF89a would
/// reject a decade of files. The signature is the only difference the
/// decoder sees, so patching it is a faithful test.
#[test]
fn gif87a_sources_decode() {
    let mut src = build_gif(2, 2, vec![frame(0, 0, 2, 2, &[1, 1, 1, 1], None)]);
    src[..6].copy_from_slice(b"GIF87a");
    let (fmt, w, h) = pipeline::probe(&src).unwrap();
    assert_eq!((fmt, w, h), (ImageFormat::Gif, 2, 2));
    let (w, _, _, px) = render_native(&src, 2);
    assert_pixels(&px, w, &[RED, RED, RED, RED]);
}

/// With no `@fmt` and no negotiation a GIF becomes WebP — it cannot
/// stay itself, since nothing here encodes GIF — and an explicitly
/// requested target is honored as for any other source.
#[test]
fn gif_defaults_to_webp_and_transcodes_on_request() {
    let src = fixture("still.gif");
    let (out, fmt) = pipeline::process(&src, &params(100)).unwrap();
    assert_eq!(fmt, ImageFormat::Webp);
    assert_eq!(dims_of(&out), (100, 75));

    let mut targets = vec![ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::Webp];
    if cfg!(feature = "avif") {
        targets.push(ImageFormat::Avif);
    }
    for target in targets {
        let p = Params {
            output: Some(target),
            ..params(50)
        };
        let (out, fmt) = pipeline::process(&src, &p).unwrap();
        assert_eq!(fmt, target);
        assert_eq!(dims_of(&out), (50, 38), "{target:?}");
        let (sniffed, _, _) = pipeline::probe(&out).unwrap();
        assert_eq!(sniffed, target, "{target:?}: output magic bytes");
    }
}

/// GIF decodes but never encodes. No URL token can name it (`@gif` and
/// `format=gif` are rejected by the HTTP layer), but the enum variant is
/// public, so a library caller can — and must get a clean client-side
/// error rather than a 500 out of the encoder.
#[test]
fn gif_is_refused_as_an_output_format() {
    let p = Params {
        output: Some(ImageFormat::Gif),
        ..params(100)
    };
    for name in ["still.gif", "photo.jpg", "rgb.png"] {
        let err = pipeline::process(&fixture(name), &p).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Undecodable, "{name}");
        assert!(
            err.to_string().contains("GIF output is not supported"),
            "{name}: {err}"
        );
    }
}

/// The committed fixtures are an independent writer's idea of a GIF;
/// all this asks is that our reader agrees with it, including the
/// transparent index ImageMagick chose for the gray field.
#[test]
fn third_party_gif_sources_decode() {
    let (fmt, w, h) = pipeline::probe(&fixture("still.gif")).unwrap();
    assert_eq!((fmt, w, h), (ImageFormat::Gif, 240, 180));

    // still.gif has no transparent index: fully opaque, alpha dropped.
    let (w, h, color, px) = render_native(&fixture("still.gif"), 240);
    assert_eq!(((w, h), color), ((240, 180), png::ColorType::Rgb));
    let at = |x: usize, y: usize| -> [u8; 4] { px[(y * w + x) * 4..][..4].try_into().unwrap() };
    // The corner image: 60px blocks TL=red, TR=green, BL=blue, BR=white
    // on a gray field (tests/fixtures/README.md).
    assert_eq!(at(30, 30), RED);
    assert_eq!(at(210, 30), GREEN);
    assert_eq!(at(30, 150), [0, 0, 255, 255]);
    assert_eq!(at(210, 150), [255, 255, 255, 255]);
    assert_eq!(at(120, 90), [128, 128, 128, 255], "the gray field");

    // alpha.gif is the same image with the gray field transparent.
    let (w, h, color, px) = render_native(&fixture("alpha.gif"), 240);
    assert_eq!(((w, h), color), ((240, 180), png::ColorType::Rgba));
    let at = |x: usize, y: usize| -> [u8; 4] { px[(y * w + x) * 4..][..4].try_into().unwrap() };
    assert_eq!(at(30, 30), RED, "corner blocks stay opaque");
    assert_eq!(at(120, 90)[3], 0, "the gray field is transparent");
}

/// Malformed GIFs must fail as undecodable input, never panic and never
/// hang: a header with nothing after it, a zero-size logical screen, no
/// frames at all, and garbage behind a valid signature.
#[test]
fn malformed_gifs_error_instead_of_panicking() {
    let valid = build_gif(4, 4, vec![frame(0, 0, 4, 4, &[1; 16], None)]);
    let cases: Vec<(&str, Vec<u8>)> = vec![
        (
            "header only",
            b"GIF89a\x04\x00\x04\x00\x80\x00\x00".to_vec(),
        ),
        ("zero-size screen", {
            let mut v = valid.clone();
            v[6..10].copy_from_slice(&[0, 0, 0, 0]);
            v
        }),
        ("no frames", {
            // Everything up to (not including) the first image block.
            let end = valid.iter().position(|&b| b == 0x2C).unwrap();
            let mut v = valid[..end].to_vec();
            v.push(0x3B); // trailer
            v
        }),
        ("garbage body", {
            let mut v = b"GIF89a".to_vec();
            v.extend((0..64u16).map(|i| (i * 37 % 251) as u8));
            v
        }),
    ];
    for (what, src) in cases {
        let err = pipeline::process(&src, &params(100))
            .err()
            .unwrap_or_else(|| panic!("{what}: should not decode"));
        assert!(
            matches!(err.kind(), ErrorKind::Undecodable),
            "{what}: {:?} — {err:#}",
            err.kind()
        );
    }
}

/// Truncation is checked exhaustively rather than sampled, because the
/// interesting part is where the boundary sits: every cut that lands
/// inside the first frame must be a 422, and the one cut that does not —
/// losing only the 0x3B trailer, with the frame already complete — must
/// still render. That mirrors the JPEG rule in `tests/formats.rs`
/// (browsers show what arrived), except that for GIF only a whole frame
/// counts as "arrived": there is no partial-block fallback, so unlike
/// JPEG's filler blocks the tolerance is exactly one byte wide.
#[test]
fn truncation_is_undecodable_until_only_the_trailer_is_missing() {
    let valid = build_gif(4, 4, vec![frame(0, 0, 4, 4, &[1; 16], None)]);
    assert_eq!(*valid.last().unwrap(), 0x3B, "the last byte is the trailer");

    for cut in 0..valid.len() - 1 {
        let err = pipeline::process(&valid[..cut], &params(100))
            .err()
            .unwrap_or_else(|| panic!("cut at {cut}/{}: should not decode", valid.len()));
        assert!(
            matches!(err.kind(), ErrorKind::Undecodable),
            "cut at {cut}: {:?} — {err:#}",
            err.kind()
        );
    }

    // Trailer gone, first frame intact: a complete image, so it decodes.
    let (w, h, _, px) = render_native(&valid[..valid.len() - 1], 4);
    assert_eq!((w, h), (4, 4));
    assert_pixels(&px, w, &[RED; 16]);
}
