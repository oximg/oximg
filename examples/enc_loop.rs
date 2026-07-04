//! In-process AVIF encode loop for profiler attachment and preset/library
//! A/B timing (pairs with avif_parity; OXIMG_SPEED overrides the preset).
//! Usage: enc_loop <in.ppm> <quality> [iters]
fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&args[1])?;
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
    let iters: usize = args.get(3).and_then(|a| a.parse().ok()).unwrap_or(100);
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
    let mut total = 0usize;
    for _ in 0..iters {
        total += oximg::avif::encode_avif(rgb, w, h, 3, &p)?.len();
    }
    eprintln!(
        "{:.2} ms/encode (bytes {})",
        t.elapsed().as_secs_f64() * 1e3 / iters as f64,
        total / iters
    );
    Ok(())
}
