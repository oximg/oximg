//! Quality-benchmark CLI: exercises the pipeline outside the HTTP server.
//!
//! Modes:
//!   resize:    qcli resize <in.jpg> <max_w> <max_h> <quality> <fast|small> <out.jpg> [out.ppm]
//!              (full pipeline; optional PPM dump of the pre-encode pixels)
//!   encode:    qcli encode <in.ppm> <quality> <fast|small> <out.jpg>
//!              (encoder isolation: PPM in, JPEG out, no resize)
//!   transcode: qcli transcode <in> <max_w> <max_h> <jpg|png|webp|avif> <out>
//!              (full cross-format pipeline, any supported source)

use oximg::pipeline;
use std::fs;

fn write_ppm(path: &str, rgb: &[u8], w: usize, h: usize) -> anyhow::Result<()> {
    let mut buf = format!("P6\n{w} {h}\n255\n").into_bytes();
    buf.extend_from_slice(rgb);
    fs::write(path, buf)?;
    Ok(())
}

fn read_ppm(path: &str) -> anyhow::Result<(Vec<u8>, usize, usize)> {
    let data = fs::read(path)?;
    // P6\n<w> <h>\n255\n<binary rgb>
    let mut fields = Vec::new(); // (magic, w, h, maxval)
    let mut pos = 0;
    while fields.len() < 4 {
        // skip whitespace and comments
        while pos < data.len() && (data[pos] as char).is_whitespace() {
            pos += 1;
        }
        if data[pos] == b'#' {
            while data[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let start = pos;
        while pos < data.len() && !(data[pos] as char).is_whitespace() {
            pos += 1;
        }
        fields.push(std::str::from_utf8(&data[start..pos])?.to_string());
    }
    pos += 1; // single whitespace after maxval
    anyhow::ensure!(fields[0] == "P6" && fields[3] == "255", "unsupported PPM");
    let (w, h): (usize, usize) = (fields[1].parse()?, fields[2].parse()?);
    anyhow::ensure!(data.len() - pos >= w * h * 3, "truncated PPM");
    Ok((data[pos..pos + w * h * 3].to_vec(), w, h))
}

fn params(quality: f32, preset: &str) -> pipeline::Params {
    pipeline::Params {
        max_width: 0,
        max_height: 0,
        quality,
        encoder: pipeline::Encoder::from_preset(preset),
        parallel: 1,
        output: None,
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    match args[1].as_str() {
        "resize" => {
            let (input, max_w, max_h) = (&args[2], args[3].parse()?, args[4].parse()?);
            let (quality, preset, out) = (args[5].parse()?, args[6].as_str(), &args[7]);
            let jpeg = fs::read(input)?;
            let par: usize = std::env::var("OXIMG_PAR")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1);
            let (rgb, w, h) = pipeline::decode_and_resize(&jpeg, max_w, max_h, par)?;
            fs::write(out, pipeline::encode(&rgb, w, h, &params(quality, preset))?)?;
            if let Some(ppm) = args.get(8) {
                write_ppm(ppm, &rgb, w, h)?;
            }
            eprintln!("{w}x{h}");
        }
        "encode" => {
            let (input, quality, preset, out) =
                (&args[2], args[3].parse()?, args[4].as_str(), &args[5]);
            let (rgb, w, h) = read_ppm(input)?;
            fs::write(out, pipeline::encode(&rgb, w, h, &params(quality, preset))?)?;
        }
        "transcode" => {
            let (input, max_w, max_h) = (&args[2], args[3].parse()?, args[4].parse()?);
            let (fmt, out) = (args[5].as_str(), &args[6]);
            let target = pipeline::ImageFormat::from_token(fmt)
                .ok_or_else(|| anyhow::anyhow!("unknown format token: {fmt}"))?;
            let p = pipeline::Params {
                max_width: max_w,
                max_height: max_h,
                quality: std::env::var("QUALITY")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(80.0),
                encoder: pipeline::Encoder::from_preset(
                    std::env::var("PRESET").as_deref().unwrap_or("jpegli"),
                ),
                parallel: 1,
                output: Some(target),
            };
            let (bytes, got) = pipeline::process(&fs::read(input)?, &p)?;
            anyhow::ensure!(got == target, "pipeline returned {got:?}");
            fs::write(out, bytes)?;
        }
        other => anyhow::bail!("unknown mode: {other}"),
    }
    Ok(())
}
