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
  `0` leaves an axis unconstrained (`/resize/750/0/…` is width-only —
  what `srcset` `w` descriptors and Next.js loaders emit), and
  `{file}` may span directories, so S3-style prefixes and nested trees
  are addressable as-is. Optional imgproxy-style HMAC URL signing.
- **Cloudflare Images URL compatibility**: mount a second route
  (`OXIMG_OPTIONS_PREFIX`) speaking the option-list grammar —
  `/image/width=750,quality=80/path/to/photo.png` — so URLs built for
  Cloudflare Images survive a migration without a rewrite layer,
  per-request quality included.
- **Sources**: a local directory, any HTTP(S) origin, or a **private
  GCS bucket** (`gs://` with GCP-attached credentials — no public
  bucket, no public-endpoint egress). Streaming decode overlaps the
  download; transient fetch failures are retried, so a network blip is
  a slower response, not a broken image.
- **Production operability**: graceful SIGTERM drain, upstream fetch
  deadlines (slow-origin 504s distinct from broken-origin 502s), and
  an opt-in Prometheus `/metrics` page whose queue-wait/processing
  split tells "needs more CPU" apart from "sources got bigger".
- **Quality-first processing**: resizing happens in linear light on
  16-bit samples with Lanczos3, JPEG sources are decoded supersampled
  (DCT shrink-on-load kept ≥ 1.7x the target), and alpha is
  premultiplied across the resample — the properties behind the
  SSIMULACRA2 scores in [Benchmarks](#benchmarks).
- **Performance as architecture, not flags**: per-arch row-streaming
  SIMD resize kernels (AVX2 on x86-64, NEON on aarch64, both verified
  against an f64 reference), JPEG decode fused with resize+encode on a
  second thread under low load, request coalescing for concurrent
  identical URLs (per-process — a horizontally scaled deployment gets
  its dedup from the CDN in front, not from here), and CPU concurrency
  pinned to the core count. Peak
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
| PNG | palette / grayscale / 16-bit, normalized to RGB(A)8 | lossless RGB(A); opt-in palette quantization (`OXIMG_PNG_QUANTIZE`) |
| WebP | lossy & lossless, alpha | lossy (`OXIMG_WEBP_QUALITY`, 75), alpha; output is scaled to fit WebP's 16383 px limit |
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

Choose the preference order by your goal: the AVIF defaults target
fidelity, not minimum bytes — at default quality settings AVIF output
measures 10–28% *larger* than WebP on photographic sources, and costs
the more expensive encode. If the deployment's goal is byte reduction,
prefer `webp,avif` (or `webp` alone), or lower `OXIMG_AVIF_QUALITY`
until AVIF earns its slot; put `avif` first only after comparing sizes
on your own corpus at your own settings. Also note what negotiation
does *not* cover: when it doesn't fire (client sends `Accept: */*` —
link-preview scrapers, social-card fetchers, curl integrations), the
source format is kept, and PNG output defaults to lossless RGB(A) — a
large photographic PNG stays large unless `OXIMG_PNG_QUANTIZE=1` is
set. Deployments that care about those clients should enable
quantization or prefer explicit `@{fmt}` URLs over relying on
negotiation. On flat graphics (charts, screenshots, text-heavy
panels), a quantized PNG is often both smaller *and* truer to the
source than any WebP quality setting — worth remembering when tuning
`OXIMG_AUTO_FORMAT` for mixed content.

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

## Install

**Docker** (recommended — multi-arch linux/amd64 + linux/arm64, AVIF
included; both registries rebuild on every `main` push, so pin a
version tag in production):

```sh
docker run -p 8081:8081 -v $PWD/images:/images:ro ghcr.io/oximg/oximg:latest
# or: docker.io/oximg/oximg:latest
curl "localhost:8081/resize/500/500/photo.jpg" -o out.jpg
```

