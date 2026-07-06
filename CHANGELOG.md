# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). oximg is an
experimental PoC: until 1.0.0, minor versions may change APIs and the
HTTP interface without notice.

## [Unreleased]

### Added

- Animated AVIF and WebP sources render their first frame instead of
  being rejected. AVIF: MIAF requires image sequences to carry a valid
  still primary item, so the bounded container walk assembles its
  `iloc` extents and hands them to dav1d directly (avif-parse still
  declines sequences); `probe` falls back to the still item's `ispe`;
  alpha tracks are not decoded. WebP: the demuxer's first frame is
  decoded when it covers the full canvas (virtually every real file —
  partial first frames would need compositing and keep the clean
  animation error). Orientation and ICC metadata apply to the first
  frame like any still source.

### Changed

- The oriented-source penalty shrank on both axes it could shrink
  without risking bytes. The post-resize rotation pass is now
  channel-monomorphized (flips reduce to reversed row copies, the
  transpose family runs cache-blocked) — ~3x faster on both
  architectures, pinned byte-for-byte against the original loop as an
  in-tree oracle. And oriented AVIF targets preheat their SVT session
  on the fused worker during the decode (creation failure downgrades
  to the serial encode, never a failed request). Measured on the Ryzen
  SMT pair at 512px: oriented JPEG→JPEG single-request penalty
  +1.35ms → +0.88ms; oriented JPEG→AVIF +1.25ms → +0.49ms. The
  remaining gap is the serial YUV conversion and the encode overlap,
  closed by measurement as not worth their complexity: streaming
  jpegli only works for the rare flip-h case, and streaming the YUV
  conversion under 90° rotation would pair chroma across resized
  columns.

- Profiled JPEG sources targeting jpegli (the default same-format
  path) ride the fused pipeline again: the fused worker writes the
  APP2 ICC chain ahead of its scanlines — byte-identical to the serial
  encoder, tested — instead of falling back to the one-shot encode.
  Measured on the Ryzen SMT pair: profiled-JPEG single-request p50
  8.8 → 7.6 ms (-13%). This closes the TODO(icc-fuse) gap: no
  metadata forces a fusing penalty anywhere except rotation itself.

## [0.4.0] - 2026-07-05

Theme: correctness metadata. Every source format's orientation is
honored and every color profile survives — rotated phone photos come
out upright and wide-gamut images keep rendering the way the original
did, in any output format.

### Added

- EXIF auto-rotation for JPEG sources (on by default;
  `OXIMG_AUTO_ROTATE=0` disables): the orientation tag steers the
  target box (it fits the *displayed* frame) and the pixels are
  rotated after the resize on the small output frame — Lanczos is
  separable, so resize-then-rotate is exactly rotate-then-resize at a
  fraction of the cost, and every streaming decode/resize path stays
  untouched. Oriented sources take the pixel-fuse path (rotation is
  incompatible with streaming rows into the incremental encoders);
  untagged, profile-less sources are byte-identical to 0.3.0 and pay
  no measurable cost. Applies before cross-format encoding, so a
  rotated phone photo converts upright into any target format. Tag semantics deliberately
  match Chrome and Firefox: the *first* Exif APP1 decides (an
  orientation-less one pins upright) and only strict `SHORT/count==1`
  entries rotate, so oximg output always agrees with how browsers
  render the original. The tag is read by a bounded in-tree scan of
  the leading JPEG segments (hard-capped at 256KB) rather than
  libjpeg marker saving, whose memory would scale with
  attacker-supplied APP1 counts; `qcli resize` honors the same
  rotation.

- Auto-rotation for the remaining source formats: PNG `eXIf`, WebP
  `EXIF` chunks (raw-TIFF or JPEG-style prefixed payloads — writers
  disagree, browsers accept both, so does oximg), and AVIF
  `irot`/`imir` transforms, composed in MIAF's mandated order
  (rotation, then mirror) into the same rotate-after-resize path JPEG
  uses. The AVIF mapping is pinned against libheif's rendering of
  avifenc-authored fixtures (avifdec itself does not apply the
  transforms, libheif — what ImageMagick and most viewers use — does).
  The WebP decode-scaler picks its decode size from the *displayed*
  fit, so axis-swapping orientations under non-square boxes cannot
  under-decode. `OXIMG_AUTO_ROTATE=0` covers all formats.

