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

## Features

- **HTTP resize service**: `GET /resize/{w}/{h}/{file}` fits the source
  within `w x h` (never enlarges) and re-encodes it in its own format.
  Sources come from a local directory or any HTTP(S) origin
  (`OXIMG_SOURCE_BASE_URL`), where decoding overlaps the download.
  Optional imgproxy-style HMAC URL signing.
- **Quality-first processing**: resizing happens in linear light on
  16-bit samples with Lanczos3, JPEG sources are decoded supersampled
  (DCT shrink-on-load kept ≥ 1.7x the target), and alpha is
  premultiplied across the resample — the properties behind the
  SSIMULACRA2 scores in [Benchmarks](#benchmarks).
- **Performance as architecture, not flags**: per-arch row-streaming
  SIMD resize kernels (AVX2 on x86-64, NEON on aarch64, both verified
  against an f64 reference), JPEG decode fused with resize+encode on a
  second thread under low load, request coalescing for concurrent
  identical URLs, and CPU concurrency pinned to the core count. Peak
  memory stays at a fraction of imgproxy's under identical load
  ([BENCH.md](BENCH.md)).
- **Tunable profiles**: the default maximizes quality per byte
  (progressive jpegli); one env flip (`OXIMG_JPEG_PROGRESSIVE=0`)
  trades ~10% output size for the lowest latency at unchanged pixels.
  `PRESET=fast|small` selects mozjpeg profiles instead.
- **Self-contained deploys**: multi-arch Docker images
  (linux/amd64 + linux/arm64) on Docker Hub (`oximg/oximg`) and GHCR
  (`ghcr.io/oximg/oximg`); a single static-leaning binary otherwise.

### Supported formats

Sources are identified by magic bytes (extensions are never trusted).
By default the output format is the source's own; any decode column
combines with any encode column:

| Format | Decode | Encode |
|---|---|---|
| JPEG | baseline & progressive, grayscale; streaming, DCT shrink-on-load | jpegli progressive (default), mozjpeg profiles via `PRESET` |
| PNG | palette / grayscale / 16-bit, normalized to RGB(A)8 | lossless RGB(A) |
| WebP | lossy & lossless, alpha | lossy (`OXIMG_WEBP_QUALITY`, 75), alpha |
| AVIF (`--features avif`) | dav1d: 8/10/12-bit, all subsamplings, alpha | SVT-AV1: 10-bit 4:2:0, tune=ssim, alpha as auxiliary image |

**Cross-format output**: append an imgproxy-style `@{fmt}` token to the
filename — `/resize/300/200/photo.jpg@webp` (`jpg`/`jpeg`, `png`,
`webp`, `avif`; `jxl` is reserved). Only exact tokens count, so
`photo@2x.jpg` is still a filename. Precedence: explicit `@{fmt}` >
`Accept` negotiation > source format. Negotiation is opt-in: set
`OXIMG_AUTO_FORMAT` to a preference list (e.g. `avif,webp`) and
bare-URL responses follow the request's `Accept` header; every response
then carries `Vary: Accept` (make sure your CDN honors it or normalizes
`Accept` into the cache key — explicit `@{fmt}` URLs avoid the issue
entirely, which is what signed deployments should prefer since headers
are outside the signature). Alpha sources encoded to JPEG are flattened
in linear light onto `OXIMG_FLATTEN_BG` (hex `RRGGBB`, default white).
Encode settings are keyed by the *output* format, using the same knobs
as same-format requests.

**Orientation**: every source format auto-rotates — JPEG EXIF, PNG
`eXIf`, WebP `EXIF` chunks, and AVIF `irot`/`imir` transforms. The
target box applies to the displayed frame and the pixels come out
upright in every output format (the metadata itself is not forwarded,
so nothing double-rotates). `OXIMG_AUTO_ROTATE=0` restores the raw
stored orientation.

**ICC profiles**: a source's color profile (JPEG APP2 chain, PNG
`iCCP`, WebP `ICCP`, AVIF `colr`) passes through byte-for-byte into
any output format, across format conversion included. RGB pixels are
never color-converted. This matters for wide-gamut sources: the
common proxy default is to normalize pixels to sRGB and strip the
profile, which permanently clips every color outside the sRGB gamut —
a Display P3 phone photo loses exactly the saturated reds and greens
that made it worth shooting in P3. oximg keeps the pixels and the
profile as they were, so wide-gamut images render on a wide-gamut
display the way the original did (and identically everywhere else).
`OXIMG_ICC=0` opts into stripping instead.

