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

Measured 2026-07-04 (this repo's Docker image built from source on
each machine, all runs 100% successful checks). Converting from JPEG
sources swaps the expensive source decode for oximg's cheapest one
(streaming mozjpeg with DCT shrink-on-load), so JPEG→WebP runs ~2x the
WebP→WebP cell and JPEG→AVIF ~3x the AVIF→AVIF cell.

Local Ryzen harness (same cpuset 0-1 environment as the table above;
req/s, p95 in parentheses):

| Cell | oximg | imgproxy |
|---|---|---|
| JPEG→WebP | **158.8** (17 ms) | 81.5 (34 ms) |
| JPEG→AVIF | **115.0** (23 ms) | 102.2 (28 ms) |

AWS reference instances (same harness deployment as the section
below; oximg p95 from a second identical pass, imgproxy p95 not
captured on AWS):

| c7i.large (x86-64) | oximg | imgproxy |
|---|---|---|
| JPEG→WebP | **65.2** (41 ms) | 35.1 |
| JPEG→AVIF | 43.3 (59 ms) | 44.8 |

| c7g.large (Graviton3) | oximg | imgproxy |
|---|---|---|
| JPEG→WebP | **78.2** (34 ms) | 36.9 |
| JPEG→AVIF | **57.2** (45 ms) | 52.5 |

JPEG→WebP leads imgproxy ~2x everywhere. JPEG→AVIF leads on Graviton3
(+9%) and the Ryzen (+13%) and lands ~3% behind on c7i — while
encoding at oximg's default operating point (10-bit tune=ssim q55),
which produces smaller files at higher SSIMULACRA2 than the q65 the
harness hands the competitors (see
[bench/quality/QUALITY.md](bench/quality/QUALITY.md)); nominal
qualities are not comparable across encoders.

The c7i gap traces to SMT: c7i.large is one physical core running two
hyperthreads, and pinning this Ryzen harness to an SMT sibling pair
(cpuset 0,8) reproduces it — oximg's lead narrows from +13% to +3%
(oximg loses 28% to SMT contention, imgproxy 22%; SVT-AV1's dense
vector kernels contend harder than libaom's). A follow-up landed after
these tables: the fused AVIF path now converts YUV row-by-row inside
the decode overlap with AVX2 conversion rows, measuring +3.5-4% on
JPEG→AVIF in interleaved A/B on both topologies (bytes unchanged) —
enough to roughly close the c7i gap; the AWS cells will be refreshed
at the next release re-measure.

The same runs re-verified every same-format cell: oximg and the
same-run imgproxy anchors landed within the ~3% instance variance of
the tables below on both instance types — except JPEG on c7g.large,
which measured 90.8 req/s (28 ms p95) against the published 81.3 with
its anchor unchanged: the fused-path scratch-pool fix landed between
the runs. The full tables will be refreshed wholesale at the next
release re-measure.

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
all defaults). req/s (p95); all runs 100% successful checks.

c7i.large (x86-64, 2 vCPU = one SMT core):

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **81.8** (32 ms) | **32.9** (79 ms) | **31.0** (91 ms) | **16.0** (176 ms) |
| imgproxy | 68.4 (40 ms) | 14.3 (187 ms) | 20.5 (137 ms) | 15.6 (185 ms) |
| imagor 1.9.2 | 59.4 (43 ms) | 15.5 (174 ms) | 17.5 (153 ms) | 10.2 (283 ms) |
| thumbor 7.x | 52.1 (48 ms) | 8.7 (304 ms) | 14.1 (185 ms) | 13.0 (210 ms) |

c7g.large (Graviton3, 2 physical cores):

| Server | JPEG | PNG | WebP | AVIF |
|---|---|---|---|---|
| oximg (defaults) | **81.3** (31 ms) | **37.6** (68 ms) | **40.0** (72 ms) | **23.1** (126 ms) |
| imgproxy | 68.4 (39 ms) | 21.0 (123 ms) | 25.4 (111 ms) | 20.1 (141 ms) |
| imagor 1.9.2 | 57.7 (44 ms) | 22.0 (116 ms) | 19.7 (132 ms) | 13.7 (208 ms) |
| thumbor 7.x | 63.3 (41 ms) | 12.4 (209 ms) | 20.5 (128 ms) | 14.8 (195 ms) |

Notes:

- The AVIF cells reflect the current defaults and the pinned SVT-AV1
  revision; both were re-measured together with a same-box imgproxy
  anchor (c7i: 15.7, c7g: 20.1) after the encoder upgrade, the
  index-free scalar conversion paths, and the architecture-aware
  decode-thread default landed.
- The c7g oximg cells and the imgproxy AVIF cell were re-run together
  on a fresh c7g.large after the counter-guided aarch64 work (the NEON
  resize kernel and schedule, TBL deinterleaving, NEON YUV-to-RGB
  conversion, and scratch-buffer hygiene); the same-run imgproxy anchor
  (19.7 vs its earlier 20.1-20.4) bounds instance-to-instance variance
  at ~3%. dav1d's in-frame threading works on Graviton3 (1.9x on two
  cores, verified against dav1d 1.4.1/1.5.1/1.5.3 with minimal repros).

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
