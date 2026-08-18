//! GIF sources: composition, transparency, animation degradation, and
//! the fact that GIF is never an output format.
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
//! composition bug and not a resampling artifact.

mod common;

use common::{dims_of, fixture, params};
use oximg::pipeline::{self, ErrorKind, ImageFormat, Params};

/// Palette shared by the hand-built sources: index 0 doubles as the
/// transparent index where a frame declares one, 1 = red, 2 = green.
const PAL: &[u8] = &[0, 0, 0, 255, 0, 0, 0, 255, 0];
const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const CLEAR: [u8; 4] = [0, 0, 0, 0];

/// Assemble a GIF around frames the caller has fully specified.
fn build_gif(sw: u16, sh: u16, frames: Vec<gif::Frame<'static>>) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = gif::Encoder::new(&mut out, sw, sh, PAL).unwrap();
    enc.set_repeat(gif::Repeat::Infinite).unwrap();
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

/// Animated sources render their first frame, like the animated-WebP
/// and animated-AVIF paths. (Tier 1 — animated GIF to animated WebP —
/// is the follow-up; this pins today's behavior, which must stay the
/// fallback whenever animation is off or over budget.)
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
