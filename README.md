# oximg

High-performance image compression in Rust: a library, a CLI, and a
self-hostable HTTP server. Currently a JPEG resize/compression PoC.

## Pipeline

```
JPEG bytes
  → mozjpeg decode with DCT shrink-on-load (kept ≥ 1.7x target size)
  → linear-light resize: sRGB u8 → linear u16 → Lanczos3 (SIMD) → sRGB u8
  → jpegli encode (progressive; PRESET=fast / PRESET=small select mozjpeg profiles)
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

Environment variables: `PORT` (8081), `IMAGES_DIR` (./images), `QUALITY`
(80), `PRESET` (`jpegli` default; `fast` = mozjpeg baseline profile,
`small` = mozjpeg trellis+progressive), `OXIMG_RESIZE=srgb` (resize in
sRGB space instead of linear light), `OXIMG_DCT_MARGIN` (1.7),
`OXIMG_PAR` (resize threads, 1).

## Not yet implemented (out of PoC scope)

- WebP / AVIF / JXL output and content negotiation
- EXIF orientation / ICC profile handling
- Remote sources (S3 / HTTP), URL signing, caching
- Production-grade load testing

## Status

Experimental PoC — APIs and the HTTP interface will change without
notice. The `oximg` crates.io / `@oximg` npm packages are name
reservations for now.

## License

[Apache-2.0](LICENSE)
