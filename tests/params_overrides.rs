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
