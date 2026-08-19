# oximg

[![Crates.io](https://img.shields.io/crates/v/oximg.svg)](https://crates.io/crates/oximg)
[![Docs.rs](https://docs.rs/oximg/badge.svg)](https://docs.rs/oximg)
[![CI](https://github.com/oximg/oximg/actions/workflows/ci.yml/badge.svg)](https://github.com/oximg/oximg/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/badge/license-Apache--2.0-blue.svg)](LICENSE)

High-performance image compression in Rust: a library, a CLI, and a
self-hostable HTTP server (PoC). JPEG, PNG, WebP — and AVIF with the
`avif` feature — in and out, plus GIF in (animated GIF included, as
animated WebP); sources are format-sniffed by magic bytes and re-encoded
in their own format (GIF, having no encoder here, becomes WebP). On
imgproxy's official benchmark harness, run on
the same AWS instance types as their published results, oximg leads
every format cell on both x86-64 and Graviton while resizing in linear
light at measurably higher output quality (see
[Benchmarks](#benchmarks)).

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
  bucket, no public-endpoint egress). The origin round trip never
  holds a CPU slot (fetches are buffered and separately bounded), and
  transient fetch failures are retried, so a network blip is a slower
  response, not a broken image.
- **Production operability**: graceful SIGTERM drain, upstream fetch
  deadlines (slow-origin 504s distinct from broken-origin 502s), and
  an opt-in Prometheus `/metrics` page whose queue-wait/processing
  split tells "needs more CPU" apart from "sources got bigger".
- **Quality-first processing**: resizing happens in linear light on
  16-bit samples with Lanczos3, JPEG sources are decoded at full size
  so the whole reduction is the resampler's, and alpha is premultiplied
  across the resample — the properties behind the SSIMULACRA2 scores in
  [Benchmarks](#benchmarks). Shrink-on-load is available
  (`OXIMG_DCT_MARGIN`) and off by default: it buys decode time with
  quality, and libjpeg's reduced IDCT charges erratically for it — 13.4
  SSIMULACRA2 points on a 5.3x downscale, for the same output size and
  the same bytes.
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
| JPEG | baseline & progressive, grayscale; streaming, full-size decode (shrink-on-load opt-in) | jpegli progressive (default), mozjpeg profiles via `PRESET` |
| PNG | palette / grayscale / 16-bit, normalized to RGB(A)8 | lossless RGB(A); opt-in palette quantization (`OXIMG_PNG_QUANTIZE`) |
| WebP | lossy & lossless, alpha | lossy (`OXIMG_WEBP_QUALITY`, 75), alpha; output is scaled to fit WebP's 16383 px limit |
| AVIF (`--features avif`) | dav1d: 8/10/12-bit, all subsamplings, alpha | SVT-AV1: 10-bit 4:2:0, tune=ssim, alpha as auxiliary image |
| GIF | GIF87a/89a, every frame composited onto the logical screen (frame sub-rectangle, transparent index, all four disposal methods) | none — see below |

GIF is the one decode-only format, so it is also the one source whose
output format is not its own: with no `@{fmt}` and no negotiation, a GIF
becomes **WebP**, and an *animated* GIF becomes an **animated WebP**
(see [Animation](#animation) for the budgets that decides under). That
is a deliberate choice, not a missing encoder —
on a 15-file real-world corpus, lossless GIF→GIF saved *nothing* on 9 of
them (median 100% of the source bytes, which is also what imgproxy's
GIF→GIF measured), and at native size the smallest GIF variant measured
still landed at 80.7% where WebP reached 25.2% at the same visual score.
`@gif` and `format=gif` are rejected with a 400 instead of quietly
answering with different bytes under the name the client asked for. See
[`docs/gif-evaluation.md`](docs/gif-evaluation.md) for the measurements.

**Cross-format output**: append an imgproxy-style `@{fmt}` token to the
filename — `/resize/300/200/photo.jpg@webp` (`jpg`/`jpeg`, `png`,
`webp`, `avif`; `jxl` is reserved, `gif` permanently so). Only exact
tokens count, so `photo@2x.jpg` is still a filename. Precedence:
explicit `@{fmt}` > `Accept` negotiation > source format. Negotiation is
opt-in: set `OXIMG_AUTO_FORMAT` to a preference list (e.g. `avif,webp`) and
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
      JPEG: mozjpeg streaming decode at full size (shrink-on-load is
            opt-in via OXIMG_DCT_MARGIN; CMYK/YCCK keep it at 1.7x,
            where it caps a staged frame instead of costing quality)
      PNG:  png crate (palette/gray/16-bit normalized to RGB(A)8)
      WebP: libwebp
      AVIF: dav1d (8/10/12-bit, all subsamplings, alpha, bilinear chroma upsampling)
      GIF:  gif crate, frames composited onto the logical screen (every
            frame for an animated GIF into a WebP target, else the first)
  → linear-light resize: sRGB u8 → linear u16 → Lanczos3 → sRGB u8
      (alpha is premultiplied before resampling, unpremultiplied after;
       JPEG rows stream through in-tree ring-scheduled f32 row kernels —
       AVX2 on x86-64, NEON on aarch64, both verified against an f64
       reference — optionally fused with the decode on a second thread;
       other formats resize full-frame: pic-scale on x86-64, the same
       in-tree kernel on aarch64)
  → encode in the source format (GIF sources: WebP, the default target)
      JPEG: jpegli, progressive (PRESET=fast / PRESET=small select mozjpeg profiles)
      PNG:  png crate | WebP: libwebp | AVIF: SVT-AV1 (10-bit 4:2:0, tune=ssim)
      animated GIF: each frame composited, resized and streamed into
            libwebp's animation encoder (one canvas in memory, not N)
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

Remote sources are downloaded into a bounded buffer (`OXIMG_MAX_SOURCE_BYTES`)
*before* the request takes a CPU slot, so the origin round trip never
holds one — measured at ~50% of a permit's hold time on a production
corpus before the split (issue #20/#22). Download concurrency has its
own bound, `OXIMG_FETCH_CONCURRENCY`, and local sources keep the
streaming decode (no buffering, the page cache serves the read).
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
oximg probe loop.gif                           # animated sources also print frames, duration and loop count
```

The output format is `-f/--format`, else the `<out>` extension, else
the source's own format — the same precedence idea as the server's
`@fmt` grammar. The `OXIMG_*` encode knobs below apply to CLI encodes
the same way. Usage errors exit 2; processing failures exit 1.

## Library

The `oximg::pipeline` module is usable without the HTTP server —
`process`/`process_path` take a `Params` and return the re-encoded
bytes plus their format, `probe` reads just the header, and
`probe_animation` reports frame count, play time and loop count for an
animated source (GIF and WebP) without decoding pixels. Depend on it
with `default-features = false` to drop the entire HTTP stack (axum,
tokio, reqwest, hmac, sha2, serde_json); add `features = ["avif"]`
for AVIF. The remote-source functions need the `server` feature:
`fetch_url`/`fetch_gcs` download a bounded buffer (with `_async`
variants for callers already inside a runtime), and
`process_url`/`process_gcs` are fetch-then-decode in one call.

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
| `OXIMG_WORKERS` | observed parallelism | Pins the CPU permit count (1-512). The default is right almost everywhere — on quota-scheduled platforms like Cloud Run, "pinning to the billed number" measured 17-36% slower (issue #10). The knob exists for noisy-neighbor hosts, tail-latency-over-throughput shapes, and platforms where observed parallelism is unrelated to what is available. **The remote-source reason to raise it is gone** (issue #22): permits are no longer held across the origin fetch, so `fetch/process` no longer names throughput that extra permits would recover — the earlier guidance ("above the CPU count is often right for remote sources", with its `permits x (1 - fetch/process)` saturation arithmetic) applied to 0.8.x and earlier. Permits are still bounded by memory, not just CPU: `(memory limit - idle RSS) / decoded-bytes p99` (see `OXIMG_MAX_DECODED_BYTES`) is the other ceiling, and it is the binding one on small pods with a heavy decode tail. **Verify with the `oximg_cpu_workers` gauge**, which is the only way to know what a given deployment actually got. What "observed" observes is worth knowing — see below |
| `OXIMG_FETCH_CONCURRENCY` | `4 x permits`, max 256 | Bounds concurrent origin downloads (1-1024). Fetches hold no CPU permit (issue #22), so they need their own bound: the buffered-source memory hazard is this knob times `OXIMG_MAX_SOURCE_BYTES` at worst case. The default absorbs an 8-wide `srcset` burst per permit at production-like fetch shares; raise it when the origin RTT is large relative to per-request CPU work (many fetches must overlap to keep one core fed) and the sources are known-small |
| `OXIMG_LOG` | `error` | `error` = one stderr line per failure; `request` also logs successes. The only accepted values |
| `OXIMG_METRICS` | `0` | `1` serves Prometheus text at `/metrics`: requests by status class and resolved format, upstream outcomes (timeout distinct from fault — and note that `rejected` reading zero is itself the signal in `gs://` mode: an over-length key is refused locally, so the store is never asked and the request lands in `not_found`. If `rejected` ever moves there, the store refused something, which is a different event), duration histograms split into remote-source `fetch` (everything between "ready to fetch" and "source in hand" — fetch-slot wait plus the whole download — none of it holding a CPU permit since issue #22), CPU-permit queue wait, and processing (the permit's actual hold). `fetch/process` therefore no longer names recoverable throughput; it names the wait the permit no longer pays for. Read fetch numbers from *warm* traffic, since a fresh process pays connection and TLS setup and reads high for its first requests. Permit/coalescing gauges included. Outside the signing scheme — expose it to your scrape network only |

### Sources

| Variable | Default | Meaning |
|---|---|---|
| `OXIMG_SOURCE_BASE_URL` | unset | `https://…` or `gs://bucket[/prefix]` (see [Serving](#serving)) |
| `OXIMG_GCS_ENDPOINT` | `https://storage.googleapis.com` | Override for Private Service Connect or emulators; `GCE_METADATA_HOST` is honored the same way for the token source |
| `OXIMG_UPSTREAM_TIMEOUT` | `30` | Seconds for the whole origin fetch — bounds how long a stalled upstream can hold a fetch slot (and its buffer); timeouts answer 504, distinct from other upstream failures' 502 |
| `OXIMG_UPSTREAM_CONNECT_TIMEOUT` | `5` | Seconds to establish the origin connection |
| `OXIMG_MAX_SOURCE_BYTES` | 64 MiB | Compressed-size cap; over-limit remote sources answer 413 |
| `OXIMG_MAX_SRC_PIXELS` | 64,000,000 | Cheap sanity guard on source dimensions, enforced after each format's header parse; over-cap sources answer 413. Not a memory budget — see the next row |
| `OXIMG_MAX_DECODED_BYTES` | unset | Cap on what a single decode is *estimated* to allocate, in bytes — the unit a container limit is in. Source pixels cannot be mapped to memory here: cost per pixel varies ~16x with the encoding, because baseline JPEG decodes through DCT shrink-on-load (cost tracks the **output**) while progressive JPEG buffers whole-image coefficients and PNG/AVIF decode full frames (cost tracks the **source**), and CMYK stages four channels. The estimate models the buffers the code actually holds at once: the decoder's frame, the linear-light resize input (the same frame as u16), the output-side `dst16`+`out8`, progressive JPEG's coefficient arrays, and the compressed source where a format needs it whole. Field-validated at 1.2-1.8x above measured peaks across four real sources — deliberately conservative, since under-estimating is what gets a container OOM-killed while the cap reports itself satisfied. Encode-side buffers are still excluded. Over-cap sources answer 413. The response body is deliberately generic across all three source caps (it would otherwise hand clients the configured limits); the stderr line names which limit was hit and the estimated figure, so that is where to look when calibrating. Unset (the default) still computes and exposes the estimate as the `oximg_decoded_bytes_estimate` histogram, so a cap can be read off a real corpus before being enforced |
| `OXIMG_LOG_DECODED_BYTES_ABOVE` | unset | Report any decode whose estimate exceeds this — filename and per-term breakdown to stderr — and **serve it normally**. Orthogonal to the cap: the cap refuses and names what it refused, this names without refusing. That distinction is what makes a cap settable: a cap high enough to be safe names nothing, and one set at the tail buys names by refusing live traffic. The histogram tells you *that* a request cost 512 MiB; this tells you *which image*. Setting only this is the natural first step for a new deployment — learn the corpus, then choose the cap. Applies to the CLI too |

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

#### What "observed parallelism" observes

On a Linux container the count comes from the cgroup CPU quota and the
process's CPU affinity, whichever is smaller, floored at 1 — measured
on cgroup v2 (`workers` is the `oximg_cpu_workers` gauge):

| container CPU config | cgroup | workers |
|---|---|---|
| no limit | `cpu.max: max` | host CPU count |
| 1 CPU | quota 1.0 | 1 |
| 1.5 CPU (`1500m`) | quota 1.5 | **1** |
| 1.9 CPU (`1900m`) | quota 1.9 | **1** |
| 2 CPU | quota 2.0 | 2 |
| 2.5 CPU (`2500m`) | quota 2.5 | 2 |
| 0.5 CPU (`500m`) | quota 0.5 | 1 (the floor) |
| CPU shares only, no quota | `cpu.weight` set, `cpu.max: max` | host CPU count |
| pinned to 2 cores | affinity `0-1`, no quota | 2 |
| pinned to 3 cores + quota 1 | both | 1 (the smaller) |

Three consequences that catch people out:

- **On Kubernetes this is `limits.cpu`.** `requests.cpu` has no effect:
  it becomes `cpu.weight`, a scheduling share with no count in it, so
  there is nothing there to observe. A `limits.cpu` set as a
  blast-radius guard silently becomes a concurrency decision.
- **Fractional limits round down.** `limits.cpu: 1500m` yields the same
  single permit as `1000m` while costing 50% more, and `1900m` is still
  1. Whole numbers are the only way to buy concurrency — the second
  permit arrives at `2`, not at `1001m`.
- **A pod with only `requests.cpu` and no limit is the dangerous
  shape**: it sizes itself to the *node*, so on a 64-core node it will
  admit 64 concurrent decodes while being scheduled for a fraction of
  one core — and since peak memory is permits x per-request decode cost
  (`OXIMG_MAX_DECODED_BYTES`), that presents as unexplained memory
  pressure rather than as queue latency, with nothing in the pod spec
  looking wrong. oximg prints a startup note when it takes its permit
  count from full host parallelism with no CPU quota visible; a
  deliberately restricted cpuset (Kubernetes' static CPU-manager
  policy) is not warned about, because that count is correct.
- **Platforms without a hard quota fall back to host parallelism**,
  which is why an equivalently-sized container reports a different
  number on Cloud Run (`cpu: "1"` there observes 2 — see
  [the Cloud Run guide](docs/deploy-cloud-run.md), where pinning it
  down measured *slower*).

### Animation

An animated GIF into a WebP target is served as an animated WebP; every
other target renders its first frame. The knobs below bound what one
such request may cost — and unlike the source caps above, **exceeding
one is not an error**: the request degrades to the still first frame and
still answers 200. That is the point. An animation is not one image but
N, so a single request can cost hundreds of still requests' CPU
(measured: 3.2 s for the worst file in a 15-file corpus, against 5 ms
for a still — [docs/gif-evaluation.md](docs/gif-evaluation.md) §5), and
a service that answers a *smaller* thing beats one that answers 413 to
an image a browser will happily display.

| Variable | Default | Meaning |
|---|---|---|
| `OXIMG_GIF_ANIMATION` | `1` | `0` renders every animated GIF as its still first frame, i.e. 0.11.0 behaviour |
| `OXIMG_MAX_ANIM_FRAMES` | `200` | Source frames an animation may carry before it degrades to a still. Bounds the decode+composite half of the work, which shrinking the output does not touch |
| `OXIMG_MAX_ANIM_WORK` | `8,000,000` | Encoded frames x **post-resize** frame area, in pixels — the product that predicts encode time, which is what dominates an animation (3018 ms of a 3213 ms worst case). The default admits the measured corpus' worst in-budget file (26 frames of 1280x720 into a 512 box, ~6.8 Mpx, 517 ms) and refuses its worst overall (~34 Mpx, 3.2 s). Because it is measured *after* the resize, a small output box is what buys an expensive source back: the same source that is refused at native size fits into a thumbnail |
| `OXIMG_ANIM_FRAME_STEP` | `1` | Encode every Nth frame. Total play time is preserved (a skipped frame extends the previous frame's duration), so this costs smoothness, not fidelity — which is why it is off by default. `2` roughly halves encode cost |

One caveat on bytes: animated WebP wins hugely on photographic and
video-like GIFs (4–30% of the source across the corpus) but can *lose*
on small flat-graphics ones, where GIF's tiny palette is exactly what
LZW compresses best — a measured 25 KB, 8-frame cartoon comes back at
41 KB. oximg re-encodes whatever it is asked to, in every format; if
a caller has such sources, serving them at their own size gains nothing
and `OXIMG_GIF_ANIMATION=0` is the cheaper answer.

The estimated-memory cap (`OXIMG_MAX_DECODED_BYTES`) applies here too,
and degrades rather than refusing for the same reason: a still of the
same GIF fits under any cap that admits one frame. Frames are
composited, resized and handed to the encoder one at a time, so *our*
staging is a function of the canvas alone — but libwebp's animation
encoder retains every frame it has compressed until the container is
assembled, so peak memory does grow with the animation. The estimate
prices that retained output at one byte per encoded pixel (~9x the
0.11 B/px measured across the corpus, i.e. deliberately pessimistic),
which makes it `OXIMG_MAX_ANIM_WORK`, not the canvas, that bounds it.

### Pixel pipeline

| Variable | Default | Meaning |
|---|---|---|
| `OXIMG_AUTO_ROTATE` | `1` | `0` serves the stored orientation |
| `OXIMG_ICC` | `1` | `0` strips source ICC profiles and converts CMYK naively instead of through their profile |
| `OXIMG_RESIZE` | `linear` | `srgb` resizes in sRGB space instead of linear light |
| `OXIMG_RESIZE_BACKEND` | `kernel` | `fir` selects the portable fast_image_resize convolution instead of the platform SIMD kernel |
| `OXIMG_OVERLAP` | `auto` | JPEG decode fused with resize+encode on a second thread (~-20% single-request latency); `auto` fuses while `2 x active requests <= visible CPUs`. Bytes are identical either way |
| `OXIMG_PAR` | `1` | Resize threads per request |
| `OXIMG_DCT_MARGIN` | unset | Decode-size headroom over the target: shrink-on-load, off by default. It is a **speed** knob. libjpeg's reduced IDCT is erratic per scale, so the cost is not graded: on a 5.3x downscale the 1.7 that used to be the default selects libjpeg's 3/8 scale, which measured 13.4 SSIMULACRA2 points below a full decode for the same output size and the same bytes, while 5/8 on the same image was optimal — and no single value avoids the bad scales at every ratio ([dct_sweep.py](bench/quality/dct_sweep.py)). Set it to trade quality for decode time on large sources. The **buffered** paths ignore the default and keep 1.7 — CMYK/YCCK JPEG and WebP stage a whole frame at the decode size, so there the shrink caps peak RSS (WebP 21.7 MB against 133.9 MB full-size) rather than costing throughput; an explicit value still applies to them |
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
- Animated output from an animated **AVIF or WebP** source (those render
  their first frame; animated GIF sources do animate — see
  [Animation](#animation))
- GIF output (GIF is decode-only; `@gif` returns a clear error and GIF
  sources default to WebP)
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
