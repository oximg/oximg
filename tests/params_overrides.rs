//! The per-call override fields on `Params`: each test proves (a) the
//! `Some(...)` value actually steers the pipeline, and (b) explicitly
//! passing the documented default produces the same bytes as `None`
//! (the env-fallback path) — which pins the precedence contract in a
//! process where no OXIMG_* variable is set, without touching the
//! environment. Running contradictory settings back-to-back in one
//! process is itself the point: these knobs used to be process-global.

mod common;

use common::{corner_base, dims_of, fixture, jpeg_with_orientation, png_icc, png_with_icc};
use oximg::pipeline::{self, ImageFormat, Params, PngEffort};

fn run(src: &[u8], p: &Params) -> Vec<u8> {
    pipeline::process(src, p).expect("process").0
}

fn base(output: ImageFormat) -> Params {
    Params {
        max_width: 100,
        max_height: 100,
        output: Some(output),
        ..Params::default()
    }
}

#[test]
fn webp_quality_override_steers_the_encoder() {
    let src = fixture("photo.jpg");
    let at = |q: Option<f32>| {
        run(
            &src,
            &Params {
                webp_quality: q,
                ..base(ImageFormat::Webp)
            },
        )
    };
    let (low, high) = (at(Some(30.0)), at(Some(95.0)));
    assert!(
        low.len() < high.len(),
        "q30 ({}) must be smaller than q95 ({})",
        low.len(),
        high.len()
    );
    // Some(default) == None: the override path is byte-identical to the
    // env-fallback path at the documented default (75).
    assert_eq!(at(Some(75.0)), at(None));
}

#[test]
fn png_effort_override_steers_the_encoder() {
    let src = fixture("photo.jpg");
    let at = |e: Option<PngEffort>| {
        run(
            &src,
            &Params {
                png_effort: e,
                ..base(ImageFormat::Png)
            },
        )
    };
    let (fastest, high) = (at(Some(PngEffort::Fastest)), at(Some(PngEffort::High)));
    assert!(
        high.len() < fastest.len(),
        "high ({}) must compress smaller than fastest ({})",
        high.len(),
        fastest.len()
    );
    assert_eq!(at(Some(PngEffort::Fast)), at(None), "documented default");
}

#[test]
fn auto_rotate_override_controls_orientation() {
    // Stored 48x64, orientation 6 -> displayed 64x48.
    let px = vec![128u8; 48 * 64 * 3];
    let src = jpeg_with_orientation(&px, 48, 64, Some(6));
    let at = |rot: Option<bool>| {
        let out = run(
            &src,
            &Params {
                auto_rotate: rot,
                ..Params::default()
            },
        );
        dims_of(&out)
    };
    assert_eq!(at(None), (64, 48), "default applies the rotation");
    assert_eq!(at(Some(false)), (48, 64), "override keeps stored axes");
    assert_eq!(at(Some(true)), at(None));
}

#[test]
fn icc_override_strips_or_keeps_the_profile() {
    let px = corner_base(64, 48, 8);
    let profile = common::fake_icc(400);
    let src = png_with_icc(&px, 64, 48, &profile);
    let at = |icc: Option<bool>| {
        run(
            &src,
            &Params {
                icc,
                ..base(ImageFormat::Png)
            },
        )
    };
    assert_eq!(
        png_icc(&at(None)).as_deref(),
        Some(&profile[..]),
        "default passes the profile through"
    );
    assert_eq!(png_icc(&at(Some(false))), None, "override strips it");
    assert_eq!(at(Some(true)), at(None));
}

