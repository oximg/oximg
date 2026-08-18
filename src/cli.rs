//! One-shot subcommands (`oximg resize`, `oximg probe`): the same
//! pipeline as the server, without the HTTP layer. Argument handling
//! stays dependency-free, like the server's flag handling in main.rs.
//!
//! Encode knobs that are env-configured on the server
//! (OXIMG_WEBP_QUALITY, OXIMG_PNG_EFFORT, ...) apply to CLI encodes
//! the same way, and are validated fail-closed at startup like the
//! server does — a typo'd knob must not silently encode at a default.

use anyhow::Context;
use oximg::pipeline::{self, Encoder, ImageFormat, Params};

pub fn print_help() {
    println!(
        "oximg {}\n\n\
         High-performance image compression: HTTP server and one-shot CLI.\n\n\
         Usage:\n  \
           oximg [serve]\n          \
             Run the HTTP resize server (the default). All server\n          \
             configuration is via environment variables (PORT, IMAGES_DIR,\n          \
             OXIMG_*); see the README.\n  \
           oximg resize <in> <max_w> <max_h> <out> [options]\n          \
             Fit one image within <max_w> x <max_h> (never enlarges) and\n          \
             re-encode it. 0 leaves an axis unconstrained: `750 0` is\n          \
             width-only, `0 0` re-encodes at the source's own size.\n          \
             Output format: --format, else the <out> file\n          \
             extension, else the source's own format (GIF sources,\n          \
             which have no encoder here, become WebP).\n          \
             -q, --quality N    JPEG quality, 1-100 (default 80)\n          \
             -f, --format FMT   jpg | png | webp | avif\n          \
             --preset P         jpegli (default) | fast | small\n  \
           oximg probe <file>\n          \
             Print the format and stored dimensions (header-only, no\n          \
             pixel decode).\n  \
           oximg --version | --help",
        env!("CARGO_PKG_VERSION")
    );
}

fn usage_error(msg: &str) -> ! {
    eprintln!("oximg: {msg} (try --help)");
    std::process::exit(2);
}

/// `oximg resize <in> <max_w> <max_h> <out> [-q N] [-f fmt] [--preset P]`
pub fn resize(args: &[String]) -> anyhow::Result<()> {
    // Same fail-closed startup contract as the server: a set-but-invalid
    // OXIMG_* knob is a fatal configuration error, never a silent default.
    if let Err(e) = oximg::config_validate() {
        eprintln!("oximg: fatal: {e}");
        std::process::exit(2);
    }
    let mut positional: Vec<&str> = Vec::new();
    let mut quality = 80.0f32;
    let mut explicit: Option<ImageFormat> = None;
    let mut encoder = Encoder::Jpegli;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "-h" | "--help" => {
                print_help();
                return Ok(());
            }
            "-q" | "--quality" => {
                let v = it
                    .next()
                    .unwrap_or_else(|| usage_error("--quality needs a value"));
                quality = v
                    .parse()
                    .ok()
                    .filter(|q| (1.0..=100.0).contains(q))
                    .unwrap_or_else(|| usage_error(&format!("invalid quality {v:?} (1-100)")));
            }
            "-f" | "--format" => {
                let v = it
                    .next()
                    .unwrap_or_else(|| usage_error("--format needs a value"));
                explicit = Some(ImageFormat::from_token(v).unwrap_or_else(|| {
                    usage_error(&format!("unknown format {v:?} (jpg|png|webp|avif)"))
                }));
            }
            "--preset" => {
                let v = it
                    .next()
                    .unwrap_or_else(|| usage_error("--preset needs a value"));
                // Unlike the server's PRESET env (which falls back to the
                // default), an explicitly typed flag value must not be
                // silently reinterpreted.
                encoder = match v.as_str() {
                    "jpegli" => Encoder::Jpegli,
                    "fast" => Encoder::MozFast,
                    "small" => Encoder::MozSmall,
                    _ => usage_error(&format!("unknown preset {v:?} (jpegli|fast|small)")),
                };
            }
            flag if flag.starts_with('-') && flag.len() > 1 => {
                usage_error(&format!("unknown option {flag:?}"))
            }
            p => positional.push(p),
        }
    }
    let [input, max_w, max_h, output] = positional[..] else {
        usage_error("usage: oximg resize <in> <max_w> <max_h> <out> [-q N] [-f fmt] [--preset P]");
    };
    // 0 = unconstrained axis, spelled u32::MAX in Params (the library's
    // "no downscale bound"). Unlike the server, 0 0 is allowed here:
    // "re-encode at the source's own size" is a useful one-shot
    // transcode, and the CLI has no reason to refuse it.
    let dim = |v: &str, name: &str| -> u32 {
        match v.parse() {
            Ok(0) => u32::MAX,
            Ok(d) => d,
            Err(_) => usage_error(&format!("invalid {name} {v:?}")),
        }
    };
    let params = Params {
        max_width: dim(max_w, "max_w"),
        max_height: dim(max_h, "max_h"),
        quality,
        encoder,
        // Precedence mirrors the server URL grammar: explicit flag >
        // output extension > source format.
        output: explicit.or_else(|| format_from_ext(output)),
        ..Default::default()
    };
    let (bytes, format) = pipeline::process_path(std::path::Path::new(input), &params)
        .with_context(|| format!("process {input}"))?;
    // Same reporting threshold as the server: useful for sizing a cap
    // against a corpus offline, one file at a time.
    if let Some(report) = pipeline::decode_report_above_threshold() {
        eprintln!("oximg: decoded-bytes file={input:?} {report}");
    }
    std::fs::write(output, &bytes).with_context(|| format!("write {output}"))?;
    // Summary on stderr: stdout stays clean for scripting.
    let (_, w, h) = pipeline::probe(&bytes)?;
    eprintln!(
        "oximg: wrote {output} ({} bytes, {w}x{h}, {})",
        bytes.len(),
        format.content_type()
    );
    Ok(())
}

/// `oximg probe <file>` — the probe example as a shipped command.
pub fn probe(args: &[String]) -> anyhow::Result<()> {
    let [input] = args else {
        usage_error("usage: oximg probe <file>");
    };
    if input == "-h" || input == "--help" {
        print_help();
        return Ok(());
    }
    let bytes = std::fs::read(input).with_context(|| format!("read {input}"))?;
    let (format, w, h) = pipeline::probe(&bytes)?;
    println!(
        "{input}: {} {w}x{h} ({} stored pixels)",
        format.content_type(),
        w as u64 * h as u64
    );
    Ok(())
}

/// Map an output filename's extension to a format the same way the
/// server maps `@{fmt}` tokens; unknown extensions keep the source
/// format (the summary line makes the actual format visible). `.gif` is
/// among the unknowns on purpose — nothing here encodes GIF, so a GIF
/// source resolves to WebP whatever the output file is called, and the
/// summary line is what says so.
fn format_from_ext(path: &str) -> Option<ImageFormat> {
    let ext = std::path::Path::new(path)
        .extension()?
        .to_str()?
        .to_ascii_lowercase();
    ImageFormat::from_token(&ext)
}
