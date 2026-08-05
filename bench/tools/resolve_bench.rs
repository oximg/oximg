//! Per-request knob-resolution cost: the measured baseline for the
//! settings refactor. `pipeline::bench_resolve` performs the knob
//! resolution work of one JPEG->WebP request — the resolver calls the
//! pipeline actually makes on that path. Run this on the pre-refactor
//! commit and on the refactor commit, same machine, and compare:
//!
//!     cargo run --release --example resolve_bench --features bench-internals
//!
//! Rounds are printed individually (bench/METHODOLOGY.md rule 6:
//! expect outliers, never conclude from one round).

use std::hint::black_box;
use std::time::Instant;

const WARMUP: u32 = 100_000;
const ITERS: u32 = 10_000_000;
const ROUNDS: u32 = 3;

fn bench(name: &str, mut f: impl FnMut()) {
    for _ in 0..WARMUP {
        f();
    }
    for round in 1..=ROUNDS {
        let t = Instant::now();
        for _ in 0..ITERS {
            f();
        }
        let ns = t.elapsed().as_nanos() as f64 / ITERS as f64;
        println!("{name} (round {round}): {ns:.2} ns/iter");
    }
}

fn main() {
    use oximg::pipeline::{Params, PngEffort};

    let defaults = Params::default();
    let all_set = Params {
        webp_quality: Some(40.0),
        png_effort: Some(PngEffort::Fast),
        png_quantize: Some(true),
        png_quantize_colors: Some(128),
        auto_rotate: Some(true),
        icc: Some(true),
        flatten_bg: Some([0, 0, 0]),
        linear_light: Some(true),
        #[cfg(feature = "avif")]
        avif_quality: Some(60),
        ..Params::default()
    };

    bench("resolve/defaults", || {
        black_box(oximg::pipeline::bench_resolve(black_box(&defaults)));
    });
    bench("resolve/all-overrides", || {
        black_box(oximg::pipeline::bench_resolve(black_box(&all_set)));
    });
}