#[test]
fn flatten_bg_override_sets_the_composite_background() {
    // Fully transparent RGBA PNG -> JPEG: the output is pure background.
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, 32, 32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(&vec![0u8; 32 * 32 * 4]).unwrap();
    writer.finish().unwrap();
    let src = out;

    let center_px = |bg: Option<[u8; 3]>| {
        let jpeg = run(
            &src,
            &Params {
                flatten_bg: bg,
                ..base(ImageFormat::Jpeg)
            },
        );
        let (rgb, w, _) = pipeline::decode_and_resize(&jpeg, 32, 32, 1).unwrap();
        let i = (16 * w + 16) * 3;
        [rgb[i], rgb[i + 1], rgb[i + 2]]
    };
    let white = center_px(None);
    assert!(
        white.iter().all(|&c| c > 240),
        "default is white: {white:?}"
    );
    let red = center_px(Some([255, 0, 0]));
    assert!(
        red[0] > 200 && red[1] < 60 && red[2] < 60,
        "override composes onto red: {red:?}"
    );
}

#[test]
fn linear_light_override_selects_the_srgb_resize() {
    let src = fixture("photo.jpg");
    let at = |ll: Option<bool>| {
        run(
            &src,
            &Params {
                linear_light: ll,
                max_width: 64,
                max_height: 64,
                ..Params::default()
            },
        )
    };
    assert_ne!(
        at(Some(false)),
        at(None),
        "srgb mode must produce different bytes on a downscale"
    );
    assert_eq!(at(Some(true)), at(None));
}

/// The point of per-call overrides: contradictory settings coexist in
/// one process, interleaved, with deterministic results — impossible
/// with process-global env knobs.
#[test]
fn two_configs_coexist_in_one_process() {
    let src = fixture("photo.jpg");
    let q30 = Params {
        webp_quality: Some(30.0),
        ..base(ImageFormat::Webp)
    };
    let q95 = Params {
        webp_quality: Some(95.0),
        ..base(ImageFormat::Webp)
    };
    let (a1, b1) = (run(&src, &q30), run(&src, &q95));
    let (b2, a2) = (run(&src, &q95), run(&src, &q30));
    assert_eq!(a1, a2);
    assert_eq!(b1, b2);
    assert_ne!(a1, b1);
}

#[cfg(feature = "avif")]
#[test]
fn avif_quality_override_steers_the_encoder() {
    let src = fixture("photo.jpg");
    let at = |q: Option<u8>| {
        run(
            &src,
            &Params {
                avif_quality: q,
                ..base(ImageFormat::Avif)
            },
        )
    };
    let (low, high) = (at(Some(30)), at(Some(85)));
    assert!(
        low.len() < high.len(),
        "q30 ({}) must be smaller than q85 ({})",
        low.len(),
        high.len()
    );
    assert_eq!(at(Some(55)), at(None), "documented default");
}

/// PNG quantization overrides: `Some(true)` produces a strictly
/// smaller indexed encode, the palette-size knob steers it further,
/// `Some(false)`/`Some(256)` match the env-fallback defaults
/// byte-for-byte, and the quantized output still decodes at the same
/// dimensions.
#[test]
fn png_quantize_override_steers_the_encoder() {
    let src = fixture("photo.jpg");
    let at = |q: Option<bool>, colors: Option<u16>| {
        run(
            &src,
            &Params {
                png_quantize: q,
                png_quantize_colors: colors,
                ..base(ImageFormat::Png)
            },
        )
    };
    let lossless = at(None, None);
    let quant = at(Some(true), None);
    assert!(
        quant.len() < lossless.len(),
        "quantized ({}) must undercut lossless ({})",
        quant.len(),
        lossless.len()
    );
    let q16 = at(Some(true), Some(16));
    assert!(
        q16.len() < quant.len(),
        "16 colors ({}) must undercut 256 ({})",
        q16.len(),
        quant.len()
    );
    assert_eq!(dims_of(&quant), dims_of(&lossless));
    // Some(documented default) == None, the override-precedence pin.
    assert_eq!(at(Some(false), None), lossless);
    assert_eq!(at(Some(true), Some(256)), quant);
}

