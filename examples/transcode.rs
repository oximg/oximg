//! Library example: resize *and* convert to a different format.
//!
//!   cargo run --release --example transcode -- photo.jpg 800 800 webp out.webp
//!
//! Same call as `thumbnail`, but `Params.output` names a target format
//! (parsed from the same tokens the HTTP `@fmt` suffix accepts). A
//! source's color profile passes through and EXIF/AVIF orientation is
//! applied, so a rotated phone JPEG converts upright into WebP with its
//! ICC intact. `avif` targets require the `avif` cargo feature.

use oximg::pipeline::{self, Encoder, ImageFormat, Params};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, max_w, max_h, fmt, output] = args.as_slice() else {
        eprintln!("usage: transcode <input> <max_w> <max_h> <jpg|png|webp|avif> <output>");
        std::process::exit(2);
    };

    let target = ImageFormat::from_token(fmt)
        .ok_or_else(|| anyhow::anyhow!("unknown target format {fmt:?}"))?;

    let params = Params {
        max_width: max_w.parse()?,
        max_height: max_h.parse()?,
        quality: 80.0,
        encoder: Encoder::Jpegli,
        parallel: 1,
        output: Some(target),
    };

    let src = std::fs::read(input)?;
    let (bytes, format) = pipeline::process(&src, &params)?;
    assert_eq!(format, target);

    std::fs::write(output, &bytes)?;
    println!(
        "{} -> {output} ({} bytes, {})",
        input,
        bytes.len(),
        format.content_type()
    );
    Ok(())
}
