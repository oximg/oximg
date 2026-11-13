//! Isolated, one-shot native animation experiment. See bench/gif-native.py.
//! No hidden env mode; the server and normal CLI cannot select these modes.

use anyhow::{Context, Result};
use oximg::pipeline::{Params, gif_bench};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("decode") && args.len() == 3 {
        let bytes = std::fs::read(&args[1])?;
        std::fs::write(&args[2], gif_bench::decode_webp(&bytes)?)?;
        return Ok(());
    }
    anyhow::ensure!(
        args.len() == 5,
        "usage: gif_native <gif> <width> <height> <lossy|lossless|mixed|reference> <out>; or decode <webp> <raw>"
    );
    let mode = match args[3].as_str() {
        "lossy" => gif_bench::Mode::Lossy,
        "lossless" => gif_bench::Mode::Lossless,
        "mixed" => gif_bench::Mode::Mixed,
        "reference" => gif_bench::Mode::Reference,
        _ => anyhow::bail!("unknown animation mode"),
    };
    let bytes = std::fs::read(&args[0]).context("read GIF")?;
    let p = Params {
        max_width: match args[1].parse()? {
            0 => u32::MAX,
            n => n,
        },
        max_height: match args[2].parse()? {
            0 => u32::MAX,
            n => n,
        },
        ..Params::default()
    };
    let t = std::time::Instant::now();
    let out = gif_bench::encode(&bytes, &p, mode)?;
    let ms = t.elapsed().as_secs_f64() * 1000.0;
    let peak_rss_bytes = peak_rss()?;
    std::fs::write(&args[4], &out.bytes)?;
    println!(
        "{}",
        serde_json::json!({
            "ok": true, "mode": args[3], "bytes": out.bytes.len(), "pipeline_ms": ms,
            "peak_rss_bytes": peak_rss_bytes,
            "gif_animation": serde_json::from_str::<serde_json::Value>(&out.decision)?,
            "memory_model": "production lossy estimate only; not validated for experimental modes"
        })
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn peak_rss() -> Result<u64> {
    // Linux getrusage can retain the Python launcher's pre-exec RSS high-water
    // mark. VmHWM belongs to this executable's mm, so small cells don't inherit
    // a fictitious ~30 MiB floor from the orchestration process.
    let status = std::fs::read_to_string("/proc/self/status")?;
    let kb = status
        .lines()
        .find_map(|l| l.strip_prefix("VmHWM:"))
        .and_then(|s| s.split_whitespace().next())
        .context("missing VmHWM")?;
    Ok(kb.parse::<u64>()? * 1024)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn peak_rss() -> Result<u64> {
    // SAFETY: getrusage initializes a writable rusage on success; read only
    // after checking its return. Fresh process per cell, no previous mode peak.
    unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        anyhow::ensure!(
            libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0,
            "getrusage"
        );
        let unit = if cfg!(target_os = "macos") { 1 } else { 1024 };
        Ok(usage.ru_maxrss as u64 * unit)
    }
}

#[cfg(not(unix))]
fn peak_rss() -> Result<u64> {
    anyhow::bail!("native RSS experiment requires Unix")
}
