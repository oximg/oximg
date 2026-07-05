# Benchmarks

Throughput, latency, and peak memory for "photo → fit into 500x500 JPEG
(quality 80)", measured against imgproxy and imagor. Output quality for
the same scenarios is measured separately in
[bench/quality/QUALITY.md](bench/quality/QUALITY.md).

Two load patterns are reported where relevant:

- **single-URL**: `ab -n N -c C` against one URL. Servers that coalesce
  concurrent identical requests compute the result once per burst, so
  this pattern reflects duplicate/hot-key traffic.
- **diverse**: 16 concurrent workers, each requesting a distinct target
  width (485-500), so every request requires full processing.

## Linux x86_64 (AMD Ryzen 7 8745HS, 8C/16T, Arch Linux)

All servers as Docker containers: oximg (this repo's Dockerfile),
`ghcr.io/imgproxy/imgproxy:latest` (v4.0.11),
`ghcr.io/cshum/imagor:latest` (v1.9.2). Load generated on-host. CPU per
request measured via cgroup v2 `cpu.stat` deltas.

### Medium source: 2000x1333 JPEG (0.8MB), c=16

| Server | single-URL req/s | single-URL CPU/req | diverse req/s | diverse CPU/req |
|---|---|---|---|---|
| oximg (defaults) | **1262** | **845 µs** | **617** | **19.0 ms** |
| imagor 1.9.2 | 1234 | 1041 µs | 531 | 21.8 ms |
| imgproxy 4.0.11 | 791 | 16312 µs | 482 | 21.2 ms |

oximg speed mode (`OXIMG_RESIZE=srgb OXIMG_DCT_MARGIN=1.0`): 820 req/s
diverse, 12.5 ms CPU/req.

### Large source: 7360x4912 JPEG (10.6MB), diverse, c=16

| Server | diverse req/s |
|---|---|
| oximg (defaults) | **106** |
| imgproxy 4.0.11 | 96 |
| imagor 1.9.2 | 76 |

### Same-URL matrix (c=8 / c=16, peak RSS via cgroup `memory.peak`)

| Server | medium c=8 | medium c=16 | large c=8 | large c=16 | peak RSS (medium / large) |
|---|---|---|---|---|---|
| oximg (defaults) | 555 | 1101 | 88.9 | 174 | **21 / 19 MB** |
| imgproxy 4.0.11 | 655 | 817 | 70.7 | 108 | 82 / 271 MB |
| imagor 1.9.2 | 668 | 1275 | 71.0 | 139 | 167 / 209 MB |

Note: oximg and imagor coalesce concurrent identical requests, so their
same-URL columns reflect duplicate-traffic handling rather than pipeline
throughput (see the diverse tables above for the latter); it also keeps
their same-URL peak RSS low. Peak RSS under 16-way diverse load: oximg
172 MB, imagor 167-177 MB.

## macOS (Apple M2 Max, 12 cores), native installs

oximg release build vs imgproxy (Homebrew) + vips, both at quality 80,
identical output dimensions, 20-request warm-up, servers restarted per
scenario; tool and versions noted per table (`ab` with a single URL
unless stated otherwise). Methodology after
[the imgproxy benchmark gist](https://gist.github.com/DarthSim/9d971d2859f3714a29cf8ce094b3fc55).

### Large: 7360x4912 (10.6MB) → 500x500, N=400, c=8

| Server | req/s | p50 | p95 | peak RSS | output |
|---|---|---|---|---|---|
| oximg (defaults) | **71.7** | 109 ms | 116 ms | — | 23.9 KB |
| oximg speed mode | 72.2 | 107 ms | 111 ms | **130 MB** | 23.9 KB |
| imgproxy | 60.7 | 127 ms | 138 ms | 317 MB | 22.9 KB |

### Medium: 2000x1333 (0.9MB) → 500x500 (re-measured on Linux/Zen4)

**Methodology correction (2026-07).** An earlier revision of this
table drove `wrk` with a URL-cycling counter per thread; every wrk
thread's Lua VM starts at the same counter, so threads walk the same
URL sequence in near-lockstep and concurrent duplicates hit oximg's
request coalescing (imgproxy does not coalesce), inflating oximg's
multi-connection numbers by up to 2x. Current numbers assign each
thread a disjoint URL residue class, which makes coalescing
impossible; a same-box A/B confirmed imgproxy's numbers do not move
under either script. Same trap, different tool, as the k6 pigeonhole
note below.

The corrected reference matrix comes from a dedicated Linux box (AMD
Zen4 8 cores/16 threads, Arch, oximg native, imgproxy 4.0.11 via
host-network Docker — Docker overhead separately measured at ±3%),
five interleaved A/B rounds per concurrency, medians; q92 4:4:4
plasma source, identical 500x333 outputs:

| Server | SSIM2 | c=1 latency | c=8 req/s | c=16 req/s | output |
|---|---|---|---|---|---|
| oximg (default) | **76.1** | 10.8 ms | 583 | 740 | **20.2 KB** |
| oximg speed profile | **76.1** | **9.4 ms** | **659** | **810** | 22.5 KB |
| imgproxy | 74.6 | 10.4 ms | 614 | 784 | 22.4 KB |

SSIM2 scores this table's own outputs against a linear-light Lanczos
reference of the plasma source (differences above ~2 points are
generally perceptible; both oximg profiles decode to identical pixels
— baseline vs progressive jpegli differ only in entropy layout).
Smooth synthetic noise is the content where oximg's supersampled
linear-light resize matters least; on the real-photo corpus the same
q80 comparison is **77.5 / 72.1 / 67.3 vs imgproxy's 71.2 / 60.1 /
49.7** for 768px/2000px/4000px sources — +6 to +18 points, with
imgproxy unable to close it at any byte cost (63.8 KB at q90 still
scores 76.0 vs oximg's 77.5 from 34.0 KB). Full protocol and sweeps
in [bench/quality/QUALITY.md](bench/quality/QUALITY.md).

Both oximg rows are the auto overlap gate composing one pipeline:
decode fused with resize+encode on a second thread below saturation,
one core per request at saturation — serial and fused stream through
the same SIMD row kernel, so a URL's bytes never depend on load. The
speed profile is `OXIMG_JPEG_PROGRESSIVE=0` (baseline jpegli: entropy
coding leaves the latency tail and per-request CPU drops ~1.2 ms):
output lands at libjpeg-turbo size for this source at unchanged
quality, ahead of imgproxy at every concurrency in this table.

The default keeps the 10% smaller progressive output and leads the
real-photo DIV2K harness (196-197 req/s on this box's 2-cpu pinned
replica); its residual throughput gap here — 4% at c=1, 5-6% at
saturation on this one synthetic — is the deliberate quality work
itself (2x-supersampled Lanczos, progressive jpegli), not overhead:
the resize kernels, staging, and IDCT were each profiled to their
practical floors (a prototyped AVX2 4x4 IDCT measured +3% over
mozjpeg's SSE2 assembly, which already sustains ~4 IPC on Zen4 —
left alone deliberately).

This synthetic is the most imgproxy-favorable shape we know: a
Huffman-heavy source (entropy decode is ~47% of oximg's request CPU
and scale-invariant) resized 2:1, where imgproxy's shrink-on-load
decodes at 1/4 resolution — skipping the supersampling that buys the
quality column above — so its per-request CPU is lower, and
per-request CPU is all that matters at SMT saturation. On the
real-photo DIV2K harness below, oximg leads the same JPEG matchup on
every box measured.

macOS numbers for this shape are withheld: the M2 box carries
fluctuating background load that swamps a ±10% effect; the Zen4
matrix above plus the M2 single-connection latency pair are the
reproducible facts.

### Pure HTTP layer (`/health`, zero image work), N=20000/50000

| Server | req/s (no keep-alive) | req/s (keep-alive) |
|---|---|---|
| oximg (axum/hyper) | **30,227** | **107,562** |
| imgproxy (Go net/http) | 9,010 | 10,181 |

Fixed HTTP overhead is under 1% of image work on the resize path for
both servers.

## imgproxy's official benchmark harness (JPEG, PNG, WebP, and AVIF)

[imgproxy's current benchmark](https://imgproxy.net/blog/image-processing-servers-benchmark/)
([harness](https://github.com/imgproxy/image-servers-benchmark)) replaces
the gist below: 100 DIV2K photographs served by nginx over HTTP, fit into
512x512 (JPEG q80, WebP q75, AVIF q65, PNG default), k6 with 2 VUs for 5 minutes,
everything in Docker. Run here on the Ryzen 7 8745HS with all services
pinned to 2 cores (`cpuset: "0-1"`) to approximate the 2-vCPU c7i.large
used in their published results; oximg added via
[bench/image-servers-benchmark.patch](bench/image-servers-benchmark.patch)
(a compose service and a k6 URL case) and fetching sources from nginx
like every other contender (`OXIMG_SOURCE_BASE_URL`).

req/s (p95 latency); all runs 100% successful checks:

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **192.7** (92 ms) | **76.6** (227 ms) | **72.6** (242 ms) | **36.5** (469 ms) |
| imgproxy | 155.8 (121 ms) | 30.5 (617 ms) | 46.0 (412 ms) | 33.4 (545 ms) |
| imagor 1.9.2 | 143.1 (169 ms) | 35.8 (670 ms) | 44.6 (493 ms) | 24.5 (920 ms) |
| thumbor 7.x | 106.8 (188 ms) | 18.4 (1150 ms) | 33.7 (616 ms) | 30.8 (646 ms) |

The relative order of the other three matches imgproxy's published
c7i.large results. PNG output at these settings measures 448KB per
image vs libvips' 482KB default. Output quality is measured in
[bench/quality/QUALITY.md](bench/quality/QUALITY.md).

AVIF (oximg built with `--features avif`: SVT-AV1 encode at the
revision pinned in the Dockerfile, dav1d decode) is decode-heavy for
every server. Nominal quality numbers are not comparable across
encoders, so the quality table in QUALITY.md is the other half of this
cell: at the defaults, oximg's 10-bit tune=ssim output is smaller than
imgproxy's q65 output and scores +6.7 SSIMULACRA2; at matched nominal
q65 (`OXIMG_AVIF_QUALITY=65`) it spends 28% more bytes and scores
+12.1.

### AVIF with alpha

The DIV2K dataset has no alpha, so this variant re-encodes the same 99
sources with a synthetic alpha ramp (`avifenc -s 8 -q 65`) and runs the
identical 512x512-fit AVIF-out cell. oximg carries alpha as a second
SVT-AV1 auxiliary-image encode plus a second dav1d decode:

| Server | req/s (p95) |
|---|---|
| oximg (defaults) | **32.1** (531 ms) |
| thumbor 7.x | 27.3 (738 ms) |
| imagor 1.9.2 | 27.2 (720 ms) |
| imgproxy | 26.2 (684 ms) |

All runs 100% successful checks; every server's output carries the
alpha item (verified with avifdec).

### Cross-format cells (our extension of the harness)

The official harness only measures same-format cells: its single
`FORMAT` variable both selects the source files (by extension) and
names the output format, so every published cell is jpg→jpg, png→png,
webp→webp, or avif→avif. Cross-format conversion is not covered
upstream.

Our patch adds an `OUT_FORMAT` variable: `FORMAT` keeps selecting the
sources, `OUT_FORMAT` overrides the output for every contender using
each server's native syntax (oximg `@{fmt}`, imgproxy `f:{fmt}`,
imagor/thumbor `filters:format({fmt})`), and the quality follows the
output format with the harness's own mapping (JPEG q80, WebP q75,
AVIF q65) so cross-format cells stay comparable with the same-format
cells above. Example: `FORMAT=jpg OUT_FORMAT=avif` measures
JPEG-source → AVIF-output across all four servers.

Converting from JPEG sources swaps the expensive source decode for
oximg's cheapest one (streaming mozjpeg with DCT shrink-on-load), so
JPEG→WebP runs ~2x the WebP→WebP cell and JPEG→AVIF ~3x the AVIF→AVIF
cell.

Local Ryzen harness, measured 2026-07-04 on the pre-fused-YUV build
(same cpuset 0-1 environment as the table above; req/s, p95 in
parentheses):

| Cell | oximg | imgproxy |
|---|---|---|
| JPEG→WebP | **158.8** (17 ms) | 81.5 (34 ms) |
| JPEG→AVIF | **115.0** (23 ms) | 102.2 (28 ms) |

AWS reference instances, measured 2026-07-05 in the wholesale re-run
(fresh instances, current build, same run as the tables in the next
section):

| c7i.large (x86-64) | oximg | imgproxy |
|---|---|---|
| JPEG→WebP | **65.3** (41 ms) | 35.3 (73 ms) |
| JPEG→AVIF | 44.6 (57 ms) | 44.9 (59 ms) |

| c7g.large (Graviton3) | oximg | imgproxy |
|---|---|---|
| JPEG→WebP | **79.3** (33 ms) | 37.0 (69 ms) |
| JPEG→AVIF | **56.5** (46 ms) | 52.7 (50 ms) |

JPEG→WebP leads imgproxy ~2x everywhere. JPEG→AVIF leads on Graviton3
(+7%) and the Ryzen (+13%) and lands at parity on c7i (-0.7%) — while
encoding at oximg's default operating point (10-bit tune=ssim q55),
which produces smaller files at higher SSIMULACRA2 than the q65 the
harness hands the competitors (see
[bench/quality/QUALITY.md](bench/quality/QUALITY.md)); nominal
qualities are not comparable across encoders.

The c7i cell traces to SMT: c7i.large is one physical core running two
hyperthreads, and pinning this Ryzen harness to an SMT sibling pair
(cpuset 0,8) reproduces the effect — oximg's lead narrows from +13% to
+3% (oximg loses 28% to SMT contention, imgproxy 22%; SVT-AV1's dense
vector kernels contend harder than libaom's). Fusing the RGB→YUV
conversion into the decode overlap (with AVX2 conversion rows)
measured +3.5-4% on JPEG→AVIF in interleaved A/B on both topologies
with bytes unchanged, and moved the c7i cell from -3.3% to the parity
above; it is included in the 2026-07-05 tables.

At this point the cell is encode-work-bound at the default operating
point: the SVT session setup (~1ms) also moved into the decode overlap
(bytes unchanged), which cut light-load latency by ~1ms/request but —
as interleaved A/B confirms — leaves the saturated 2-VU cell unmoved,
since only removing work (not relocating it) changes that number.

The remaining lever is the operating point itself: `OXIMG_AVIF_SPEED`
(the SVT preset; default 8) at 9 removes ~28% of the encode work for
-0.6 SSIMULACRA2 at unchanged bytes (quality data in QUALITY.md).
Verified on a real c7i.large with interleaved official 5-minute cells
(two rounds per arm, 100% checks, spread under 0.1 req/s per arm):

| c7i.large JPEG→AVIF | req/s | p95 |
|---|---|---|
| oximg `OXIMG_AVIF_SPEED=9` | **53.3** | **48 ms** |
| imgproxy (same-run anchor) | 45.8 | 58 ms |
| oximg default (preset 8) | 44.8 | 57 ms |

+19% over the default and +16% over imgproxy — the knob turns the
parity cell into a clear lead for deployments that accept the
operating-point trade. The default stays at preset 8: quality per byte
is the shipped identity, and every published cell is measured there.

Fused-overlap A/B for cross-format (`XFMT=1 FEATURES=avif
bench/native.sh`, Apple M2 Max, 2000x1333 plasma JPEG → 500x500, ab
c=8; "serial" pins `OXIMG_OVERLAP=0`):

| Output | serial req/s (p50) | fused req/s (p50) |
|---|---|---|
| JPEG (bare URL) | 609 (13 ms) | 604 (13 ms) |
| JPEG (`@jpeg`) | 609 (13 ms) | 596 (13 ms) |
| WebP (`@webp`) | 469 (18 ms) | 521 (16 ms) |
| AVIF (`@avif`) | 385 (22 ms) | 399 (20 ms) |

The `@jpeg` row matching the bare row is the no-regression check: an
explicit same-format token takes the identical code path. Single-
request medians (c=1, same box): JPEG→WebP 17.8→15.7 ms (-12%),
JPEG→AVIF 22.3→20.1 ms (-10%) — cross-format requests overlap the
mozjpeg decode with the SIMD resize on a second thread (the same
`OXIMG_OVERLAP` gate as same-format JPEG), leaving only the one-shot
target encode outside the decode wall.

## Official harness on real AWS hardware (c7i.large and c7g.large)

The same harness run unmodified on the instance types imgproxy uses for
its published results, deployed with the harness's own CloudFormation
template (Ubuntu 24.04, Docker, k6 with 2 VUs for 5 minutes per cell,
all defaults). req/s (p95); all runs 100% successful checks. All four
servers re-measured together per instance in one wholesale run
(2026-07-05, fresh instances, oximg built from source at the
cross-format + fused-overlap state).

c7i.large (x86-64, 2 vCPU = one SMT core):

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **78.7** (33 ms) | **32.8** (79 ms) | **30.9** (92 ms) | **15.6** (181 ms) |
| imgproxy | 67.0 (40 ms) | 14.3 (187 ms) | 20.3 (136 ms) | 15.2 (190 ms) |
| imagor 1.9.2 | 58.7 (44 ms) | 15.5 (174 ms) | 17.7 (152 ms) | 10.1 (283 ms) |
| thumbor 7.x | 50.0 (50 ms) | 8.7 (304 ms) | 14.0 (187 ms) | 12.1 (225 ms) |

c7g.large (Graviton3, 2 physical cores):

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **91.2** (28 ms) | **39.0** (66 ms) | **41.5** (70 ms) | **23.4** (124 ms) |
| imgproxy | 68.0 (39 ms) | 21.0 (123 ms) | 25.4 (110 ms) | 20.3 (139 ms) |
| imagor 1.9.2 | 57.5 (44 ms) | 22.1 (115 ms) | 19.7 (133 ms) | 13.7 (204 ms) |
| thumbor 7.x | 63.2 (41 ms) | 12.5 (210 ms) | 20.2 (129 ms) | 14.7 (196 ms) |

Notes:

- Deltas vs the previous tables (2026-06, retired by this run): JPEG
  on c7g jumped 81.3 → 91.2 — the fused-path scratch-pool fix (kernel
  scratch now returns to the request thread's pool instead of dying
  with the ephemeral worker's TLS) landed in between; the remaining
  oximg cells and every competitor cell moved within the ~3%
  instance-to-instance variance the same-run anchors bound (e.g.
  imgproxy JPEG 68.4 → 67.0/68.0, AVIF 15.6 → 15.2 and 20.1 → 20.3).
- The AVIF cells reflect the current defaults and the pinned SVT-AV1
  revision. dav1d's in-frame threading works on Graviton3 (1.9x on two
  cores, verified against dav1d 1.4.1/1.5.1/1.5.3 with minimal
  repros).
- History of what previous re-measures covered (encoder upgrade,
  index-free scalar conversion paths, architecture-aware decode-thread
  default, counter-guided aarch64 work) is in the git log of this
  file.

## Reproduction of the imgproxy benchmark gist (superseded)

Methodology from
[DarthSim's benchmark gist](https://gist.github.com/DarthSim/9d971d2859f3714a29cf8ce094b3fc55):
a real photograph of Wat Arun (JPEG, 7360x4912, 29MB —
[the original image from Wikimedia Commons](https://commons.wikimedia.org/wiki/File:The_sculptures_of_two_mythical_giant_demons,_Thotsakan_and_Sahatsadecha,_guarding_the_eastern_gate_of_the_main_chapel_of_Wat_Arun,_Bangkok.jpg)),
resized to fit 500x500, `ab -n 1000 -c 4`, default settings. Ryzen 7
8745HS, all servers as Docker containers (thumbor run with
`--processes=16` to use the machine; the diverse column requests 4
distinct widths so request coalescing cannot serve duplicates).

| Server | req/s | mean | peak memory | output | diverse req/s |
|---|---|---|---|---|---|
| oximg (defaults) | **24.1** | **166 ms** | **17 MB** | 47 KB | **23** |
| thumbor 7.x | 21.7 | 185 ms | 648 MB | 44 KB | 20 |
| imgproxy 4.0.11 | 19.3 | 208 ms | 430 MB | 44 KB | 19 |
| imagor 1.9.2 | 18.6 | 215 ms | 276 MB | 88 KB | 17 |

## Encoder presets

Linux x86_64 native (Ryzen 7 8745HS, `bench/native.sh`, c=16):

| PRESET | medium diverse | medium single-URL | large diverse |
|---|---|---|---|
| `jpegli` (default) | 639 | 1073 | 121 |
| `fast` (mozjpeg baseline profile) | **696** | **1157** | 121 |
| `small` (mozjpeg trellis+progressive) | 445 | 753 | 114 |

Apple M2 Max (c=12, single URL, coalescing active — relative values):
`jpegli` 685 / `fast` 751 / `small` 456 req/s; output sizes for
test-medium: 20.1 / 22.9 / 18.6 KB.

Quality per byte for each encoder is measured in
[bench/quality/QUALITY.md](bench/quality/QUALITY.md).

## Notes

- Measurement provenance: the official-harness tables (local Ryzen and
  AWS) reflect the current code; the oximg rows were re-measured after
  each significant pipeline change, and competitor rows are re-measured
  whenever the environment changes (same-box anchors bound
  instance-to-instance variance at ~3%). The earlier sustained-load,
  macOS, and gist-reproduction sections are historical measurements of
  the JPEG path and predate the format expansion; their competitor
  ratios still hold for that path.
- The sustained-load tables were measured with `PRESET=fast` as the
  encoder, before jpegli became the default; the preset table shows the
  relative cost of the current default.
- oximg defaults resize in linear light with 1.7x DCT decode headroom;
  speed mode (`OXIMG_RESIZE=srgb OXIMG_DCT_MARGIN=1.0`) matches the
  competitors' processing approach. Output quality for both settings is
  quantified in [bench/quality/QUALITY.md](bench/quality/QUALITY.md).
- The plasma-fractal test images compress differently from real photos;
  both servers consume the same files, so relative values hold. The
  quality benchmark uses Kodak and real photographs.
- imgproxy is a full-featured product (many formats, watermarks, a rich
  processing URL grammar); oximg covers same-format resizing for JPEG,
  PNG, WebP, and AVIF with URL signing and HTTP sources.

## Reproduce

```sh
cargo build --release
magick -size 7360x4912 plasma:fractal -colorspace sRGB -quality 92 images/test-large.jpg
magick -size 2000x1333 plasma:fractal -colorspace sRGB -quality 92 images/test-medium.jpg
IMAGES_DIR=./images PORT=8081 ./target/release/oximg &
IMGPROXY_BIND=:8082 IMGPROXY_LOCAL_FILESYSTEM_ROOT=$PWD/images IMGPROXY_QUALITY=80 imgproxy &
./bench/bench.sh oximg "http://127.0.0.1:8081/resize/500/500/test-large.jpg" <rs-pid>
./bench/bench.sh imgproxy "http://127.0.0.1:8082/insecure/resize:fit:500:500/plain/local:///test-large.jpg" <go-pid>
```

Docker (Linux): build with the repo `Dockerfile`; run competitors from
their official images with the same `images/` volume; drive load with
`ab` (e.g. from `httpd:2.4-alpine` with `--network host`); read
`/sys/fs/cgroup/memory.peak` and `cpu.stat` inside each container.
