# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). oximg is an
experimental PoC: until 1.0.0, minor versions may change APIs and the
HTTP interface without notice.

## [Unreleased]

### Added

- Cross-format output: an imgproxy-style `@{fmt}` token on the filename
  (`/resize/300/200/photo.jpg@webp`; `jpg`/`jpeg`, `png`, `webp`,
  `avif` — `jxl` reserved) re-encodes any supported source in the
  requested format. Unknown suffixes stay part of the filename, and
  signed URLs cover the token, so a signature for one format does not
  authorize a heavier one. Alpha sources targeting JPEG are flattened
  in linear light onto `OXIMG_FLATTEN_BG` (hex, default white).
- Opt-in `Accept` negotiation for bare URLs: `OXIMG_AUTO_FORMAT`
  (comma-separated preference list, e.g. `avif,webp`); when enabled,
  every `/resize` response carries a config-static `Vary: Accept`.
  Precedence: explicit `@{fmt}` > negotiated > source format.
- `pipeline::Params` gains `output: Option<ImageFormat>` (`None` keeps
  the sniff-and-match behavior, byte-identical to 0.2.0), and
  `ImageFormat` gains `from_token`. `qcli` gains a `transcode` mode.
  **Breaking** for library users writing exhaustive `Params { ... }`
  literals — the next release must be 0.3.0, not 0.2.x.

### Changed

- Request coalescing keys on the resolved output format, so mixed
  same-URL traffic (e.g. `photo.jpg` and `photo.jpg@webp`) can never
  share a response. Same-format requests keep the fused JPEG path and
  its byte-identical output; cross-format JPEG requests stream through
  the same SIMD resize kernel without the second-thread overlap.
- Fully opaque RGBA no longer pays for an AVIF alpha item: an
  early-exit scan drops it to the 3-channel output (byte-identical to
  encoding the same pixels as RGB), skipping the second SVT-AV1
  session entirely. Images with any transparency are unaffected.
- Cross-format JPEG requests now fuse too: decode overlaps the SIMD
  resize into the pixel buffer on a second thread (same `OXIMG_OVERLAP`
  gate and byte-identical output vs the serial path), with the one-shot
  WebP/AVIF/PNG encode after — ~-10-12% single-request latency on
  JPEG→WebP/AVIF locally. The same-format jpegli fused path is
  untouched.
- The encode-side RGB→YUV conversion (AVIF output) gained NEON row
  kernels on aarch64, bit-identical to the scalar reference (the
  division is replaced by an exhaustively-proven exact magic-multiply;
  chroma mirrors the f32 arithmetic operation for operation). Not
  measurable end-to-end at thumbnail sizes; scales with output
  resolution.
- AVIF targets now fuse the RGB→YUV conversion too: the fused worker
  converts each resized row straight into the 10-bit planes (the
  resized frame never exists as an interleaved RGB copy), and the
  conversion gained AVX2 rows on x86-64 under the same bit-exact
  contract (0.44→0.08 ms per 512x340 frame — note the scalar luma was
  already LLVM-auto-vectorized; the hand kernels only win with
  `target_feature` on their inlined helpers, which an interleaved A/B
  caught as a 12x-slower footgun first). Interleaved official-harness
  A/B on the Ryzen: JPEG→AVIF +3.5-4% req/s on both the two-core and
  the SMT-pair (c7i-like) topologies, output bytes unchanged.

### Changed (benchmarks)

- BENCH.md's AWS c7i.large/c7g.large tables were refreshed wholesale
  (2026-07-05, fresh instances, all four servers re-measured in one
  run, cross-format cells included with full p95 capture). Headline
  movement: Graviton3 JPEG 81.3 → 91.2 req/s from the scratch-pool
  fix; JPEG→AVIF on c7i reaches parity with imgproxy after the
  fused-YUV work while producing smaller, higher-scoring files.

### Fixed

- Truncated or malformed AVIF containers now return a parse error
  instead of crashing the request: avif-parse 2.1.0 can panic on such
  input (internal parser-state assertion), so the container parse is
  unwind-caught at the library boundary — quietly (no crash-shaped
  stderr trace per malformed input), with the upstream assertion text
  preserved in the error message.
- `OXIMG_RESIZE_BACKEND=fir` now also disables the fused decode overlap
  (both the jpegli and the new cross-format variants): the fused
  workers run the in-tree SIMD kernel, so fusing under the fir escape
  hatch made a URL's bytes depend on the instantaneous load gate.
- Fused workers return their kernel scratch to the request thread's
  pool instead of leaking it into the ephemeral worker's TLS, and a
  failed worker-thread spawn (thread limits) now falls back to the
  byte-identical serial path instead of failing the request.

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