- ICC profile pass-through (on by default; `OXIMG_ICC=0` strips): the
  source's profile — JPEG APP2 `ICC_PROFILE` chain (reassembled with
  libjpeg's chunk rules by the same bounded header scan that reads the
  orientation), PNG `iCCP`, WebP `ICCP` — is carried byte-for-byte
  into any profile-capable output, across format conversion included.
  Pixels are never color-converted.
  Profiled JPEG sources take the one-shot encode path (the profile
  must precede the incremental encoder's scanlines). The header scan
  now spans every pre-frame segment — the same span libjpeg's marker
  saving covers — so an Exif tag placed after the tables (which
  browsers honor) now rotates too, where 0.3.x-era scanning would
  have missed it.

- AVIF ICC in both directions — neither avif-parse nor avif-serialize
  exposes ICC in any released version, so both run on a bounded
  ISOBMFF walk of our own: extraction resolves the primary item's
  `ipma` associations to its `colr` (`prof`/`ricc`) property, and
  embedding splices a `colr` (`prof`) property into the serialized
  container (sizes and `iloc` offsets re-patched; the property surgery
  is proven by re-extraction before shipping, and anything the patcher
  does not fully recognize is refused whole — the unprofiled bytes are
  served instead). The nclx `colr` stays alongside: matrix
  coefficients still describe the YUV→RGB step, the profile governs
  the resulting RGB. Extraction is pinned against a committed
  avifenc-authored fixture; the splice was verified readable by
  libavif 1.0.4's avifdec and is pinned by decode roundtrips in the
  suite. Profiles ride the fused YUV path (byte-identical to the
  serial path, tested) — no fusing penalty for profiled AVIF targets.

### Changed

- The mozjpeg presets (`PRESET=fast|small`) now fuse the JPEG decode
  with the SIMD resize on a second thread under the same
  `OXIMG_OVERLAP` gate as the other paths, running the one-shot
  mozjpeg encode after — bytes identical to the serial path (and
  gate-independent, tested). Single-request latency on the Ryzen SMT
  pair: `fast` -10% (8.0 → 7.2 ms), `small` -2.6%; saturated
  throughput unchanged.

## [0.3.0] - 2026-07-05

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
- `OXIMG_AVIF_SPEED` (SVT preset, default 8): the AVIF throughput
  knob. Setting 9 trades ~-0.6 SSIMULACRA2 at unchanged bytes for ~28%
  less encode CPU — measured +19% JPEG→AVIF req/s on a real c7i.large,
  +16% over the same-run imgproxy anchor (BENCH.md, QUALITY.md).
- `pipeline::Params` gains `output: Option<ImageFormat>` (`None` keeps
  the sniff-and-match behavior, byte-identical to 0.2.0), and
  `ImageFormat` gains `from_token`; `qcli` gains a `transcode` mode.
  **Breaking** for library users writing exhaustive `Params { ... }`
  literals — hence 0.3.0.
- New reproducible benchmarks: `bench/coldstart.sh` (start → ready →
  first real response, as distributions; oximg native is ready in 6ms
  and serves its first image at 13ms) and `bench/stress.sh` (k6
  connection-capacity ramp with coalescing-proof URL assignment; zero
  failures through 8192 concurrent connections at ~25-30KB each).
  `OXIMG_TIMING` now also reports the SVT init/encode split.

### Changed

- Request coalescing keys on the resolved output format, so mixed
  same-URL traffic (e.g. `photo.jpg` and `photo.jpg@webp`) can never
  share a response. Same-format requests keep the fused JPEG path and
  its byte-identical output.
- Cross-format JPEG requests fuse the decode with the SIMD resize on a
  second thread (same `OXIMG_OVERLAP` gate, byte-identical to the
  serial path). AVIF targets go further: the fused worker converts
  each resized row straight into the 10-bit planes (the resized frame
  never exists as an interleaved RGB copy) and creates the SVT session
  during the decode overlap — ~-10-12% single-request latency on
  JPEG→WebP and JPEG→AVIF, plus +3.5-4% saturated JPEG→AVIF
  throughput from the fused conversion.
- The encode-side RGB→YUV conversion gained bit-identical SIMD rows on
  both architectures (NEON on aarch64, AVX2 on x86-64): the exact
  integer division is replaced by an exhaustively-proven magic
  multiply, and chroma mirrors the scalar f32 arithmetic operation for
  operation, asserted bit-exact in tests.
- Fully opaque RGBA no longer pays for an AVIF alpha item: an
  early-exit scan drops it to the 3-channel output (byte-identical to
  encoding the same pixels as RGB), skipping the second SVT-AV1
  session entirely. Images with any transparency are unaffected.
- BENCH.md was re-measured wholesale on fresh c7i.large/c7g.large
  instances (all four servers per instance, cross-format cells with
  full p95 capture) and gained cells for the newest generations
  (c8i.large "Granite Rapids" and c9g.large), where oximg leads all 40
  cells; plus cold-start and connection-capacity sections. Headline
  movement: Graviton3 JPEG 81.3 → 91.2 req/s (scratch-pool fix);
  JPEG→AVIF reaches imgproxy parity on c7i at the default operating
  point and +2% on c8i, while producing smaller, higher-scoring files.

### Fixed

- Truncated or malformed AVIF containers now return a parse error
  instead of crashing the request: avif-parse 2.1.0 can panic on such
  input (internal parser-state assertion), so the container parse is
  unwind-caught at the library boundary — quietly (no crash-shaped
  stderr trace per malformed input), with the upstream assertion text
  preserved in the error message.
- `OXIMG_RESIZE_BACKEND=fir` now also disables the fused decode overlap
  (both the jpegli and the cross-format variants): the fused workers
  run the in-tree SIMD kernel, so fusing under the fir escape hatch
  made a URL's bytes depend on the instantaneous load gate.
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

[unreleased]: https://github.com/oximg/oximg/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/oximg/oximg/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/oximg/oximg/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/oximg/oximg/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/oximg/oximg/releases/tag/v0.1.0
