# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). oximg is an
experimental PoC: until 1.0.0, minor versions may change APIs and the
HTTP interface without notice.

## [Unreleased]

## [0.2.0] - 2026-07-04

### Added

- AVIF input and output behind the `avif` feature (the Docker images
  ship it): dav1d decode — 8/10/12-bit, all subsamplings, alpha,
  premultiplied sources — and SVT-AV1 still-image encode at 10-bit
  4:2:0 with `tune=ssim`, alpha carried as an auxiliary image. Knobs:
  `OXIMG_AVIF_QUALITY` (55), `OXIMG_AVIF_ALPHA_QUALITY`,
  `OXIMG_AVIF_DECODE_THREADS`.
- PNG and WebP support (same-format in/out) with alpha-correct
  premultiplied resampling; `OXIMG_PNG_EFFORT`, `OXIMG_WEBP_QUALITY`.
- jpegli as the default JPEG encoder (progressive); `PRESET=fast|small`
  select mozjpeg profiles.
- In-tree row-streaming SIMD resize kernels for the linear-light u16
  Lanczos3 path: NEON on aarch64 and AVX2+FMA on x86-64, verified
  against an f64 reference, with zero-padded tap blocks (no scalar
  tails) and four-row-batched horizontal convolution.
- Fused JPEG pipeline: decode overlaps resize+encode on a second
  thread (`OXIMG_OVERLAP`, default `auto` with a load gate); the
  serial path streams rows through the same kernel with the
  sRGB→linear transfer fused into staging, so a URL's bytes are
  identical on either side of the gate. Single-request latency drops
  ~20%.
- Speed profile: `OXIMG_JPEG_PROGRESSIVE=0` selects baseline jpegli —
  pixel-identical output at ~10% larger size, taking the entropy pass
  off the latency tail (~-13% latency, ~+9% saturated throughput).
- HTTP(S) remote sources (`OXIMG_SOURCE_BASE_URL`,
  `OXIMG_MAX_SOURCE_BYTES`) with decoding overlapping the download.
- imgproxy-style HMAC-SHA256 URL signing (`OXIMG_KEY`, `OXIMG_SALT`).
- Header probe API, plus unit, format-matrix, and HTTP end-to-end test
  coverage; CI smoke-tests the Docker image on amd64 and arm64.
- Multi-arch Docker publishing (linux/amd64 + linux/arm64) to GHCR and
  Docker Hub on every `main` push and on release tags.
- Benchmark and quality documentation: imgproxy's official harness on
  c7i.large/c7g.large ([BENCH.md](BENCH.md)) and SSIMULACRA2
  comparisons ([bench/quality/QUALITY.md](bench/quality/QUALITY.md)).

### Changed

- x86-64 serial JPEG resizing moved from pic-scale to the in-tree
  streamed AVX2 kernel (byte parity with the fused path); pic-scale
  remains for full-frame formats. U16 alpha resizing moved from
  fast_image_resize to the AVX2 kernel.
- dav1d worker default is architecture-aware: 2 on x86-64 (SMT), 1 on
  aarch64.
- System allocator by default; mimalloc is opt-in.
- The Docker image builds a pinned post-4.1 SVT-AV1 revision carrying
  the aarch64 quantization-matrix kernels.

### Fixed

- fast_image_resize's internal alpha multiply/divide pass is kept off
  (the pipeline premultiplies around the resample), fixing
  double-premultiplied colors on alpha images.
- Scalar YUV row conversion no longer hits a per-pixel bounds-check
  codegen pathology that inflated AVIF decode times on x86-64.

## [0.1.0] - 2026-07-02

### Added

- Initial release: HTTP resize server (`/resize/{w}/{h}/{file}`) for
  JPEG — mozjpeg streaming decode with DCT shrink-on-load,
  linear-light u16 Lanczos3 resize, request coalescing, CPU
  concurrency pinned to the core count — published to crates.io via
  Trusted Publishing.

[unreleased]: https://github.com/oximg/oximg/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/oximg/oximg/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/oximg/oximg/releases/tag/v0.1.0