/// The opaque-only scope is a contract: an alpha source ignores the
/// quantize knob entirely and encodes the same lossless RGBA bytes.
#[test]
fn png_quantize_leaves_alpha_sources_lossless() {
    // A PNG with a real alpha gradient (not fully opaque).
    let (w, h) = (64usize, 48usize);
    let mut rgba = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for x in 0..w {
            rgba.extend([x as u8 * 3, y as u8 * 5, 128, (x * 4).min(255) as u8]);
        }
    }
    let mut src = Vec::new();
    let mut enc = png::Encoder::new(&mut src, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(&rgba).unwrap();
    writer.finish().unwrap();

    let at = |q: Option<bool>| {
        run(
            &src,
            &Params {
                png_quantize: q,
                ..base(ImageFormat::Png)
            },
        )
    };
    assert_eq!(
        at(Some(true)),
        at(None),
        "alpha sources must be untouched by the quantize knob"
    );
}

/// Quantization must not cost the ICC profile: the indexed encode
/// carries the source profile through like the lossless one does.
#[test]
fn png_quantize_preserves_icc_profile() {
    let px = corner_base(64, 48, 8);
    let profile = common::fake_icc(400);
    let src = png_with_icc(&px, 64, 48, &profile);
    let out = run(
        &src,
        &Params {
            png_quantize: Some(true),
            ..base(ImageFormat::Png)
        },
    );
    assert_eq!(png_icc(&out).as_deref(), Some(&profile[..]));
}

/// A low-color flat image survives quantization essentially exactly:
/// four well-separated colors at 256 palette slots decode back within
/// a small tolerance (Wu's histogram bins merge nothing here, and
/// dithering has no error to diffuse).
#[test]
fn png_quantize_is_near_exact_on_flat_low_color_images() {
    let (w, h) = (64usize, 64usize);
    let palette = [[0u8, 0, 0], [255, 0, 0], [0, 255, 0], [64, 128, 255]];
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        for x in 0..w {
            rgb.extend(palette[(x / 32) + 2 * (y / 32)]);
        }
    }
    let mut src = Vec::new();
    let mut enc = png::Encoder::new(&mut src, w as u32, h as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().unwrap();
    writer.write_image_data(&rgb).unwrap();
    writer.finish().unwrap();

    let out = run(
        &src,
        &Params {
            png_quantize: Some(true),
            ..base(ImageFormat::Png)
        },
    );
    // Decode with the png crate directly (EXPAND resolves the palette;
    // pipeline::decode_and_resize is the JPEG-path helper, not for PNG).
    let mut dec = png::Decoder::new(std::io::Cursor::new(&out));
    dec.set_transformations(png::Transformations::EXPAND);
    let mut reader = dec.read_info().unwrap();
    let mut decoded = vec![0u8; reader.output_buffer_size().unwrap()];
    let info = reader.next_frame(&mut decoded).unwrap();
    assert_eq!(info.color_type, png::ColorType::Rgb, "EXPAND resolves PLTE");
    decoded.truncate(info.buffer_size());
    assert_eq!((info.width, info.height), (64, 64));
    let max_delta = decoded
        .iter()
        .zip(&rgb)
        .map(|(a, b)| a.abs_diff(*b))
        .max()
        .unwrap();
    assert!(max_delta <= 4, "max channel delta {max_delta} > 4");
}

/// The effort/quantize interaction (issue #5 field data): with
/// quantization active and no explicit effort, the encoder runs at
/// `balanced` — byte-identical to asking for it — because `fast`
/// leaves half the reduction on the table (1.7x vs 3.0x). An explicit
/// effort still wins, and the lossless path's `fast` default is
/// untouched (pinned by png_effort_override_steers_the_encoder).
#[test]
fn png_quantize_defaults_effort_to_balanced() {
    let src = fixture("photo.jpg");
    let at = |q: Option<bool>, e: Option<PngEffort>| {
        run(
            &src,
            &Params {
                png_quantize: q,
                png_effort: e,
                ..base(ImageFormat::Png)
            },
        )
    };
    let default_effort = at(Some(true), None);
    assert_eq!(
        default_effort,
        at(Some(true), Some(PngEffort::Balanced)),
        "quantized default effort is balanced"
    );
    let explicit_fast = at(Some(true), Some(PngEffort::Fast));
    assert!(
        default_effort.len() < explicit_fast.len(),
        "balanced default ({}) must undercut explicit fast ({})",
        default_effort.len(),
        explicit_fast.len()
    );
}
