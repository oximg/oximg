//! Library example: identify an image without decoding its pixels.
//!
//!   cargo run --release --example probe -- input.webp
//!
//! `probe` reads only the header — format sniff plus *stored*
//! dimensions — so it is cheap enough to run on every upload before
//! deciding whether to accept it. Note the dimensions are the stored
//! ones: for EXIF/AVIF orientations that swap axes, the displayed
//! frame `process` emits has width and height exchanged.

use oximg::pipeline;

fn main() -> anyhow::Result<()> {
    let Some(input) = std::env::args().nth(1) else {
        eprintln!("usage: probe <input>");
        std::process::exit(2);
    };

    let bytes = std::fs::read(&input)?;
    let (format, w, h) = pipeline::probe(&bytes)?;
    println!(
        "{input}: {} {w}x{h} ({} stored pixels)",
        format.content_type(),
        w as u64 * h as u64
    );
    Ok(())
}