**Prebuilt binaries** ([GitHub Releases](https://github.com/oximg/oximg/releases),
v0.6.0+; Linux x86_64/aarch64 and macOS arm64; JPEG/PNG/WebP, no
AVIF) — suited to CI asset pipelines where a Docker pull or a source
build is too slow. Assets are `oximg-<tag>-<target>.tar.gz` with a
`.sha256` alongside; each is smoke-tested before upload. Linux builds
link glibc >= 2.39 with libstdc++ static.

**Homebrew** (builds the latest release from source; JPEG/PNG/WebP):

```sh
brew install oximg/tap/oximg
```

**Cargo** (crates.io; add `--features avif` if SVT-AV1 >= 4.1 and
dav1d are installed and visible to pkg-config):

```sh
cargo install oximg
```

**From source** (the Docker build needs no system dependencies — it
compiles a pinned SVT-AV1 itself):

```sh
cargo build --release                    # JPEG, PNG, WebP
cargo build --release --features avif    # + AVIF (needs SVT-AV1 >= 4.1, dav1d)
IMAGES_DIR=./images PORT=8081 ./target/release/oximg   # = oximg serve
```

Release channels lag `main`: crates.io and the brew formula ship the
last tagged release, while the Docker images rebuild on every `main`
push. The npm package
[`@oximg/oximg`](https://www.npmjs.com/package/@oximg/oximg) is a name
reservation that points here.

## Serving

**URL grammars.** The positional route is
`/resize/{w}/{h}/{file}[@fmt]`; `0` leaves an axis unconstrained, and
`{file}` may span directories. Setting `OXIMG_OPTIONS_PREFIX` mounts a
second route speaking the Cloudflare Images option grammar at that
prefix:

```
/image/width=750,quality=80/albums/2026/photo.png
```

with `width`/`height` (1-8192; one suffices, the other axis follows
the aspect ratio), `quality` (1-100, applied to whichever format the
output resolves to; PNG output is lossless and ignores it), and
`format` (`jpeg|png|webp|avif`, or `auto` = the same Accept
negotiation as a bare positional URL, which also runs when `format`
is absent). Unknown or duplicate options answer 400 naming the key —
Cloudflare silently ignores unknown options, but a silently dropped
`fit=cover` changes the output, so the divergence is deliberate. The
filename is taken literally on this route (no `@fmt` token).

**Sources.** With `OXIMG_SOURCE_BASE_URL` unset, sources come from
`IMAGES_DIR`. Set it and the scheme selects the transport:

- `https://host/prefix` — anonymous HTTP. **Exposure prerequisite**:
  no credentials are sent, so the origin must be anonymously readable;
  for an object-store bucket that means public objects, and anyone who
  can guess a path can fetch the original at full resolution,
  bypassing every resize/signing/CDN control in front.
- `gs://bucket[/prefix]` — a **private GCS bucket**, read directly
  with GCP-attached credentials (GKE Workload Identity, Cloud Run, and
  GCE metadata credentials; tokens cached and refreshed; boot fails
  closed with a clear message when no credentials are reachable).
  `service_account` JSON keys are not supported — on GCP use Workload
  Identity, off GCP use the HTTP mode. `s3://` is planned (issue #11).

Streaming decode overlaps the download in every mode.
Connection-level transients (reset, refused, DNS blips) are retried
once before any body bytes are consumed, and the `gs://` mode also
retries 429/5xx SDK-style; `oximg_upstream_retries_total` counts both.

**Format ceilings are part of the fit**: WebP cannot express a side
past 16383 px, so a request whose output would exceed that is scaled
down until it fits, aspect ratio preserved — a 2000x19708 source asked
for `width=1920` as WebP comes back 1663x16383. Tall single-column
images (infographics, long product pages) hit this routinely, and the
alternative is failing a request the format simply cannot serve at the
asked-for size. The returned image reports its own dimensions; other
output formats have no ceiling worth enforcing here (their limits sit
past `OXIMG_MAX_SRC_PIXELS`).

**Error classes follow fault, not convenience**: a source key that no
store can serve — past an object store's key-length limit, or refused
by the origin as a malformed request (400/414) — answers **400**, and
an absent object **404**. Only a genuinely unwell upstream (connect
failure, reset, 5xx) answers **502**, with slow origins split off as
**504**. This matters downstream: CDNs retry and fail over on 5xx but
pass 4xx through to their error cache, so misfiling a client error as
an upstream failure both inflates the 5xx rate an operator watches and
turns a crawler into origin load. `oximg_upstream_fetch_total` splits
the same way (`rejected` and `not_found` apart from `error`), so that
series stays a signal of upstream health. Over-length keys are refused
locally, without a round trip.

**Source paths** are validated component-wise — `.`/`..` components,
empty components, `\`, `?`, `#`, and control bytes answer 400. Local
sources also pass a symlink-containment check (a path resolving
outside `IMAGES_DIR` answers 404), and remote paths are re-encoded
segment-wise so a percent in a name is never double-decoded upstream.

**URL signing** (optional): set `OXIMG_KEY` and `OXIMG_SALT` (hex) to
require imgproxy-style signed URLs —
`/{base64url(HMAC-SHA256(key, salt || path))}/resize/{w}/{h}/{file}`,
and the same scheme over `{prefix}/{options}/{file}` on the options
route. The signed `path` is the percent-decoded form, so one signature
covers every URL encoding of the same source.

**CORS preflight**: `OPTIONS` on an image route answers **204** with
`Allow: GET, HEAD, OPTIONS`, because a browser preflight requires a
2xx — a 405 fails it no matter what CORS headers a CDN attaches, since
the status itself is the blocker. Preflights are not signature-checked
(they perform no work and answer identically for every path; the GET
that follows still is). oximg does not emit the CORS response headers
themselves — `Access-Control-Allow-Origin` and friends come from
whatever fronts it. Other methods still answer 405.

**Graceful shutdown**: on SIGTERM (what `docker stop`, Kubernetes, and
Cloud Run send) or SIGINT the server stops accepting connections,
finishes in-flight requests, and exits 0. There is no drain timeout of
its own — the orchestrator's grace period backstops a response that
never finishes, so allow a few seconds more than your slowest expected
encode.

## CLI

One-shot commands over the same pipeline, no server:

```sh
oximg resize photo.jpg 1600 1600 out.webp     # fit within 1600x1600; format from the extension
oximg resize photo.jpg 800 800 out.jpg -q 70  # JPEG quality 70 (--preset fast|small for mozjpeg)
oximg resize photo.jpg 750 0 out.jpg          # width-only: height follows the aspect ratio
oximg resize photo.jpg 0 0 out.webp           # 0 0 = re-encode at the source's own size
oximg probe photo.webp                        # format + stored dimensions, header-only
```

The output format is `-f/--format`, else the `<out>` extension, else
the source's own format — the same precedence idea as the server's
`@fmt` grammar. The `OXIMG_*` encode knobs below apply to CLI encodes
the same way. Usage errors exit 2; processing failures exit 1.

## Library

The `oximg::pipeline` module is usable without the HTTP server —
`process`/`process_path` take a `Params` and return the re-encoded
bytes plus their format, `probe` reads just the header. Depend on it
with `default-features = false` to drop the entire HTTP stack (axum,
tokio, ureq, hmac, sha2, serde_json); add `features = ["avif"]` for
AVIF. `process_url` and `process_gcs` (remote sources) need the
`server` feature.

Failures are typed: every entry point returns `pipeline::Error`, whose
`kind()` (`ErrorKind`: SourceNotFound / SourceTooLarge /
SourceUnreadable / Upstream / UpstreamTimeout / Undecodable /
Internal) is the stable classification the server's own status mapping
is built on — match on it instead of parsing messages, with a wildcard
arm for kinds added later. `Params` also carries per-call overrides
(`webp_quality`, `png_effort`, `png_quantize`, `auto_rotate`, `icc`,
`flatten_bg`, `linear_light`, `avif_quality`, …) for the knobs that
are otherwise process-global environment variables: `None` keeps the
env-configured behavior, `Some` wins per call — so one process can run
different settings side by side. Rustdoc examples on `probe`,
`process`, and `Params` are compiled and run in CI; see also
[`examples/`](examples/):

```sh
cargo run --release --example thumbnail -- photo.jpg 300 200 out.jpg
cargo run --release --example transcode -- photo.jpg 800 800 webp out.webp
cargo run --release --example probe     -- photo.webp
```

## Configuration

Everything is environment variables, read once at startup. The shared
rule is **fail-closed**: any variable that is set but unparseable or
out of range refuses to boot with a message naming it — a typo'd limit
never silently falls back to a default.

### Server

| Variable | Default | Meaning |
|---|---|---|
| `PORT` | `8081` | Listen port (`0` = OS-assigned, printed on stderr) |
| `IMAGES_DIR` | `./images` | Local source directory (when no source URL is set) |
| `OXIMG_OPTIONS_PREFIX` | unset | Mounts the Cloudflare-style options route at this prefix (e.g. `/image`, `/cdn-cgi/image`) |
| `OXIMG_KEY` / `OXIMG_SALT` | unset | Hex HMAC key/salt; setting both requires signed URLs |
| `OXIMG_WORKERS` | observed parallelism | Pins the CPU permit count (1-512). The default is right almost everywhere — on quota-scheduled platforms like Cloud Run, "pinning to the billed number" measured 17-36% slower (issue #10). For noisy-neighbor hosts or tail-latency-over-throughput shapes; verify with the `oximg_cpu_workers` gauge |
| `OXIMG_LOG` | `error` | `error` = one stderr line per failure; `request` also logs successes. The only accepted values |
| `OXIMG_METRICS` | `0` | `1` serves Prometheus text at `/metrics`: requests by status class and resolved format, upstream outcomes (timeout distinct from fault), queue-wait vs processing histograms, permit/coalescing gauges. Outside the signing scheme — expose it to your scrape network only |

### Sources

| Variable | Default | Meaning |
|---|---|---|
| `OXIMG_SOURCE_BASE_URL` | unset | `https://…` or `gs://bucket[/prefix]` (see [Serving](#serving)) |
| `OXIMG_GCS_ENDPOINT` | `https://storage.googleapis.com` | Override for Private Service Connect or emulators; `GCE_METADATA_HOST` is honored the same way for the token source |
| `OXIMG_UPSTREAM_TIMEOUT` | `30` | Seconds for the whole origin fetch — bounds how long a stalled upstream can hold a CPU permit; timeouts answer 504, distinct from other upstream failures' 502 |
| `OXIMG_UPSTREAM_CONNECT_TIMEOUT` | `5` | Seconds to establish the origin connection |
| `OXIMG_MAX_SOURCE_BYTES` | 64 MiB | Compressed-size cap; over-limit remote sources answer 413 |
| `OXIMG_MAX_SRC_PIXELS` | 64,000,000 | Cheap sanity guard on source dimensions, enforced after each format's header parse; over-cap sources answer 413. Not a memory budget — see the next row |
| `OXIMG_MAX_DECODED_BYTES` | unset | Cap on what a single decode is *estimated* to allocate, in bytes — the unit a container limit is in. Source pixels cannot be mapped to memory here: cost per pixel varies ~16x with the encoding, because baseline JPEG decodes through DCT shrink-on-load (cost tracks the **output**) while progressive JPEG buffers whole-image coefficients and PNG/AVIF decode full frames (cost tracks the **source**), and CMYK stages four channels. The estimate models the buffers the code actually holds at once: the decoder's frame, the linear-light resize input (the same frame as u16), the output-side `dst16`+`out8`, progressive JPEG's coefficient arrays, and the compressed source where a format needs it whole. Field-validated at 1.2-1.8x above measured peaks across four real sources — deliberately conservative, since under-estimating is what gets a container OOM-killed while the cap reports itself satisfied. Encode-side buffers are still excluded. Over-cap sources answer 413 naming the figure. Unset (the default) still computes and exposes the estimate as the `oximg_decoded_bytes_estimate` histogram, so a cap can be read off a real corpus before being enforced |

### Encoding

| Variable | Default | Meaning |
|---|---|---|
| `QUALITY` | `80` | JPEG quality |
| `PRESET` | `jpegli` | `fast` = mozjpeg baseline, `small` = mozjpeg trellis+progressive |
| `OXIMG_JPEG_PROGRESSIVE` | `1` | `0` = baseline jpegli: a few percent larger output for lower latency; with `OXIMG_OVERLAP` this is the speed profile (~-13% single-request latency, ~+9% saturated throughput) |
| `OXIMG_WEBP_QUALITY` | `75` | WebP quality |
| `OXIMG_WEBP_EFFORT` | `2` | libwebp `method` |
| `OXIMG_AVIF_QUALITY` | `55` | AVIF quality (libavif semantics; chosen by operating point, see [bench/quality/QUALITY.md](bench/quality/QUALITY.md)) |
| `OXIMG_AVIF_ALPHA_QUALITY` | color quality | Alpha-plane quality |
| `OXIMG_AVIF_SPEED` | `8` | SVT preset; `9` trades ~-0.6 SSIMULACRA2 at unchanged bytes for ~28% less encode CPU |
| `OXIMG_PNG_EFFORT` | path-dependent | `fastest`/`fast`/`balanced`/`high`. Unset resolves to `fast` for lossless output and `balanced` for quantized output, where effort matters ~2x more; setting it pins one level for both |
| `OXIMG_PNG_QUANTIZE` | `0` | `1` palette-quantizes opaque PNG output (Wu + Floyd–Steinberg): typically ~3x smaller photographic PNGs at the quantized `balanced` default (about half that if effort is forced `fast`), near-exact on flat graphics. Opt-in because quality loss on a lossless format must be deliberate; alpha sources always encode lossless RGBA and ignore this knob |
| `OXIMG_PNG_QUANTIZE_COLORS` | `256` | Palette size, 2-256; 64 trades visible-on-inspection banding for another ~15% |
| `OXIMG_AUTO_FORMAT` | unset | Comma-separated `Accept`-negotiation preference list (e.g. `avif,webp`); see the ordering guidance under [Supported formats](#supported-formats) |
| `OXIMG_FLATTEN_BG` | `ffffff` | Background for alpha → JPEG flattening |

### Pixel pipeline

| Variable | Default | Meaning |
|---|---|---|
| `OXIMG_AUTO_ROTATE` | `1` | `0` serves the stored orientation |
| `OXIMG_ICC` | `1` | `0` strips source ICC profiles and converts CMYK naively instead of through their profile |
| `OXIMG_RESIZE` | `linear` | `srgb` resizes in sRGB space instead of linear light |
| `OXIMG_RESIZE_BACKEND` | `kernel` | `fir` selects the portable fast_image_resize convolution instead of the platform SIMD kernel |
| `OXIMG_OVERLAP` | `auto` | JPEG decode fused with resize+encode on a second thread (~-20% single-request latency); `auto` fuses while `2 x active requests <= visible CPUs`. Bytes are identical either way |
| `OXIMG_PAR` | `1` | Resize threads per request |
| `OXIMG_DCT_MARGIN` | `1.7` | JPEG shrink-on-load headroom over the target size |
| `OXIMG_WEBP_DECODE_THREADS` | `1` | `0` disables libwebp's two-thread decode pipelining |
| `OXIMG_AVIF_DECODE_THREADS` | arch-dependent | dav1d workers: 2 on x86-64 (SMT absorbs the second thread), 1 on aarch64 |
| `OXIMG_TIMING` | unset | Print per-stage timing lines to stderr |

## Deployment

Per-platform guides live in [`docs/`](docs/):

- [Docker / docker-compose](docs/deploy-docker.md) — tag pinning
  (`latest` rebuilds on every main push), read-only mounts, remote
  origins, graceful `docker stop`, building tuned images.
- [Kubernetes](docs/deploy-kubernetes.md) — an example Deployment
  with probes, resource limits (the worker count follows the cgroup
  CPU quota), security context, rolling-update drain behavior, and
  restoring cross-pod request coalescing via ingress URI hashing.
- [Cloud Run & serverless containers](docs/deploy-cloud-run.md) —
  the `PORT` contract, remote-origin mode, `gs://` with the service
  identity, concurrency-vs-vCPU sizing, and why per-process request
  coalescing yields nothing on scaled-out shapes.

The short version for every platform: pin an image version, put a CDN
in front (responses carry a 1-year `Cache-Control`), give the process
whole CPUs, and allow ≥10s of shutdown grace so in-flight encodes
drain.

## Not yet implemented (out of PoC scope)

- Private S3 / S3-compatible sources (`gs://` landed in 0.7.4; `s3://`
  is tracked in [#11](https://github.com/oximg/oximg/issues/11) and
  fails at boot with a pointer rather than misbehaving)
- JXL output (the `@jxl` token is reserved and returns a clear error)
- Animated output (animated AVIF and WebP *sources* render their
  first frame, like other image proxies)
- Response caching

## Roadmap

Rough order, subject to change (experimental PoC):

- **`s3://` sources** — S3 and S3-compatible endpoints (R2, MinIO,
  B2) with static credentials first, the AWS credential chain after
  ([#11](https://github.com/oximg/oximg/issues/11)).
- **Per-image output format selection** — choose quantized-PNG vs
  WebP per image rather than per deployment
  ([#6](https://github.com/oximg/oximg/issues/6)).
- **JXL output** once a maintained encoder binding stabilizes.
- **Response caching** (keyed on the resolved URL + format).

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
