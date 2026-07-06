//! Library example: resize an image to a thumbnail, keeping its format.
//!
//!   cargo run --release --example thumbnail -- input.jpg 300 200 out.jpg
//!
//! The whole library surface here is `Params` + `process_path`: give it
//! a maximum box, get back the re-encoded bytes and the format they are
//! in. The source is never upscaled and its aspect ratio is preserved
//! (the box is a bound, not a target).

use oximg::pipeline::{self, Params};

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let [_, input, max_w, max_h, output] = args.as_slice() else {
        eprintln!("usage: thumbnail <input> <max_w> <max_h> <output>");
        std::process::exit(2);
    };

    // Set only what this task needs; ..Default::default() fills the
    // rest (q80 jpegli, single-threaded, source format) and keeps
    // compiling if the library adds a field later.
    let params = Params {
        max_width: max_w.parse()?,
        max_height: max_h.parse()?,
        ..Default::default()
    };

    // process_path streams the decode straight from the file. For bytes
    // already in memory use pipeline::process(&bytes, &params) instead.
    let (bytes, format) = pipeline::process_path(input.as_ref(), &params)?;

    std::fs::write(output, &bytes)?;
    let (_, w, h) = pipeline::probe(&bytes)?;
    println!(
        "wrote {output} ({} bytes, {w}x{h}, {})",
        bytes.len(),
        format.content_type()
    );
    Ok(())
}
