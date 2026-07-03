//! Parity probe: encode a PPM via our SVT path for comparison with
//! `avifenc -c svt` at identical settings.
//! Usage: avif_parity <in.ppm> <quality> <out.avif>
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1])?;
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
    let p = oximg::avif::AvifParams {
        quality: args[2].parse()?,
        ..Default::default()
    };
    let t = std::time::Instant::now();
    let out = oximg::avif::encode_avif(rgb, w, h, &p)?;
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
