//! Format-matrix integration tests over `pipeline::process`, driven by
//! committed fixtures (tests/fixtures/, all 200x150 unless noted).

use oximg::pipeline::{self, Encoder, ImageFormat, Params};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

fn params(max: u32) -> Params {
    Params {
        max_width: max,
        max_height: max,
        quality: 80.0,
        encoder: Encoder::Jpegli,
        parallel: 1,
    }
}

/// Dims of any output, via the pipeline's own header probe.
fn dims_of(bytes: &[u8]) -> (usize, usize) {
    let (_, w, h) = pipeline::probe(bytes).unwrap();
    (w, h)
}

#[test]
fn every_source_shape_resizes_in_its_own_format() {
    for (name, format) in [
        ("rgb.png", ImageFormat::Png),
        ("rgba.png", ImageFormat::Png),
        ("gray.png", ImageFormat::Png),
        ("graya.png", ImageFormat::Png),
        ("palette.png", ImageFormat::Png),
        ("gray16.png", ImageFormat::Png),
        ("rgb16.png", ImageFormat::Png),
        ("interlaced.png", ImageFormat::Png),
        ("photo.jpg", ImageFormat::Jpeg),
        ("photo.webp", ImageFormat::Webp),
        ("alpha.webp", ImageFormat::Webp),
    ] {
        let (out, fmt) = pipeline::process(&fixture(name), &params(100))
            .unwrap_or_else(|e| panic!("{name}: {e:#}"));
        assert_eq!(fmt, format, "{name}");
        assert!(!out.is_empty(), "{name}: empty output");
        if format == ImageFormat::Jpeg {
            assert_eq!(dims_of(&out), (100, 75), "{name}");
        }
    }
}

#[test]
fn png_and_webp_outputs_have_expected_dims() {
    for name in ["rgb.png", "rgba.png", "photo.webp", "alpha.webp"] {
        let (out, _) = pipeline::process(&fixture(name), &params(100)).unwrap();
        assert_eq!(dims_of(&out), (100, 75), "{name}");
    }
}

#[test]
fn never_upscales() {
    let (out, _) = pipeline::process(&fixture("tiny.jpg"), &params(500)).unwrap();
    assert_eq!(dims_of(&out), (40, 30));
}

#[test]
fn alpha_survives_and_does_not_bleed() {
    // rgba.png has alpha 0 at the left edge and ~255 at the right edge.
    let (out, _) = pipeline::process(&fixture("rgba.png"), &params(50)).unwrap();
    let mut r = png::Decoder::new(std::io::Cursor::new(&out))
        .read_info()
        .unwrap();
    let info = r.info();
    assert_eq!(info.color_type, png::ColorType::Rgba);
    let (w, h) = (info.width as usize, info.height as usize);
    let mut buf = vec![0u8; r.output_buffer_size().unwrap()];
    r.next_frame(&mut buf).unwrap();
    let px = |x: usize, y: usize| -> [u8; 4] {
        buf[(y * w + x) * 4..(y * w + x) * 4 + 4]
            .try_into()
            .unwrap()
    };
    let mid = h / 2;
    assert!(px(1, mid)[3] < 24, "left edge should stay transparent");
    assert!(px(w - 2, mid)[3] > 230, "right edge should stay opaque");
}

#[test]
fn animated_webp_is_rejected_cleanly() {
    let err = pipeline::process(&fixture("animated.webp"), &params(100)).unwrap_err();
    assert!(format!("{err:#}").contains("animated"), "got: {err:#}");
}

#[test]
fn garbage_and_truncation_error_instead_of_panicking() {
    assert!(pipeline::process(b"GIF89a not supported here", &params(100)).is_err());
    assert!(pipeline::process(b"", &params(100)).is_err());
    for name in ["photo.jpg", "rgb.png", "photo.webp"] {
        let full = fixture(name);
        // Header-level truncation must always error.
        for cut in [16, 64] {
            assert!(
                pipeline::process(&full[..cut], &params(100)).is_err(),
                "{name} truncated at {cut} should error"
            );
        }
        // Mid-stream truncation: PNG and WebP error; JPEG follows the
        // libjpeg-family convention of completing with filler blocks (the
        // behavior browsers and other proxies share), so it must at least
        // not panic and still produce a decodable image.
        let half = pipeline::process(&full[..full.len() / 2], &params(100));
        if name == "photo.jpg" {
            let (out, _) = half.expect("truncated jpeg should degrade gracefully");
            assert_eq!(dims_of(&out), (100, 75));
        } else {
            assert!(half.is_err(), "{name} truncated at half should error");
        }
    }
}

#[test]
fn jpeg_presets_all_produce_decodable_output() {
    for encoder in [Encoder::Jpegli, Encoder::MozFast, Encoder::MozSmall] {
        let p = Params {
            encoder,
            ..params(100)
        };
        let (out, fmt) = pipeline::process(&fixture("photo.jpg"), &p).unwrap();
        assert_eq!(fmt, ImageFormat::Jpeg);
        assert_eq!(dims_of(&out), (100, 75), "{encoder:?}");
    }
}
