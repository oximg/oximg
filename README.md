# oximg

[![Crates.io](https://img.shields.io/crates/v/oximg.svg)](https://crates.io/crates/oximg)
[![Docs.rs](https://docs.rs/oximg/badge.svg)](https://docs.rs/oximg)
[![CI](https://github.com/oximg/oximg/actions/workflows/ci.yml/badge.svg)](https://github.com/oximg/oximg/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

High-performance image compression in Rust: a library, a CLI, and a
self-hostable HTTP server (PoC). JPEG, PNG, and WebP in and out; sources
are format-sniffed by magic bytes and re-encoded in their own format.

## Pipeline

```
source bytes (local file or HTTP origin)
  → format sniff → decode
      JPEG: mozjpeg streaming decode, DCT shrink-on-load (kept ≥ 1.7x target size)
      PNG:  png crate (palette/gray/16-bit normalized to RGB(A)8)
      WebP: libwebp
  → linear-light resize: sRGB u8 → linear u16 → Lanczos3 (SIMD) → sRGB u8
      (alpha is premultiplied before resampling, unpremultiplied after)
  → encode in the source format
      JPEG: jpegli, progressive (PRESET=fast / PRESET=small select mozjpeg profiles)
      PNG:  png crate | WebP: libwebp
```

Concurrent identical requests are coalesced and share one result.
CPU concurrency is pinned to the core count with a semaphore; the HTTP
layer (axum/tokio) only does queueing and IO.

## Benchmarks

- [BENCH.md](BENCH.md) — throughput, latency, and memory vs imgproxy and
  imagor, on macOS and Linux x86_64.
- [bench/quality/QUALITY.md](bench/quality/QUALITY.md) — output quality
  (SSIMULACRA2) at matched settings vs imgproxy and sharp.

## Usage

```sh
cargo build --release
IMAGES_DIR=./images PORT=8081 QUALITY=80 ./target/release/oximg
curl "localhost:8081/resize/500/500/photo.jpg" -o out.jpg
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
(80), `PRESET` (`jpegli` default; `fast` = mozjpeg baseline profile,
`small` = mozjpeg trellis+progressive), `OXIMG_RESIZE=srgb` (resize in
sRGB space instead of linear light), `OXIMG_DCT_MARGIN` (1.7),
`OXIMG_PAR` (resize threads, 1).

## Not yet implemented (out of PoC scope)

- Cross-format output and content negotiation (AVIF / JXL / `Accept`-driven)
- EXIF orientation / ICC profile handling
- S3 sources, URL signing, caching
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
