# oximg

[![Crates.io](https://img.shields.io/crates/v/oximg.svg)](https://crates.io/crates/oximg)
[![Docs.rs](https://docs.rs/oximg/badge.svg)](https://docs.rs/oximg)
[![CI](https://github.com/oximg/oximg/actions/workflows/ci.yml/badge.svg)](https://github.com/oximg/oximg/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

High-performance image compression in Rust: a library, a CLI, and a
self-hostable HTTP server (PoC). JPEG, PNG, WebP — and AVIF with the
`avif` feature — in and out; sources are format-sniffed by magic bytes
and re-encoded in their own format. On imgproxy's official benchmark
harness, run on the same AWS instance types as their published
results, oximg leads every format cell on both x86-64 and Graviton
while resizing in linear light at measurably higher output quality
(see [Benchmarks](#benchmarks)).

## Pipeline

```
source bytes (local file or HTTP origin)
  → format sniff → decode
      JPEG: mozjpeg streaming decode, DCT shrink-on-load (kept ≥ 1.7x target size)
      PNG:  png crate (palette/gray/16-bit normalized to RGB(A)8)
      WebP: libwebp
      AVIF: dav1d (8/10/12-bit, all subsamplings, alpha, bilinear chroma upsampling)
  → linear-light resize: sRGB u8 → linear u16 → Lanczos3 → sRGB u8
      (alpha is premultiplied before resampling, unpremultiplied after;
       x86-64 convolves via pic-scale AVX-512, aarch64 via an in-tree
       ring-scheduled NEON f32 kernel verified against an f64 reference)
  → encode in the source format
      JPEG: jpegli, progressive (PRESET=fast / PRESET=small select mozjpeg profiles)
      PNG:  png crate | WebP: libwebp | AVIF: SVT-AV1 (10-bit 4:2:0, tune=ssim)
```

Concurrent identical requests are coalesced and share one result.
CPU concurrency is pinned to the core count with a semaphore; the HTTP
layer (axum/tokio) only does queueing and IO.

## Benchmarks

imgproxy's official harness (DIV2K corpus over nginx, fit into 512x512,
k6, all defaults) on the AWS instance types behind imgproxy's published
numbers — req/s, higher is better, p95 in parentheses:

| c7i.large (x86-64) | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg | **81.8** (32 ms) | **32.9** (79 ms) | **31.0** (91 ms) | **19.1** (149 ms) |
| best of imgproxy/imagor/thumbor | 68.4 | 15.5 | 20.5 | 15.7 |

| c7g.large (Graviton3) | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg | **81.3** (31 ms) | **37.6** (68 ms) | **40.0** (72 ms) | **23.1** (126 ms) |
| best of imgproxy/imagor/thumbor | 68.4 | 22.0 | 25.4 | 20.1 |

At the same time, output quality is higher, not traded away: pure
resize quality (lossless PNG path) scores 97.6 SSIMULACRA2 vs
imgproxy's 81.9 on the 24-image Kodak corpus, and the AVIF default
produces smaller files than imgproxy's default at +6.7 SSIMULACRA2.

- [BENCH.md](BENCH.md) — full methodology and tables: official harness
  (local and AWS), sustained-load and memory measurements, presets.
- [bench/quality/QUALITY.md](bench/quality/QUALITY.md) — output quality
  (SSIMULACRA2) at matched settings vs imgproxy and sharp.

## Usage

```sh
cargo build --release
IMAGES_DIR=./images PORT=8081 QUALITY=80 ./target/release/oximg
curl "localhost:8081/resize/500/500/photo.jpg" -o out.jpg
```

AVIF support is an opt-in feature with two system dependencies
(SVT-AV1 >= 4.1 and dav1d, found via pkg-config; the Docker image
builds a pinned post-4.1 SVT-AV1 revision that carries the aarch64
kernels for the still-image path):

```sh
cargo build --release --features avif
```

Or with Docker:

```sh
docker build -t oximg .
docker run -p 8081:8081 -v $PWD/images:/images:ro oximg
```

URL signing (optional): set `OXIMG_KEY` and `OXIMG_SALT` (hex) to
require imgproxy-style signed URLs —
`/{base64url(HMAC-SHA256(key, salt || path))}/resize/{w}/{h}/{file}`.

Environment variables: `PORT` (8081), `IMAGES_DIR` (./images),
`OXIMG_SOURCE_BASE_URL` (fetch sources from `<base>/<file>` over HTTP
instead of the local filesystem; streaming decode overlaps the
download), `OXIMG_MAX_SOURCE_BYTES` (64MiB), `QUALITY`
(JPEG quality, 80), `OXIMG_WEBP_QUALITY` (75), `OXIMG_AVIF_QUALITY`
(55), `OXIMG_AVIF_ALPHA_QUALITY` (same as color), `PRESET` (`jpegli` default; `fast` = mozjpeg baseline profile,
`small` = mozjpeg trellis+progressive), `OXIMG_RESIZE=srgb` (resize in
sRGB space instead of linear light), `OXIMG_RESIZE_BACKEND=fir` (use
the portable fast_image_resize convolution instead of the platform
kernel), `OXIMG_AVIF_DECODE_THREADS` (dav1d workers; defaults to 2 on
x86-64 where SMT absorbs the second thread and 1 on SMT-less aarch64),
`OXIMG_DCT_MARGIN` (1.7), `OXIMG_PAR` (resize threads, 1),
`OXIMG_OVERLAP` (JPEG requests fuse decode with resize+encode on a
second thread, cutting single-request latency ~20%; `auto` fuses while
`2 x active requests <= visible CPUs` and falls back to one core per
request under contention. Default `auto` on aarch64, where output
bytes are identical either way; default off on x86-64 — set `1` to
force fusing every request there), `OXIMG_JPEG_PROGRESSIVE` (`0`
selects baseline jpegli: a few percent larger JPEG output — still at
or below libjpeg-turbo size for the same input, at higher quality —
in exchange for moving jpegli's entropy pass off the latency tail:
combined with `OXIMG_OVERLAP` this is the speed profile, ~-13%
single-request latency and ~+9% saturated throughput over the
default).

## Not yet implemented (out of PoC scope)

- Cross-format output and content negotiation (JXL / `Accept`-driven)
- Animated AVIF sources
- EXIF orientation / ICC profile handling
- Private S3 sources (public/presigned HTTP origins work), caching
- Production-grade load testing

## Status

Experimental PoC — APIs and the HTTP interface will change without
notice. The `oximg` crates.io / `@oximg` npm packages are name
reservations for now.

## License

[Apache-2.0](LICENSE).

The compiled binary statically links third-party code (jpegli/libjxl —
BSD-3-Clause, Highway — Apache-2.0, mozjpeg/libjpeg-turbo — IJG). Their
license texts and required notices are bundled in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md), generated with
`cargo about`. Dependency licensing is gated in CI by `cargo deny`
([deny.toml](deny.toml)).
