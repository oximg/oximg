//! Decode-only A/B benchmark: mozjpeg (libjpeg-turbo) vs jpegli, at several
//! DCT shrink-on-load factors. Measures decode wall-time and cross-checks the
//! two decoders' RGB output for drop-in compatibility.
//!
//! Run: cargo run --release --example decode_bench

use std::time::Instant;

const SAMPLES: usize = 25;
const WARMUP: usize = 3;
// num/8 DCT scaling; 8 = full decode, 4 = half, 2 = quarter, 1 = eighth.
const SCALES: [u8; 5] = [8, 6, 4, 2, 1];

fn moz_decode(jpeg: &[u8], num: u8) -> (usize, usize, Vec<u8>) {
    let mut dec = mozjpeg::Decompress::new_mem(jpeg).unwrap();
    dec.scale(num);
    let mut started = dec.rgb().unwrap();
    let (w, h) = (started.width(), started.height());
    let mut out = vec![0u8; w * h * 3];
    started.read_scanlines_into(&mut out).unwrap();
    started.finish().unwrap();
    (w, h, out)
}

fn jpegli_decode(jpeg: &[u8], num: u8) -> (usize, usize, Vec<u8>) {
    let mut dec = jpegli::Decompress::new_mem(jpeg).unwrap();
    dec.scale(num);
    let mut started = dec.rgb().unwrap();
    let (w, h) = (started.width(), started.height());
    let mut out = vec![0u8; w * h * 3];
    started.read_scanlines_into::<u8>(&mut out).unwrap();
    started.finish().unwrap();
    (w, h, out)
}

type DecodeFn = fn(&[u8], u8) -> (usize, usize, Vec<u8>);

fn median_ms(jpeg: &[u8], num: u8, f: DecodeFn) -> f64 {
    for _ in 0..WARMUP {
        std::hint::black_box(f(jpeg, num));
    }
    let mut ts: Vec<f64> = (0..SAMPLES)
        .map(|_| {
            let t = Instant::now();
            std::hint::black_box(f(jpeg, num));
            t.elapsed().as_secs_f64() * 1e3
        })
        .collect();
    ts.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ts[ts.len() / 2]
}

fn compare(a: &[u8], b: &[u8]) -> (f64, u8) {
    let n = a.len().min(b.len());
    let mut sum = 0u64;
    let mut max = 0u8;
    for i in 0..n {
        let d = a[i].abs_diff(b[i]);
        sum += d as u64;
        max = max.max(d);
    }
    (sum as f64 / n as f64, max)
}

fn bench_image(name: &str, jpeg: &[u8]) {
    let (sw, sh, _) = moz_decode(jpeg, 8);
    println!(
        "\n=== {name} ({sw}x{sh}, {:.1} MB) ===",
        jpeg.len() as f64 / 1e6
    );
    println!(
        "{:>5} {:>12} {:>9} {:>9} {:>7}  {:>10} {:>7}",
        "num", "decoded", "moz ms", "jpegli ms", "speed", "meanΔ", "maxΔ"
    );
    for num in SCALES {
        let m = median_ms(jpeg, num, moz_decode);
        let j = median_ms(jpeg, num, jpegli_decode);
        let (mw, mh, mpx) = moz_decode(jpeg, num);
        let (jw, jh, jpx) = jpegli_decode(jpeg, num);
        let dims = if (mw, mh) == (jw, jh) {
            format!("{mw}x{mh}")
        } else {
            format!("{mw}x{mh}/{jw}x{jh}")
        };
        let (mean, max) = if (mw, mh) == (jw, jh) {
            compare(&mpx, &jpx)
        } else {
            (f64::NAN, 255)
        };
        let ratio = m / j;
        println!(
            "{num:>5} {dims:>12} {m:>9.2} {j:>9.2} {:>6.2}x  {mean:>10.3} {max:>7}",
            ratio
        );
    }
}

fn main() {
    println!("decode A/B: mozjpeg 0.10.x vs jpegli (fork of mozjpeg-rust) — median of {SAMPLES}");
    println!(
        "speed = moz_ms / jpegli_ms  (>1 means jpegli faster); Δ = |moz-jpegli| RGB byte diff"
    );
    for (name, path) in [
        ("test-medium", "images/test-medium.jpg"),
        ("test-large", "images/test-large.jpg"),
    ] {
        match std::fs::read(path) {
            Ok(bytes) => bench_image(name, &bytes),
            Err(e) => println!("skip {name}: {e}"),
        }
    }
}