**CMYK/YCCK JPEG sources** (print-workflow assets) are the one
exception, since no browser renders CMYK pixels: they are converted
to sRGB — through the embedded CMYK profile (moxcms, relative
colorimetric, like imgproxy/libvips) when one is present, with the
naive composite browsers use otherwise — and the CMYK profile is
consumed, never passed through. `OXIMG_ICC=0` skips profile
extraction entirely, so it also selects the naive conversion.

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
       JPEG rows stream through in-tree ring-scheduled f32 row kernels —
       AVX2 on x86-64, NEON on aarch64, both verified against an f64
       reference — optionally fused with the decode on a second thread;
       other formats resize full-frame: pic-scale on x86-64, the same
       in-tree kernel on aarch64)
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
| oximg | **78.7** (33 ms) | **32.8** (79 ms) | **30.9** (92 ms) | **15.6** (181 ms) |
| best of imgproxy/imagor/thumbor | 67.0 | 15.5 | 20.3 | 15.2 |

| c7g.large (Graviton3) | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg | **91.2** (28 ms) | **39.0** (66 ms) | **41.5** (70 ms) | **23.4** (124 ms) |
| best of imgproxy/imagor/thumbor | 68.0 | 22.1 | 25.4 | 20.3 |

Cross-format cells (our harness extension; JPEG sources, oximg vs
imgproxy):

| JPEG→ | c7i oximg | c7i imgproxy | c7g oximg | c7g imgproxy |
|---|---|---|---|---|
| WebP | **65.3** (41 ms) | 35.3 | **79.3** (33 ms) | 37.0 |
| AVIF | 44.6 (57 ms) | 44.9 | **56.5** (46 ms) | 52.7 |

At the same time, output quality is higher, not traded away:
end-to-end JPEG at the same q80 scores +6 to +18 SSIMULACRA2 over
imgproxy (77.5 vs 71.2 on the Kodak corpus, the gap widening with
source size — and imgproxy at q90 with twice the bytes still scores
lower), pure resize quality (lossless PNG path) scores 97.6 vs 81.9,
and the AVIF default produces smaller files than imgproxy's default
at +6.7 SSIMULACRA2.

- [BENCH.md](BENCH.md) — full methodology and tables: official harness
  (local and AWS), sustained-load and memory measurements, presets.
- [bench/quality/QUALITY.md](bench/quality/QUALITY.md) — output quality
  (SSIMULACRA2) at matched settings vs imgproxy and sharp.

## Usage

**Docker** (recommended — multi-arch linux/amd64 + linux/arm64, AVIF
included; both registries rebuild on every `main` push):

```sh
docker run -p 8081:8081 -v $PWD/images:/images:ro ghcr.io/oximg/oximg:latest
# or: docker.io/oximg/oximg:latest
curl "localhost:8081/resize/500/500/photo.jpg" -o out.jpg
```

**Homebrew** (builds v0.5.0 from source; JPEG/PNG/WebP, no AVIF):

```sh
brew install oximg/tap/oximg
```

**Cargo** (crates.io, v0.5.0; add `--features avif` if SVT-AV1 >= 4.1
and dav1d are installed and visible to pkg-config):

```sh
cargo install oximg
```

