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
  `0` leaves an axis unconstrained: `/resize/750/0/…` is width-only
  (height follows the aspect ratio — what `srcset` `w` descriptors and
  Next.js loaders emit), `/resize/0/1024/…` height-only. Prefer that
  over a large sentinel height, which silently narrows sources taller
  than the sentinel's aspect ratio.
  `{file}` may span directories (`/resize/300/200/albums/2026/photo.jpg`),
  so S3-style prefixes and nested trees are addressable as-is. Sources
  come from a local directory or any HTTP(S) origin
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

## Usage

**Docker** (recommended — multi-arch linux/amd64 + linux/arm64, AVIF
included; both registries rebuild on every `main` push):

```sh
docker run -p 8081:8081 -v $PWD/images:/images:ro ghcr.io/oximg/oximg:latest
# or: docker.io/oximg/oximg:latest
curl "localhost:8081/resize/500/500/photo.jpg" -o out.jpg
```

**Prebuilt binaries** ([GitHub Releases](https://github.com/oximg/oximg/releases),
v0.6.0+; Linux x86_64/aarch64 and macOS arm64;
JPEG/PNG/WebP, no AVIF) — suited to CI asset pipelines where a Docker
pull or a source build is too slow. Assets are named
`oximg-<tag>-<target>.tar.gz` with a `.sha256` alongside; each is
smoke-tested before upload. Linux builds link glibc >= 2.39
(Ubuntu 24.04) with libstdc++ static — self-contained on any current
CI runner.

**Homebrew** (builds v0.7.4 from source; JPEG/PNG/WebP, no AVIF):

```sh
brew install oximg/tap/oximg
```

**Cargo** (crates.io, v0.7.4; add `--features avif` if SVT-AV1 >= 4.1
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
IMAGES_DIR=./images PORT=8081 QUALITY=80 ./target/release/oximg   # = oximg serve
```

**One-shot CLI** (the same pipeline, no server):

```sh
oximg resize photo.jpg 1600 1600 out.webp     # fit within 1600x1600; format from the extension
oximg resize photo.jpg 800 800 out.jpg -q 70  # JPEG quality 70 (--preset fast|small for mozjpeg)
oximg resize photo.jpg 750 0 out.jpg          # width-only: height follows the aspect ratio
oximg resize photo.jpg 0 0 out.webp           # 0 0 = re-encode at the source's own size
oximg probe photo.webp                        # format + stored dimensions, header-only
```

The output format is `-f/--format`, else the `<out>` extension
(`jpg|jpeg|png|webp|avif`), else the source's own format — the same
precedence idea as the server's `@{fmt}` URL grammar. Encode knobs
that are env-configured on the server (`OXIMG_WEBP_QUALITY`,
`OXIMG_PNG_EFFORT`, ...) apply to CLI encodes the same way. Usage
errors exit 2; processing failures exit 1.

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

Failures are typed: every entry point returns `pipeline::Error`, whose
`kind()` (`ErrorKind`: SourceNotFound / SourceTooLarge /
SourceUnreadable / Upstream / Undecodable / Internal) is the stable
classification the server's own status mapping is built on — match on
it instead of parsing messages, with a wildcard arm for kinds added
later. `Params` also carries per-call overrides (`webp_quality`,
`png_effort`, `auto_rotate`, `icc`, `flatten_bg`, `linear_light`,
`avif_quality`) for the knobs that are otherwise process-global
`OXIMG_*` environment variables: `None` keeps the env-configured
behavior, `Some` wins per call — so one process can run different
settings side by side. See [`examples/`](examples/):

```sh
cargo run --release --example thumbnail -- photo.jpg 300 200 out.jpg
cargo run --release --example transcode -- photo.jpg 800 800 webp out.webp
cargo run --release --example probe     -- photo.webp
```

**Graceful shutdown**: on SIGTERM (what `docker stop`, Kubernetes,
and Cloud Run send) or SIGINT the server stops accepting connections,
finishes in-flight requests, and exits 0. There is no drain timeout
of its own — the orchestrator's grace period (10s for `docker stop`
and Cloud Run, `terminationGracePeriodSeconds` on Kubernetes)
backstops a response that never finishes, so give it a few seconds
more than your slowest expected encode.

URL signing (optional): set `OXIMG_KEY` and `OXIMG_SALT` (hex) to
require imgproxy-style signed URLs —
`/{base64url(HMAC-SHA256(key, salt || path))}/resize/{w}/{h}/{file}`.
The signed `path` is the percent-decoded form (nested `{file}` included,
with any `@{fmt}` token), so one signature covers every URL encoding of
the same source.

**Source paths**: `{file}` is validated component-wise — `.` or `..`
components, empty components (leading/trailing/double slashes), `\`,
`?`, `#`, and control bytes answer 400. Local sources additionally
resolve through a symlink-containment check: a path that escapes
`IMAGES_DIR` answers 404. Remote sources are re-encoded segment-wise
before the origin fetch, so a percent in a name is never double-decoded
upstream.

Environment variables: `PORT` (8081), `IMAGES_DIR` (./images),
`OXIMG_SOURCE_BASE_URL` (fetch sources from `<base>/<file>` instead of
the local filesystem; the scheme selects the transport. `https://` is
the anonymous HTTP mode; **`gs://bucket[/prefix]` reads a private GCS
bucket directly** with GCP-attached credentials — GKE Workload
Identity, Cloud Run, and GCE metadata credentials all work, tokens are
cached and refreshed, and boot fails closed with a clear message if no
credentials are reachable. No public bucket, no egress through a
public endpoint, and SDK-style retries on 429/5xx. (`service_account`
JSON keys are not supported — on GCP use Workload Identity; off GCP
use the HTTP mode. `s3://` is planned, see issue #11.) Streaming
decode overlaps the download in every mode. Connection-level transients — reset, refused, DNS blips —
are retried once before any body bytes are consumed, so a single
network blip is a slightly slower response instead of a broken image;
`oximg_upstream_retries_total` counts them. **Exposure prerequisite**:
this mode sends no credentials, so the origin must be anonymously
readable — for an object-store bucket that means public objects, and
anyone who can guess a path can fetch the original at full resolution,
bypassing every resize/signing/CDN control in front. Weigh that before
pointing this at a bucket that was private under an SDK-based
fetcher), `OXIMG_GCS_ENDPOINT` (`https://storage.googleapis.com`; override for
Private Service Connect endpoints or emulators — `GCE_METADATA_HOST`
is honored the same way for the token source),
`OXIMG_WORKERS` (unset = the parallelism the container
observes, which is the right default almost everywhere — including
platforms like Cloud Run whose `cpu` setting is a time quota, not a
core count, where "pinning to the billed number" measured 17-36%
slower (issue #10); 1-512 pins the CPU permit count explicitly for
the shapes that genuinely want it — noisy-neighbor hosts, trading
throughput for tail latency, or platforms where observed parallelism
is unrelated to what is actually available; verify with the
`oximg_cpu_workers` gauge),
`OXIMG_OPTIONS_PREFIX` (unset; mounts a second route
speaking the Cloudflare Images option grammar at the given prefix —
`OXIMG_OPTIONS_PREFIX=/image` serves
`/image/width=750,quality=80/albums/2026/photo.png` — so URLs built
for Cloudflare Images survive a migration without a rewrite layer.
Options: `width`/`height` (1-8192; one suffices, the other axis
follows the aspect ratio), `quality` (1-100, applied to whichever
format the output resolves to; PNG output is lossless and ignores
it), `format` (`jpeg|png|webp|avif`, or `auto` = the same
Accept negotiation as a bare positional URL, which also runs when
`format` is absent). Unknown or duplicate options answer 400 naming
the key — Cloudflare silently ignores unknown options, but a dropped
`fit=cover` changes the output, so this divergence is deliberate. The
filename is taken literally on this route (no `@{fmt}` token), and
with signing enabled the same HMAC scheme covers
`/{signature}{prefix}/{options}/{file}` over the decoded path),
`OXIMG_METRICS` (`0`; `1` serves a Prometheus text page at
`/metrics`: requests by status class and resolved output format,
upstream fetch outcomes with timeouts distinct from faults, latency
histograms split into CPU-permit queue wait vs processing — rising
queue wait under flat processing means "needs more CPU", both rising
means "sources got bigger" — plus permit and coalescing gauges. The
route sits outside the URL-signing scheme, so expose it to your
scrape network only. Failure-rate attribution still needs a
platform-side memory/restart alert to catch an OOM loop — metrics
from a process that keeps dying cannot tell that story alone), `OXIMG_MAX_SOURCE_BYTES` (64MiB; over-limit remote sources answer
413), `OXIMG_MAX_SRC_PIXELS` (64,000,000; decoded-size cap enforced
after each format's header parse, before any pixel allocation —
compressed size does not bound decoded size; over-cap sources also
answer 413), `OXIMG_UPSTREAM_TIMEOUT` (30; seconds for the whole
origin fetch — this bounds how long a stalled upstream can hold one
of the core-count CPU permits, so it is the knob that keeps a slow
origin from silently eating throughput; timeouts answer 504, distinct
from other upstream failures' 502), `OXIMG_UPSTREAM_CONNECT_TIMEOUT`
(5; seconds to establish the origin connection), `QUALITY`
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
`OXIMG_PNG_EFFORT` (unset; `fastest`/`fast`/`balanced`/`high` trade
PNG size against encode time — unset resolves to `fast` for lossless
output and `balanced` for quantized output, where effort matters ~2x
more; setting it explicitly pins one level for both paths),
`OXIMG_PNG_QUANTIZE` (`0`; `1` palette-quantizes
opaque PNG output — Wu quantization with Floyd–Steinberg dithering —
typically a ~3x byte reduction on photographic PNGs at the quantized
path's `balanced` effort default (about half that if effort is forced
to `fast`), and nearly indistinguishable on flat graphics; opt-in
because quality loss on a lossless format must be a deliberate choice;
sources with alpha always encode lossless RGBA and ignore this knob
entirely), `OXIMG_PNG_QUANTIZE_COLORS` (`256`; palette
size, 2-256 — 64 colors trades visible-on-inspection banding for
another ~15% on photographic sources), `OXIMG_WEBP_EFFORT` (libwebp `method`, 2), `OXIMG_WEBP_DECODE_THREADS` (`1`; `0` disables
libwebp's two-thread decode pipelining), `OXIMG_TIMING` (set to print
per-stage timing lines to stderr), `OXIMG_LOG` (`error`: one stderr
line per failed request, always on; `request` also logs successes,
with a request id and wall time; these two are the only accepted
values — anything else, `info` included, refuses to boot like every
other misconfigured knob),
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

## Deployment

Per-platform guides live in [`docs/`](docs/):

- [Docker / docker-compose](docs/deploy-docker.md) — tag pinning
  (`latest` rebuilds on every main push), read-only mounts, remote
  origins, graceful `docker stop`, building tuned images.
- [Kubernetes](docs/deploy-kubernetes.md) — an example Deployment
  with probes, resource limits (the worker count follows the cgroup
  CPU quota), security context, and rolling-update drain behavior.
- [Cloud Run & serverless containers](docs/deploy-cloud-run.md) —
  the `PORT` contract, remote-origin mode (no local disk),
  concurrency-vs-vCPU sizing, and CDN caching in front.

The short version for every platform: pin an image version, put a
CDN in front (responses carry a 1-year `Cache-Control`), give the
process whole CPUs, and allow ≥10s of shutdown grace so in-flight
encodes drain.

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
