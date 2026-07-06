//! Parity probe against the reference libavif tools at identical settings.
//! Encode: avif_parity <in.ppm> <quality> <out.avif>   (compare with avifenc -c svt)
//! Decode: avif_parity <in.avif> <out.ppm>             (compare with avifdec)
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1])?;

    if data.len() > 12 && &data[4..8] == b"ftyp" {
        let t = std::time::Instant::now();
        let (rgb, w, h, channels) = oximg::avif::decode_avif(&data)?;
        anyhow::ensure!(channels == 3, "decode mode writes P6 (RGB) only");
        eprintln!(
            "decoded {}x{} in {:.1}ms",
            w,
            h,
            t.elapsed().as_secs_f64() * 1e3
        );
        let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
        out.extend_from_slice(&rgb);
        std::fs::write(&args[2], out)?;
        return Ok(());
    }

    // minimal P6 parse (qcli-compatible)
    let mut parts = Vec::new();
    let mut pos = 0;
    while parts.len() < 4 {
        while (data[pos] as char).is_whitespace() {
            pos += 1;
        }
        if data[pos] == b'#' {
            while data[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }
        let s = pos;
        while !(data[pos] as char).is_whitespace() {
            pos += 1;
        }
        parts.push(String::from_utf8_lossy(&data[s..pos]).to_string());
    }
    pos += 1;
    let (w, h): (usize, usize) = (parts[1].parse()?, parts[2].parse()?);
    let rgb = &data[pos..pos + w * h * 3];
    let speed: i8 = std::env::var("OXIMG_SPEED")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(8);
    let p = oximg::avif::AvifParams {
        quality: args[2].parse()?,
        speed,
        ..Default::default()
    };
    let t = std::time::Instant::now();
    let out = oximg::avif::encode_avif(rgb, w, h, 3, &p, None)?;
    eprintln!(
        "{}x{} q{} -> {} bytes in {:.1}ms",
        w,
        h,
        p.quality,
        out.len(),
        t.elapsed().as_secs_f64() * 1e3
    );
    std::fs::write(&args[3], out)?;
    Ok(())
}