Note the release channels lag `main`: crates.io and the brew formula
ship the last tagged release, while the Docker images rebuild on every
`main` push. The npm package
[`@oximg/oximg`](https://www.npmjs.com/package/@oximg/oximg) is a name
reservation that points here.

**From source**:

```sh
cargo build --release            # JPEG, PNG, WebP
cargo build --release --features avif   # + AVIF (needs SVT-AV1 >= 4.1, dav1d)
IMAGES_DIR=./images PORT=8081 QUALITY=80 ./target/release/oximg
./target/release/oximg --version   # all config is via env; the only flags
```

The Docker build needs no system dependencies — it compiles a pinned
post-4.1 SVT-AV1 revision that carries the aarch64 kernels for the
still-image path:

```sh
docker build -t oximg .
```

**As a library**: the `oximg::pipeline` module is usable without the
HTTP server — `process`/`process_path` take a `Params` and return the
re-encoded bytes plus their format, `probe` reads just the header.
Depend on it with `default-features = false` to drop the entire HTTP
stack (axum, tokio, ureq, hmac, sha2); add `features = ["avif"]` for
AVIF. `process_url` (remote HTTP sources) needs the `server` feature.
See [`examples/`](examples/):

```sh
cargo run --release --example thumbnail -- photo.jpg 300 200 out.jpg
cargo run --release --example transcode -- photo.jpg 800 800 webp out.webp
cargo run --release --example probe     -- photo.webp
```

URL signing (optional): set `OXIMG_KEY` and `OXIMG_SALT` (hex) to
require imgproxy-style signed URLs —
`/{base64url(HMAC-SHA256(key, salt || path))}/resize/{w}/{h}/{file}`.

Environment variables: `PORT` (8081), `IMAGES_DIR` (./images),
`OXIMG_SOURCE_BASE_URL` (fetch sources from `<base>/<file>` over HTTP
instead of the local filesystem; streaming decode overlaps the
download), `OXIMG_MAX_SOURCE_BYTES` (64MiB; over-limit remote sources answer
413), `OXIMG_MAX_SRC_PIXELS` (64,000,000; decoded-size cap enforced
after each format's header parse, before any pixel allocation —
compressed size does not bound decoded size), `QUALITY`
(JPEG quality, 80), `OXIMG_WEBP_QUALITY` (75), `OXIMG_AVIF_QUALITY`
(55), `OXIMG_AVIF_ALPHA_QUALITY` (same as color), `OXIMG_AVIF_SPEED`
(SVT preset, 8; setting 9 trades ~-0.6 SSIMULACRA2 at unchanged bytes
for ~28% less encode CPU — measured +19% JPEG→AVIF req/s on a real
c7i.large, ahead of imgproxy by +16%; see [BENCH.md](BENCH.md) and
[bench/quality/QUALITY.md](bench/quality/QUALITY.md)), `PRESET` (`jpegli` default; `fast` = mozjpeg baseline profile,
`small` = mozjpeg trellis+progressive), `OXIMG_AUTO_FORMAT` (unset;
comma-separated `Accept`-negotiation preference list, e.g. `avif,webp`),
`OXIMG_FLATTEN_BG` (`ffffff`; background for alpha → JPEG flattening),
`OXIMG_AUTO_ROTATE` (`1`; `0` serves the stored orientation),
`OXIMG_ICC` (`1`; `0` strips source ICC profiles from outputs and
converts CMYK sources naively instead of through their profile; the
shared JPEG header scan is skipped only when both knobs are off),
`OXIMG_RESIZE=srgb` (resize in
sRGB space instead of linear light), `OXIMG_RESIZE_BACKEND=fir` (use
the portable fast_image_resize convolution instead of the platform
kernel), `OXIMG_AVIF_DECODE_THREADS` (dav1d workers; defaults to 2 on
x86-64 where SMT absorbs the second thread and 1 on SMT-less aarch64),
`OXIMG_DCT_MARGIN` (1.7), `OXIMG_PAR` (resize threads, 1),
`OXIMG_PNG_EFFORT` (`fast`; `fastest`/`balanced`/`high` trade PNG size
against encode time), `OXIMG_WEBP_EFFORT` (libwebp `method`, 2), `OXIMG_WEBP_DECODE_THREADS` (`1`; `0` disables
libwebp's two-thread decode pipelining), `OXIMG_TIMING` (set to print
per-stage timing lines to stderr), `OXIMG_LOG` (`error`: one stderr
line per failed request, always on; `request` also logs successes,
with a request id and wall time),
`OXIMG_OVERLAP` (JPEG requests fuse decode with resize+encode on a
second thread, cutting single-request latency ~20%; the default `auto`
fuses while `2 x active requests <= visible CPUs` and falls back to
one core per request under contention. Serial and fused stream through
the same SIMD kernel, so a URL's bytes are identical either way; `1`
forces fusing, `0` disables it), `OXIMG_JPEG_PROGRESSIVE` (`0`
selects baseline jpegli: a few percent larger JPEG output — still at
or below libjpeg-turbo size for the same input, at higher quality —
in exchange for moving jpegli's entropy pass off the latency tail:
combined with `OXIMG_OVERLAP` this is the speed profile, ~-13%
single-request latency and ~+9% saturated throughput over the
default).

## Not yet implemented (out of PoC scope)

- JXL output (the `@jxl` token is reserved and returns a clear error)
- Animated output (animated AVIF and WebP *sources* render their
  first frame, like other image proxies)
- Private S3 sources (public/presigned HTTP origins work), caching
- Production-grade load testing

## Roadmap

Rough order, subject to change (experimental PoC):

- **JXL output** once a maintained encoder binding stabilizes (the
  `@jxl` token is already reserved).
- **Response caching** (keyed on the resolved URL + format) and
  private-origin support (presigned S3 already works via HTTP).
- **0.5.0 library-API cleanup**: `Params` gains `Default` +
  `#[non_exhaustive]`, the server-only dependencies move behind a
  feature so library users do not compile the HTTP stack, and the
  raw codec bindings stop being part of the public surface.

## Status

Experimental PoC — APIs and the HTTP interface will change without
notice. The `@oximg` npm package is a name reservation.

## License

[Apache-2.0](LICENSE).

The compiled binary statically links third-party code (jpegli/libjxl —
BSD-3-Clause, Highway — Apache-2.0, mozjpeg/libjpeg-turbo — IJG). Their
license texts and required notices are bundled in
[THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md), generated with
`cargo about`. Dependency licensing is gated in CI by `cargo deny`
([deny.toml](deny.toml)).
